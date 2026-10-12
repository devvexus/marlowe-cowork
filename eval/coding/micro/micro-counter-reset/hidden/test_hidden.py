from counter import Counter


def test_reset_returns_old():
    c = Counter()
    c.increment(5)
    assert c.reset() == 5


def test_reset_zeroes():
    c = Counter()
    c.increment(3)
    c.reset()
    assert c.count == 0
    assert c.increment() == 1


def test_reset_fresh():
    assert Counter().reset() == 0
