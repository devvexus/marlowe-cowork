"""Generates crates/marlowe-code/src/edit/fixtures/edit_cases.json: 150 edit-engine cases with
expectations computed by this independent implementation of the documented tier semantics
(exact -> eol -> whitespace with one uniform indentation shift; unique unless replace_all)."""
import json, random, sys

random.seed(46)

RUST = '''use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A cache of parsed manifests, keyed by path.
pub struct ManifestCache {
    entries: HashMap<PathBuf, Manifest>,
    hits: u64,
    misses: u64,
}

impl ManifestCache {
    pub fn new() -> Self {
        Self { entries: HashMap::new(), hits: 0, misses: 0 }
    }

    pub fn get(&mut self, path: &Path) -> Option<&Manifest> {
        if self.entries.contains_key(path) {
            self.hits += 1;
            return self.entries.get(path);
        }
        self.misses += 1;
        None
    }

    pub fn insert(&mut self, path: PathBuf, manifest: Manifest) {
        self.entries.insert(path, manifest);
    }

    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            return 0.0;
        }
        self.hits as f64 / total as f64
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub dependencies: Vec<String>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut name = None;
        let mut version = None;
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("name = ") {
                name = Some(v.trim_matches('"').to_string());
            } else if let Some(v) = line.strip_prefix("version = ") {
                version = Some(v.trim_matches('"').to_string());
            }
        }
        Ok(Self {
            name: name.ok_or("no name")?,
            version: version.ok_or("no version")?,
            dependencies: Vec::new(),
        })
    }
}
'''

PY = '''import math
import os
from dataclasses import dataclass, field


@dataclass
class RunConfig:
    lr: float = 3e-4
    batch_size: int = 32
    warmup_steps: int = 500
    tags: list = field(default_factory=list)


def cosine_schedule(step, total, base_lr, warmup=500):
    if step < warmup:
        return base_lr * step / warmup
    progress = (step - warmup) / max(1, total - warmup)
    return 0.5 * base_lr * (1 + math.cos(math.pi * progress))


class Trainer:
    def __init__(self, model, config):
        self.model = model
        self.config = config
        self.step = 0
        self.history = []

    def train_step(self, batch):
        loss = self.model.loss(batch)
        lr = cosine_schedule(self.step, 10000, self.config.lr)
        self.model.update(lr)
        self.history.append(loss)
        self.step += 1
        return loss

    def save(self, path):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(str(self.history))


def main():
    config = RunConfig(tags=["baseline"])
    trainer = Trainer(model=None, config=config)
    for _ in range(3):
        trainer.train_step(batch=None)
    trainer.save("runs/baseline/history.txt")
'''

JS = '''import { readFile } from "node:fs/promises";

export async function loadConfig(path) {
  const raw = await readFile(path, "utf8");
  const config = JSON.parse(raw);
  if (!config.port) {
    config.port = 8080;
  }
  return config;
}

export function route(app, config) {
  app.get("/health", (req, res) => {
    res.json({ ok: true, port: config.port });
  });
  app.post("/items", async (req, res) => {
    const item = req.body;
    if (!item || !item.name) {
      res.status(400).json({ error: "name required" });
      return;
    }
    res.status(201).json(item);
  });
}
'''

YAML = '''name: ci
on:
  push:
    branches: [main]
  pull_request:
jobs:
  test:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - name: Build
        run: cargo build --workspace
      - name: Test
        run: cargo test --workspace --no-fail-fast
'''

UNI = '''# Notes — café résumé
日本語のテキスト: 東京
emoji: 🚀 launch ✅ done
naïve façade coöperate
Ελληνικά: αβγ
end of notes — fin
'''

REGEX = '''let re = Regex::new(r"^(\\d+)\\.(\\d+)$").unwrap();
let glob = "src/**/*.{rs,toml}";
let price = "$9.99 (approx.)";
let pipe = a | b || c;
let set = [a-z]+?;
let braces = {x: {y: 1}};
'''

