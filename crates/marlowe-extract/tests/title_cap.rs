//! **The `<title>` cap, asserted on the paths G11's site-fix did not cover.**
//!
//! Audit finding G11 was closed in `html.rs` with `MAX_TITLE_BYTES`, and
//! `multibyte::an_unclosed_title_does_not_become_the_whole_document` pins it. That test is correct
//! and it is not enough: `title` is written by **four** parsers, and the fix was in one of them.
//! An RSS `<title>`, an epub OPF `<dc:title>` and a Markdown `# heading` all reached the same
//! permanent storage by a different route with no cap at all.
//!
//! So these tests deliberately do **not** go through HTML. Every one would have passed before the
//! change if it had used an HTML document, which is precisely what makes a site-fix look like a
//! type-fix.
//!
//! They also go through the public [`marlowe_extract::extract`] rather than calling a parser
//! directly, because that is where the cap is enforced. `office::extract_xml` still returns an
//! uncapped title by itself — asserting on it would be asserting where the value is produced
//! rather than where it is bounded, which is the failure this project files as instance #16.

use marlowe_extract::{extract, Format, Input, MAX_TITLE_CHARS};

/// Long enough that no plausible cap lets it through, and made of a repeated marker so a partial
/// survival is obvious in a failure message.
fn oversized() -> String {
    "TITLE-PAYLOAD-".repeat(20_000)
}

#[test]
fn an_rss_title_is_capped_and_rss_is_not_html() {
    let feed = format!(
        "<rss><channel><title>{}</title>\
         <item><description>a feed item</description></item></channel></rss>",
        oversized()
    );
    let d = extract(
        &Input::new(feed.as_bytes()).content_type(Some("application/rss+xml")),
    )
    .expect("extracts");

    assert_eq!(d.format, Format::Xml, "this test is worthless if it routed to the HTML parser");
    let title = d.title.unwrap_or_default();
    assert!(
        title.chars().count() <= MAX_TITLE_CHARS,
        "an RSS title survived at {} chars against a cap of {MAX_TITLE_CHARS}",
        title.chars().count()
    );
    // The vacuity control: a cap that produced an empty title would pass the assertion above and
    // silently destroy every feed title in the corpus.
    assert!(title.starts_with("TITLE-PAYLOAD-"), "the title was emptied, not capped: {title:?}");
}

#[test]
fn a_markdown_heading_title_is_capped_too() {
    // `plain.rs` lifts the first `# heading` into the title with no cap of its own — the third of
    // the four writers, and a path no HTML test can reach.
    let md = format!("# {}\n\nBody text.\n", oversized());
    let d = extract(&Input::new(md.as_bytes()).content_type(Some("text/markdown")))
        .expect("extracts");

    let title = d.title.unwrap_or_default();
    assert!(
        title.chars().count() <= MAX_TITLE_CHARS,
        "a Markdown title survived at {} chars",
        title.chars().count()
    );
    assert!(!title.is_empty(), "the title was emptied, not capped");
}

/// A multi-byte title must be cut on a character boundary.
///
/// This is audit finding G1's shape: `String::truncate` on a byte index panics mid-codepoint, and
/// Rust's panic message quotes ~256 characters of the string being sliced — which is how document
/// bytes reached an error the model could read. A cap that panics is worse than no cap.
#[test]
fn a_multibyte_title_is_cut_on_a_character_boundary() {
    let feed = format!(
        "<rss><channel><title>{}</title></channel></rss>",
        "日本語テキスト".repeat(5_000)
    );
    let d = extract(&Input::new(feed.as_bytes()).content_type(Some("application/rss+xml")))
        .expect("must not panic on a multi-byte title");
    let title = d.title.unwrap_or_default();
    assert!(title.chars().count() <= MAX_TITLE_CHARS);
    assert!(title.starts_with('日'), "cut mid-character: {title:?}");
}

/// The HTML path is still capped — the site-fix and the type-fix are complementary, and this
/// asserts the type-fix did not somehow relax the tighter bound below it.
#[test]
fn the_html_path_is_still_capped_after_the_cap_moved() {
    let body = format!("<html><head><title>{}", oversized());
    let d = extract(&Input::new(body.as_bytes()).content_type(Some("text/html")))
        .expect("extracts");
    let title = d.title.unwrap_or_default();
    assert!(title.chars().count() <= MAX_TITLE_CHARS, "{} chars", title.chars().count());
}
