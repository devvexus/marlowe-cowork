use super::*;

const DEMO: &str = include_str!("fixtures/demo.ipynb");

/// The fixture renders with every cell, its outputs, the traceback without ANSI codes, and the
/// image output decoded (design 2.1 T4).
#[test]
fn parity_t4_fixture_notebook_rendered() {
    let nb = parse(DEMO.as_bytes()).unwrap();
    let r = render(&nb);
    assert_eq!(r.cells, 4);
    let t = &r.text;
    assert!(t.starts_with("<cell index=\"0\" id=\"intro\" type=\"markdown\">\n# Loss curves\nPlots for run *baseline* — café.\n</cell>\n"), "{t}");
    assert!(t.contains("<cell index=\"1\" id=\"load\" type=\"code\" language=\"python\" execution_count=\"1\">\nlosses = [0.9, 0.7, 0.5]\nprint('loaded', len(losses), 'rows')\nlosses\n</cell>"), "{t}");
    assert!(t.contains("<outputs cell=\"1\">\nloaded 3 rows\n[0.9, 0.7, 0.5]\n</outputs>"), "{t}");
    assert!(t.contains("<outputs cell=\"2\">\n[image output: image/png, 1x1, attached]\n</outputs>"), "{t}");
    assert!(t.contains("ZeroDivisionError: division by zero"), "{t}");
    assert!(!t.contains('\u{1b}'), "ANSI stripped");
    assert_eq!(r.images.len(), 1);
    let img = &r.images[0];
    assert_eq!((img.mime, img.width, img.height, img.cell), ("image/png", 1, 1, 2));
    assert!(img.bytes.starts_with(b"\x89PNG"));
}

#[test]
fn an_unchanged_notebook_serialises_byte_identical() {
    let nb = parse(DEMO.as_bytes()).unwrap();
    assert_eq!(serialize(&nb, indent_of(DEMO), false), DEMO.as_bytes());
    assert_eq!(indent_of(DEMO), 1);
}

fn edit_of(cell: Option<&str>, source: &str, kind: Option<&str>, mode: EditMode) -> CellEdit {
    CellEdit { cell: cell.map(str::to_string), source: source.into(), cell_type: kind.map(str::to_string), mode }
}

/// Insert, replace and delete, by id and by index (design 2.1 T19).
#[test]
fn parity_t19_insert_replace_delete() {
    let mut nb = parse(DEMO.as_bytes()).unwrap();
    // Replace by id: source set as lines, outputs and count cleared.
    let r = edit(&mut nb, &edit_of(Some("load"), "losses = [1.0, 0.5]\nlosses", None, EditMode::Replace)).unwrap();
    assert_eq!((r.index, r.id.as_deref()), (1, Some("load")));
    assert_eq!(nb["cells"][1]["source"], json!(["losses = [1.0, 0.5]\n", "losses"]));
    assert_eq!(nb["cells"][1]["outputs"], json!([]));
    assert_eq!(nb["cells"][1]["execution_count"], Value::Null);
    // Insert after index 0, as markdown, with a fresh id (nbformat 4.5).
    let r = edit(&mut nb, &edit_of(Some("0"), "## Setup", Some("markdown"), EditMode::Insert)).unwrap();
    assert_eq!((r.index, r.cells), (1, 5));
    let id = r.id.unwrap();
    assert_eq!(id.len(), 8);
    assert_eq!(nb["cells"][1]["cell_type"], "markdown");
    assert!(nb["cells"][1].get("outputs").is_none());
    // Insert at the top with no cell given.
    let r = edit(&mut nb, &edit_of(None, "import os", None, EditMode::Insert)).unwrap();
    assert_eq!(r.index, 0);
    assert_eq!(nb["cells"][0]["cell_type"], "code");
    // Delete by id.
    let r = edit(&mut nb, &edit_of(Some("boom"), "", None, EditMode::Delete)).unwrap();
    assert_eq!(r.cells, 5);
    assert!(nb["cells"].as_array().unwrap().iter().all(|c| c["id"] != "boom"));
    // Replace changing type: code -> markdown drops outputs.
    edit(&mut nb, &edit_of(Some("plot"), "a note", Some("markdown"), EditMode::Replace)).unwrap();
    let plot = nb["cells"].as_array().unwrap().iter().find(|c| c["id"] == "plot").unwrap().clone();
    assert_eq!(plot["cell_type"], "markdown");
    assert!(plot.get("outputs").is_none() && plot.get("execution_count").is_none());
    // Unknown cell, bad type.
    assert!(matches!(edit(&mut nb, &edit_of(Some("nope"), "", None, EditMode::Delete)), Err(NotebookError::NoCell(..))));
    assert!(edit(&mut nb, &edit_of(Some("0"), "x", Some("python"), EditMode::Replace)).is_err());
    // Still a notebook after the round trip.
    let bytes = serialize(&nb, 1, false);
    let again = parse(&bytes).unwrap();
    assert_eq!(again, nb);
}

#[test]
fn non_notebooks_are_refused() {
    assert!(parse(b"{\"x\": 1}").is_err());
    assert!(parse(b"not json").is_err());
}
