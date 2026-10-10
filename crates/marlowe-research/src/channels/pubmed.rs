//! PubMed through NCBI E-utilities, keyless (3 requests/s): `esearch` for ranked PMIDs, then one
//! `efetch` for full records (title, authors, journal, date, abstract, DOI, PMCID and the
//! "Retracted Publication" type). `tool=` names the client; no email is sent.

use marlowe_proto::ChannelId;
use serde_json::Value;

use super::{check_status, clip, enc, get_json, get_raw, limit, spec_of, SNIPPET_CHARS};
use crate::channel::{Channel, ChannelCtx, ChannelError, ChannelQuery, ChannelSpec, HttpRequest};
use crate::source::{Identity, Namespace, SourceRef};
use crate::xml::{self, Node};

const TOOL: &str = "marlowecowork";

pub struct PubmedChannel {
    spec: ChannelSpec,
}

impl Default for PubmedChannel {
    fn default() -> Self {
        Self::new()
    }
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// `PubDate` / `ArticleDate` as `YYYY[-MM[-DD]]`; `MedlineDate` ("2019 Jan-Feb") gives its year.
fn pub_date(d: &Node) -> Option<String> {
    let year = d
        .child_text("Year")
        .or_else(|| d.child_text("MedlineDate").and_then(|m| m.split_whitespace().next().map(str::to_string)))?;
    let month = d.child_text("Month").and_then(|m| {
        m.parse::<u32>()
            .ok()
            .or_else(|| MONTHS.iter().position(|x| m.to_ascii_lowercase().starts_with(x)).map(|i| i as u32 + 1))
    });
    let day = d.child_text("Day").and_then(|x| x.parse::<u32>().ok());
    Some(match (month, day) {
        (Some(m), Some(dd)) => format!("{year}-{m:02}-{dd:02}"),
        (Some(m), None) => format!("{year}-{m:02}"),
        _ => year,
    })
}

/// One `PubmedArticle` as a route.
pub fn pubmed_article(a: &Node) -> Option<SourceRef> {
    let citation = a.child("MedlineCitation")?;
    let pmid = citation.child_text("PMID")?;
    let article = citation.child("Article")?;
    let title = article.child_text("ArticleTitle").filter(|t| !t.is_empty())?;
    let abstract_text: Vec<String> = article
        .child("Abstract")
        .map(|ab| {
            ab.children_named("AbstractText")
                .map(|t| match t.attr("Label") {
                    Some(l) if !l.is_empty() => format!("{l}: {}", t.clean_text()),
                    _ => t.clean_text(),
                })
                .collect()
        })
        .unwrap_or_default();
    let authors: Vec<String> = article
        .child("AuthorList")
        .map(|l| {
            l.children_named("Author")
                .filter_map(|au| {
                    match (au.child_text("LastName"), au.child_text("ForeName"), au.child_text("CollectiveName")) {
                        (Some(l), Some(f), _) => Some(format!("{l}, {f}")),
                        (Some(l), None, _) => Some(l),
                        (_, _, Some(c)) => Some(c),
                        _ => None,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let journal = article.child("Journal");
    let date = journal
        .and_then(|j| j.child("JournalIssue"))
        .and_then(|i| i.child("PubDate"))
        .and_then(pub_date)
        .or_else(|| article.child("ArticleDate").and_then(pub_date));
    let mut identity = Identity::default().with_pmid(&pmid);
    if let Some(ids) = a.child("PubmedData").and_then(|d| d.child("ArticleIdList")) {
        for id in ids.children_named("ArticleId") {
            match id.attr("IdType") {
                Some("doi") => identity = identity.with_doi(&id.clean_text()),
                Some("pmc") => identity = identity.with_pmcid(&id.clean_text()),
                _ => {}
            }
        }
    }
    let types: Vec<String> = article
        .child("PublicationTypeList")
        .map(|l| l.children_named("PublicationType").map(Node::clean_text).collect())
        .unwrap_or_default();
    let retracted = types.iter().any(|t| t == "Retracted Publication");
    Some(
        SourceRef {
            identity: Some(identity),
            authors,
            date,
            snippet: clip(&abstract_text.join(" "), SNIPPET_CHARS),
            retracted,
            ..SourceRef::new(ChannelId::Pubmed, &format!("https://pubmed.ncbi.nlm.nih.gov/{pmid}/"), &title)
        }
        .with_meta("venue", journal.and_then(|j| j.child_text("Title")).unwrap_or_default())
        .with_meta("types", types.join("; ")),
    )
}

/// Every `PubmedArticle` in an efetch answer, ordered as `pmids`.
pub fn parse_efetch(body: &str, pmids: &[String]) -> Result<Vec<SourceRef>, String> {
    let root = xml::parse(body)?;
    let mut refs: Vec<SourceRef> = root.find_all("PubmedArticle").into_iter().filter_map(pubmed_article).collect();
    let rank = |r: &SourceRef| {
        let p = r.identity.as_ref().and_then(|i| i.pmid.clone()).unwrap_or_default();
        pmids.iter().position(|x| *x == p).unwrap_or(usize::MAX)
    };
    refs.sort_by_key(rank);
    Ok(refs)
}

impl PubmedChannel {
    pub fn new() -> Self {
        Self { spec: spec_of(ChannelId::Pubmed) }
    }

    pub fn esearch_url(&self, q: &ChannelQuery) -> String {
        let mut u = format!(
            "{}/esearch.fcgi?db=pubmed&retmode=json&sort=relevance&retmax={}&term={}&tool={TOOL}",
            self.spec.endpoint,
            limit(q).min(50),
            enc(q.text.trim())
        );
        if let Some(y) = q.since_year {
            u.push_str(&format!("&datetype=pdat&mindate={y}&maxdate=3000"));
        }
        u
    }

    fn efetch(&self, pmids: &[String], ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        if pmids.is_empty() {
            return Ok(Vec::new());
        }
        let id = ChannelId::Pubmed;
        let url =
            format!("{}/efetch.fcgi?db=pubmed&retmode=xml&id={}&tool={TOOL}", self.spec.endpoint, pmids.join(","));
        let r = check_status(id, get_raw(ctx, id, HttpRequest::get(url).accept("application/xml"))?)?;
        parse_efetch(&String::from_utf8_lossy(&r.bytes), pmids)
            .map_err(|detail| ChannelError::Parse { channel: id, detail })
    }
}

impl Channel for PubmedChannel {
    fn spec(&self) -> &ChannelSpec {
        &self.spec
    }

    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        let v = get_json(ctx, ChannelId::Pubmed, self.esearch_url(query))?;
        let list = v.get("esearchresult").and_then(|r| r.get("idlist")).and_then(Value::as_array).ok_or_else(|| {
            ChannelError::Parse { channel: ChannelId::Pubmed, detail: "no `esearchresult.idlist`".into() }
        })?;
        let pmids: Vec<String> = list
            .iter()
            .filter_map(Value::as_str)
            .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .map(String::from)
            .collect();
        self.efetch(&pmids, ctx)
    }

    fn lookup(&self, ns: Namespace, id: &str, ctx: &ChannelCtx<'_>) -> Result<Option<SourceRef>, ChannelError> {
        let pmid = match ns {
            Namespace::Pmid => Identity::default().with_pmid(id).pmid,
            _ => None,
        };
        let Some(pmid) = pmid else {
            return Err(ChannelError::NoLookup { channel: ChannelId::Pubmed, namespace: ns });
        };
        Ok(self.efetch(&[pmid], ctx)?.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn efetch_records_keep_labels_ids_and_retractions() {
        let x = r#"<?xml version="1.0"?><!DOCTYPE PubmedArticleSet><PubmedArticleSet>
<PubmedArticle><MedlineCitation><PMID Version="1">111</PMID><Article><Journal><Title>J Test</Title>
<JournalIssue><PubDate><Year>2019</Year><Month>Feb</Month></PubDate></JournalIssue></Journal>
<ArticleTitle>A <i>trial</i> of things.</ArticleTitle><Abstract><AbstractText Label="BACKGROUND">Why.</AbstractText>
<AbstractText Label="RESULTS">It worked.</AbstractText></Abstract><AuthorList><Author><LastName>Lovelace</LastName>
<ForeName>Ada</ForeName></Author><Author><CollectiveName>The Group</CollectiveName></Author></AuthorList>
<PublicationTypeList><PublicationType>Journal Article</PublicationType><PublicationType>Retracted Publication</PublicationType></PublicationTypeList>
</Article></MedlineCitation><PubmedData><ArticleIdList><ArticleId IdType="pubmed">111</ArticleId>
<ArticleId IdType="doi">10.1/ABC</ArticleId><ArticleId IdType="pmc">PMC9</ArticleId></ArticleIdList></PubmedData></PubmedArticle>
</PubmedArticleSet>"#;
        let r = parse_efetch(x, &["111".into()]).unwrap();
        assert_eq!(r.len(), 1);
        let a = &r[0];
        assert_eq!(a.title, "A trial of things.");
        assert_eq!(a.snippet, "BACKGROUND: Why. RESULTS: It worked.");
        assert_eq!(a.authors, ["Lovelace, Ada", "The Group"]);
        assert_eq!(a.date.as_deref(), Some("2019-02"));
        let id = a.identity.as_ref().unwrap();
        assert_eq!(
            (id.doi.as_deref(), id.pmcid.as_deref(), id.pmid.as_deref()),
            (Some("10.1/abc"), Some("PMC9"), Some("111"))
        );
        assert!(a.retracted);
        assert_eq!(a.url, "https://pubmed.ncbi.nlm.nih.gov/111/");
    }
}
