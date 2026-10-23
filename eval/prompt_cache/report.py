"""Loads the runs under a results directory, judges every request, and writes analysis.json per run plus report.md."""

import json
from pathlib import Path

import analyze as an
import gguf
import tap as tp

THINK_MARK = "<|im_start|>assistant"


# ---------------------------------------------------------------- renderers for the paths where the server does not hand over the prompt

def produced_message(call):
    """The model's reply as an Ollama-shaped assistant message, tool arguments in the order the model wrote them."""
    a = call.agg
    m = {"role": "assistant", "content": a.get("content") or "", "thinking": a.get("thinking") or ""}
    if a.get("tool_calls"):
        m["tool_calls"] = [{"id": t.get("id", ""), "function": {"name": t.get("name"), "arguments": t.get("arguments") or {}}} for t in a["tool_calls"]]
    return m


def mirror_render(call):
    """Ollama's own prompt for a renderer model (qwen3.5), through the Python port in qwen35_mirror.py; and the same
    conversation with the model's reply appended as it wrote it (the prompt the next request has to start with)."""
    import qwen35_mirror as q
    think = call.body.get("think")
    think = think if isinstance(think, bool) or think is None else True
    marks = []
    text = q.render(call.messages, call.tools, think, marks)
    starts = [0] + marks[1:]
    cut = text.rfind(THINK_MARK)
    no_gp = cut if cut >= 0 and call.messages and call.messages[-1].get("role") != "assistant" else len(text)
    call.render = {"text": text, "no_gp_len": no_gp, "starts": starts, "via": "qwen35 mirror (Ollama's renderer, ported)"}
    call.ideal = {"text": q.render(call.messages + [produced_message(call)], call.tools, think)}


def _openai_messages(msgs):
    """Ollama-shaped messages as the OpenAI ones llama-server's template sees (arguments as objects, thinking as reasoning_content)."""
    out = []
    for m in msgs:
        o = {"role": m.get("role", "user"), "content": m.get("content") or ""}
        if m.get("thinking"):
            o["reasoning_content"] = m["thinking"]
        if m.get("tool_call_id"):
            o["tool_call_id"] = m["tool_call_id"]
        if m.get("tool_name"):
            o["name"] = m["tool_name"]
        if m.get("tool_calls"):
            o["tool_calls"] = [{"id": c.get("id", ""), "type": "function", "function": {"name": c["function"]["name"], "arguments": _obj(c["function"].get("arguments"))}} for c in m["tool_calls"]]
        out.append(o)
    return out


def _obj(a):
    if isinstance(a, str):
        try:
            return json.loads(a)
        except ValueError:
            return a
    return a if a is not None else {}


def llama_tools(tools):
    """Tools the way llama-server hands them to the template: type, then name, description, parameters."""
    out = []
    for t in tools or []:
        f = t.get("function") or {}
        o = {"name": f.get("name")}
        for k in ("description", "parameters"):
            if k in f:
                o[k] = f[k]
        out.append({"type": "function", "function": o})
    return out


class JinjaRenderer:
    """The model's GGUF chat template run in jinja2, set up the way llama.cpp's minja runs it. Checked against the real server
    wherever the audit has both (the `llama` runs): the report says how many prompts were byte-identical."""

    def __init__(self, template):
        import jinja2
        env = jinja2.Environment(trim_blocks=True, lstrip_blocks=True, keep_trailing_newline=False, extensions=["jinja2.ext.loopcontrols"])
        env.filters["tojson"] = lambda v, **kw: json.dumps(v, ensure_ascii=False)

        def raise_exception(msg):
            raise ValueError(msg)

        env.globals.update(raise_exception=raise_exception, strftime_now=lambda f: "2026-01-01")
        self.t = env.from_string(template)

    def text(self, messages, tools, think, add_gp=True):
        kw = {"messages": messages, "tools": llama_tools(tools) or None, "add_generation_prompt": add_gp, "bos_token": "", "eos_token": ""}
        if isinstance(think, bool):
            kw["enable_thinking"] = think
        return self.t.render(**kw)

    def render(self, call):
        """This request as Ollama hands it to llama-server (its chat path sends no reasoning_content: llamaServerChatMessage in
        llm_llama_server.go converts role, content, tool calls and tool names only), and the conversation as the model wrote it."""
        think = call.body.get("think")
        think = think if isinstance(think, bool) else None
        full = _openai_messages(call.messages)
        msgs = [{k: v for k, v in m.items() if k != "reasoning_content"} for m in full]
        text = self.text(msgs, call.tools, think)
        bare = self.text(msgs, call.tools, think, False)
        starts = [0]
        for k in range(1, len(msgs)):
            try:
                starts.append(len(self.text(msgs[:k], call.tools, None, False)))
            except ValueError:  # a template that needs a user message cannot render a system message alone
                end = text.find("<|im_end|>")
                starts.append(end + len("<|im_end|>\n") if k == 1 and end >= 0 else starts[-1])
        call.render = {"text": text, "no_gp_len": len(bare) if text.startswith(bare) else len(text), "starts": starts, "via": "Ollama's llama-server chat request (no reasoning_content) through the GGUF template in jinja2"}
        call.ideal = {"text": self.text(full + _openai_messages([produced_message(call)]), call.tools, think, False)}

    def render_openai(self, body):
        """For validating against the server: the same template over an OpenAI-style request body."""
        kw = body.get("chat_template_kwargs") or {}
        msgs = []
        for m in body["messages"]:
            m = dict(m)
            for c in m.get("tool_calls") or []:
                c["function"] = dict(c["function"], arguments=_obj(c["function"].get("arguments")))
            msgs.append(m)
        k = {"messages": msgs, "tools": llama_tools(body.get("tools")) or None, "add_generation_prompt": True, "bos_token": "", "eos_token": ""}
        k.update({a: b for a, b in kw.items()})
        return self.t.render(**k)


