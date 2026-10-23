"""Diagnosis only: a Python port of the prompt renderer Ollama v0.32.5 runs for models whose manifest says renderer "qwen3.5"
(model/renderers/qwen35.go, MIT). Marlowe has no such renderer; the audit uses this one to see what Ollama puts in the
prompt of marlowe-dawn and marlowe-mini, because Ollama does not log the prompt it renders. The numbers the audit judges by
(tokens read, tokens reused) always come from the server's own log, never from this port.
"""
import json
IM_START, IM_END = "<|im_start|>", "<|im_end|>"
POST = """
</tools>

If you choose to call a function ONLY reply in the following format with NO suffix:

<tool_call>
<function=example_function_name>
<parameter=example_parameter_1>
value_1
</parameter>
<parameter=example_parameter_2>
This is the value for the second parameter
that can span
multiple lines
</parameter>
</function>
</tool_call>

<IMPORTANT>
Reminder:
- Function calls MUST follow the specified format: an inner <function=...></function> block must be nested within <tool_call></tool_call> XML tags
- Required parameters MUST be specified
- You may provide optional reasoning for your function call in natural language BEFORE the function call, but NOT after
- If there is no function call available, answer the question like normal with your current knowledge and do not tell the user about function calls
</IMPORTANT>"""


def _prop(p):
    o = {}
    if p.get("anyOf"):
        o["anyOf"] = [_prop(x) for x in p["anyOf"]]
    if p.get("type"):
        o["type"] = p["type"][0] if isinstance(p["type"], list) and len(p["type"]) == 1 else p["type"]
    if p.get("items") is not None:
        o["items"] = p["items"]
    if p.get("description"):
        o["description"] = p["description"]
    if p.get("enum"):
        o["enum"] = p["enum"]
    if p.get("properties") is not None:
        o["properties"] = {k: _prop(v) for k, v in p["properties"].items()}
    if p.get("required"):
        o["required"] = p["required"]
    return o


def go_tool(t):
    f = t["function"]
    pr = f.get("parameters") or {}
    params = {"type": pr.get("type", "object")}
    if pr.get("$defs") is not None:
        params["$defs"] = pr["$defs"]
    if pr.get("items") is not None:
        params["items"] = pr["items"]
    if pr.get("required"):
        params["required"] = pr["required"]
    props = pr.get("properties")
    params["properties"] = {k: _prop(v) for k, v in props.items()} if props is not None else None
    fn = {"name": f["name"]}
    if f.get("description"):
        fn["description"] = f["description"]
    fn["parameters"] = params
    o = {"type": t.get("type", "function")}
    if t.get("items") is not None:
        o["items"] = t["items"]
    o["function"] = fn
    return o


def _go_escape(s):
    return s.replace("<", "\\u003c").replace(">", "\\u003e").replace("&", "\\u0026").replace("\u2028", "\\u2028").replace("\u2029", "\\u2029")


def marshal_with_spaces(v):
    s = _go_escape(json.dumps(v, ensure_ascii=False, separators=(",", ":")))
    out = []
    in_str = esc = False
    for c in s:
        if in_str:
            out.append(c)
            if esc:
                esc = False
                continue
            if c == "\\":
                esc = True
                continue
            if c == '"':
                in_str = False
            continue
        if c == '"':
            in_str = True
            out.append(c)
        elif c == ":":
            out.append(": ")
        elif c == ",":
            out.append(", ")
        else:
            out.append(c)
    return "".join(out)


def fmt_arg(v):
    if v is None:
        return "null"
    if isinstance(v, str):
        return v
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (dict, list)):
        return _go_escape(json.dumps(v, ensure_ascii=False, separators=(",", ":")))
    if isinstance(v, float) and v == int(v) and abs(v) < 1e21:
        return str(int(v))
    return str(v)


def split_reasoning(content, msg_thinking, is_thinking):
    if is_thinking and msg_thinking:
        return msg_thinking.strip(), content
    reasoning = ""
    i = content.find("</think>")
    if i != -1:
        before = content[:i]
        o = before.rfind("<think>")
        reasoning = before[o + 7:] if o != -1 else before
        content = content[i + 8:].lstrip("\n")
    return reasoning.strip(), content


def render(messages, tools, think, marks=None):
    """The prompt text; `marks`, if a list, receives the offset where each message's output starts."""
    sb = []
    is_thinking = True if think is None else bool(think)
    if tools:
        sb.append(IM_START + "system\n# Tools\n\nYou have access to the following functions:\n\n<tools>")
        for t in tools:
            sb.append("\n" + marshal_with_spaces(go_tool(t)))
        sb.append(POST)
        if messages and messages[0]["role"] == "system":
            sc = (messages[0].get("content") or "").strip()
            if sc:
                sb.append("\n\n" + sc)
        sb.append(IM_END + "\n")
    elif messages and messages[0]["role"] == "system":
        sb.append(IM_START + "system\n" + (messages[0].get("content") or "").strip() + IM_END + "\n")
    multi = True
    last_q = len(messages) - 1
    for i in range(len(messages) - 1, -1, -1):
        m = messages[i]
        if multi and m["role"] == "user":
            c = (m.get("content") or "").strip()
            if not (c.startswith("<tool_response>") and c.endswith("</tool_response>")):
                multi = False
                last_q = i
    for i, m in enumerate(messages):
        if marks is not None:
            marks.append(sum(map(len, sb)))
        content = (m.get("content") or "").strip()
        last = i == len(messages) - 1
        prefill = last and m["role"] == "assistant"
        role = m["role"]
        if role == "user" or (role == "system" and i != 0):
            sb.append(IM_START + role + "\n" + content + IM_END + "\n")
        elif role == "assistant":
            rt = is_thinking and i > last_q
            reasoning, content = split_reasoning(content, m.get("thinking") or "", rt)
            if rt:
                sb.append(IM_START + role + "\n<think>\n" + reasoning + "\n</think>\n\n" + content)
            else:
                sb.append(IM_START + role + "\n" + content)
            for j, tc in enumerate(m.get("tool_calls") or []):
                if j == 0:
                    if content.strip():
                        sb.append("\n\n")
                else:
                    sb.append("\n")
                sb.append("<tool_call>\n<function=" + tc["function"]["name"] + ">\n")
                for k, v in (tc["function"].get("arguments") or {}).items():
                    sb.append("<parameter=" + k + ">\n" + fmt_arg(v) + "\n</parameter>\n")
                sb.append("</function>\n</tool_call>")
            if not prefill:
                sb.append(IM_END + "\n")
        elif role == "tool":
            if i == 0 or messages[i - 1]["role"] != "tool":
                sb.append(IM_START + "user")
            sb.append("\n<tool_response>\n" + content + "\n</tool_response>")
            if last or messages[i + 1]["role"] != "tool":
                sb.append(IM_END + "\n")
        if last and not prefill:
            sb.append(IM_START + "assistant\n")
            sb.append("<think>\n" if is_thinking else "<think>\n\n</think>\n\n")
    return "".join(sb)


