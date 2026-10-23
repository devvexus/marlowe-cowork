"""Reads a model's own chat template out of its GGUF header, and answers what the template does with earlier reasoning.

No GGUF library: the header is a short key/value list, and the template is one string in it. Also resolves an Ollama
model tag to its weights blob and to the manifest facts that decide who renders its prompt.
"""

import json
import os
import re
import struct

FIXED = {0: 1, 1: 1, 2: 2, 3: 2, 4: 4, 5: 4, 6: 4, 7: 1, 10: 8, 11: 8, 12: 8}
FMT = {0: "<B", 1: "<b", 2: "<H", 3: "<h", 4: "<I", 5: "<i", 6: "<f", 7: "<?", 10: "<Q", 11: "<q", 12: "<d"}
STRING, ARRAY = 8, 9


def _str(f):
    (n,) = struct.unpack("<Q", f.read(8))
    return f.read(n).decode("utf-8", "replace")


def _skip(f, t):
    if t in FIXED:
        f.seek(FIXED[t], 1)
    elif t == STRING:
        (n,) = struct.unpack("<Q", f.read(8))
        f.seek(n, 1)
    elif t == ARRAY:
        (et,) = struct.unpack("<I", f.read(4))
        (n,) = struct.unpack("<Q", f.read(8))
        if et in FIXED:
            f.seek(FIXED[et] * n, 1)
        else:
            for _ in range(n):
                _skip(f, et)
    else:
        raise ValueError(f"unknown gguf type {t}")


def read_header(path, keep=("general.architecture", "general.name", "tokenizer.chat_template")):
    """The wanted string/number keys of a GGUF file (the big token arrays are skipped, not read)."""
    out = {}
    with open(path, "rb", buffering=1 << 20) as f:
        if f.read(4) != b"GGUF":
            raise ValueError("not a GGUF file")
        (_version,) = struct.unpack("<I", f.read(4))
        _tensors, kvs = struct.unpack("<QQ", f.read(16))
        for _ in range(kvs):
            key = _str(f)
            (t,) = struct.unpack("<I", f.read(4))
            wanted = key in keep or key.startswith("tokenizer.chat_template")
            if wanted and t == STRING:
                out[key] = _str(f)
            elif wanted and t in FMT:
                out[key] = struct.unpack(FMT[t], f.read(FIXED[t]))[0]
            else:
                _skip(f, t)
    return out


def ollama_root():
    return os.path.join(os.environ.get("OLLAMA_MODELS") or os.path.join(os.path.expanduser("~"), ".ollama", "models"))


def manifest_path(model):
    name, _, tag = model.partition(":")
    return os.path.join(ollama_root(), "manifests", "registry.ollama.ai", "library", name, tag or "latest")


def manifest_facts(model):
    """{blob, renderer, parser, template (text or None), layers} for an Ollama tag."""
    with open(manifest_path(model), encoding="utf-8") as f:
        m = json.load(f)
    blobs = os.path.join(ollama_root(), "blobs")

    def blob(d):
        return os.path.join(blobs, d.replace(":", "-"))

    cfg = {}
    try:
        with open(blob(m["config"]["digest"]), encoding="utf-8") as f:
            cfg = json.load(f)
    except OSError:
        pass
    facts = {"blob": None, "renderer": cfg.get("renderer") or None, "parser": cfg.get("parser") or None, "template": None, "layers": []}
    for l in m.get("layers", []):
        kind = l["mediaType"].rsplit(".", 1)[-1]
        facts["layers"].append(kind)
        if kind == "model":
            facts["blob"] = blob(l["digest"])
        elif kind == "template":
            with open(blob(l["digest"]), encoding="utf-8") as f:
                facts["template"] = f.read()
    return facts


def engine_eligible(facts):
    """Marlowe's engine takes a model Ollama hands to llama-server whole: no renderer, parser or extra layer."""
    extra = [k for k in facts["layers"] if k not in ("model", "params", "license")]
    return not facts["renderer"] and not facts["parser"] and not extra


# What a template does with reasoning, read from its text. A heuristic; template_probe() in audit.py checks it by rendering.
def template_notes(tpl):
    if not tpl:
        return {"has_template": False}
    t = tpl
    return {
        "has_template": True,
        "chars": len(t),
        "mentions_reasoning": bool(re.search(r"reasoning_content|<think>", t)),
        "last_query_index": "last_query_index" in t,
        "drops_by_position": bool(re.search(r"loop\.index0\s*>\s*(ns\.)?last_query_index|last_query_index", t)),
        "switches": sorted(set(re.findall(r"\b(preserve_thinking|clear_thinking|keep_thinking|enable_thinking|reasoning_effort|thinking)\b", t))),
    }
