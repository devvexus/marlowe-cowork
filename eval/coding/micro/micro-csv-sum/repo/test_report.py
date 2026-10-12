from report import column_sum


def test_missing_file_is_an_error():
    try:
        column_sum("nope.csv", "a")
    except OSError:
        return
    raise AssertionError("expected an error")