CORPUS = {"rust": RUST, "python": PY, "js": JS, "yaml": YAML}
cases = []


def add(cat, file, old, new, replace_all, expect):
    cases.append({"id": f"{cat}-{sum(1 for c in cases if c['category']==cat)+1:02}", "category": cat,
                  "file": file, "old": old, "new": new, "replace_all": replace_all, "expect": expect})


# ---------------------------------------------------------------- reference semantics

def to_eol(s, eol):
    lf = s.replace("\r\n", "\n")
    return lf if eol == "\n" else lf.replace("\n", "\r\n")


def dominant(t):
    crlf = t.count("\r\n")
    lf = t.count("\n") - crlf
    return "\r\n" if crlf > lf else "\n"


def uniformly_crlf(t):
    c = t.count("\r\n")
    return c > 0 and c == t.count("\n")


def follow(t, new):
    if uniformly_crlf(t):
        return to_eol(new, "\r\n")
    if "\r\n" not in t and "\n" in t and "\r\n" in new:
        return to_eol(new, "\n")
    return new


def spans(t):
    out, start = [], 0
    for line in t.split("\n"):
        pass
    i = 0
    parts = t.split("\n")
    for k, p in enumerate(parts):
        has_nl = k < len(parts) - 1
        if not has_nl and p == "":
            break
        body = p[:-1] if (has_nl and p.endswith("\r")) else p
        nxt = start + len(p) + (1 if has_nl else 0)
        out.append((start, start + len(body), nxt))
        start = nxt
    return out


def lead(s):
    return len(s) - len(s.lstrip())


def shift_of(t, sp, olines):
    shift = None
    for (a, b, _), o in zip(sp, olines):
        f = t[a:b].rstrip()
        o = o.rstrip()
        if f == "" and o == "":
            continue
        if (f == "") != (o == ""):
            return None
        lf, lo = lead(f), lead(o)
        if f[lf:] != o[lo:]:
            return None
        pf, po = f[:lf], o[:lo]
        if pf.endswith(po):
            s = ("add", pf[:lf - lo])
        elif po.endswith(pf):
            s = ("remove", po[:lo - lf])
        else:
            return None
        if s[0] == "remove" and s[1] == "":
            s = ("add", "")
        if shift is None:
            shift = s
        elif shift != s:
            return None
    return shift


def reindent(new, shift, eol):
    n = to_eol(new, "\n")
    if n == "":
        return ""
    ends = n.endswith("\n")
    body = n[:-1] if ends else n
    out = []
    for l in body.split("\n"):
        if l.strip() == "":
            out.append(l)
        elif shift[0] == "add":
            out.append(shift[1] + l)
        else:
            p = shift[1]
            if l.startswith(p):
                out.append(l[len(p):])
            else:
                lw = l[:lead(l)]
                out.append(l[len(lw):] if p.startswith(lw) else l)
    r = eol.join(out)
    if ends:
        r += eol
    return r


def ws_sites(t, old, new):
    olf = to_eol(old, "\n")
    ends_nl = olf.endswith("\n")
    body = olf[:-1] if ends_nl else olf
    olines = body.split("\n")
    if all(l.strip() == "" for l in olines):
        return []
    sp = spans(t)
    k = len(olines)
    eol = dominant(t)
    out, i = [], 0
    while i + k <= len(sp):
        s = shift_of(t, sp[i:i + k], olines)
        if s is not None:
            last = sp[i + k - 1]
            end = last[2] if ends_nl else last[1]
            r = reindent(new, s, eol)
            if ends_nl and last[2] == last[1] and r.endswith(eol):
                r = r[:-len(eol)]
            out.append((sp[i][0], end, r))
            i += k
        else:
            i += 1
    return out


