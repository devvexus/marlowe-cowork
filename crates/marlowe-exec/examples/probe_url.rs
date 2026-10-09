//! Print what one URL actually extracts to. Diagnostic, not a test.
//!
//! Run: `cargo run --release -p marlowe-exec --example probe_url -- <url>`

fn main() {
    let url = std::env::args().nth(1).expect("usage: probe_url <url>");
    let target = marlowe_net::Target::parse(&url).expect("parses");
    let res = marlowe_net::fetch(&target).expect("fetches");
    println!(
        "status {}  content-type {:?}  wire {} B  decoded {} B",
        res.status,
        res.content_type,
        res.wire_bytes,
        res.bytes.len()
    );
    let input = marlowe_extract::Input::new(&res.bytes)
        .content_type(res.content_type.as_deref())
        .url(Some(&url));
    let d = marlowe_extract::extract(&input).expect("extracts");
    println!(
        "format {}  encoding {}  title {:?}  links {}  headings {}",
        d.format.as_str(),
        d.encoding,
        d.title,
        d.links.len(),
        d.headings.len()
    );
    println!("warnings: {:?}", d.warnings);
    println!("text {} chars, reduction {:.1}%", d.text.len(), d.reduction() * 100.0);
    println!("---8<--- first 1200 chars ---8<---");
    let n = d.text.char_indices().nth(1200).map(|(i, _)| i).unwrap_or(d.text.len());
    println!("{}", &d.text[..n]);
}
