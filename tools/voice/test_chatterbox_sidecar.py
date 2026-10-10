"""Pure parts of the Chatterbox sidecar (no torch, no model): `python -m pytest tools/voice`."""

import importlib.util
import os
import wave

import numpy as np

_spec = importlib.util.spec_from_file_location("chatterbox_sidecar", os.path.join(os.path.dirname(__file__), "chatterbox_sidecar.py"))
sc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(sc)


def test_punctuation_becomes_pauses_and_fragments_merge():
    assert sc.split("Good morning, sir. Systems are online.") == [("Good morning, sir", 0.55), ("Systems are online", 0.55)]
    assert sc.split("Wait... what? Really!") == [("Wait, what?, Really!", 0.55)]
    assert sc.split("No punctuation at all here") == [("No punctuation at all here.", 0.5)]
    assert sc.split("") == [] and sc.split("...") == []


def test_blank_lines_take_the_longest_pause():
    assert sc.split("First line of text\n\nSecond line of text") == [("First line of text", 0.9), ("Second line of text.", 0.5)]


def test_wavs_are_16_bit_mono_and_pruned(tmp_path):
    path = str(tmp_path / "utt-1-1-1.wav")
    ms = sc.write_wav(path, np.linspace(-2.0, 2.0, 2400, dtype="float32"), 24000)
    assert ms == 100
    with wave.open(path, "rb") as w:
        assert (w.getnchannels(), w.getsampwidth(), w.getframerate(), w.getnframes()) == (1, 2, 24000, 2400)
    for i in range(3):
        sc.write_wav(str(tmp_path / ("utt-2-%d-1.wav" % i)), np.zeros(10, dtype="float32"), 24000)
    (tmp_path / "keep.wav").write_bytes(b"x")
    sc.prune(str(tmp_path), 1)
    assert sorted(p.name for p in tmp_path.iterdir()) == ["keep.wav", "utt-2-2-1.wav"]


def test_prune_tolerates_files_vanishing(tmp_path, monkeypatch):
    for i in range(3):
        (tmp_path / ("utt-1-%d-1.wav" % i)).write_bytes(b"x")
    real = sc.os.path.getmtime

    def flaky(p):
        if p.endswith("utt-1-1-1.wav"):
            raise FileNotFoundError(p)
        return real(p)

    monkeypatch.setattr(sc.os.path, "getmtime", flaky)
    sc.prune(str(tmp_path), 0)
    assert list(tmp_path.iterdir()) == []


class _Tensor:
    def __init__(self, a):
        self.a = a

    def squeeze(self, _):
        return self

    def detach(self):
        return self

    def cpu(self):
        return self

    def numpy(self):
        return self.a


class _Model:
    """generate() returns 0.1 s of ones per call; no prepare_conditionals, like old chatterbox."""

    sr = 1000

    def __init__(self):
        self.calls = []

    def generate(self, text, **kw):
        self.calls.append((text, kw.get("audio_prompt_path")))
        return _Tensor(np.ones((1, 100), dtype="float32").reshape(100))


def test_parts_stream_one_clause_at_a_time_with_their_pauses(tmp_path):
    ref = tmp_path / "me.wav"
    ref.write_bytes(b"RIFF")
    model = _Model()
    speaker = sc.Speaker(model, str(ref), dict(sc.DEFAULTS))
    chunks = sc.split("Good morning, sir. Systems are online.")
    parts = list(speaker.parts(1, chunks, {}, set()))
    assert [(seq, last) for seq, _, last in parts] == [(0, False), (1, True)]
    # 100 samples of speech, then the 0.55 s full stop scaled by 0.7 (385 samples) before the next.
    assert [len(a) for _, a, _ in parts] == [100 + 385, 100]
    assert [c[0] for c in model.calls] == ["Good morning, sir", "Systems are online"]
    assert all(c[1] == str(ref) for c in model.calls)
    (seq, whole, last), = list(speaker.parts(2, chunks, {}, set(), whole=True))
    assert (seq, len(whole), last) == (0, 585, True)
    (seq, silence, last), = list(speaker.parts(3, [], {}, set()))
    assert (seq, len(silence), last) == (0, 100, True) and not silence.any()


def test_a_cancel_stops_between_clauses(tmp_path):
    ref = tmp_path / "me.wav"
    ref.write_bytes(b"RIFF")
    speaker = sc.Speaker(_Model(), str(ref), dict(sc.DEFAULTS))
    gen = speaker.parts(7, sc.split("One clause here. Another clause there."), {}, {7})
    try:
        next(gen)
        raise AssertionError("expected Cancelled")
    except sc.Cancelled:
        pass
