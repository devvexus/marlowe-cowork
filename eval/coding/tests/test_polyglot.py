"""The polyglot tier on a fixture tree in Aider's layout (no download, no network)."""

from __future__ import annotations

import json
from pathlib import Path

from mcoding import polyglot, toolchains
from mcoding.adapters import CallableAdapter
from mcoding.runner import RunOptions, run_matrix
from mcoding.workspace import materialize


def exercise(root: Path, lang: str, name: str, files: dict, solution: list, tests: list, instructions="Do the thing.") -> Path:
    d = root / lang / "exercises" / "practice" / name
    (d / ".meta").mkdir(parents=True)
    (d / ".docs").mkdir()
    (d / ".meta" / "config.json").write_text(json.dumps({"files": {"solution": solution, "test": tests, "example": [".meta/example.py"]}}), encoding="utf-8")
    (d / ".meta" / "example.py").write_text("# the reference solution\n", encoding="utf-8")
    (d / ".docs" / "instructions.md").write_text(instructions, encoding="utf-8")
    for rel, text in files.items():
        (d / rel).parent.mkdir(parents=True, exist_ok=True)
        (d / rel).write_text(text, encoding="utf-8", newline="")
    return d


def tree(tmp: Path) -> Path:
    root = tmp / "polyglot-benchmark"
    exercise(
        root,
        "python",
        "two-fer",
        {"two_fer.py": "def two_fer(name=None):\n    pass\n", "two_fer_test.py": "from two_fer import two_fer\n\n\ndef test_none():\n    assert two_fer() == 'One for you, one for me.'\n\n\ndef test_name():\n    assert two_fer('Bob') == 'One for Bob, one for me.'\n"},
        ["two_fer.py"],
        ["two_fer_test.py"],
        "Say 'One for X, one for me.'; X defaults to 'you'.",
    )
    exercise(root, "rust", "leap", {"Cargo.toml": "[package]\nname='leap'\n", "src/lib.rs": "", "tests/leap.rs": ""}, ["src/lib.rs"], ["tests/leap.rs"])
    exercise(root, "go", "hello", {"hello.go": "package hello\n", "hello_test.go": "package hello\n"}, ["hello.go"], ["hello_test.go"])
    return root


def test_exercises_are_found_in_every_language_folder(tmp_path):
    tasks = polyglot.discover(tree(tmp_path))
    assert [t.id for t in tasks] == ["polyglot-go-hello", "polyglot-python-two-fer", "polyglot-rust-leap"]
    t = next(t for t in tasks if t.language == "python")
    assert t.attempts == 2 and t.tier == "polyglot"
    assert "Say 'One for X" in t.prompt and "two_fer.py" in t.prompt and "two_fer_test.py" in t.prompt
    assert t.hidden == {"two_fer_test.py": "two_fer_test.py"}


def test_languages_without_a_toolchain_are_left_out_of_the_score_not_failed(tmp_path):
    tasks = {t.language: t for t in polyglot.discover(tree(tmp_path))}
    have = {lang: tc.available for lang, tc in toolchains.detect_all().items()}
    for lang, t in tasks.items():
        assert bool(t.meta.get("skip")) == (not have[lang]), lang
    assert have["python"] and tasks["python"].meta.get("skip") is None
    if not have["go"]:
        assert "go toolchain unavailable" in tasks["go"].meta["skip"]


def test_the_agent_sees_stubs_and_instructions_never_the_reference_solution(tmp_path):
    t = next(t for t in polyglot.discover(tree(tmp_path)) if t.language == "python")
    ws = materialize(t, tmp_path / "ws")
    assert (ws / "two_fer.py").is_file() and (ws / "two_fer_test.py").is_file()
    assert not (ws / ".meta").exists() and not (ws / ".docs").exists()


def test_a_second_attempt_sees_the_failure_and_editing_a_test_cannot_pass(tmp_path):
    t = next(t for t in polyglot.discover(tree(tmp_path)) if t.language == "python")
    calls = []

    def agent(ws: Path, prompt: str):
        calls.append(prompt)
        if len(calls) == 1:
            # First try: sabotage the test instead of solving.
            (ws / "two_fer_test.py").write_text("def test_nothing():\n    assert True\n", encoding="utf-8")
        else:
            (ws / "two_fer.py").write_text("def two_fer(name=None):\n    return f'One for {name or \"you\"}, one for me.'\n", encoding="utf-8")

    a = CallableAdapter(agent, name="a")
    rec = run_matrix([t], {"a": a}, RunOptions(out_dir=tmp_path / "o", progress=lambda m: None))[0]
    assert rec.passed and rec.attempts_used == 2
    assert "See the testing errors above" in calls[1] and "two_fer.py" in calls[1] and "assert" in calls[1]


def test_a_sample_spreads_over_the_languages_that_can_run_and_repeats_for_a_seed(tmp_path):
    root = tmp_path / "pb"
    for lang in ("python", "javascript", "rust"):
        for i in range(6):
            exercise(root, lang, f"ex{i}", {"a.txt": ""}, ["a.txt"], ["a.txt"])
    tasks = polyglot.discover(root)
    s1 = polyglot.sample(tasks, 6, seed=1)
    assert [t.id for t in s1] == [t.id for t in polyglot.sample(tasks, 6, seed=1)]
    assert len(s1) == 6 and all(not t.meta.get("skip") for t in s1)
    runnable = sorted({t.language for t in tasks if not t.meta.get("skip")})
    assert sorted({t.language for t in s1}) == runnable, "every runnable language is represented"
