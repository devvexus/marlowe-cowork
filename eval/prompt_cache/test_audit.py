"""Offline checks for the prompt-cache audit: the tee, the answer readers, the log reader, the verdicts, the template reader.
Nothing here starts a model, the daemon or a server.  python -I -m pytest eval/prompt_cache"""

import json
import socket
import struct
import threading

import analyze as an
import gguf
import qwen35_mirror as q
import tap as tp


# ---------------------------------------------------------------- the tee

def fake_server(reply):
    """One-shot HTTP server: returns (port, requests-seen list)."""
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    s.listen(1)
    seen = []

    def run():
        c, _ = s.accept()
        buf = b""
        while b"\r\n\r\n" not in buf:
            buf += c.recv(65536)
        head, rest = buf.split(b"\r\n\r\n", 1)
        n = int([l for l in head.split(b"\r\n") if l.lower().startswith(b"content-length")][0].split(b":")[1])
        while len(rest) < n:
            rest += c.recv(65536)
        seen.append((head, rest))
        c.sendall(reply)
        c.close()
        s.close()

    threading.Thread(target=run, daemon=True).start()
    return s.getsockname()[1], seen


def test_the_tee_forwards_untouched_and_records_both_bodies(tmp_path):
    body = b'data: {"choices":[{"delta":{"content":"hi"}}]}\n\ndata: [DONE]\n\n'
    chunked = b"%x\r\n%s\r\n0\r\n\r\n" % (len(body), body)
    port, seen = fake_server(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n" + chunked)
    got = []
    t = tp.Tap(0, ("127.0.0.1", port), got.append, raw_dir=str(tmp_path / "raw"))
    t.start()
    front = t.sock.getsockname()[1]
    req = json.dumps({"messages": [{"role": "user", "content": "hello"}]}).encode()
    c = socket.create_connection(("127.0.0.1", front))
    c.sendall(b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\nContent-Length: %d\r\n\r\n%s" % (len(req), req))
    out = b""
    while True:
        d = c.recv(65536)
        if not d:
            break
        out += d
    c.close()
    t.stop()
    assert out.endswith(chunked), "the answer reaches the client byte for byte"
    head, rest = seen[0]
    assert rest == req and b"Connection: close" in head and b"keep-alive" not in head
    assert len(got) == 1 and got[0]["status"] == 200 and got[0]["path"] == "/v1/chat/completions"
    sent, answer = tp.load_record_bodies(str(tmp_path / "raw"), got[0])
    assert json.loads(sent)["messages"][0]["content"] == "hello" and answer == body.decode()


def test_dechunk():
    assert tp.dechunk(b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n") == b"abcde"


# ---------------------------------------------------------------- what the servers say

def test_an_ollama_answer_keeps_the_order_of_tool_arguments():
    text = "\n".join(json.dumps(f) for f in [
        {"message": {"role": "assistant", "thinking": "hm"}},
        {"message": {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "write", "arguments": {"path": "a", "content": "b"}}}]}},
        {"message": {"role": "assistant", "content": ""}, "done": True, "done_reason": "stop", "prompt_eval_count": 12, "eval_count": 5},
    ])
    a = an.agg_ollama(text)
    assert a["thinking"] == "hm" and a["eval_count"] == 5
    assert list(a["tool_calls"][0]["arguments"]) == ["path", "content"]


def test_an_openai_stream_is_folded_with_its_timings():
    ev = [
        {"choices": [{"delta": {"reasoning_content": "t"}}]},
        {"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c", "function": {"name": "read", "arguments": "{\"pa"}}]}}]},
        {"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "th\":\"a\"}"}}]}}]},
        {"choices": [{"delta": {}, "finish_reason": "tool_calls"}], "timings": {"prompt_n": 10, "cache_n": 90, "predicted_n": 7}},
    ]
    a = an.agg_openai("".join(f"data: {json.dumps(e)}\n\n" for e in ev) + "data: [DONE]\n\n")
    assert a["thinking"] == "t" and a["tool_calls"][0]["arguments"] == '{"path":"a"}'
    n = an.numbers_openai(a)
    assert (n["total"], n["cached"], n["gen"], n["end"]) == (100, 90, 7, 107)


def test_an_interrupted_call_is_estimated_from_its_progress_and_the_tokenised_reply():
    a = an.agg_openai('data: {"choices":[{"delta":{"content":"x"}}],"prompt_progress":{"total":100,"cache":90,"processed":100}}\n\n')
    n = an.numbers_openai(a, {"ideal_tokens": 140})
    assert (n["total"], n["cached"], n["end"]) == (100, 90, 140) and "estimated" in n["source"]


