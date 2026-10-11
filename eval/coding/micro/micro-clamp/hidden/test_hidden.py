from util import clamp


def test_below():
    assert clamp(-3, 0, 10) == 0


def test_above():
    assert clamp(42, 0, 10) == 10


def test_edges():
    assert clamp(0, 0, 10) == 0
    assert clamp(10, 0, 10) == 10


def test_floats():
    assert clamp(0.5, 1.5, 2.5) == 1.5
