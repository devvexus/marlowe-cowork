"""Turns the taps' records of one audit run into the verdicts: which request broke the prompt cache, where, and why.

The rule being checked, for every model and engine: each request's prompt is the previous request's prompt, plus the tokens the
model generated, plus new content at the end. So the server must have found the whole previous sequence in its cache.

  expected reuse = tokens the slot held after the previous request (its prompt + what it generated)
  deficit        = expected reuse - tokens the server says it served from cache for this request

A deficit of a few tokens is the end-of-turn marker the template adds after the model's own stop token; more is a break. For
each break it localises the first differing byte of the rendered prompt, names the message that produced it, and says which
of the known causes it matches. Pure functions over parsed records, so test_audit.py runs them without a model.
"""

import hashlib
import json
import re

TOLERANCE = 8  # tokens: the stop token and a newline, which the template re-adds after the model's own end


# ---------------------------------------------------------------- what the servers answered

def agg_ollama(text):
    """An Ollama /api/chat answer (ndjson frames) as one message, key order of tool arguments kept."""
    content, thinking, calls, last = [], [], [], {}
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            fr = json.loads(line)
        except ValueError:
            continue
        m = fr.get("message") or {}
        if m.get("content"):
            content.append(m["content"])
        if m.get("thinking"):
            thinking.append(m["thinking"])
        for c in m.get("tool_calls") or []:
            calls.append({"id": c.get("id", ""), "name": c.get("function", {}).get("name"), "arguments": c.get("function", {}).get("arguments")})
        if fr.get("done"):
            last = fr
    return {"content": "".join(content), "thinking": "".join(thinking), "tool_calls": calls, "done": last.get("done", False),
            "done_reason": last.get("done_reason"), "prompt_eval_count": last.get("prompt_eval_count"), "eval_count": last.get("eval_count"),
            "error": next((json.loads(l).get("error") for l in text.splitlines() if l.startswith("{") and '"error"' in l), None)}


def agg_openai(text):
    """An OpenAI-style SSE answer from llama-server as one message with its own timings."""
    content, thinking, calls, timings, progress, finish = [], [], {}, {}, {}, None
    for line in text.splitlines():
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if not data or data == "[DONE]":
            continue
        try:
            fr = json.loads(data)
        except ValueError:
            continue
        if "error" in fr:
            return {"error": fr["error"], "content": "", "thinking": "", "tool_calls": [], "timings": {}}
        if fr.get("timings"):
            timings = fr["timings"]
        if fr.get("prompt_progress"):
            progress = fr["prompt_progress"]
        ch = (fr.get("choices") or [{}])[0]
        d = ch.get("delta") or {}
        if d.get("content"):
            content.append(d["content"])
        if d.get("reasoning_content"):
            thinking.append(d["reasoning_content"])
        for tc in d.get("tool_calls") or []:
            c = calls.setdefault(tc.get("index", 0), {"id": "", "name": "", "arguments": ""})
            if tc.get("id"):
                c["id"] = tc["id"]
            f = tc.get("function") or {}
            c["name"] += f.get("name") or ""
            c["arguments"] += f.get("arguments") or ""
        if ch.get("finish_reason"):
            finish = ch["finish_reason"]
    return {"content": "".join(content), "thinking": "".join(thinking), "tool_calls": [calls[k] for k in sorted(calls)], "timings": timings,
            "progress": progress, "finish_reason": finish, "error": None}


# ---------------------------------------------------------------- what Ollama's runner logged

LOG_TASK = re.compile(r"slot\s+\S+: id\s+(\d+) \| task (-?\d+) \| (.*)")


