//! Tool calls a model leaked as markup into its prose.
//!
//! qwen in think mode sometimes writes `<function=read><parameter=path>a.md</parameter></function>`
//! instead of a structured call. Left in history, that markup makes Ollama's template fail on the
//! next request (`HTTP 500: XML syntax error`). Recovery is deliberately narrow; anything it
//! declines is defanged before it is sent back.

use serde_json::{Map, Value};

/// Parses one `<function=NAME>` call with `<parameter=KEY>VALUE</parameter>` children.
/// Returns the name, the arguments, and the prose before the markup.
pub fn recover_leaked_call(content: &str) -> Option<(String, Value, String)> {
    let start = content.find("<function=")?;
    let rest = &content[start + "<function=".len()..];
    let name_end = rest.find('>')?;
    let name = rest[..name_end].trim();
    if name.is_empty() || name.contains(char::is_whitespace) || name.contains('<') {
        return None;
    }
    let body_end = rest.find("</function>").unwrap_or(rest.len());
    let body = &rest[name_end + 1..body_end.max(name_end + 1)];

    let mut args = Map::new();
    let mut cursor = body;
    while let Some(p) = cursor.find("<parameter=") {
        let after = &cursor[p + "<parameter=".len()..];
        let Some(key_end) = after.find('>') else { break };
        let key = after[..key_end].trim().to_string();
        let value_region = &after[key_end + 1..];
        let value_end = value_region.find("</parameter>").unwrap_or(value_region.len());
        let value = value_region[..value_end].trim().to_string();
        if !key.is_empty() {
            let v = match value.parse::<i64>() {
                Ok(n) => Value::from(n),
                Err(_) => Value::String(value),
            };
            args.insert(key, v);
        }
        let consumed = (value_end + "</parameter>".len()).min(value_region.len());
        cursor = &value_region[consumed..];
    }
    Some((name.to_string(), Value::Object(args), content[..start].trim_end().to_string()))
}

/// Rewrites tool-call markup so it cannot parse as XML, leaving it readable.
pub fn defang_tool_markup(content: &str) -> String {
    if !content.contains("<function") && !content.contains("<parameter") && !content.contains("</function")
        && !content.contains("</parameter")
    {
        return content.to_string();
    }
    content
        .replace("</function>", "[/function]")
        .replace("</parameter>", "[/parameter]")
        .replace("<function", "[function")
        .replace("<parameter", "[parameter")
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
}
