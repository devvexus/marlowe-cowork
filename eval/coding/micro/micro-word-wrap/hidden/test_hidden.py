from wrapper import wrap


def test_wraps():
    assert wrap("the quick brown fox jumps", 10) == "the quick\nbrown fox\njumps"


def test_exact_fit():
    assert wrap("abc def", 7) == "abc def"


def test_long_word():
    assert wrap("a extraordinarily b", 5) == "a\nextraordinarily\nb"


def test_collapses_spaces():
    assert wrap("a    b", 10) == "a b"


def test_empty():
    assert wrap("", 5) == ""