class Tokenizer:
    """Token counts from a CPU-only llama-server on the model's weights (for the runs where the model's server is Ollama's)."""

    def __init__(self, blob):
        self.blob, self.proc, self.port, self.memo = blob, None, None, {}

    def count(self, text):
        import templates as tm
        if text not in self.memo:
            if self.port is None:
                self.proc, self.port = tm.cpu_server(self.blob)
            if self.port is None:
                return None
            self.memo[text] = len(tm._post(self.port, "/tokenize", {"content": text, "add_special": False, "parse_special": True}, 120).get("tokens", []))
        return self.memo[text]

    def close(self):
        if self.proc:
            self.proc.kill()
            self.proc.wait()


# ---------------------------------------------------------------- a run

def load_calls(run_dir):
    run_dir = Path(run_dir)
    recs = []
    p = run_dir / "tap.jsonl"
    if p.exists():
        recs = [json.loads(l) for l in p.read_text(encoding="utf-8").splitlines() if l.strip()]
    recs.sort(key=lambda r: r["t_start"])
    calls, unloads = [], []
    ahead = None  # the engine's read-ahead into the slot (`/completion`, n_predict 0): tokens the server evaluated
    for r in recs:
        req, resp = tp.load_record_bodies(str(run_dir / "raw"), r)
        c = an.Call(r, req, resp, r.get("log_text", ""))
        if r["path"] == "/completion" and r.get("status") == 200:
            try:
                if json.loads(req).get("n_predict") == 0:
                    ahead = json.loads(resp).get("tokens_evaluated")
            except ValueError:
                pass
        if c.is_model_call and c.status == 200:
            c.preread_tokens, ahead = ahead, None
            calls.append(c)
        elif r["path"] == "/api/generate" and '"keep_alive":0' in req.replace(" ", ""):
            unloads.append(r["t_start"])
    ports = run_dir / "tap.jsonl.ports"  # the engine's wrapper writes a line per server it starts
    if ports.exists():
        unloads += [json.loads(l)["at"] for l in ports.read_text().splitlines() if l.strip()][1:]
    return calls, unloads


