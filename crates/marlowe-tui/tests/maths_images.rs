//! **Typeset images in the pane**, with a stand-in typesetter so this runs in every build.
//!
//! One test function on purpose: the image service and the frame plan are process-wide, and
//! parallel tests in this binary would share them.

mod common;

use std::time::{Duration, Instant};

use common::*;
use crossterm::event::KeyCode;
use marlowe_proto::Event;
use marlowe_tui::images::{planned, Emitter};
use marlowe_typeset::sixel::Coverage;
use marlowe_typeset::{Caps, Service};

const REPLY: &str = "Before the block.\n\n$$\n\\mathcal{L} = -\\frac{1}{N}\\sum_{i=1}^{N} \\log p_\\theta(y_i \\mid x_i)\n$$\n\nAfter the block.";

fn rows(buf: &ratatui::buffer::Buffer) -> Vec<String> {
    dump(buf).lines().map(str::to_string).collect()
}

#[test]
fn a_display_formula_is_held_then_drawn_as_an_image_in_its_reserved_rows() {
    // A typesetter that draws a solid block the size of a 3-row, 20-column formula.
    let (wake_tx, wake) = crossbeam_channel::unbounded();
    let typeset: marlowe_typeset::service::Typesetter = Box::new(|_, em, _| {
        let (w, h) = ((em * 10.0) as usize, (em * 3.0) as usize);
        Ok(Coverage { w, h, alpha: vec![255; w * h] })
    });
    let caps = Caps { cell_w: 10, cell_h: 20, fg: [0xcd, 0xc9, 0xd6], bg: [0x0f, 0x0e, 0x14] };
    marlowe_typeset::install(Service::start(caps, typeset, wake_tx));

    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    type_str(&mut s, "hi");
    s.on_key(key(KeyCode::Enter));
    feed(&mut s, 2, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
    feed(&mut s, 3, Event::TurnStarted { session: sid(), turn: 1, model: sonnet_ref(), user: "hi".into() });
    feed(&mut s, 4, Event::TextDelta { session: sid(), turn: 1, text: REPLY.into() });

    // Frame 1: the image is being typeset. The block's rows are held behind the placeholder.
    let grid_buf = {
        // What the grid alone would draw, for the row positions.
        let b = render(&s, W, 60);
        rows(&b)
    };
    let before = grid_buf.iter().position(|l| l.contains("Before the block.")).unwrap();
    let after = grid_buf.iter().position(|l| l.contains("After the block.")).unwrap();
    let held = rows(&render(&s, W, 60));
    let held_screen = held.join("\n");
    assert!(held_screen.contains('…') || held_screen.contains('⎯'), "{held_screen}");
    assert!(!held_screen.contains('\\') && !held_screen.contains('$'), "{held_screen}");

    // Wait for the worker.
    let deadline = Instant::now() + Duration::from_secs(10);
    while wake.try_recv().is_err() {
        assert!(Instant::now() < deadline, "the worker never finished");
        std::thread::sleep(Duration::from_millis(5));
    }

    // Frame 2: the image is ready. The grid rows are blank, the text around them has not moved,
    // and one strip per row of the block is planned at the block's position.
    let buf = render(&s, W, 60);
    let ready = rows(&buf);
    assert_eq!(ready.iter().position(|l| l.contains("Before the block.")), Some(before));
    assert_eq!(ready.iter().position(|l| l.contains("After the block.")), Some(after), "a row moved");
    for y in before + 2..after - 1 {
        let inner: String = ready[y].chars().skip(3).take(60).collect();
        assert!(inner.trim().is_empty(), "row {y} still shows grid maths: {:?}", ready[y]);
    }
    let plan = planned();
    assert_eq!(plan.len(), after - 1 - (before + 2), "one strip per reserved row: {plan:?}");
    assert!(plan.iter().all(|p| p.strip.starts_with(b"\x1bP0;1;0q")), "transparent-background Sixel");
    assert_eq!(plan[0].y as usize, before + 2);

    // Sent once; a still frame sends nothing; a frame drawn on top is not painted over.
    let mut e = Emitter::default();
    let first = e.frame(&buf, plan.clone());
    assert_eq!(first.windows(4).filter(|w| *w == b"\x1bP0;").count(), plan.len());
    assert!(e.frame(&buf, plan).is_empty(), "an unchanged frame re-sent the image");
    assert_palette_only(&buf);
    println!("tui maths images: held {} rows, then {} strips, sent once", after - before - 3, planned().len());
}
