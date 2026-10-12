from text import slugify


def test_punctuation():
    assert slugify("Hello, World!") == "hello-world"


def test_runs():
    assert slugify("a   b---c") == "a-b-c"


def test_ends():
    assert slugify("  --Edge Case--  ") == "edge-case"


def test_digits():
    assert slugify("Python 3.11 rocks") == "python-3-11-rocks"


def test_empty():
    assert slugify("!!!") == ""