def analyse_run(run_dir):
    run_dir = Path(run_dir)
    meta = json.loads((run_dir / "meta.json").read_text(encoding="utf-8"))
    model, engine = meta["model"], meta["engine"]
    facts = gguf.manifest_facts(model)
    calls, unloads = load_calls(run_dir)
    # which calls are the conductor's conversation: the stream with the most requests
    by_key = {}
    for c in calls:
        by_key.setdefault(c.key, []).append(c)
    main_key = max(by_key, key=lambda k: len(by_key[k])) if by_key else None
    side = {k: len(v) for k, v in by_key.items() if k != main_key}
    stream = by_key.get(main_key, [])
    validated = {"server_vs_jinja": None}
    jr = None
    if engine == "llama":
        for c in stream:
            an.attach_render_from_server(c)
        try:
            tpl = gguf.read_header(facts["blob"]).get("tokenizer.chat_template")
            jr = JinjaRenderer(tpl)
            same = n = 0
            for c in stream:
                if c.render:
                    n += 1
                    same += jr.render_openai(c.body) == c.render["text"]
            validated["server_vs_jinja"] = f"{same}/{n} prompts byte-identical"
        except Exception as e:
            validated["server_vs_jinja"] = f"not checked ({e!r:.80})"
    else:
        for c in stream:
            try:
                if facts["renderer"] == "qwen3.5":
                    mirror_render(c)
                elif facts["template"] is not None:
                    validated["render"] = "Ollama's own Go template engine renders this model's prompt; it is not mirrored, so no bytes are located"
                    break
                else:
                    jr = jr or JinjaRenderer(gguf.read_header(facts["blob"]).get("tokenizer.chat_template"))
                    jr.render(c)
            except Exception as e:
                c.render = None
                validated["render_error"] = repr(e)[:200]
    tk = None
    if engine == "ollama" and stream:
        tk = Tokenizer(facts["blob"])
        try:
            for c in stream:
                if c.render and c.ideal:
                    c.render_tokens, c.ideal_tokens = tk.count(c.render["text"]), tk.count(c.ideal["text"])
        except Exception as e:
            validated["tokenizer"] = repr(e)[:200]
        finally:
            tk.close()
    if engine == "llama":
        dropper = "the model's chat template"
    elif facts["renderer"]:
        dropper = f"Ollama's {facts['renderer']} renderer"
    elif "template" in facts["layers"]:
        dropper = "Ollama's template"
    else:
        dropper = "Ollama (its llama-server chat request carries no reasoning_content, so the template prints an empty think block)"
    rows = an.judge(stream, restarts=meta.get("restarts", []), unloads=unloads, dropper=dropper)
    # which user-facing step each request belongs to is not needed; the message count and tail say enough
    for n, (c, r) in enumerate(zip(stream, rows), 1):
        r["n"] = n
        last = c.messages[-1] if c.messages else {}
        r["last_role"] = last.get("role")
        r["produced"] = {"thinking_chars": len(c.agg.get("thinking") or ""), "content_chars": len(c.agg.get("content") or ""),
                         "tool_calls": [t.get("name") for t in c.agg.get("tool_calls") or []]}
        r["render_via"] = c.render["via"] if c.render else None
    out = {"model": model, "engine": engine, "renderer": facts["renderer"], "parser": facts["parser"], "layers": facts["layers"], "requests": len(stream), "side_requests": side,
           "rows": rows, "validated": validated, "seconds": meta.get("seconds"), "turns": meta.get("turns"), "coverage": coverage(meta, stream)}
    (run_dir / "analysis.json").write_text(json.dumps(out, indent=1, ensure_ascii=False, default=str), encoding="utf-8")
    return out


def coverage(meta, stream):
    """What the model actually did of what the script asked, so a quiet run is not read as a clean one."""
    tools = {}
    for t in meta.get("turns") or []:
        for x in t.get("tools") or []:
            tools[x["tool"]] = tools.get(x["tool"], 0) + 1
    produced_thinking = sum(1 for c in stream if (c.agg.get("thinking") or ""))
    return {"tools_called": tools, "requests_with_thinking": f"{produced_thinking}/{len(stream)}",
            "interrupted": any(t.get("stop") == "interrupted" for t in meta.get("turns") or []),
            "restarted": bool(meta.get("restarts")), "notes": [n["kind"] for n in meta.get("notes") or []]}


# ---------------------------------------------------------------- the report

def short(s, n=110, table=True):
    s = (s or "").replace("\n", "\\n")
    if table:
        s = s.replace("|", "\\|")
    return s if len(s) <= n else s[: n - 1] + "…"


def breaks_of(run):
    """Every request whose cache was not whole: breaks, and the cold ones (still listed: the rule has no exception for them)."""
    return [r for r in run["rows"] if r.get("verdict") not in ("ok", "first", "no numbers") and not str(r.get("verdict")).startswith(("reset", "compaction"))]


