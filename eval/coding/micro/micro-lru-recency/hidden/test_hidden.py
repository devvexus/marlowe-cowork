from lru import LRU


def test_get_protects_from_eviction():
    c = LRU(2)
    c.put("a", 1)
    c.put("b", 2)
    assert c.get("a") == 1
    c.put("c", 3)
    assert c.get("b") is None
    assert c.get("a") == 1
    assert c.get("c") == 3


def test_default():
    assert LRU(1).get("x", 7) == 7


def test_put_refreshes():
    c = LRU(2)
    c.put("a", 1)
    c.put("b", 2)
    c.put("a", 10)
    c.put("c", 3)
    assert c.get("b") is None
    assert c.get("a") == 10
