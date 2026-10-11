//! Tool calls a model leaked as markup into its prose, and the markup readers for them.
//!
//! qwen in think mode sometimes writes `<function=read><parameter=path>a.md</parameter></function>`
//! instead of a structured call. Left in history, that markup makes Ollama's template fail on the
//! next request (`HTTP 500: XML syntax error`). This file reads the two XML dialects
//! ([`xml_calls`] for Qwen's `<function=...>`, [`glm_call`] for GLM's `<arg_key>/<arg_value>`),
//! keeps the narrow legacy [`recover_leaked_call`], and [`defang_tool_markup`] makes whatever
//! stays text unparseable. Deciding *which* text is a call (known tool names, fences, JSON forms)
//! is `scan.rs`.

use std::ops::Range;

use serde_json::{Map, Value};

const FUNCTION_OPEN: &str = "<function=";
const FUNCTION_CLOSE: &str = "</function>";
const PARAM_OPEN: &str = "<parameter=";
const PARAM_CLOSE: &str = "</parameter>";

/// One `<function=NAME>` block. Values are the raw strings; the schema types them later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlCall {
    pub name: String,
    pub params: Vec<(String, String)>,
    /// Byte range in the scanned text, from `<function=` through `</function>` when present.
    pub span: Range<usize>,
    /// Every `<parameter>` and the `<function>` itself were closed. A block cut off mid-value is
    /// not complete and must not run: `write` with half a file is worse than no write.
    pub complete: bool,
}

/// Every `<function=NAME>...</function>` block in `text`, in order.
pub fn xml_calls(text: &str) -> Vec<XmlCall> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find(FUNCTION_OPEN) {
        let start = from + rel;
        let after = start + FUNCTION_OPEN.len();
        let Some(name_len) = text[after..].find('>') else { break };
        let name = text[after..after + name_len].trim();
        let body_start = after + name_len + 1;
        if name.is_empty() || name.contains(char::is_whitespace) || name.contains('<') {
            from = after;
            continue;
        }
        // The block ends at its close, else where the next block or a wrapper close begins, else
        // at the end of the text.
        let hard_end = [FUNCTION_CLOSE, FUNCTION_OPEN, "</tool_call>"]
            .iter()
            .filter_map(|m| text[body_start..].find(m))
            .min();
        let closed = hard_end.is_some_and(|e| text[body_start + e..].starts_with(FUNCTION_CLOSE));
        let body_end = body_start + hard_end.unwrap_or(text.len() - body_start);
        let (params, params_complete) = parameters(&text[body_start..body_end]);
        let end = if closed { body_end + FUNCTION_CLOSE.len() } else { body_end };
        out.push(XmlCall {
            name: name.to_string(),
            params,
            span: start..end,
            complete: closed && params_complete,
        });
        from = end.max(after);
    }
    out
}

fn parameters(body: &str) -> (Vec<(String, String)>, bool) {
    let mut params = Vec::new();
    let mut complete = true;
    let mut cursor = body;
    while let Some(p) = cursor.find(PARAM_OPEN) {
        let after = &cursor[p + PARAM_OPEN.len()..];
        let Some(key_end) = after.find('>') else {
            complete = false;
            break;
        };
        let key = after[..key_end].trim().to_string();
        let region = &after[key_end + 1..];
        let (value, consumed) = match region.find(PARAM_CLOSE) {
            Some(e) => (&region[..e], e + PARAM_CLOSE.len()),
            None => {
                complete = false;
                (region, region.len())
            }
        };
        if !key.is_empty() {
            params.push((key, param_value(value)));
        }
        cursor = &region[consumed..];
    }
    (params, complete)
}

/// A parameter's text as the model meant it. A multi-line value (file content) loses exactly the
/// one newline the template puts after the open tag and before the close tag, nothing more:
/// indentation and a trailing newline are content. A one-line value is trimmed.
fn param_value(raw: &str) -> String {
    if !raw.contains('\n') {
        return raw.trim().to_string();
    }
    let s = raw.strip_prefix("\r\n").or_else(|| raw.strip_prefix('\n')).unwrap_or(raw);
    s.strip_suffix("\r\n").or_else(|| s.strip_suffix('\n')).unwrap_or(s).to_string()
}

/// GLM writes `NAME` then `<arg_key>K</arg_key><arg_value>V</arg_value>` pairs inside
/// `<tool_call>`. `body` is the text between the tags.
pub fn glm_call(body: &str) -> Option<XmlCall> {
    let first = body.find("<arg_key>");
    let name = body[..first.unwrap_or(body.len())].trim();
    if name.is_empty() || name.contains(char::is_whitespace) || name.contains('<') || first.is_none() {
        return None;
    }
    let mut params = Vec::new();
    let mut complete = true;
    let mut cursor = &body[first?..];
    while let Some(k) = cursor.find("<arg_key>") {
        let after = &cursor[k + "<arg_key>".len()..];
        let Some(ke) = after.find("</arg_key>") else {
            complete = false;
            break;
        };
        let key = after[..ke].trim().to_string();
        let rest = &after[ke + "</arg_key>".len()..];
        let Some(vs) = rest.find("<arg_value>") else {
            complete = false;
            break;
        };
        let vrest = &rest[vs + "<arg_value>".len()..];
        let Some(ve) = vrest.find("</arg_value>") else {
            complete = false;
            break;
        };
        params.push((key, param_value(&vrest[..ve])));
        cursor = &vrest[ve + "</arg_value>".len()..];
    }
    Some(XmlCall { name: name.to_string(), params, span: 0..body.len(), complete })
}

