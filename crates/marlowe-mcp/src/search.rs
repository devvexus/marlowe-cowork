//! `tool_search`: finding deferred MCP tools by keyword, or selecting them by name.
//!
//! Ranking is a small BM25-flavoured score over the tool's name words and description words with
//! an inverse-document-frequency weight, so a rare word ("invoice") outweighs a common one
//! ("get"). No model call: this runs on the tool thread.

use serde_json::Value;

/// One searchable tool.
#[derive(Debug, Clone)]
pub struct Entry {
    pub exposed: String,
    pub server: String,
    pub tool: String,
    pub description: String,
    pub schema: Value,
}

fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            // camelCase boundary
            if c.is_uppercase() && prev_lower && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = c.is_lowercase();
            cur.extend(c.to_lowercase());
        } else {
            prev_lower = false;
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Indices into `entries`, best first. `select:a,b` picks by exact tool name; anything else is
/// ranked. Tools of a server named in the query rank higher.
pub fn search(entries: &[Entry], query: &str, limit: usize) -> Vec<usize> {
    let q = query.trim();
    if let Some(list) = q.strip_prefix("select:") {
        let wanted: Vec<&str> = list.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        return wanted
            .iter()
            .filter_map(|w| entries.iter().position(|e| e.exposed == *w || e.tool == *w || format!("{}__{}", e.server, e.tool) == *w))
            .collect();
    }
    let qwords = words(q);
    if qwords.is_empty() {
        return Vec::new();
    }
    let docs: Vec<(Vec<String>, Vec<String>, Vec<String>)> =
        entries.iter().map(|e| (words(&e.tool), words(&e.description), words(&e.server))).collect();
    let n = entries.len() as f64;
    let mut scored: Vec<(f64, usize)> = Vec::new();
    for (i, (name, desc, server)) in docs.iter().enumerate() {
        let mut score = 0.0;
        for w in &qwords {
            let df = docs.iter().filter(|(n, d, s)| n.contains(w) || d.contains(w) || s.contains(w)).count() as f64;
            let idf = (1.0 + n / (1.0 + df)).ln();
            if name.contains(w) {
                score += 3.0 * idf;
            }
            if desc.contains(w) {
                score += idf;
            }
            if server.contains(w) {
                score += 0.5 * idf;
            }
        }
        if score > 0.0 {
            scored.push((score, i));
        }
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(entries[a.1].exposed.cmp(&entries[b.1].exposed)));
    scored.into_iter().take(limit).map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn e(server: &str, tool: &str, desc: &str) -> Entry {
        Entry { exposed: crate::names::exposed(server, tool), server: server.into(), tool: tool.into(), description: desc.into(), schema: json!({}) }
    }

    fn corpus() -> Vec<Entry> {
        vec![
            e("github", "create_issue", "Open a new issue in a repository."),
            e("github", "list_pull_requests", "List pull requests of a repository."),
            e("stripe", "create_invoice", "Create an invoice for a customer."),
            e("stripe", "list_customers", "List customers."),
            e("fs", "read_file", "Read a file from disk."),
        ]
    }

    #[test]
    fn keywords_rank_by_name_then_description_with_rare_words_heavier() {
        let c = corpus();
        assert_eq!(c[search(&c, "invoice", 3)[0]].tool, "create_invoice");
        assert_eq!(c[search(&c, "pull requests", 3)[0]].tool, "list_pull_requests");
        let create = search(&c, "create", 5);
        assert_eq!(create.len(), 2, "only the two `create_*` tools match");
        assert_eq!(c[search(&c, "customers list", 3)[0]].tool, "list_customers");
        assert!(search(&c, "kubernetes", 3).is_empty());
    }

    #[test]
    fn camel_case_and_server_names_are_searchable() {
        let c = vec![e("db", "runQuery", "Execute SQL."), e("other", "x", "y")];
        assert_eq!(search(&c, "query", 3), vec![0]);
        assert_eq!(search(&c, "db", 3), vec![0]);
    }

    #[test]
    fn select_picks_exact_names_in_the_order_asked() {
        let c = corpus();
        let got = search(&c, "select:read_file, mcp__github__create_issue, nope", 10);
        assert_eq!(got.iter().map(|&i| c[i].tool.as_str()).collect::<Vec<_>>(), ["read_file", "create_issue"]);
        assert_eq!(search(&c, "select:stripe__list_customers", 10), vec![3]);
    }

    #[test]
    fn the_limit_applies() {
        let c = corpus();
        assert_eq!(search(&c, "list", 1).len(), 1);
    }
}
