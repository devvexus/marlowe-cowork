"""Stands in for llama-server (MARLOWE_LLAMA_SERVER) so the audit can see what the engine sends it.

The daemon starts this with the arguments it would give llama-server and then talks to the --port it chose. This starts
the real server (AUDIT_REAL_SERVER) on another port, listens on the daemon's port as a tap, and for every chat call asks
the real server's /apply-template how that exact request renders (with and without the generation prompt), so the audit
can compare consecutive prompts byte for byte. Records go to AUDIT_TAP_LOG (jsonl) and AUDIT_TAP_RAW (bodies).

  python -I llama_wrap.py <llama-server arguments>
"""

import json
import os
import socket
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import tap  # noqa: E402

NO_WINDOW = 0x08000000


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def post(port, path, body, timeout=60):
    r = urllib.request.Request(f"http://127.0.0.1:{port}{path}", json.dumps(body).encode(), {"Content-Type": "application/json"})
    with urllib.request.urlopen(r, timeout=timeout) as f:
        return json.loads(f.read())


def main():
    args = sys.argv[1:]
    real = os.environ["AUDIT_REAL_SERVER"]
    log = os.environ["AUDIT_TAP_LOG"]
    raw = os.environ.get("AUDIT_TAP_RAW")
    raw = os.path.join(raw, f"w{os.getpid()}") if raw else None  # one folder per server process: sequence numbers restart with it
    i = args.index("--port")
    front = int(args[i + 1])
    back = free_port()
    args[i + 1] = str(back)
    env = dict(os.environ)
    # the engine points GGML_BACKEND_PATH at the junction beside this script; the real files are Ollama's
    lib = os.path.dirname(real)
    be = env.get("GGML_BACKEND_PATH")
    if be:
        mapped = os.path.join(lib, os.path.basename(os.path.dirname(be)), os.path.basename(be))
        if os.path.isfile(mapped):
            env["GGML_BACKEND_PATH"] = mapped
    env["PATH"] = os.pathsep.join([lib] + [d for d in (os.path.join(lib, "cuda_v13"), os.path.join(lib, "cuda_v12")) if os.path.isdir(d)] + [env.get("PATH", "")])
    # the server's own log goes where the daemon pointed this process's output
    proc = subprocess.Popen([real] + args, env=env, creationflags=NO_WINDOW, stdin=subprocess.DEVNULL, stdout=sys.stdout, stderr=sys.stderr)

    def on_request(method, path, body):
        if not path.startswith("/v1/chat/completions"):
            return None
        b = json.loads(body)
        probe = {k: b[k] for k in ("messages", "tools", "chat_template_kwargs", "tool_choice", "parallel_tool_calls") if k in b}
        out = {}
        for name, extra in (("rendered", {}), ("rendered_no_gp", {"add_generation_prompt": False})):
            try:
                out[name] = post(back, "/apply-template", {**probe, **extra}).get("prompt")
            except Exception as e:
                out[name] = None
                out[name + "_error"] = repr(e)[:300]
        # where each message's text starts in the prompt: the length of the render of the messages before it. A template that
        # needs a user message cannot render a system message alone; that one start is read off the first end-of-turn marker.
        lens = []
        for k in range(len(b.get("messages", []))):
            try:
                r = post(back, "/apply-template", {**probe, "messages": b["messages"][:k], "add_generation_prompt": False})
                lens.append(len(r["prompt"]) if k else 0)
            except Exception:
                lens.append(None)
        if len(lens) > 1 and lens[1] is None and out.get("rendered"):
            end = out["rendered"].find("<|im_end|>")
            lens[1] = end + len("<|im_end|>\n") if end >= 0 else 0
        lens = [x if x is not None else (lens[i - 1] if i else 0) for i, x in enumerate(lens)]
        out["starts"] = lens
        try:
            out["rendered_tokens"] = len(post(back, "/tokenize", {"content": out.get("rendered") or "", "add_special": False, "parse_special": True}).get("tokens", []))
        except Exception as e:
            out["tokenize_error"] = repr(e)[:200]
        return out

    def on_done(rec, body, answer):
        """The conversation with the model's reply appended as it wrote it, rendered: what the next request has to start with."""
        if not rec["path"].startswith("/v1/chat/completions") or rec.get("status") != 200:
            return None
        import analyze
        b = json.loads(body)
        a = analyze.agg_openai(answer.decode("utf-8", "replace"))
        msg = {"role": "assistant", "content": a.get("content") or ""}
        if a.get("thinking"):
            msg["reasoning_content"] = a["thinking"]
        if a.get("tool_calls"):
            msg["tool_calls"] = [{"id": c["id"], "type": "function", "function": {"name": c["name"], "arguments": c["arguments"]}} for c in a["tool_calls"]]
        probe = {k: b[k] for k in ("tools", "chat_template_kwargs") if k in b}
        # llama-server treats a last assistant message as a prefill and leaves its tool calls out, so the reply is rendered as a
        # middle message, followed by a tool result (a user message would make a template drop the reply's thinking, as it does
        # for every turn before the newest question). Two different results follow it; what the two renders share, up to the
        # result's turn, is the prompt as the model saw it with this reply as its latest message.
        texts = [post(back, "/apply-template", {**probe, "messages": b["messages"] + [msg, {"role": "tool", "tool_call_id": "audit", "name": "audit", "content": d}],
                                                  "add_generation_prompt": False}).get("prompt") or "" for d in ("X", "YY")]
        shared = os.path.commonprefix(texts)
        cut = shared.rfind("<|im_start|>")
        ideal = shared[:cut] if cut > 0 else shared
        out = {"ideal": ideal}
        try:  # token counts of the same texts, for calls the server never finished (an interrupt leaves no timings)
            out["ideal_tokens"] = len(post(back, "/tokenize", {"content": ideal, "add_special": False, "parse_special": True}).get("tokens", []))
        except Exception as e:
            out["tokenize_error"] = repr(e)[:200]
        return out

    t = tap.Tap(front, ("127.0.0.1", back), tap.JsonlSink(log), on_request=on_request, on_done=on_done, raw_dir=raw).start()
    with open(log + ".ports", "a") as f:
        f.write(json.dumps({"front": front, "back": back, "pid": proc.pid, "at": time.time()}) + "\n")
    # live as long as the real server; the daemon's job object ends both
    proc.wait()
    t.stop()
    sys.exit(proc.returncode or 0)


if __name__ == "__main__":
    main()
