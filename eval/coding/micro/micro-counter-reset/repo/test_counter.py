from counter import Counter


def test_increment():
    c = Counter()
    assert c.increment() == 1