def find_all(t, s):
    out, i = [], 0
    while True:
        j = t.find(s, i)
        if j < 0:
            return out
        out.append(j)
        i = j + len(s)


def reference(t, old, new, replace_all):
    if old == "":
        return {"ok": False, "error": "empty_old"}
    if old == new:
        return {"ok": False, "error": "no_change"}
    tiers = []
    sites = find_all(t, old)
    if sites:
        tiers.append(("exact", [(s, s + len(old), follow(t, new)) for s in sites]))
    elif "\n" in old:
        first = dominant(t)
        for eol in [first, "\r\n" if first == "\n" else "\n"]:
            o = to_eol(old, eol)
            if o == old:
                continue
            ss = find_all(t, o)
            if ss:
                tiers.append(("eol", [(s, s + len(o), to_eol(new, eol)) for s in ss]))
                break
    if not tiers:
        w = ws_sites(t, old, new)
        if w:
            tiers.append(("whitespace", w))
    if not tiers:
        return {"ok": False, "error": "not_found"}
    tier, sp = tiers[0]
    if len(sp) > 1 and not replace_all:
        return {"ok": False, "error": "ambiguous", "count": len(sp)}
    out, at = "", 0
    for a, b, r in sp:
        out += t[at:a] + r
        at = b
    out += t[at:]
    if out == t:
        return {"ok": False, "error": "no_change"}
    return {"ok": True, "tier": tier, "count": len(sp), "result": out}


def blocks(text, n):
    lines = text.split("\n")[:-1]
    i = random.randrange(0, len(lines) - n)
    return lines[i:i + n]


def mutate(s):
    choices = [lambda x: x.replace("self", "this", 1) if "self" in x else x + " // changed",
               lambda x: x.upper() if len(x) < 30 else x[:-1] + "!",
               lambda x: "# replaced\n" + x,
               lambda x: x + "\n" + x.strip() + "_extra",
               lambda x: ""]
    return random.choice(choices)(s)


def unique_block(text, nmin=1, nmax=4, tries=200):
    for _ in range(tries):
        n = random.randint(nmin, nmax)
        b = blocks(text, n)
        if all(l.strip() == "" for l in b):
            continue
        old = "\n".join(b) + random.choice(["", "\n"])
        if len(find_all(text, old)) == 1:
            return old
    raise RuntimeError("no unique block")


def check(cat, file, old, new, ra, want_tier=None, want_error=None):
    exp = reference(file, old, new, ra)
    if want_tier and exp.get("tier") != want_tier:
        return False
    if want_error and exp.get("error") != want_error:
        return False
    add(cat, file, old, new, ra, exp)
    return True


def fill(cat, n, make):
    got, tries = 0, 0
    while got < n:
        tries += 1
        if tries > 5000:
            raise RuntimeError(cat)
        if make():
            got += 1


# 1. exact, unique (30)
def exact():
    name = random.choice(list(CORPUS))
    t = CORPUS[name]
    old = unique_block(t)
    new = mutate(old)
    return new != old and check("exact", t, old, new, False, want_tier="exact")
fill("exact", 30, exact)

# 2. replace_all (10)
TOKENS = ["self.", "config", "path", "res.", "hits", "name", "version", "cargo", "step", "Manifest"]
def rall():
    t = CORPUS[random.choice(list(CORPUS))]
    tok = random.choice(TOKENS)
    if t.count(tok) < 2:
        return False
    return check("replace_all", t, tok, tok.upper() + "_X", True, want_tier="exact")
fill("replace_all", 10, rall)

# 3. ambiguous (10)
def amb():
    t = CORPUS[random.choice(list(CORPUS))]
    tok = random.choice(TOKENS + ["return", "if ", "{", "}"])
    if len(find_all(t, tok)) < 2:
        return False
    return check("ambiguous", t, tok, "zz", False, want_error="ambiguous")
fill("ambiguous", 10, amb)

