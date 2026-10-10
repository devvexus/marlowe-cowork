use super::*;

const ARXIV: &str = include_str!("../../tests/fixtures/arxiv_paper.html");

fn arxiv_paper() -> Paper {
    let (doc, blocks, _) = crate::html::extract_structured(
        ARXIV.as_bytes(),
        Some("text/html; charset=utf-8"),
        Some("https://arxiv.org/html/2301.01234v2"),
    );
    from_html(&doc, &blocks, true)
}

#[test]
fn arxiv_html_yields_title_abstract_sections_and_references() {
    let p = arxiv_paper();
    assert_eq!(p.source, PaperSource::ArxivHtml);
    assert_eq!(p.title.as_deref(), Some("Attention and Memory in Small Models"));
    let a = p.abstract_text.as_deref().unwrap_or("");
    assert!(a.contains("key-value memory of size $m$ recovers"), "{a}");
    let s: Vec<(Option<&str>, &str)> = p.sections.iter().map(|s| (s.number.as_deref(), s.title.as_str())).collect();
    assert!(s.contains(&(Some("1"), "Introduction")) && s.contains(&(Some("2.1"), "Training")), "{s:?}");
    assert!(!s.iter().any(|(_, t)| *t == "References"), "the bibliography is not a body section");
    assert!(p.text.contains("$O(\\log m)$ per token"), "math alttext kept: {}", p.text);
    assert!(p.text.contains("| Heads kept | No memory | With memory |"), "{}", p.text);
    assert!(p.text.contains("Table 1: Accuracy with pruned heads."), "captions are content in a paper: {}", p.text);
    assert!(!p.text.contains("Report issue") && !p.text.contains("experimental HTML"), "{}", p.text);
    // Section offsets point at their headings.
    let intro = p.sections.iter().find(|s| s.title == "Introduction").unwrap();
    assert!(p.text[intro.offset..].starts_with("## 1 Introduction"), "{}", &p.text[intro.offset..]);

    assert_eq!(p.references.len(), 4);
    let r = &p.references;
    assert_eq!(r[0].label.as_deref(), Some("1"));
    assert_eq!(r[0].arxiv_id.as_deref(), Some("1706.03762"));
    assert_eq!(r[0].year, Some(2017));
    assert_eq!(r[0].title.as_deref(), Some("Attention is all you need"), "{:?}", r[0]);
    assert_eq!(
        r[1].title.as_deref(),
        Some("FlashAttention: fast and memory-efficient exact attention with IO-awareness")
    );
    assert_eq!(r[2].arxiv_id.as_deref(), Some("2112.04426"), "an arXiv DOI yields the arXiv id");
    assert_eq!(r[2].doi.as_deref(), Some("10.48550/arxiv.2112.04426"));
    assert_eq!(r[3].doi.as_deref(), Some("10.1038/s41586-021-03819-2"), "a DOI printed in text, trailing dot trimmed");
    assert_eq!(r[3].year, Some(2021));
}

#[test]
fn pdf_text_yields_sections_and_a_split_reference_list() {
    let text = "A Study of Things\nAbstract\nWe study things at length.\n1 Introduction\nThings matter.\n\
                2.1 Model Design\nThe model is small.\nReferences\n[1] A. Author and B. Author. A first paper \
                title. In ICML, 2020.\n[2] C. Writer. Second paper\ncontinues here. Nature, 2019. \
                doi:10.1000/xyz123.\n[3] D. Person. Third. arXiv:2101.00001, 2021.\n";
    let p = from_text(text, Some("A Study of Things".into()));
    let s: Vec<&str> = p.sections.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(s, ["Abstract", "Introduction", "Model Design", "References"]);
    assert_eq!(p.abstract_text.as_deref(), Some("We study things at length."));
    assert!(!p.text.contains("[2] C. Writer"), "references are split off the body");
    assert_eq!(p.references.len(), 3);
    assert_eq!(p.references[1].text, "C. Writer. Second paper continues here. Nature, 2019. doi:10.1000/xyz123.");
    assert_eq!(p.references[1].doi.as_deref(), Some("10.1000/xyz123"));
    assert_eq!(p.references[2].arxiv_id.as_deref(), Some("2101.00001"));
    assert_eq!(p.references[0].title.as_deref(), Some("A first paper title"));
}

