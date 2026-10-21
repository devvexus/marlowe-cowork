# Built-in fan-out (the `workflow` tool's short form): one read-only agent per item in parallel,
# then one agent combines their answers. The tool fills the args; the script never changes.
# args: {"items": [str], "prompt": str ({item} is replaced), "then": str, "read_only": bool (default true),
#        "workspace": str, "title": str}

META = {
    "name": "fanout",
    "description": "One agent per item in parallel, then one agent combines their answers",
    "when_to_use": "The user asks for a workflow, a fan-out or one agent per item (the workflow tool's short form).",
    "phases": ["each", "combine"],
}

DEFAULT_THEN = "Combine these reports into one answer for the user."

def clip(text, n):
    if text == None:
        return "(no answer)"
    t = str(text)
    if len(t) <= n:
        return t
    return t[:n] + " ..."

def item_prompt(prompt, item, workspace):
    return (prompt.replace("{item}", item) + "\n\nWorkspace: " + workspace + ". Your item: " + item +
            ". Answer in at most 200 words and name the files you read.")

def main(args):
    items = [str(i) for i in args["items"]]
    prompt = str(args["prompt"])
    then = str(args.get("then", "") or DEFAULT_THEN)
    workspace = str(args.get("workspace", "") or ".")
    kind = "explore" if args.get("read_only", True) else "workflow"
    phase("each")
    answers = parallel([
        (lambda item = item: agent(item_prompt(prompt, item, workspace), label = clip(item, 40), agent_type = kind))
        for item in items
    ])
    reports = "\n\n".join(["## " + items[i] + "\n" + clip(answers[i], 2500) for i in range(len(items))])
    phase("combine")
    summary = agent(
        then + "\n\nThe task each agent had: " + prompt + "\n\nReports:\n\n" + reports,
        label = "combine",
        agent_type = kind,
    )
    return {
        "summary": summary if summary != None else "(the combining agent gave no answer; the item answers are below)",
        "items": [{"item": items[i], "answer": answers[i]} for i in range(len(items))],
    }