def parse_server_log(text):
    """The llama-server lines (Ollama relays them into server.log) as one dict per task."""
    tasks, order, lcp = {}, [], None
    for line in text.splitlines():
        m = LOG_TASK.search(line)
        if not m:
            continue
        slot, task, rest = int(m.group(1)), int(m.group(2)), m.group(3)
        if task == -1:
            s = re.search(r"selected slot by LCP similarity, sim_best = ([\d.]+).*f_keep = ([\d.]+)", rest)
            if s:
                lcp = {"slot": slot, "sim_best": float(s.group(1)), "f_keep": float(s.group(2))}
            continue
        t = tasks.get(task)
        if t is None:
            t = tasks[task] = {"task": task, "slot": slot, "restored": [], "forced_full": False, "cached_marks": []}
            order.append(task)
            if lcp and lcp["slot"] == slot:
                t["sim_best"], t["f_keep"] = lcp["sim_best"], lcp["f_keep"]
        s = re.search(r"new prompt, n_ctx_slot = (\d+), n_keep = (-?\d+), task.n_tokens = (\d+)", rest)
        if s:
            t["n_ctx_slot"], t["n_tokens"] = int(s.group(1)), int(s.group(3))
        s = re.search(r"cached n_tokens = (\d+)", rest)
        if s:
            t["cached_marks"].append(int(s.group(1)))
        s = re.search(r"restored context checkpoint \(pos_min = (\d+), pos_max = (\d+), n_tokens = (\d+), n_past = (\d+)", rest)
        if s:
            t["restored"].append({"pos_max": int(s.group(2)), "n_tokens": int(s.group(3)), "n_past": int(s.group(4))})
        if "forcing full prompt re-processing" in rest:
            t["forced_full"] = True
        s = re.search(r"prompt eval time =\s+([\d.]+) ms /\s+(\d+) tokens", rest)
        if s:
            t["prompt_eval_ms"], t["prompt_eval_n"] = float(s.group(1)), int(s.group(2))
        s = re.search(r"^\s*eval time =\s+([\d.]+) ms /\s+(\d+) tokens", rest)
        if s:
            t["eval_n"] = int(s.group(2))
        s = re.search(r"stop processing: n_tokens = (\d+), truncated = (\d+)", rest)
        if s:
            t["end_n_tokens"], t["truncated"] = int(s.group(1)), int(s.group(2))
    return [tasks[k] for k in order]


def numbers_ollama(log_text, agg):
    """(total prompt tokens, tokens served from cache, tokens generated, slot length after, details) for one /api/chat call."""
    tasks = [t for t in parse_server_log(log_text) if t.get("n_tokens")]
    gen = agg.get("eval_count")
    if not tasks:
        return {"total": agg.get("prompt_eval_count"), "cached": None, "gen": gen, "end": None, "source": "frames"}
    want = agg.get("prompt_eval_count")
    t = next((t for t in tasks if want and t["n_tokens"] == want), None) or max(tasks, key=lambda t: t["n_tokens"])
    cached = t["cached_marks"][0] if t["cached_marks"] else (t["n_tokens"] - t["prompt_eval_n"] if "prompt_eval_n" in t else None)
    return {"total": t["n_tokens"], "cached": cached, "gen": t.get("eval_n", gen), "end": t.get("end_n_tokens"), "source": "ollama server.log",
            "slot": t["slot"], "sim_best": t.get("sim_best"), "f_keep": t.get("f_keep"), "restored": t["restored"], "forced_full": t["forced_full"]}


def numbers_openai(agg, extra=None):
    tm = agg.get("timings") or {}
    if not tm:
        # an interrupted call has no timings: the read progress gives the prompt, the tokenised partial reply gives the end
        pg, end = agg.get("progress") or {}, (extra or {}).get("ideal_tokens")
        if pg.get("total") and end:
            return {"total": pg["total"], "cached": pg.get("cache", 0), "gen": max(0, end - pg["total"]), "end": end, "source": "estimated (the call was interrupted)"}
        return {"total": None, "cached": None, "gen": None, "end": None, "source": "none"}
    total = (tm.get("prompt_n") or 0) + (tm.get("cache_n") or 0)
    gen = tm.get("predicted_n") or 0
    return {"total": total, "cached": tm.get("cache_n") or 0, "gen": gen, "end": total + gen, "source": "llama-server timings"}