def write_all(base):
    base = Path(base)
    runs = []
    for d in sorted(p for p in base.iterdir() if p.is_dir() and (p / "meta.json").exists()):
        try:
            runs.append(analyse_run(d))
        except Exception as e:
            (d / "analysis-failed.txt").write_text(repr(e), encoding="utf-8")
    md = ["# Prompt-cache audit", "", "Every request is judged against one rule: its prompt is the previous prompt, plus what the model generated, plus new content at the end, "
          f"so the server must serve the whole previous sequence from cache (a gap of {an.TOLERANCE} tokens or less is the stop token and newline the template re-adds). "
          "`cold` is a cache the daemon emptied by restarting; `reset` is a new conversation, `compaction` is Marlowe eliding old tool results; the one rewrite the rule allows (the report says how full the window was).", ""]
    md += ["## Summary", "", "| model | engine | requests | clean | of which template drop, nothing re-read | breaks | cold | reset or compaction | worst gap (tokens) | tokens re-read in all |", "|---|---|---|---|---|---|---|---|---|---|"]
    for r in runs:
        rows = r["rows"][1:]
        b = breaks_of(r)
        cold = [x for x in rows if str(x.get("verdict", "")).startswith("cold")]
        gaps = [x["deficit"] for x in rows if x.get("deficit") and x["verdict"] == "BREAK"]
        md.append(f"| {r['model']} | {r['engine']} | {r['requests']} | {sum(1 for x in rows if x['verdict'] == 'ok')} | {sum(1 for x in rows if x.get('template_drop'))} | {len(b) - len(cold)} | {len(cold)} | {sum(1 for x in rows if str(x['verdict']).startswith(('reset', 'compaction')))} | {max(gaps) if gaps else 0} | {sum(gaps)} |")
    md += ["", "## Every break: model x engine x cause", "", "| model | engine | n | message that produced it | first differing byte | tokens re-read | cause |", "|---|---|---|---|---|---|---|"]
    for r in runs:
        for x in breaks_of(r):
            pd = x.get("prompt_diff") or {}
            md.append("| {} | {} | {} ({} msgs) | {} | {} | {} | {} |".format(
                r["model"], r["engine"], x["n"], x["messages"],
                short(msg_name(x, pd)), f"{pd['offset']} of {pd['of']}" if pd else "n/a", x["deficit"], short(("(cold) " if str(x["verdict"]).startswith("cold") else "") + (x.get("cause") or ""), 240)))
    for r in runs:
        md += ["", f"## {r['model']} on {r['engine']}", "",
               f"renderer `{r['renderer']}`, parser `{r['parser']}`; {r['requests']} conductor requests in {r['seconds']} s; "
               f"other streams: {r['side_requests'] or 'none'}; prompt source: {sorted({x.get('render_via') or 'none' for x in r['rows']})}; server-vs-jinja check: {r['validated'].get('server_vs_jinja')}", "",
               f"coverage: {json.dumps(r['coverage'], ensure_ascii=False)}", "",
               "| n | tap # | msgs | last | read | cached | expected | gap | server | verdict |", "|---|---|---|---|---|---|---|---|---|---|"]
        for x in r["rows"]:
            md.append(f"| {x['n']} | {x['seq']} | {x['messages']} | {x.get('last_role')} | {x['total']} | {x['cached']} | {x.get('expected', '')} | {x['deficit'] if x['deficit'] is not None else ''} | "
                      f"{short(json.dumps(x.get('server') or {}), 60)} | {x['verdict']} |")
        md += [""] + [f"- n={x['n']}: {x['cause']}" for x in r["rows"] if str(x["verdict"]).startswith("compaction")]
        for x in breaks_of(r):
            md += ["", f"request n={x['n']} (tap #{x['seq']}): **{x.get('cause')}**"]
            if x.get("msg_diff"):
                md.append(f"- wire diff: message {x['msg_diff']['index']} field `{x['msg_diff']['field']}`: `{short(x['msg_diff']['prev'], 90, False)}` -> `{short(x['msg_diff']['cur'], 90, False)}`")
            if x.get("echo"):
                md.append(f"- reply vs what was sent back: {'; '.join(x['echo'])}")
            if x.get("prompt_diff"):
                pd = x["prompt_diff"]
                md.append(f"- first differing byte {pd['offset']} of {pd['of']} ({pd['region']}), message {pd['message']} ({pd.get('role')})")
                md.append(f"  - prompt it should have started with: `{short(pd['ideal_snip'], 170, False)}`")
                md.append(f"  - prompt it had:                      `{short(pd['cur_snip'], 170, False)}`")
            if x.get("consecutive"):
                cs = x["consecutive"]
                md.append(f"- the two consecutive rendered prompts first differ at byte {cs['offset']} of {cs['of']} (message {cs['message']})")
            elif "consecutive" in x:
                md.append("- the two consecutive rendered prompts: the second starts with the whole first")
            if x.get("reply_features"):
                md.append(f"- the previous reply has: {', '.join(x['reply_features'])}")
    (base / "report.md").write_text("\n".join(md) + "\n", encoding="utf-8")
    (base / "report.json").write_text(json.dumps(runs, indent=1, ensure_ascii=False, default=str), encoding="utf-8")
    return runs


def msg_name(x, pd):
    j = pd.get("message") if pd else None
    role = pd.get("role") if pd else None
    if x.get("msg_diff"):
        j = x["msg_diff"]["index"]
    return (f"message {j}" + (f" ({role})" if role else "")) if j is not None else "unknown"
