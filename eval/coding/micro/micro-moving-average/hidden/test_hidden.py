from stats import moving_average


def test_full():
    assert moving_average([1, 2, 3, 4], 2) == [1.5, 2.5, 3.5]


def test_window_is_length():
    assert moving_average([2, 4, 6], 3) == [4.0]


def test_window_one():
    assert moving_average([5, 7], 1) == [5.0, 7.0]


def test_too_short():
    assert moving_average([1], 2) == []