LOG = """slot get_availabl: id  0 | task -1 | selected slot by LCP similarity, sim_best = 0.949 (> 0.100 thold), f_keep = 0.956
slot   operator(): id  0 | task 50 | new prompt, n_ctx_slot = 32768, n_keep = 4, task.n_tokens = 8662
slot   operator(): id  0 | task 50 | restored context checkpoint (pos_min = 7195, pos_max = 7195, n_tokens = 7196, n_past = 7195, size = 99.5 MiB)
slot   operator(): id  0 | task 50 | cached n_tokens = 7196, memory_seq_rm [7196, end)
slot   operator(): id  0 | task 50 | cached n_tokens = 7708, memory_seq_rm [7708, end)
slot print_timing: id  0 | task 50 | prompt eval time =     340.19 ms /  1466 tokens (    0.24 ms per token,  4156.45 tokens per second)
slot print_timing: id  0 | task 50 |        eval time =    1142.16 ms /   129 tokens (    5.98 ms per token,   167.23 tokens per second)
slot      release: id  0 | task 50 | stop processing: n_tokens = 8791, truncated = 0
"""


def test_the_server_log_gives_read_cached_and_slot_length():
    n = an.numbers_ollama(LOG, {"prompt_eval_count": 8662, "eval_count": 129})
    assert (n["total"], n["cached"], n["gen"], n["end"]) == (8662, 7196, 129, 8791)
    assert n["restored"][0]["n_past"] == 7195 and n["f_keep"] == 0.956


# ---------------------------------------------------------------- the history and the reply

def test_a_reply_sent_back_with_sorted_keys_is_named():
    produced = {"thinking": "t", "content": "", "tool_calls": [{"name": "write", "arguments": {"path": "a", "content": "b"}}]}
    sent = {"role": "assistant", "thinking": "t", "content": "", "tool_calls": [{"function": {"name": "write", "arguments": {"content": "b", "path": "a"}}}]}
    assert any("keys reordered" in e for e in an.echo_diff(produced, sent))
    again = {"role": "assistant", "thinking": "t", "content": "", "tool_calls": [{"function": {"name": "write", "arguments": {"path": "a", "content": "b"}}}]}
    assert an.echo_diff(produced, again) == []


def test_openai_arguments_are_compared_as_text_and_as_objects():
    produced = {"thinking": "", "content": "", "tool_calls": [{"name": "r", "arguments": '{"b": 1, "a": 2}'}]}
    spaced = {"content": "", "tool_calls": [{"function": {"name": "r", "arguments": '{"b":1,"a":2}'}}]}
    assert any("re-serialised" in e for e in an.echo_diff(produced, spaced))
    sorted_ = {"content": "", "tool_calls": [{"function": {"name": "r", "arguments": '{"a":2,"b":1}'}}]}
    assert any("reordered" in e for e in an.echo_diff(produced, sorted_))


def test_thinking_dropped_is_named():
    out = an.echo_diff({"thinking": "long thought", "content": "x", "tool_calls": []}, {"role": "assistant", "content": "x"})
    assert out and "thinking dropped" in out[0]


def test_the_first_message_the_daemon_changed():
    a = [{"role": "user", "content": "x"}, {"role": "assistant", "content": "y"}]
    assert an.msg_diff(a, a + [{"role": "user", "content": "z"}]) is None
    d = an.msg_diff(a, [a[0], {"role": "assistant", "content": "y2"}])
    assert d["index"] == 1 and d["field"] == "content"


def test_nested_object_keys_that_arrive_sorted_are_flagged():
    sorted_ = {"tool_calls": [{"arguments": {"edits": [{"new_string": "b", "old_string": "a"}], "path": "p"}}]}
    assert any("already sorted" in f for f in an.reply_features(sorted_))
    own = {"tool_calls": [{"arguments": {"edits": [{"old_string": "a", "new_string": "b"}]}}]}
    assert not any("already sorted" in f for f in an.reply_features(own))


# ---------------------------------------------------------------- the prompts

def test_the_prompt_diff_locates_the_byte_and_the_message():
    ideal = {"text": "SYS|U1|A1<think>deep</think>"}
    cur = {"text": "SYS|U1|A1|U2", "starts": [0, 4, 7, 11]}
    pd = an.prompt_diff(ideal, cur, 3)
    assert pd["char"] == 9 and pd["message"] == 2 and pd["region"] == "history"
    assert an.prompt_diff({"text": "SYS|U1"}, {"text": "SYS|U1|A1", "starts": [0, 4]}, 2) is None


def test_the_ollama_renderer_port_drops_earlier_thinking_after_a_new_user_message():
    msgs = [{"role": "system", "content": "s"}, {"role": "user", "content": "u1"}, {"role": "assistant", "content": "", "thinking": "RSN-A"}]
    assert "RSN-A" in q.render(msgs, [], True)
    after = q.render(msgs + [{"role": "user", "content": "u2"}], [], True)
    assert "RSN-A" not in after


