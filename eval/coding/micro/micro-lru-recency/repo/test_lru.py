from lru import LRU


def test_put_get():
    c = LRU(2)
    c.put("a", 1)
    assert c.get("a") == 1
