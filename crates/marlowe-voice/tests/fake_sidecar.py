"""A stand-in for tools/voice/chatterbox_sidecar.py in the marlowe-voice tests: the same protocol,
one part per sentence (". "), each a 300 ms tone, no model, no GPU. FAKE_SIDECAR_MODE picks a
behaviour:

  ok        answers every request (after a stale reply with the wrong id)
  fatal     reports {"fatal": ...} instead of loading
  crash     writes a traceback to stderr and exits before ready
  slowload  takes FAKE_SIDECAR_LOAD seconds (default 10) to load
  die-once  exits on its first request ever (a marker in the cache dir), then behaves
  die-mid   on its first request ever sends the first part, then exits; then behaves
  hang      never answers
  slow      takes 5 s per part unless cancelled
  error     answers every request with an error
  oom       answers every request with a CUDA out-of-memory error (numbers vary)
  child     starts a child process (as a venv launcher does) and records both pids

Every start appends its pid to <cache>/spawns.log; cancels go to <cache>/cancels.log.
FAKE_SIDECAR_SILENT=1 makes every tone silence (a binary-level check that must not be heard).
"""

import argparse
import json
import math
import os
import queue
import struct
import subprocess
import sys
import threading
import time
import wave


def tone(path, sr=24000, seconds=0.3):
    n = int(sr * seconds)
    amp = 0 if os.environ.get("FAKE_SIDECAR_SILENT") == "1" else 16000
    frames = b"".join(struct.pack("<h", int(amp * math.sin(2 * math.pi * 220 * i / sr))) for i in range(n))
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(sr)
        w.writeframes(frames)
    return int(seconds * 1000)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cache", required=True)
    ap.add_argument("--ref", required=True)
    ap.add_argument("--idle", type=float, default=300)
    ap.add_argument("--device", default="auto")
    for flag in ("--ex", "--cfg", "--temp", "--pause"):
        ap.add_argument(flag)
    args = ap.parse_args()
    mode = os.environ.get("FAKE_SIDECAR_MODE", "ok")
    cache = args.cache
    with open(os.path.join(cache, "spawns.log"), "a") as f:
        f.write("%d\n" % os.getpid())
    out = sys.stdout

    def send(obj):
        out.write(json.dumps(obj) + "\n")
        out.flush()

    print("fake sidecar starting in mode " + mode, file=sys.stderr, flush=True)
    if mode == "fatal":
        send({"fatal": "chatterbox is not importable with this interpreter (No module named 'chatterbox')"})
        return 1
    if mode == "crash":
        print("RuntimeError: CUDA error: out of memory", file=sys.stderr, flush=True)
        return 3
    if mode == "slowload":
        time.sleep(float(os.environ.get("FAKE_SIDECAR_LOAD", "10")))
    # Library chatter on stdout must not break the protocol.
    out.write("Loading weights...\n")
    out.flush()
    ready = {"ready": True, "device": "cpu", "sr": 24000, "pid": os.getpid()}
    if mode == "child":
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        ready["child"] = child.pid
        with open(os.path.join(cache, "pids.txt"), "w") as f:
            f.write("%d %d\n" % (os.getpid(), child.pid))
    send(ready)

    requests, cancelled = queue.Queue(), set()

    def reader():
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            msg = json.loads(line)
            if "cancel" in msg:
                cancelled.add(msg["cancel"])
                with open(os.path.join(cache, "cancels.log"), "a") as f:
                    f.write("%s\n" % msg["cancel"])
                continue
            requests.put(msg)
        requests.put(None)

    threading.Thread(target=reader, daemon=True).start()
    while True:
        try:
            req = requests.get(timeout=args.idle if args.idle > 0 else None)
        except queue.Empty:
            return 0
        if req is None:
            return 0
        rid = req["id"]
        marker = os.path.join(cache, "died.marker")
        first_ever = not os.path.exists(marker)
        if first_ever:
            open(marker, "w").close()
        if mode == "die-once" and first_ever:
            os._exit(5)
        if mode == "hang":
            continue
        if mode == "error":
            send({"id": rid, "error": "RuntimeError: text too strange"})
            continue
        if mode == "oom":
            send({"id": rid, "error": "OutOfMemoryError: CUDA out of memory. Tried to allocate %d.00 MiB" % (rid * 20)})
            continue
        plan = [s.strip().rstrip(".") for s in req["text"].split(". ") if s.strip()]
        send({"id": rid + 1000, "wav": "stale"})
        send({"id": rid, "plan": plan})
        parts = 1 if req.get("whole") else max(1, len(plan))
        for seq in range(parts):
            if mode == "slow":
                for _ in range(100):
                    if rid in cancelled:
                        break
                    time.sleep(0.05)
            if rid in cancelled:
                send({"id": rid, "error": "cancelled"})
                break
            path = os.path.join(cache, "utt-%d-%s-%d-%d.wav" % (os.getpid(), rid, seq, time.time_ns()))
            ms = tone(path)
            send({"id": rid, "seq": seq, "wav": path, "ms": ms, "last": seq + 1 == parts})
            if mode == "die-mid" and first_ever:
                time.sleep(0.2)
                os._exit(6)
        print("fake sidecar: request %s: %d chars in %d parts" % (rid, len(req["text"]), parts), file=sys.stderr, flush=True)


if __name__ == "__main__":
    sys.exit(main())