const TEX: &str = r#"\documentclass{article}
\usepackage{amsmath} % packages
\title{Sparse Memory for \emph{Tiny} Models}
\begin{document}
\maketitle
\begin{abstract}
We add a sparse memory. It costs 5\% more.
\end{abstract}
\section{Introduction}\label{sec:intro}
Prior work~\cite{vaswani2017,dao2022} studies attention; see \url{https://example.org/x}.
\input{method}
\begin{figure}[t]\centering\includegraphics[width=\linewidth]{fig1.pdf}
\caption{Memory hit rate by layer.}\end{figure}
\begin{equation}
y = \mathrm{softmax}(QK^\top) V
\end{equation}
\begin{itemize}\item First point. \item Second point.\end{itemize}
\bibliography{refs}
\end{document}
"#;

const METHOD: &str = r"\section{Method}
\subsection{Training}
We train for $20$k steps.\footnote{On one GPU.}
\begin{verbatim}
python train.py --steps 20000
\end{verbatim}
";

const BIB: &str = r#"@inproceedings{vaswani2017,
  title = {Attention is All You Need},
  author = {Vaswani, Ashish and Shazeer, Noam},
  booktitle = {NeurIPS},
  year = {2017},
  eprint = {1706.03762}
}
@article{dao2022,
  title = "FlashAttention",
  author = "Dao, Tri",
  journal = {arXiv},
  year = 2022,
  doi = {10.48550/arXiv.2205.14135}
}
"#;

#[test]
fn latex_source_from_a_gzipped_tar_with_inputs_and_bibtex() {
    let tar =
        tar::build(&[("main.tex", TEX.as_bytes()), ("method.tex", METHOD.as_bytes()), ("refs.bib", BIB.as_bytes())]);
    let mut gz = Vec::new();
    {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::fast());
        e.write_all(&tar).unwrap();
        e.finish().unwrap();
    }
    let p = latex::from_source(&gz).unwrap();
    assert_eq!(p.source, PaperSource::Latex);
    assert_eq!(p.title.as_deref(), Some("Sparse Memory for Tiny Models"));
    assert_eq!(p.abstract_text.as_deref(), Some("We add a sparse memory. It costs 5% more."));
    let s: Vec<&str> = p.sections.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(s, ["Introduction", "Method", "Training"]);
    let t = &p.text;
    assert!(t.contains("Prior work [vaswani2017, dao2022] studies attention; see https://example.org/x."), "{t}");
    assert!(t.contains("We train for $20$k steps. (On one GPU.)"), "{t}");
    assert!(t.contains("```\npython train.py --steps 20000\n```"), "{t}");
    assert!(t.contains("Figure: Memory hit rate by layer."), "{t}");
    assert!(t.contains("$$\ny = \\mathrm{softmax}(QK^\\top) V\n$$"), "{t}");
    assert!(t.contains("- First point.") && t.contains("- Second point."), "{t}");
    assert!(!t.contains("includegraphics") && !t.contains("fig1.pdf") && !t.contains("amsmath"), "{t}");
    assert_eq!(p.references.len(), 2);
    assert_eq!(p.references[0].label.as_deref(), Some("vaswani2017"));
    assert_eq!(p.references[0].arxiv_id.as_deref(), Some("1706.03762"));
    assert_eq!(p.references[0].title.as_deref(), Some("Attention is All You Need"));
    assert_eq!(p.references[1].doi.as_deref(), Some("10.48550/arxiv.2205.14135"));
    assert_eq!(p.references[1].year, Some(2022));
}

#[test]
fn latex_thebibliography_and_a_bare_tex_file() {
    let src = "\\documentclass{article}\\begin{document}\\section{Intro}Text.\n\\begin{thebibliography}{9}\n\
               \\bibitem{a1} A. Author. \\newblock A title here. \\newblock J. Things, 2001.\n\
               \\bibitem[B02]{b2} B. Author. Other title. arXiv:0901.0001, 2009.\n\
               \\end{thebibliography}\\end{document}";
    let p = latex::from_source(src.as_bytes()).unwrap();
    assert_eq!(p.references.len(), 2);
    assert_eq!(p.references[0].label.as_deref(), Some("a1"));
    assert_eq!(p.references[0].year, Some(2001));
    assert_eq!(p.references[1].arxiv_id.as_deref(), Some("0901.0001"));
    assert!(!p.text.contains("bibitem") && p.text.contains("## Intro"), "{}", p.text);
    assert!(latex::from_source(b"\x1f\x8bnot gzip").is_err());
}

#[test]
fn hostile_tar_headers_do_not_panic_or_overrun() {
    let mut t = tar::build(&[("a.tex", b"\\begin{document}x\\end{document}")]);
    // Claim a body far larger than the archive.
    t[124..135].copy_from_slice(b"77777777777");
    assert!(tar::entries(&t).is_empty());
    assert!(tar::entries(&[0u8; 100]).is_empty());
}