# ---------------------------------------------------------------- the conversation each request carried

def stream_key(messages, tools):
    first = messages[0]["content"] if messages and messages[0].get("role") == "system" and isinstance(messages[0].get("content"), str) else ""
    names = sorted((t.get("function") or {}).get("name", "") for t in tools or [])
    return hashlib.sha1((first[:4000] + "|" + ",".join(names)).encode()).hexdigest()[:10]


def msg_text(m):
    """One message as stable text, key order as sent (the daemon's own serialiser decides it)."""
    return json.dumps(m, ensure_ascii=False)


def msg_diff(prev, cur):
    """The first message of `prev` that `cur` does not repeat verbatim: (index, field, prev text, cur text) or None."""
    for j, pm in enumerate(prev):
        if j >= len(cur):
            return {"index": j, "field": "(missing)", "prev": msg_text(pm)[:200], "cur": ""}
        cm = cur[j]
        if msg_text(pm) == msg_text(cm):
            continue
        for k in list(dict.fromkeys(list(pm) + list(cm))):
            if json.dumps(pm.get(k), ensure_ascii=False) != json.dumps(cm.get(k), ensure_ascii=False):
                return {"index": j, "field": k, "prev": json.dumps(pm.get(k), ensure_ascii=False)[:200], "cur": json.dumps(cm.get(k), ensure_ascii=False)[:200]}
    return None


def key_order(v):
    if isinstance(v, dict):
        return [(k, key_order(x)) for k, x in v.items()]
    if isinstance(v, list):
        return [key_order(x) for x in v]
    return None


def echo_diff(produced, sent):
    """What the model produced against the assistant message the daemon sent back for it. Each entry names one difference."""
    out = []
    pt = produced.get("thinking") or ""
    st = sent.get("thinking") if "thinking" in sent else sent.get("reasoning_content")
    st = st or ""
    if pt and not st:
        out.append(f"thinking dropped (model wrote {len(pt)} chars, sent back none)")
    elif pt != st:
        out.append("thinking changed" + (" (whitespace only)" if pt.strip() == st.strip() else "") + f" ({len(pt)} -> {len(st)} chars)")
    pc = produced.get("content") or ""
    sc = sent.get("content") or ""
    if isinstance(sc, list):
        sc = "".join(p.get("text", "") for p in sc if isinstance(p, dict))
    if pc != sc:
        out.append("content changed" + (" (whitespace only)" if pc.strip() == sc.strip() else "") + f" ({len(pc)} -> {len(sc)} chars)")
    pcalls, scalls = produced.get("tool_calls") or [], sent.get("tool_calls") or []
    if len(pcalls) != len(scalls):
        out.append(f"tool calls {len(pcalls)} -> {len(scalls)}")
    for i, (p, s) in enumerate(zip(pcalls, scalls)):
        sf = s.get("function") or {}
        if p.get("name") != sf.get("name"):
            out.append(f"tool_call[{i}] name {p.get('name')} -> {sf.get('name')}")
        pa, sa = p.get("arguments"), sf.get("arguments")
        pa_obj = _as_obj(pa)
        sa_obj = _as_obj(sa)
        if pa_obj is None or sa_obj is None:
            if pa != sa:
                out.append(f"tool_call[{i}] arguments differ")
            continue
        if json.dumps(pa_obj, sort_keys=True) != json.dumps(sa_obj, sort_keys=True):
            out.append(f"tool_call[{i}] arguments' values changed")
        elif key_order(pa_obj) != key_order(sa_obj):
            out.append(f"tool_call[{i}] arguments' keys reordered ({_flat_keys(pa_obj)} -> {_flat_keys(sa_obj)})")
        elif isinstance(pa, str) and isinstance(sa, str) and pa != sa:
            out.append(f"tool_call[{i}] arguments re-serialised ({len(pa)} -> {len(sa)} chars, same keys and values)")
    return out


