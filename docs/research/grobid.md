# GROBID (optional, off by default)

The `paper` tool reads papers without GROBID: arXiv's HTML first, then the e-print LaTeX source
(when robots.txt allows it; arxiv.org disallows `/e-print`), then the PDF, whose sections and
reference list are recovered heuristically (`marlowe_extract::paper::from_text`). That last step
is the weakest: PDF text loses columns and reference boundaries.

GROBID (Apache-2.0) parses a PDF into TEI XML with sections, figures and a structured reference
list. It is a Java service, so MarloweCowork never requires it and does not ship it.

## Running it locally

- Docker: run the official CRF-only image on loopback, pinned to a released tag, for example
  `docker run --rm -p 127.0.0.1:8070:8070 lfoppiano/grobid:<tag>` (see grobid.readthedocs.io for
  current tags). The deep-learning image needs a GPU and is not needed here.
- Without Docker: a JDK 17+ and the release archive, `./gradlew run` in the unpacked directory.
- Check: `curl http://127.0.0.1:8070/api/isalive` answers `true`.

## How it would be used

Not wired yet. When a `research.grobid_url` setting is added, the PDF step of the `paper` tool
would POST the PDF bytes to `/api/processFulltextDocument` (multipart field `input`,
`consolidateHeader=0`, `consolidateCitations=0` so GROBID makes no outbound requests), parse the
TEI `<div>`/`<head>` sections and `<listBibl>/<biblStruct>` references into the same `Paper` shape,
and fall back to the heuristic reader when the sidecar is absent or slow. Loopback only: the PDF
never leaves the machine.
