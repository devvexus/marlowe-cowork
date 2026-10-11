"""Chatterbox voice-clone sidecar for marlowe-voice. A placeholder until the M6 voice.

Ports `split()` and the defaults of the reference `speak.py` (AI_RESEARCH/voice): punctuation
becomes real pauses, 1-2 word fragments merge into their neighbour, exaggeration 0.5, cfg_weight
1.0, temperature 0.6, pause scale 0.7. Plays nothing: it writes a WAV and the Rust side plays it,
so barge-in and the presence meter keep working.

Protocol, JSON lines. stdout carries protocol lines only (flushed per line); everything else,
library chatter included, goes to stderr.
  out  {"ready": true, "device": "cuda", "sr": 24000, "pid": 123}     once the model is loaded
  out  {"fatal": "..."}                                               the model could not load
  in   {"id": 1, "text": "...", "whole"?, "ex"?, "cfg"?, "temp"?, "pause"?, "ref"?}
  out  {"id": 1, "plan": ["Good morning, sir", "Systems are online"]}  the clauses, first
  out  {"id": 1, "seq": 0, "wav": "<cache>\\utt-....wav", "ms": 1310, "last": false}  per clause,
       its pause included; "whole": true gives one part with everything
  out  {"id": 1, "error": "..."}  instead of the rest, at any point
  in   {"cancel": 1}        skip the rest of request 1 (checked between clauses)
The caller reads and deletes each WAV; whatever is left is deleted when the next request starts.
Exits on stdin EOF, or after --idle seconds without a request so VRAM is released.

The heavy imports (torch, chatterbox) happen in main(), so `split` imports with plain Python.
"""

import argparse
import glob
import json
import os
import queue
import re
import sys
import threading
import time
import wave

PAUSE = {",": 0.22, ";": 0.4, ":": 0.4, "-": 0.3, "—": 0.3, ".": 0.55, "!": 0.55, "?": 0.6, "...": 0.8, "\n": 0.9}
TOKEN = re.compile(r"(\.\.\.|…|[,;:.!?—]|\s-\s|\n+)")
DEFAULTS = {"ex": 0.5, "cfg": 1.0, "temp": 0.6, "pause": 0.7}


def split(text):
    """-> list of (chunk_text, pause_seconds_after). Verbatim port of speak.py's split()."""
    out, buf = [], ""
    for part in TOKEN.split(text.replace("…", "...")):
        if part is None or part == "":
            continue
        if TOKEN.fullmatch(part):
            key = "\n" if part.startswith("\n") else part.strip()
            if buf.strip():
                # keep terminal punctuation on the chunk so the model gets the right intonation
                tail = part if key in "!?" or key == "..." else ""
                out.append((buf.strip() + tail, PAUSE.get(key, 0.3)))
            elif out:
                out[-1] = (out[-1][0], max(out[-1][1], PAUSE.get(key, 0.3)))
            buf = ""
        else:
            buf += part
    if buf.strip():
        out.append((buf.strip() + ".", 0.5))
    # merge tiny chunks into the next one: 1-2 word fragments get over-emphasized alone
    merged, i = [], 0
    while i < len(out):
        txt, pause = out[i]
        while len(txt.split()) < 3 and i + 1 < len(out):
            i += 1
            txt, pause = txt.rstrip(".,") + ", " + out[i][0], out[i][1]
        merged.append((txt, pause))
        i += 1
    return merged


def log(msg):
    print("chatterbox: " + msg, file=sys.stderr, flush=True)


def protocol_stream():
    """A private copy of stdout for protocol lines; fd 1 and sys.stdout then point at stderr, so
    nothing a library prints (Python or C level) can corrupt the protocol."""
    fd = os.dup(1)
    os.dup2(2, 1)
    try:
        sys.stderr.reconfigure(encoding="utf-8", errors="backslashreplace")
    except (AttributeError, ValueError):
        pass
    sys.stdout = sys.stderr
    return os.fdopen(fd, "w", encoding="utf-8", newline="\n")


