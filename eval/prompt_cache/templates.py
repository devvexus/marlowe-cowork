"""Each model's own chat template, and what it does with the model's earlier reasoning.

Reads the template from the GGUF header (and the Ollama manifest's renderer/template layers), then renders one probe
conversation through it and looks for the earlier reasoning in the result:

    system, user U1, assistant [reasoning RSN-ALPHA + a tool call], tool result, assistant [reasoning RSN-BETA + text], user U2

llama-server's own /apply-template is the judge (a CPU-only server on the model, nothing on the GPU); where it will not start,
jinja2 stands in and the report says so. For a model Ollama renders itself, the ported renderer answers the same probe.
Several template switches are tried (the model's template names which it knows).
"""

import json
import os
import subprocess
import time
import urllib.request
from pathlib import Path

import gguf

NO_WINDOW = 0x08000000
OLLAMA_LIB = Path(os.environ.get("LOCALAPPDATA", "")) / "Programs" / "Ollama" / "lib" / "ollama"
KWARGS = [{}, {"enable_thinking": True}, {"preserve_thinking": True}, {"preserve_thinking": False}, {"clear_thinking": False}, {"enable_thinking": True, "preserve_thinking": True}]

PROBE = [
    {"role": "system", "content": "You are a test."},
    {"role": "user", "content": "U1 please read a"},
    {"role": "assistant", "content": "", "reasoning_content": "RSN-ALPHA", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "read", "arguments": {"path": "a"}}}]},
    {"role": "tool", "tool_call_id": "c1", "name": "read", "content": "T1-RESULT"},
    {"role": "assistant", "content": "C-BETA", "reasoning_content": "RSN-BETA"},
    {"role": "user", "content": "U2 thanks"},
]
TOOLS = [{"type": "function", "function": {"name": "read", "description": "read a file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}}]


def _post(port, path, body, timeout=60):
    r = urllib.request.Request(f"http://127.0.0.1:{port}{path}", json.dumps(body).encode(), {"Content-Type": "application/json"})
    with urllib.request.urlopen(r, timeout=timeout) as f:
        return json.loads(f.read())


def _free_port():
    import socket
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def cpu_server(blob):
    """A llama-server on the weights with no GPU and a tiny window; (process, port) or (None, None)."""
    exe = OLLAMA_LIB / "llama-server.exe"
    if not exe.is_file():
        return None, None
    port = _free_port()
    env = dict(os.environ, PATH=str(OLLAMA_LIB) + os.pathsep + os.environ.get("PATH", ""))
    env.pop("GGML_BACKEND_PATH", None)
    args = [str(exe), "--model", blob, "--port", str(port), "--host", "127.0.0.1", "--no-webui", "--offline", "-c", "512", "-np", "1", "--jinja",
            "--no-warmup", "-ngl", "0", "--fit", "off"]
    p = subprocess.Popen(args, env=env, creationflags=NO_WINDOW, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(120):
        if p.poll() is not None:
            return None, None
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2) as r:
                if json.loads(r.read()).get("status") == "ok":
                    return p, port
        except Exception:
            pass
        time.sleep(1)
    p.kill()
    return None, None


def seen(prompt):
    return {k: (k in prompt) for k in ("RSN-ALPHA", "RSN-BETA")}


def run(models, out):
    results = []
    for m in models:
        facts = gguf.manifest_facts(m)
        h = gguf.read_header(facts["blob"])
        tpl = h.get("tokenizer.chat_template")
        entry = {"model": m, "architecture": h.get("general.architecture"), "renderer": facts["renderer"], "parser": facts["parser"], "layers": facts["layers"],
                 "ollama_template_layer": (facts["template"] or "")[:200] or None, "gguf_template_chars": len(tpl or ""), "static": gguf.template_notes(tpl), "probe": {}}
        proc, port = cpu_server(facts["blob"])
        entry["probe_via"] = "llama-server /apply-template (CPU)" if port else "jinja2"
        jr = None
        if not port:
            import report as rp
            jr = rp.JinjaRenderer(tpl)
        for kw in KWARGS:
            key = json.dumps(kw)
            try:
                if port:
                    prompt = _post(port, "/apply-template", {"messages": PROBE, "tools": TOOLS, "chat_template_kwargs": kw})["prompt"]
                else:
                    prompt = jr.render_openai({"messages": PROBE, "tools": TOOLS, "chat_template_kwargs": kw})
                entry["probe"][key] = seen(prompt)
            except Exception as e:
                entry["probe"][key] = {"error": repr(e)[:160]}
        if proc:
            proc.kill()
            proc.wait()
        if facts["renderer"] == "qwen3.5":
            import qwen35_mirror as q
            msgs = [dict(m2, thinking=m2.get("reasoning_content", ""), tool_calls=[dict(c, function=dict(c["function"])) for c in m2.get("tool_calls", [])]) for m2 in PROBE]
            entry["ollama_renderer_probe"] = {"think=true": seen(q.render(msgs, TOOLS, True)), "think=false": seen(q.render(msgs, TOOLS, False))}
        tpl_layer = (facts["template"] or "").strip()
        entry["ollama_go_template"] = bool(tpl_layer) and tpl_layer != "{{ .Prompt }}" and not facts["renderer"]
        default = entry["probe"].get("{}", {})
        entry["keeps_earlier_reasoning_by_default"] = bool(default.get("RSN-ALPHA"))
        entry["switch_that_keeps_it"] = [k for k, v in entry["probe"].items() if v.get("RSN-ALPHA") and not default.get("RSN-ALPHA")]
        entry["switch_that_changes_it"] = [k for k, v in entry["probe"].items() if v.get("RSN-ALPHA") != default.get("RSN-ALPHA")]
        results.append(entry)
        print(m, entry["probe_via"], "keeps by default:", entry["keeps_earlier_reasoning_by_default"], "switch:", entry["switch_that_keeps_it"], flush=True)
    (out / "templates.json").write_text(json.dumps(results, indent=1), encoding="utf-8")
    md = ["# Chat templates and earlier reasoning", "", "| model | rendered by | template drops earlier reasoning? | a switch that keeps it | probe via |", "|---|---|---|---|---|"]
    for e in results:
        who = f"Ollama renderer `{e['renderer']}`" if e["renderer"] else "Ollama's Go template layer" if e["ollama_go_template"] else "GGUF template (llama-server)"
        if e["ollama_go_template"]:
            md.append(f"| {e['model']} | {who} | yes: the layer prints a fixed empty think block and only the reply text (`.Response`), no thinking and no tool calls | none (Ollama never uses the GGUF template for it) | Ollama template layer read from the manifest |")
            continue
        keeps = e["keeps_earlier_reasoning_by_default"]
        switch = ", ".join(e["switch_that_keeps_it"]) or ("none needed; " + ", ".join(e["switch_that_changes_it"]) + " would drop it" if e["switch_that_changes_it"] else "none")
        if e["renderer"]:
            rp_ = e.get("ollama_renderer_probe", {}).get("think=true", {})
            drops = ("yes: Ollama's renderer drops it" if not rp_.get("RSN-ALPHA") else "no") + f" (the GGUF template alone would {'keep' if keeps else 'drop'} it, but Ollama does not use it)"
            switch = "none: the renderer has no switch"
        else:
            drops = "no, kept by default" if keeps else "yes"
        md.append(f"| {e['model']} | {who} | {drops} | {switch} | {e['probe_via']} |")
    md += ["", "Probe: earlier assistant turn with reasoning `RSN-ALPHA` and a tool call, a later one with `RSN-BETA`, then a new user message. "
           "`yes` = RSN-ALPHA is absent from the rendered prompt, so the model's own earlier thinking is gone from what it reads."]
    (out / "templates.md").write_text("\n".join(md) + "\n", encoding="utf-8")