def _dicts(v):
    if isinstance(v, dict):
        yield v
        for x in v.values():
            yield from _dicts(x)
    elif isinstance(v, list):
        for x in v:
            yield from _dicts(x)


def _as_obj(a):
    if isinstance(a, str):
        try:
            return json.loads(a)
        except ValueError:
            return None
    return a


def _flat_keys(o, depth=0):
    if isinstance(o, dict):
        return list(o)[:8] if depth == 0 else list(o)[:4]
    return []


# ---------------------------------------------------------------- the rendered prompt and where two of them part

def common_prefix(a, b):
    n = min(len(a), len(b))
    lo, hi = 0, n
    if a[:n] == b[:n]:
        return n
    while lo < hi:  # binary search on equal prefixes: slicing compares in C
        mid = (lo + hi + 1) // 2
        if a[:mid] == b[:mid]:
            lo = mid
        else:
            hi = mid - 1
    return lo


def locate(starts, d):
    """Index of the message whose text holds offset d (starts[j] = where message j begins)."""
    j = 0
    for i, s in enumerate(starts):
        if s <= d:
            j = i
    return j


def think_len(block):
    m = re.search(r"<think>\n?(.*?)\n?</think>", block, re.S)
    return len(m.group(1).strip()) if m else 0


def segment(text, start, d):
    """The text of the message holding offset d: from its start to the next role marker after d (qwen-style templates) or the end."""
    j = text.find("<|im_start|>", d + 1)
    return text[start: j if j >= 0 else len(text)]


def prompt_diff(ideal, cur, n_prev):
    """ideal: the previous request's conversation plus the reply exactly as the model wrote it, rendered ({text} only);
    cur: this request's render ({text, starts}); n_prev: how many messages the previous request had.

    None when this prompt starts with all of that, which is what an unbroken cache needs. Otherwise where it first differs
    (in bytes), which message of this request holds that byte, and whether that is old history or the reply itself."""
    a, b = ideal["text"], cur["text"]
    d = common_prefix(a, b)
    if d >= len(a):
        return None
    starts = cur.get("starts") or []
    j = locate(starts, d) if starts else None
    res = {"offset": len(a[:d].encode("utf-8")), "of": len(a.encode("utf-8")), "char": d, "message": j,
           "region": "the model's reply" if j is not None and j >= n_prev else "history", "ideal_snip": a[max(0, d - 70):d + 90], "cur_snip": b[max(0, d - 70):d + 90]}
    if j is not None:
        s0 = starts[j]
        res["think_ideal"], res["think_cur"] = think_len(segment(a, s0, d)), think_len(segment(b, s0, d))
    return res


# ---------------------------------------------------------------- one audit run

class Call:
    """One model request of a run, with everything the verdicts need."""

    def __init__(self, rec, req_text, resp_text, log_text=""):
        self.rec = rec
        self.seq = rec["seq"]
        self.t_start = rec["t_start"]
        self.path = rec["path"]
        self.kind = "ollama" if rec["path"].startswith("/api/") else "openai"
        try:
            self.body = json.loads(req_text)
        except ValueError:
            self.body = {}
        self.messages = self.body.get("messages") or []
        self.tools = self.body.get("tools") or []
        self.is_model_call = bool(self.messages)
        self.key = stream_key(self.messages, self.tools)
        self.agg = agg_ollama(resp_text) if self.kind == "ollama" else agg_openai(resp_text)
        self.aborted = bool(rec.get("aborted"))
        self.status = rec.get("status")
        self.nums = numbers_ollama(log_text, self.agg) if self.kind == "ollama" else numbers_openai(self.agg, rec.get("extra"))
        self.render_tokens = self.ideal_tokens = None  # token counts of the two texts below, where a tokenizer was at hand
        self.render = None  # this request's prompt: {text, no_gp_len, starts, via}
        self.ideal = None   # this request's conversation + the reply as the model wrote it, rendered: {text}