class Proto:
    def __init__(self, stream):
        self.stream = stream
        self.lock = threading.Lock()

    def send(self, obj):
        with self.lock:
            self.stream.write(json.dumps(obj, ensure_ascii=True) + "\n")
            self.stream.flush()


def read_requests(requests, cancelled):
    """stdin -> `requests` (None on EOF); `{"cancel": id}` lines go straight to `cancelled`."""
    try:
        sys.stdin.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, ValueError):
        pass
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            log("ignored a line that is not JSON")
            continue
        if not isinstance(msg, dict):
            continue
        if "cancel" in msg:
            cancelled.add(msg["cancel"])
            continue
        requests.put(msg)
    requests.put(None)


def _mtime(path):
    try:
        return os.path.getmtime(path)
    except OSError:  # deleted meanwhile (the caller deletes what it reads)
        return 0.0


def prune(cache, keep):
    """Deletes all but the newest `keep` utterance WAVs. Never raises."""
    try:
        wavs = sorted(glob.glob(os.path.join(cache, "utt-*.wav")), key=_mtime)
    except OSError:
        return
    for p in wavs[: max(0, len(wavs) - keep)]:
        try:
            os.remove(p)
        except OSError:
            pass


def write_wav(path, samples, sr):
    """16-bit PCM mono. `samples` is a float numpy array in [-1, 1]."""
    import numpy as np

    pcm = (np.clip(samples, -1.0, 1.0) * 32767.0).astype("<i2")
    tmp = path + ".part"
    with wave.open(tmp, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(int(sr))
        w.writeframes(pcm.tobytes())
    os.replace(tmp, path)
    return int(round(len(pcm) * 1000 / sr))


class Cancelled(Exception):
    pass


class Speaker:
    def __init__(self, model, ref, params):
        self.model = model
        self.ref = ref
        self.params = params
        self.cond_key = None

    def conditionals(self, ref, ex):
        """speak.py passes audio_prompt_path to every generate(), which re-derives the same
        conditionals from the sample each chunk. They depend only on the sample (exaggeration is
        patched per call by generate itself), so they are prepared once per sample instead."""
        key = (os.path.abspath(ref), os.path.getmtime(ref))
        if key != self.cond_key:
            self.model.prepare_conditionals(ref, exaggeration=ex)
            self.cond_key = key

    def parts(self, rid, chunks, req, cancelled, whole=False):
        """Yields (seq, samples, last): one part per chunk with its pause after it (none after
        the last), or with `whole` a single part with everything."""
        import numpy as np

        p = {k: float(req[k]) if isinstance(req.get(k), (int, float)) else v for k, v in self.params.items()}
        ref = req.get("ref") or self.ref
        if not os.path.isfile(ref):
            raise FileNotFoundError("reference sample not found: " + ref)
        sr = self.model.sr
        cached = hasattr(self.model, "prepare_conditionals")
        if cached:
            self.conditionals(ref, p["ex"])
        pieces = []
        for n, (chunk, pause) in enumerate(chunks):
            if rid in cancelled:
                raise Cancelled()
            kw = dict(exaggeration=p["ex"], cfg_weight=p["cfg"], temperature=p["temp"])
            if not cached:
                kw["audio_prompt_path"] = ref
            wav = self.model.generate(chunk, **kw)
            audio = [wav.squeeze(0).detach().cpu().numpy().astype("float32")]
            last = n + 1 == len(chunks)
            # The gap after the last chunk is dropped: it only delays the end of speech.
            if not last:
                audio.append(np.zeros(int(sr * pause * p["pause"]), dtype="float32"))
            if whole:
                pieces.extend(audio)
            else:
                yield n, np.concatenate(audio), last
        if whole or not chunks:
            if not pieces:
                pieces.append(np.zeros(int(sr * 0.1), dtype="float32"))
            yield 0, np.concatenate(pieces), True


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--cache", required=True, help="directory the WAVs are written to")
    ap.add_argument("--ref", required=True, help="reference voice sample (wav)")
    ap.add_argument("--device", default="auto", choices=["auto", "cuda", "cpu"])
    ap.add_argument("--ex", type=float, default=DEFAULTS["ex"])
    ap.add_argument("--cfg", type=float, default=DEFAULTS["cfg"])
    ap.add_argument("--temp", type=float, default=DEFAULTS["temp"])
    ap.add_argument("--pause", type=float, default=DEFAULTS["pause"])
    ap.add_argument("--idle", type=float, default=300.0, help="seconds without a request before exiting; 0 = never")
    args = ap.parse_args(argv)

    proto = Proto(protocol_stream())
    # Weights come from the local Hugging Face cache; no network unless the caller allows it.
    os.environ.setdefault("HF_HUB_OFFLINE", "1")
    os.environ.setdefault("TQDM_DISABLE", "1")
    cache = os.path.abspath(args.cache)
    os.makedirs(cache, exist_ok=True)
    prune(cache, 0)

    try:
        if not os.path.isfile(args.ref):
            raise FileNotFoundError("reference sample not found: " + args.ref)
        started = time.time()
        import torch
        from chatterbox.tts import ChatterboxTTS

        device = args.device
        if device in ("auto", "cuda") and not torch.cuda.is_available():
            log("CUDA is not available; loading on the CPU (slow)")
            device = "cpu"
        elif device == "auto":
            device = "cuda"
        model = ChatterboxTTS.from_pretrained(device=device)
        log("loaded on %s in %.1f s" % (device, time.time() - started))
    except Exception as e:  # noqa: BLE001 - every load failure is reported, then fatal
        msg = "%s: %s" % (type(e).__name__, e)
        if isinstance(e, ImportError):
            msg = "chatterbox is not importable with this interpreter (" + msg + ")"
        log("fatal: " + msg)
        proto.send({"fatal": msg})
        return 1

    params = {"ex": args.ex, "cfg": args.cfg, "temp": args.temp, "pause": args.pause}
    speaker = Speaker(model, os.path.abspath(args.ref), params)
    requests, cancelled = queue.Queue(), set()
    threading.Thread(target=read_requests, args=(requests, cancelled), daemon=True).start()
    proto.send({"ready": True, "device": device, "sr": int(model.sr), "pid": os.getpid()})

    idle = args.idle if args.idle > 0 else None
    while True:
        try:
            req = requests.get(timeout=idle)
        except queue.Empty:
            log("idle for %d s; exiting to release the GPU" % idle)
            return 0
        if req is None:
            return 0
        rid = req.get("id")
        text = req.get("text")
        if not isinstance(text, str):
            proto.send({"id": rid, "error": "no text"})
            continue
        # Every earlier reply has been read (and its WAV deleted) or given up: what is left is
        # stale. Outside the request's try, so it can never fail the request.
        prune(cache, 0)
        try:
            started = time.time()
            chunks = split(text)
            proto.send({"id": rid, "plan": [c for c, _ in chunks]})
            total = 0
            for seq, samples, last in speaker.parts(rid, chunks, req, cancelled, whole=bool(req.get("whole"))):
                path = os.path.join(cache, "utt-%d-%s-%d-%d.wav" % (os.getpid(), rid, seq, time.time_ns()))
                ms = write_wav(path, samples, model.sr)
                total += ms
                proto.send({"id": rid, "seq": seq, "wav": path, "ms": ms, "last": last})
            log("request %s: %d chars -> %d ms of audio in %.1f s" % (rid, len(text), total, time.time() - started))
        except Cancelled:
            proto.send({"id": rid, "error": "cancelled"})
        except Exception as e:  # noqa: BLE001 - one bad request never takes the sidecar down
            log("request %s failed: %s: %s" % (rid, type(e).__name__, e))
            proto.send({"id": rid, "error": "%s: %s" % (type(e).__name__, e)})
        finally:
            cancelled.discard(rid)


if __name__ == "__main__":
    sys.exit(main())
