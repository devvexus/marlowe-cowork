"""Writes whether a credential variable reached this process, then idles (a stand-in service)."""
import os
import sys
import time

with open(sys.argv[1], "w") as f:
    f.write(os.environ.get("GROQ_API_KEY", "ABSENT"))
print("probe ready", flush=True)
time.sleep(60)
