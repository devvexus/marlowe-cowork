# Prompt-cache audit

The rule, for every local model on every engine: a request's prompt is the previous request's prompt, plus the tokens the
model generated, plus new content at the end. What the model has read stays exactly as it read it (a thinking model keeps
its earlier thinking), so the server finds the whole previous sequence in its cache. The only rewrite allowed is compaction
when the window is full.

This harness checks that against the **real daemon**, with the server's own numbers, and says where and why it fails.

```bash
python -I eval/prompt_cache/audit.py --go --models marlowe-dawn:9b-super,marlowe-mini:4b-super --engines ollama,llama
python -I eval/prompt_cache/audit.py --templates          # CPU only: each model's chat template and its reasoning rules
python -I eval/prompt_cache/audit.py --analyze DIR        # judge the records of an earlier run again
python -I -m pytest eval/prompt_cache                     # offline checks, no model
```

It loads models onto the GPU, so nothing starts without `--go`. It stops its own work and waits while
`target/cargo-quiet` exists. Results go to `eval/prompt_cache/out/<stamp>/` (git-ignored; they hold paths and prompts).

## What a run is

One (model, engine) = one run of a scripted conversation through `marlowe daemon` on a throwaway `MARLOWE_HOME`, with a
temp git repo as the workspace. The script (`STEPS` in `audit.py`) is built to exercise everything that can rewrite
history: thinking on; tool calls with several arguments (nested JSON in `multi_edit`, numbers, booleans, quotes, newlines,
unicode); a tool error; a tool result with trailing whitespace; a memory write and recall; an interrupt that leaves a
partial answer; a steer in the middle of a turn; a file changed outside; the Teams toggle on and off; several user messages;
and a daemon restart between turns. The model chooses its own calls, so `coverage` in the report says what it did.

Engines:

- `ollama`: the daemon with `MARLOWE_LLAMA_ENGINE=0`; every call goes to Ollama through a tee (`tap.py`) set as
  `MARLOWE_OLLAMA_ADDR`. Ollama 0.32 runs every model on a llama-server and relays that server's log into `server.log`;
  the tee notes the log's size before and after each call and keeps the lines in between, so the numbers are the runner's
  own: `new prompt ... task.n_tokens` (read), the first `cached n_tokens` (served from cache), `restored context checkpoint`,
  `stop processing: n_tokens` (what the slot held afterwards).
- `llama`: Marlowe's own llama-server, only for a model Ollama hands to llama-server whole (no renderer, parser or template
  layer). `MARLOWE_LLAMA_SERVER` points at `llama_wrap.py`, which starts Ollama's real server on another port and sits on the
  daemon's port as the tee. The numbers are the response `timings` (`prompt_n`, `cache_n`). For every call the wrapper also
  asks the real server's `/apply-template` how the exact request renders, and how the request plus the model's reply as it
  wrote it renders.

## What a verdict is

For request *n*: `expected` = tokens the slot held after request *n-1* (prompt + what it generated); `gap` = expected minus
what the server served from cache. Up to 8 tokens is the stop token and newline the template re-adds (`ok`). More is a
`BREAK`; a cache emptied by a daemon restart is `cold` (listed too: the rule has no exception for it).

A hybrid model (Qwen 3.5 is one) cannot roll its recurrent state back, so one differing token in the model's last reply
costs that whole reply (hundreds of tokens), and a differing token far back costs everything after the last checkpoint.

For each break the report gives the cause, found by looking at, in order:

1. **wire diff**: the first message the daemon changed between two requests (field and text);
2. **reply echo**: the model's reply against the assistant message sent back for it (thinking dropped, content trimmed, tool
   arguments re-ordered or re-serialised, a reply missing from the next request altogether, as after an interrupt);
3. **prompt diff**: the first differing **byte** between *the previous conversation plus the reply as the model wrote it*,
   rendered, and this request's rendered prompt; which message holds it; whether it is history or the reply itself; and
   whether a thinking block went missing there, and who dropped it (the model's template, Ollama's renderer, or Ollama's own
   request to llama-server, which carries no `reasoning_content`);
4. **token check**: tokens the model generated for the reply against tokens the template prints for it. Both are normally
   equal; a request that is off by more than the run's usual difference had a reply that came back in a different form.

A request whose first user message differs from every earlier one is a `reset` (a new conversation or a compaction, the one
rewrite the rule allows); it is listed, not judged. A break whose prompts are identical but whose gap is a whole reply is
reported as such: the text the model wrote is not the text the template prints for what was parsed out of it, and the chat
APIs do not hand back the raw text to say where.

Where the prompt comes from: `llama` runs use the server's own `/apply-template`. For `ollama` runs Ollama does not log
what it renders, so a renderer model (marlowe-dawn, marlowe-mini) is rendered by `qwen35_mirror.py`, a Python port of
Ollama's `qwen3.5` renderer; a model with no renderer is rendered as Ollama's llama-server chat request (no reasoning) through
its GGUF template in jinja2 (the `llama` runs report how many prompts that matched byte for byte); a model with a Go template
layer (marlowe-dusk:27b-super) is not mirrored and gets no byte position. Only the byte localisation uses these; every number
comes from the server.

`--templates` reads each model's chat template out of its GGUF header (no library: `gguf.py`), then renders a probe
conversation through a CPU-only llama-server and looks for the model's earlier reasoning, with the template's switches
(`preserve_thinking`, `clear_thinking`, `enable_thinking`) tried.

## Files

| file | job |
|---|---|
| `audit.py` | the driver: runs the script against the daemon, `--templates`, `--analyze` |
| `tap.py` | the HTTP tee and its records |
| `llama_wrap.py` | stands in for llama-server on the engine path |
| `analyze.py` | readers of the servers' answers and logs, the verdicts, the cause finder |
| `report.py` | per-run `analysis.json`, `report.md`, `report.json` |
| `templates.py`, `gguf.py` | the chat-template probe and the GGUF header reader |
| `qwen35_mirror.py` | diagnosis only: Ollama's qwen3.5 prompt renderer, ported |
| `test_audit.py` | offline checks |

Needs Python 3.11 and, for the `ollama`-path byte localisation of a template model, `jinja2` (optional).