# 4. LF snippet against a CRLF file (15)
def eol1():
    t = CORPUS[random.choice(list(CORPUS))]
    old = unique_block(t, 2, 5)
    if "\n" not in old:
        return False
    new = mutate(old)
    return new != old and check("eol_crlf_file", to_eol(t, "\r\n"), old, new, False, want_tier="eol")
fill("eol_crlf_file", 15, eol1)

# 5. CRLF snippet against an LF file (5)
def eol2():
    t = CORPUS[random.choice(list(CORPUS))]
    old = unique_block(t, 2, 4)
    if "\n" not in old:
        return False
    new = mutate(old)
    return new != old and check("eol_crlf_snippet", t, to_eol(old, "\r\n"), to_eol(new, "\r\n"), False, want_tier="eol")
fill("eol_crlf_snippet", 5, eol2)

# 6. snippet indented less than the file (10)
def dedent_block(b):
    nb = [l for l in b if l.strip()]
    p = min(lead(l) for l in nb)
    return [l[p:] if l.strip() else l for l in b], p
def ws_less():
    name = random.choice(["rust", "python", "js", "yaml"])
    t = CORPUS[name]
    for _ in range(50):
        n = random.randint(1, 4)
        b = blocks(t, n)
        if not any(l.strip() for l in b):
            continue
        d, p = dedent_block(b)
        if p == 0:
            continue
        old = "\n".join(d) + random.choice(["", "\n"])
        new = "\n".join(l.replace("(", "( ", 1) if l.strip() else l for l in d) + ("\n" if old.endswith("\n") else "")
        if new == old:
            new = old.rstrip("\n") + "\n" + d[0].strip() + "_more" + ("\n" if old.endswith("\n") else "")
        return check("whitespace_less", t, old, new, False, want_tier="whitespace")
    return False
fill("whitespace_less", 10, ws_less)

# 7. snippet indented more than the file (5)
def ws_more():
    t = CORPUS[random.choice(list(CORPUS))]
    old0 = unique_block(t, 1, 3)
    pad = random.choice(["    ", "\t", "  "])
    old = "\n".join(pad + l if l.strip() else l for l in old0.split("\n"))
    new = old.replace("e", "E", 1)
    return new != old and check("whitespace_more", t, old, new, False, want_tier="whitespace")
fill("whitespace_more", 5, ws_more)

# 8. trailing whitespace in the snippet (5)
def ws_trail():
    t = CORPUS[random.choice(list(CORPUS))]
    old0 = unique_block(t, 1, 3)
    old = "\n".join(l + "  " if l.strip() else l for l in old0.split("\n"))
    new = old0.replace("a", "A", 1) + "\n// tail"
    return check("whitespace_trailing", t, old, new, False, want_tier="whitespace")
fill("whitespace_trailing", 5, ws_trail)

# 9. not found (15): a typo in a long line (some), or absent text
def nf():
    t = CORPUS[random.choice(list(CORPUS))]
    old = unique_block(t, 1, 3)
    i = random.randrange(len(old))
    if old[i] in " \n\t":
        return False
    old2 = old[:i] + ("Q" if old[i] != "Q" else "Z") + old[i + 1:]
    return check("not_found", t, old2, "x", False, want_error="not_found")
fill("not_found", 10, nf)
for absent in ["fn does_not_exist() {}", "print('nowhere')", "zzz", "completely different text\nacross lines", "<html>"]:
    check("not_found", RUST, absent, "y", False, want_error="not_found")

# 10. indentation that is not one uniform shift (5)
def nonuni():
    t = random.choice([RUST, PY, JS])
    for _ in range(100):
        b = blocks(t, random.randint(2, 4))
        nb = [i for i, l in enumerate(b) if l.strip()]
        if len(nb) < 2 or all(lead(b[i]) == 0 for i in nb):
            continue
        j = nb[-1]
        b2 = list(b)
        b2[j] = "  " + b2[j] + ""
        b2[nb[0]] = b2[nb[0]].lstrip()
        old = "\n".join(b2)
        if check("nonuniform_indent", t, old, "x", False, want_error="not_found"):
            return True
    return False