/// The legacy narrow reader: the first `<function=NAME>` block, its arguments (whole numbers
/// become numbers, since nothing here knows the schema), and the prose before it. Blocks cut off
/// mid-parameter still return, as before; `scan` is stricter.
pub fn recover_leaked_call(content: &str) -> Option<(String, Value, String)> {
    let call = xml_calls(content).into_iter().next()?;
    let mut args = Map::new();
    for (k, v) in call.params {
        let v = match v.parse::<i64>() {
            Ok(n) => Value::from(n),
            Err(_) => Value::String(v),
        };
        args.insert(k, v);
    }
    Some((call.name, Value::Object(args), content[..call.span.start].trim_end().to_string()))
}

/// Rewrites tool-call markup so it cannot parse as XML, leaving it readable.
pub fn defang_tool_markup(content: &str) -> String {
    const TAGS: [&str; 5] = ["function", "parameter", "tool_call", "arg_key", "arg_value"];
    if !TAGS.iter().any(|t| content.contains(&format!("<{t}")) || content.contains(&format!("</{t}"))) {
        return content.to_string();
    }
    let mut out = content.to_string();
    for t in TAGS {
        out = out.replace(&format!("</{t}>"), &format!("[/{t}]")).replace(&format!("<{t}"), &format!("[{t}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_tool_call_written_as_markup_is_recovered() {
        let (name, args, prose) =
            recover_leaked_call("Let me look.\n<function=read><parameter=path>notes.md</parameter><parameter=limit>40</parameter></function>")
                .unwrap();
        assert_eq!(name, "read");
        assert_eq!(args, json!({"path": "notes.md", "limit": 40}));
        assert_eq!(prose, "Let me look.");
    }

    #[test]
    fn prose_about_tools_is_not_a_tool_call() {
        for text in [
            "I could use the read function on notes.md.",
            "The <function> element is XML.",
            "read(path=notes.md)",
            "<function=>",
        ] {
            assert!(recover_leaked_call(text).is_none(), "{text:?} was read as a call");
        }
    }

    #[test]
    fn markup_recovery_declines_is_defanged_but_readable() {
        let leaked = "I'll read it.\n<function>read</function>\n<parameter=path>x.rs</parameter>";
        assert!(recover_leaked_call(leaked).is_none());
        let out = defang_tool_markup(leaked);
        for tag in ["<function", "</function>", "<parameter", "</parameter>"] {
            assert!(!out.contains(tag), "`{tag}` survived: {out}");
        }
        assert!(out.contains("read") && out.contains("x.rs") && out.contains("I'll read it."));
    }

    #[test]
    fn ordinary_prose_passes_through_unchanged() {
        for s in ["The function reads a file.", "Use a < b and c > d.", "See `parameter`.", ""] {
            assert_eq!(defang_tool_markup(s), s);
        }
    }

    #[test]
    fn wrapper_and_glm_tags_are_defanged_too() {
        let out = defang_tool_markup("<tool_call>read<arg_key>path</arg_key><arg_value>a</arg_value></tool_call>");
        assert!(!out.contains('<'), "{out}");
        assert!(out.contains("[tool_call") && out.contains("[/tool_call]") && out.contains("[/arg_value]"));
    }

    #[test]
    fn several_blocks_are_all_read_in_order() {
        let text = "<function=read><parameter=path>a</parameter></function> and <function=grep><parameter=pattern>x</parameter></function>";
        let calls = xml_calls(text);
        assert_eq!(calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["read", "grep"]);
        assert!(calls.iter().all(|c| c.complete));
        assert_eq!(&text[calls[1].span.clone()], "<function=grep><parameter=pattern>x</parameter></function>");
    }

    #[test]
    fn multi_line_values_keep_their_indentation_and_inner_newlines() {
        let text = "<function=write>\n<parameter=path>\nsrc/a.py\n</parameter>\n<parameter=content>\ndef f():\n    return 1\n\n</parameter>\n</function>";
        let c = &xml_calls(text)[0];
        assert_eq!(c.params[0], ("path".to_string(), "src/a.py".to_string()));
        assert_eq!(c.params[1].1, "def f():\n    return 1\n", "one newline each side comes off, no more");
        assert!(c.complete);
        let inline = xml_calls("<function=read><parameter=path> a.md </parameter></function>");
        assert_eq!(inline[0].params[0].1, "a.md");
    }

    #[test]
    fn a_block_cut_off_mid_parameter_is_not_complete() {
        let cut = &xml_calls("<function=write><parameter=path>a.py</parameter><parameter=content>def f(")[0];
        assert!(!cut.complete);
        let unclosed = &xml_calls("<function=read><parameter=path>a.md</parameter>")[0];
        assert!(!unclosed.complete, "no </function>: the model may have been cut off");
        assert_eq!(unclosed.params.len(), 1);
    }

    #[test]
    fn a_block_ends_where_the_next_begins_if_the_close_was_forgotten() {
        let calls = xml_calls("<function=read><parameter=path>a</parameter><function=read><parameter=path>b</parameter></function>");
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].complete && calls[1].complete);
    }

    #[test]
    fn glm_blocks_read_name_and_pairs() {
        let c = glm_call("read\n<arg_key>path</arg_key>\n<arg_value>a.md</arg_value>\n<arg_key>limit</arg_key><arg_value>5</arg_value>\n").unwrap();
        assert_eq!(c.name, "read");
        assert_eq!(c.params, vec![("path".into(), "a.md".into()), ("limit".into(), "5".into())]);
        assert!(c.complete);
        assert!(glm_call("just words").is_none());
        assert!(!glm_call("read<arg_key>path</arg_key><arg_value>a").unwrap().complete);
    }
}