class Fake:
    def __init__(self, seq, t, total, cached, gen, msgs, agg=None, ideal=None, render=None):
        self.seq, self.t_start, self.messages, self.aborted = seq, t, msgs, False
        self.nums = {"total": total, "cached": cached, "gen": gen, "end": total + gen, "source": "t"}
        self.agg = agg or {}
        self.ideal, self.render = ideal, render
        self.render_tokens = self.ideal_tokens = None


def test_the_verdicts():
    m1 = [{"role": "user", "content": "a"}]
    m2 = m1 + [{"role": "assistant", "content": "r"}, {"role": "user", "content": "b"}]
    ok = an.judge([Fake(1, 1, 100, 0, 10, m1), Fake(2, 2, 130, 109, 5, m2)])
    assert ok[1]["verdict"] == "ok" and ok[1]["deficit"] == 1
    bad = an.judge([Fake(1, 1, 100, 0, 10, m1), Fake(2, 2, 130, 40, 5, m2)])
    assert bad[1]["verdict"] == "BREAK" and bad[1]["deficit"] == 70 and bad[1]["cause"]
    cold = an.judge([Fake(1, 1, 100, 0, 10, m1), Fake(2, 3, 130, 0, 5, m2)], restarts=[2])
    assert cold[1]["verdict"].startswith("cold")


def test_a_break_with_an_identical_prompt_is_called_a_round_trip_problem():
    m1 = [{"role": "user", "content": "a"}]
    m2 = m1 + [{"role": "assistant", "content": "r"}]
    r = {"text": "x", "starts": [0, 1]}
    rows = an.judge([Fake(1, 1, 100, 0, 10, m1, agg={"content": "r"}, ideal={"text": "x"}), Fake(2, 2, 130, 50, 5, m2, ideal={"text": "x"}, render=r)])
    assert rows[1]["verdict"] == "BREAK" and "round trip" in rows[1]["cause"]


def test_a_new_first_user_message_is_a_reset_not_a_break():
    a = [{"role": "user", "content": "a"}]
    b = [{"role": "user", "content": "summary of a"}, {"role": "user", "content": "c"}]
    rows = an.judge([Fake(1, 1, 100, 0, 10, a), Fake(2, 2, 40, 0, 5, b)])
    assert rows[1]["verdict"].startswith("reset")


def test_a_reply_missing_from_the_next_request_is_named():
    m1 = [{"role": "user", "content": "a"}]
    m2 = m1 + [{"role": "user", "content": "b"}]
    p = Fake(1, 1, 100, 0, 10, m1, agg={"content": "partial answer", "thinking": ""})
    p.aborted = True
    rows = an.judge([p, Fake(2, 2, 130, 40, 5, m2)])
    assert any("not in the next request at all" in e for e in rows[1]["echo"]) and "not sent back as written" in rows[1]["cause"]


def test_the_token_count_of_the_reply_is_checked_against_what_the_template_prints():
    m1 = [{"role": "user", "content": "a"}]
    m2 = m1 + [{"role": "assistant", "content": "r"}]
    p = Fake(1, 1, 100, 0, 10, m1, agg={"content": "r"}, ideal={"text": "x"})
    p.render_tokens, p.ideal_tokens = 100, 114   # the template prints 13 tokens for a reply the model wrote in 10
    rows = an.judge([p, Fake(2, 2, 130, 50, 5, m2, agg={"content": "r"}, ideal={"text": "x"}, render={"text": "x", "starts": [0, 1]})])
    assert rows[1]["reply_tokens"]["difference"] == -3
    assert "generated 10 tokens and the template prints 13" in rows[1]["cause"]


# ---------------------------------------------------------------- the template reader

def gguf_file(path, template):
    def s(x):
        b = x.encode()
        return struct.pack("<Q", len(b)) + b

    kvs = [
        s("general.architecture") + struct.pack("<I", 8) + s("qwen35"),
        s("tokenizer.ggml.tokens") + struct.pack("<I", 9) + struct.pack("<I", 8) + struct.pack("<Q", 3) + s("a") + s("bb") + s("ccc"),
        s("tokenizer.ggml.scores") + struct.pack("<I", 9) + struct.pack("<I", 6) + struct.pack("<Q", 3) + struct.pack("<3f", 1, 2, 3),
        s("tokenizer.chat_template") + struct.pack("<I", 8) + s(template),
    ]
    with open(path, "wb") as f:
        f.write(b"GGUF" + struct.pack("<I", 3) + struct.pack("<QQ", 0, len(kvs)) + b"".join(kvs))


def test_the_template_is_read_past_the_token_arrays(tmp_path):
    p = tmp_path / "m.gguf"
    gguf_file(p, "{% if preserve_thinking %}x{% endif %}")
    h = gguf.read_header(str(p))
    assert h["general.architecture"] == "qwen35" and h["tokenizer.chat_template"].startswith("{% if preserve")
    notes = gguf.template_notes(h["tokenizer.chat_template"])
    assert "preserve_thinking" in notes["switches"]
