from report import column_sum


def test_sum(tmp_path):
    p = tmp_path / "d.csv"
    p.write_text("name,qty\nbolt,3\nnut,4.5\n")
    assert column_sum(str(p), "qty") == 7.5


def test_blank_is_zero(tmp_path):
    p = tmp_path / "d.csv"
    p.write_text("qty,name\n2,a\n,b\n5,c\n")
    assert column_sum(str(p), "qty") == 7.0


def test_other_column(tmp_path):
    p = tmp_path / "d.csv"
    p.write_text("a,b\n1,10\n2,20\n")
    assert column_sum(str(p), "b") == 30.0