fill("nonuniform_indent", 5, nonuni)

# 11. unicode (10)
def uni():
    old = unique_block(UNI, 1, 2)
    new = old.replace("—", "-").replace("é", "e") + " ✔"
    return new != old and check("unicode", UNI, old, new, False, want_tier="exact")
fill("unicode", 10, uni)

# 12. start and end of file, no trailing newline (10)
for k in range(10):
    t = CORPUS[["rust", "python", "js", "yaml"][k % 4]]
    if k % 3 == 0:
        t = t.rstrip("\n")
    lines = t.split("\n")
    if k % 2 == 0:
        old = lines[0] if k < 5 else "\n".join(lines[:2])
        if len(find_all(t, old)) != 1:
            old = "\n".join(lines[:3])
        new = "// header\n" + old
    else:
        tail = [l for l in lines if l.strip()][-2:]
        old = "\n".join(tail)
        if len(find_all(t, old)) != 1:
            old = t[-60:]
        new = old + "\n// footer"
    add("start_end", t, old, new, False, reference(t, old, new, False))

# 13. empty old (3), no change (3)
for t in [RUST, PY, ""]:
    add("empty_old", t, "", "new text\n", False, reference(t, "", "new text\n", False))
for t, s in [(RUST, "hits: u64,"), (PY, "self.step = 0"), (JS, "return config;")]:
    add("no_change", t, s, s, False, reference(t, s, s, False))

# 14. read's line-number prefix pasted in (4)
def numbered(lines, first):
    return "".join(f"{first + i:>6}\t{l}\n" for i, l in enumerate(lines))
for t, first, n in [(RUST, 3, 2), (PY, 12, 3), (JS, 4, 2), (YAML, 9, 2)]:
    lines = t.split("\n")[first - 1:first - 1 + n]
    old = numbered(lines, first)
    exp = reference(t, old, "x", False)
    exp["contains"] = ["line-number prefix", f"starting at line {first}"]
    add("line_prefix", t, old, "x", False, exp)

# 15. regex metacharacters (5)
for l in REGEX.split("\n")[:5]:
    add("regex_chars", REGEX, l, l.replace("let", "const"), False, reference(REGEX, l, l.replace("let", "const"), False))

# 16. CRLF file, exact CRLF snippet, LF replacement takes CRLF (5)
def crlf_exact():
    t = to_eol(CORPUS[random.choice(list(CORPUS))], "\r\n")
    old = to_eol(unique_block(CORPUS["rust"], 2, 3), "\r\n")
    if old not in t:
        t = to_eol(RUST, "\r\n")
    if len(find_all(t, old)) != 1:
        return False
    new = to_eol(old, "\n") + "\n// added line\n"
    return check("crlf_exact", t, old, new, False, want_tier="exact")
fill("crlf_exact", 5, crlf_exact)

assert len(cases) == 150, len(cases)
ids = [c["id"] for c in cases]
assert len(set(ids)) == len(ids)
out = sys.argv[1]
files = {}
for c in cases:
    key = next((k for k, v in files.items() if v == c["file"]), None)
    if key is None:
        key = f"f{len(files):02}"
        files[key] = c["file"]
    c["file"] = key
with open(out, "w", encoding="utf-8", newline="\n") as f:
    json.dump({"about": "150 edit-engine cases (M4.6 track A, design 2.1 T6). Generated once by an "
                        "independent Python model of the tier rules; never regenerated to make a test pass.",
               "files": files, "cases": cases}, f, ensure_ascii=False, indent=1)
    f.write("\n")
from collections import Counter
print(Counter(c["category"] for c in cases))
print(Counter((c["expect"].get("tier") or c["expect"].get("error")) for c in cases))