def attach_render_from_server(call):
    ex = call.rec.get("extra") or {}
    if ex.get("rendered") is None:
        return
    text = ex["rendered"]
    no_gp = ex.get("rendered_no_gp")
    call.render = {"text": text, "no_gp_len": len(no_gp) if no_gp is not None and text.startswith(no_gp) else len(text), "starts": ex.get("starts") or [], "via": "llama-server /apply-template"}
    if ex.get("ideal") is not None:
        call.ideal = {"text": ex["ideal"]}
    call.render_tokens, call.ideal_tokens = ex.get("rendered_tokens"), ex.get("ideal_tokens")


def first_user(messages):
    for m in messages:
        if m.get("role") == "user":
            return msg_text(m)
    return None


def parent_of(calls, i):
    """The nearest earlier request in the same conversation (same first user message): the one this prompt must extend. A request
    with none is a new conversation, or the conversation after a compaction, which is the one rewrite the rule allows."""
    head = first_user(calls[i].messages)
    for j in range(i - 1, -1, -1):
        if first_user(calls[j].messages) == head:
            return calls[j]
    return None


def template_drop_only(p, c, row):
    """For a request after a user message: whether the earlier thinking is the only thing missing from the cache and the server read
    no more than the new content (the engine's read-ahead shows in the tap). Returns {read_ahead_tokens, processed, thinking_chars} or None."""
    pd, md = row.get("prompt_diff"), row.get("msg_diff")
    if md or not pd or not (pd.get("think_ideal") and not pd.get("think_cur")):
        return None
    total, cached, ahead = row.get("total"), row.get("cached"), getattr(c, "preread_tokens", None)
    # the engine read the prompt without the thinking into the slot (ahead tokens) before this request came: the server then served all
    # of it from cache (up to the template's few stop/newline tokens) and read only what is new
    if not total or cached is None or not ahead or cached < ahead - TOLERANCE:
        return None
    return {"read_ahead_tokens": ahead, "processed": total - cached, "thinking_chars": pd["think_ideal"]}


