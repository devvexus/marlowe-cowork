# Built-in web-research: researchers take one sub-question each in parallel, a checker tests each
# claim against its source (also in parallel), one writer drafts the cited report and writes it to a file.
# args: {"topic": "<topic or question>", "questions": [optional sub-questions], "file": optional report path}
# Also accepts "question" or "query" for the topic.

META = {
    "name": "web-research",
    "description": "Web research on a topic: researchers take one sub-question each in parallel, a checker tests each claim against its source, one writer drafts the cited report and saves it to a file. Args: topic.",
    "when_to_use": "The user asks for researched, cited work on a topic or question.",
    "phases": ["research", "report"],
    "summary": "cited web research report",
    "args": "topic (required: the subject and the focus asked for, in the user's words); questions (optional list); file (optional)",
}

FINDINGS = {
    "type": "object",
    "properties": {"findings": {"type": "array", "items": {
        "type": "object",
        "properties": {"claim": {"type": "string"}, "url": {"type": "string"}},
        "required": ["claim", "url"],
    }}},
    "required": ["findings"],
}
CHECK = {"type": "object", "properties": {"supported": {"type": "boolean"}}, "required": ["supported"]}

def sub_questions(topic):
    return [
        "How does " + topic + " work, and what are its key ideas and terms?",
        "What papers, systems or projects exist on " + topic + "?",
        "What do the official documentation and the main implementations say about " + topic + "?",
        "What are the open problems, limitations and measured results reported for " + topic + "?",
        "Which open-source implementations of " + topic + " exist, and what do their repositories and issue trackers say about limitations?",
    ]

def slug(topic):
    out = ""
    for c in topic.lower().elems():
        if c.isalnum():
            out += c
        elif out != "" and out[-1] != "-":
            out += "-"
    return out.strip("-")[:48].strip("-") or "report"

def research(prev, question, i):
    found = agent("Answer this with web_search, web_fetch and arxiv. Report the three or four strongest claims, each one a claim that a page you fetched supports, with that page's url: " + question, label = "researcher " + str(i + 1), phase = "research", schema = FINDINGS, agent_type = "researcher")
    if found == None:
        log("the researcher for question " + str(i + 1) + " failed; it is unanswered")
        return []
    checks = parallel([lambda f = f: agent("Fetch " + f["url"] + " and say whether the page supports this claim: " + f["claim"], label = "checker", phase = "research", schema = CHECK, agent_type = "researcher") for f in found["findings"]])
    kept = []
    for j in range(len(found["findings"])):
        if checks[j] != None and checks[j]["supported"]:
            kept.append(found["findings"][j])
    return kept

def main(args):
    if type(args) != "dict":
        args = {"topic": str(args)}
    topic = str(args.get("topic") or args.get("question") or args.get("query") or "").strip()
    if topic == "":
        fail("web-research needs a topic: team(name = \"web-research\", args = {\"topic\": \"what to research\"})")
    questions = args.get("questions") or sub_questions(topic)
    path = str(args.get("file") or ("research/" + slug(topic) + ".md"))
    per_question = pipeline(questions, research)
    claims = []
    for k in per_question:
        if k:
            claims.extend(k)
    for n in range(len(claims)):
        claims[n]["id"] = "C" + str(n + 1)
    log(str(len(claims)) + " checked claim(s) from " + str(len(questions)) + " question(s)")
    phase("report")
    return agent("Write the report on " + topic + " and save it to the file " + path + " with the write tool, then give the report as your answer. Use only these checked claims, cite each sentence as [C1] with the claim ids, end with a References list that gives each id its url, and say what the claims leave open:\n" + json.encode(claims), label = "writer", phase = "report", agent_type = "researcher")