def judge(calls, restarts=(), unloads=(), dropper="the template"):
    """One verdict per conductor request. calls: Call objects of one stream in order.

    restarts / unloads: times the daemon was restarted and the model was unloaded or its server stopped. A cache that was emptied
    on purpose is reported as cold, not as a break, but it is still listed: Matthew's rule has no exception for it.
    dropper: who is to blame when a model's earlier thinking is missing from the prompt (the template, or Ollama's own conversion).
    """
    rows = []
    for i, c in enumerate(calls):
        n = c.nums
        row = {"seq": c.seq, "total": n.get("total"), "cached": n.get("cached"), "gen": n.get("gen"), "source": n.get("source"), "slot": n.get("slot"),
               "messages": len(c.messages), "aborted": c.aborted, "verdict": "first", "deficit": None}
        if i == 0:
            rows.append(row)
            continue
        p = parent_of(calls, i)
        if p is None:
            row["verdict"] = "reset (a new conversation or a compaction: the first user message is not the one before)"
            rows.append(row)
            continue
        pn = p.nums
        expect = pn.get("end") if pn.get("end") else ((pn.get("total") or 0) + (pn.get("gen") or 0)) or None
        row["expected"] = expect
        emptied = any(p.t_start < t < c.t_start for t in list(restarts) + list(unloads))
        if n.get("cached") is None or expect is None:
            row["verdict"] = "no numbers"
            rows.append(row)
            continue
        row["deficit"] = expect - n["cached"]
        row["prev_reply_tokens"] = pn.get("gen")
        # a hybrid model resumes from the checkpoint taken at the end of the previous prompt, so any difference inside the reply
        # costs the whole reply: a gap equal to the reply's length says the difference is in the reply
        row["reread_whole_reply"] = bool(pn.get("gen")) and abs(row["deficit"] - pn["gen"]) <= TOLERANCE + 4
        if row["deficit"] <= TOLERANCE:
            row["verdict"] = "ok"
        elif emptied and n["cached"] < 0.1 * expect:
            row["verdict"] = "cold (daemon restart emptied the cache)"
        else:
            row["verdict"] = "BREAK"
        row["msg_diff"] = msg_diff(p.messages, c.messages)
        md = row["msg_diff"]
        if md and "elided by the harness" in (md.get("cur") or ""):  # Marlowe's own compaction: old tool results replaced by a stub
            ctx = (c.body.get("options") or {}).get("num_ctx")
            held = pn.get("end") or pn.get("total") or 0
            row["verdict"] = "compaction (the one allowed rewrite)"
            row["cause"] = f"compaction: an old tool result was elided with the conversation at {held} tokens" + (f", {round(100 * held / ctx)}% of the {ctx}-token window" if ctx else "")
            rows.append(row)
            continue
        sent = c.messages[len(p.messages)] if len(c.messages) > len(p.messages) else None
        row["echo"] = echo_diff(p.agg, sent) if sent and sent.get("role") == "assistant" else None
        wrote = len(p.agg.get("thinking") or "") + len(p.agg.get("content") or "") + len(p.agg.get("tool_calls") or [])
        if wrote and not (sent and sent.get("role") == "assistant"):
            row["echo"] = [f"the model's reply ({len(p.agg.get('thinking') or '')} chars of thinking, {len(p.agg.get('content') or '')} of text, "
                           f"{len(p.agg.get('tool_calls') or [])} tool calls) is not in the next request at all" + (" (the call was interrupted)" if p.aborted else "")]
        if p.render_tokens and p.ideal_tokens and pn.get("gen") is not None:
            # the model's own count of tokens for the reply (its stop token included) against what the template prints for it (stop token and newline included)
            printed = p.ideal_tokens - p.render_tokens - 1
            row["reply_tokens"] = {"generated": pn["gen"], "printed_by_template": printed, "difference": pn["gen"] - printed}
        row["compared"] = bool(p.ideal and c.render)
        if row["compared"]:
            row["prompt_diff"] = prompt_diff(p.ideal, c.render, len(p.messages))
            pd = row["prompt_diff"]
            if pd and pd["message"] is not None and pd["message"] < len(c.messages):
                pd["role"] = c.messages[pd["message"]].get("role")
        if p.render and c.render:  # the two rendered prompts side by side, as the server saw them
            a, b = p.render["text"], c.render["text"]
            d = common_prefix(a, b)
            row["consecutive"] = None if d >= len(a) else {"offset": len(a[:d].encode("utf-8")), "of": len(a.encode("utf-8")), "message": locate(c.render.get("starts") or [], d)}
        row["server"] = {k: n.get(k) for k in ("sim_best", "f_keep", "restored", "forced_full") if n.get(k) is not None}
        row["reply_features"] = reply_features(p.agg)
        row["dropper"] = dropper
        if row["verdict"] == "BREAK":
            tail = template_drop_only(p, c, row)
            if tail is not None:
                # the template removed the earlier thinking from the prompt, and the server read nothing but the new message: the slot
                # held the prompt without it (read ahead), so no token was read twice. The gap is only the thinking tokens that left the slot.
                row["verdict"] = "ok"
                row["template_drop"] = tail
        rows.append(row)
    # the count of tokens the template prints for a reply is a little off the model's own, the same way on every request; what
    # matters is a request that is off by more
    diffs = sorted(r["reply_tokens"]["difference"] for r in rows if r.get("verdict") == "ok" and r.get("reply_tokens"))
    base = diffs[len(diffs) // 2] if len(diffs) >= 3 else 0
    for r in rows:
        if r.get("reply_tokens"):
            r["reply_tokens"]["usual_difference"] = base
        if r.get("verdict") not in ("ok", "first", "no numbers") and not str(r.get("verdict")).startswith(("reset", "compaction")):
            r["cause"] = classify(r)
    return rows


def reply_features(agg):
    """What in the model's reply a renderer is likely to print differently from how the model wrote it."""
    feats = set()
    for c in agg.get("tool_calls") or []:
        obj = _as_obj(c.get("arguments"))
        if not isinstance(obj, dict):
            continue
        for v in obj.values():
            if isinstance(v, (dict, list)):
                feats.add("nested JSON arguments")
                nested = [d for d in _dicts(v) if len(d) > 1]
                if nested and all(list(d) == sorted(d) for d in nested):
                    feats.add("nested object keys that arrive already sorted (the server's map order, not necessarily the model's)")
            elif isinstance(v, bool):
                feats.add("boolean arguments")
            elif isinstance(v, (int, float)):
                feats.add("number arguments")
            elif isinstance(v, str) and any(ch in v for ch in ('"', "\\", "\n", "\t")):
                feats.add("quotes, backslashes or newlines inside a string argument")
            elif isinstance(v, str) and any(ord(ch) > 127 for ch in v):
                feats.add("non-ASCII text in an argument")
    multi = [_as_obj(c.get("arguments")) for c in agg.get("tool_calls") or []]
    multi = [o for o in multi if isinstance(o, dict) and len(o) > 1]
    if multi and all(list(o) == sorted(o) for o in multi):
        feats.add("argument keys that arrive alphabetically sorted (the server's map order, not necessarily the model's)")
    if len(agg.get("tool_calls") or []) > 1:
        feats.add("several tool calls")
    return sorted(feats)


def classify(row):
    """Names the cause of a break from the evidence gathered, most specific first."""
    md, ed, pd = row.get("msg_diff"), row.get("echo"), row.get("prompt_diff")
    if str(row.get("verdict", "")).startswith("cold"):
        return "the daemon was restarted and unloaded the model, so its cache was empty" + (" (and the history reloaded from the store differs from what was sent live)" if md else "")
    if md:
        return f"the daemon changed message {md['index']} between requests ({md['field']})"
    if pd:
        j = pd.get("message")
        if pd["region"] == "history":
            if pd.get("think_ideal") and not pd.get("think_cur"):
                return f"{row.get('dropper', 'the template')} dropped the model's earlier thinking (message {j}, {pd['think_ideal']} chars)"
            if ed:
                return f"the history sent back is not what the model wrote (message {j}: {'; '.join(ed)})"
            return f"the template rendered message {j} differently the second time (byte {pd['offset']} of {pd['of']})"
        if ed:
            return f"the reply was not sent back as written ({'; '.join(ed)})"
        if pd.get("think_ideal") and not pd.get("think_cur"):
            return f"{row.get('dropper', 'the template')} dropped the thinking of the model's last reply once a newer user message followed ({pd['think_ideal']} chars)"
        return f"the template renders the model's own reply differently from how it was generated (byte {pd['offset']} of {pd['of']})"
    if ed:
        return f"the reply was not sent back as written ({'; '.join(ed)})"
    srv = row.get("server") or {}
    if row.get("compared"):
        feats = row.get("reply_features") or []
        rt = row.get("reply_tokens")
        if rt and rt["difference"] != rt.get("usual_difference", 0):
            feats = [f"the model generated {rt['generated']} tokens and the template prints {rt['printed_by_template']} for the same reply (usually {rt['usual_difference']} apart)"] + feats
        return ("the prompt is exactly the previous prompt plus the reply as parsed, yet the server re-read the reply: what the model generated is not what the "
                "template renders for it (parser or renderer round trip, or tokenisation; the raw text is not exposed)"
                + (f"; the reply has {', '.join(feats)}" if feats else "") + ("; it resumed from an earlier checkpoint" if srv.get("restored") else ""))
    if row.get("reread_whole_reply"):
        return ("no rendered prompt to compare, but the gap is the length of the previous reply (" + str(row.get("prev_reply_tokens")) +
                " tokens): that reply came back in the next prompt in a form other than the one it was generated in")
    return "no rendered prompt to compare; the server cached less than the previous sequence"
