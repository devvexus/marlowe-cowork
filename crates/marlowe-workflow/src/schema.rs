//! The in-house JSON Schema subset structured output is checked against (WF3): `type` (one or a
//! list), `properties`, `required`, `items`, `enum`, `const`, numeric bounds, string and array
//! lengths, `additionalProperties` (bool or schema), `anyOf`, `oneOf`. Other keywords
//! (`description`, `title`, `$schema`, ...) are ignored. Errors name the JSON pointer.

use serde_json::{Map, Value};

/// Checks that `schema` can be used for an agent's answer before any model call: an object at the
/// root whose `required` names only declared properties, and nothing it says contradicts itself.
pub fn check_shape(schema: &Value) -> Result<(), String> {
    let obj = schema.as_object().ok_or("a schema is a dict, like {\"type\": \"object\", \"properties\": {...}}")?;
    if obj.get("type").and_then(Value::as_str) != Some("object") {
        return Err("the schema's root must be {\"type\": \"object\", ...}: an agent answers with one object".into());
    }
    check_node(schema, "")
}

fn check_node(schema: &Value, path: &str) -> Result<(), String> {
    let Some(obj) = schema.as_object() else {
        return if schema.is_boolean() { Ok(()) } else { Err(format!("{}: a schema must be a dict", at(path))) };
    };
    if let Some(t) = obj.get("type") {
        let names: Vec<&str> = match t {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => return Err(format!("{}: `type` must be a string or a list of strings", at(path))),
        };
        if names.is_empty() {
            return Err(format!("{}: `type` names no type", at(path)));
        }
        for n in names {
            if !matches!(n, "object" | "array" | "string" | "number" | "integer" | "boolean" | "null") {
                return Err(format!("{}: unknown type `{n}`", at(path)));
            }
        }
    }
    let props = obj.get("properties").map(|p| p.as_object().ok_or_else(|| format!("{}: `properties` must be a dict", at(path))));
    let props = props.transpose()?;
    if let Some(req) = obj.get("required") {
        let req = req.as_array().ok_or_else(|| format!("{}: `required` must be a list", at(path)))?;
        for r in req {
            let name = r.as_str().ok_or_else(|| format!("{}: `required` holds names (strings)", at(path)))?;
            if !props.is_some_and(|p| p.contains_key(name)) {
                return Err(format!(
                    "{}: `{name}` is required but not in `properties`; declare it there or drop it from `required`",
                    at(path)
                ));
            }
        }
    }
    if let Some(e) = obj.get("enum") {
        match e.as_array() {
            Some(a) if !a.is_empty() => {}
            _ => return Err(format!("{}: `enum` must be a non-empty list", at(path))),
        }
    }
    for (lo, hi) in [("minimum", "maximum"), ("minLength", "maxLength"), ("minItems", "maxItems")] {
        if let (Some(a), Some(b)) = (obj.get(lo).and_then(Value::as_f64), obj.get(hi).and_then(Value::as_f64)) {
            if a > b {
                return Err(format!("{}: `{lo}` {a} is above `{hi}` {b}; nothing can satisfy it", at(path)));
            }
        }
    }
    if let Some(p) = props {
        for (k, v) in p {
            check_node(v, &format!("{path}/{k}"))?;
        }
    }
    if let Some(items) = obj.get("items") {
        check_node(items, &format!("{path}/items"))?;
    }
    if let Some(ap) = obj.get("additionalProperties") {
        check_node(ap, &format!("{path}/additionalProperties"))?;
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(alts) = obj.get(key) {
            let alts = alts.as_array().filter(|a| !a.is_empty()).ok_or_else(|| format!("{}: `{key}` must be a non-empty list", at(path)))?;
            for (i, a) in alts.iter().enumerate() {
                check_node(a, &format!("{path}/{key}/{i}"))?;
            }
        }
    }
    Ok(())
}

fn at(path: &str) -> String {
    if path.is_empty() {
        "schema".into()
    } else {
        format!("schema {path}")
    }
}

/// Every way `value` breaks `schema`, as `"<pointer>: <what>"` lines; empty when it conforms.
pub fn validate(value: &Value, schema: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    walk(value, schema, "", &mut errors);
    errors
}

fn type_ok(value: &Value, t: &str) -> bool {
    match t {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some() || value.as_f64().is_some_and(|f| f.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn ptr(path: &str) -> &str {
    if path.is_empty() {
        "/"
    } else {
        path
    }
}

fn walk(value: &Value, schema: &Value, path: &str, errors: &mut Vec<String>) {
    let obj: &Map<String, Value> = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            errors.push(format!("{}: no value is allowed here", ptr(path)));
            return;
        }
        Value::Object(o) => o,
        _ => return,
    };
    if let Some(t) = obj.get("type") {
        let names: Vec<&str> = match t {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        if !names.is_empty() && !names.iter().any(|n| type_ok(value, n)) {
            errors.push(format!("{}: expected {}, got {}", ptr(path), names.join(" or "), kind(value)));
            return;
        }
    }
    if let Some(c) = obj.get("const") {
        if c != value {
            errors.push(format!("{}: must equal {c}", ptr(path)));
        }
    }
    if let Some(Value::Array(options)) = obj.get("enum") {
        if !options.contains(value) {
            let shown: Vec<String> = options.iter().map(Value::to_string).collect();
            errors.push(format!("{}: must be one of {}", ptr(path), shown.join(", ")));
        }
    }
    if let Some(n) = value.as_f64() {
        let bound = |k: &str| obj.get(k).and_then(Value::as_f64);
        if let Some(m) = bound("minimum").filter(|m| n < *m) {
            errors.push(format!("{}: {n} is below the minimum {m}", ptr(path)));
        }
        if let Some(m) = bound("maximum").filter(|m| n > *m) {
            errors.push(format!("{}: {n} is above the maximum {m}", ptr(path)));
        }
        if let Some(m) = bound("exclusiveMinimum").filter(|m| n <= *m) {
            errors.push(format!("{}: {n} must be above {m}", ptr(path)));
        }
        if let Some(m) = bound("exclusiveMaximum").filter(|m| n >= *m) {
            errors.push(format!("{}: {n} must be below {m}", ptr(path)));
        }
    }
    if let Some(s) = value.as_str() {
        let len = s.chars().count() as u64;
        if let Some(m) = obj.get("minLength").and_then(Value::as_u64).filter(|m| len < *m) {
            errors.push(format!("{}: shorter than {m} characters", ptr(path)));
        }
        if let Some(m) = obj.get("maxLength").and_then(Value::as_u64).filter(|m| len > *m) {
            errors.push(format!("{}: longer than {m} characters", ptr(path)));
        }
    }
    if let Some(items) = value.as_array() {
        let len = items.len() as u64;
        if let Some(m) = obj.get("minItems").and_then(Value::as_u64).filter(|m| len < *m) {
            errors.push(format!("{}: fewer than {m} items", ptr(path)));
        }
        if let Some(m) = obj.get("maxItems").and_then(Value::as_u64).filter(|m| len > *m) {
            errors.push(format!("{}: more than {m} items", ptr(path)));
        }
        if let Some(item_schema) = obj.get("items") {
            for (i, item) in items.iter().enumerate() {
                walk(item, item_schema, &format!("{path}/{i}"), errors);
            }
        }
    }
    if let Some(map) = value.as_object() {
        let props = obj.get("properties").and_then(Value::as_object);
        if let Some(Value::Array(req)) = obj.get("required") {
            for r in req.iter().filter_map(Value::as_str) {
                if !map.contains_key(r) {
                    errors.push(format!("{}: missing required `{r}`", ptr(path)));
                }
            }
        }
        for (k, v) in map {
            let child = format!("{path}/{k}");
            match props.and_then(|p| p.get(k)) {
                Some(s) => walk(v, s, &child, errors),
                None => match obj.get("additionalProperties") {
                    Some(Value::Bool(false)) => errors.push(format!("{}: `{k}` is not allowed", ptr(path))),
                    Some(s @ Value::Object(_)) => walk(v, s, &child, errors),
                    _ => {}
                },
            }
        }
    }
    if let Some(Value::Array(alts)) = obj.get("anyOf") {
        if !alts.iter().any(|a| validate(value, a).is_empty()) {
            errors.push(format!("{}: matches none of the `anyOf` alternatives", ptr(path)));
        }
    }
    if let Some(Value::Array(alts)) = obj.get("oneOf") {
        let n = alts.iter().filter(|a| validate(value, a).is_empty()).count();
        if n != 1 {
            errors.push(format!("{}: matches {n} of the `oneOf` alternatives, not exactly one", ptr(path)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn finding() -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": {"type": "string", "minLength": 1},
                "line": {"type": "integer", "minimum": 1},
                "severity": {"enum": ["low", "medium", "high"]},
                "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 3}
            },
            "required": ["file", "severity"],
            "additionalProperties": false
        })
    }

    #[test]
    fn conforming_values_pass_and_each_break_is_named() {
        assert!(validate(&json!({"file": "a.rs", "line": 3, "severity": "high"}), &finding()).is_empty());
        let e = validate(&json!({"line": 0, "severity": "urgent", "tags": ["a", 1], "x": 1}), &finding());
        let joined = e.join("\n");
        assert!(joined.contains("missing required `file`"), "{joined}");
        assert!(joined.contains("/line: 0 is below the minimum 1"), "{joined}");
        assert!(joined.contains("/severity: must be one of"), "{joined}");
        assert!(joined.contains("/tags/1: expected string, got number"), "{joined}");
        assert!(joined.contains("`x` is not allowed"), "{joined}");
    }

    #[test]
    fn any_of_one_of_const_and_type_lists() {
        let s = json!({"type": "object", "properties": {"v": {"anyOf": [{"type": "string"}, {"type": "null"}]}, "k": {"const": 1}, "t": {"type": ["integer", "null"]}}});
        assert!(validate(&json!({"v": null, "k": 1, "t": 2}), &s).is_empty());
        assert!(!validate(&json!({"v": 2}), &s).is_empty());
        assert!(!validate(&json!({"k": 2}), &s).is_empty());
        let one = json!({"oneOf": [{"type": "integer"}, {"type": "number"}]});
        assert!(!validate(&json!(1), &one).is_empty(), "1 is both an integer and a number");
        assert!(validate(&json!(1.5), &one).is_empty());
    }

    #[test]
    fn unusable_schemas_are_refused_before_any_call() {
        assert!(check_shape(&finding()).is_ok());
        assert!(check_shape(&json!({"type": "array"})).unwrap_err().contains("root"));
        assert!(check_shape(&json!({"type": "object", "required": ["a"]})).unwrap_err().contains("`a` is required"));
        assert!(check_shape(&json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 5, "maximum": 1}}})).is_err());
        assert!(check_shape(&json!({"type": "object", "properties": {"n": {"type": "int"}}})).unwrap_err().contains("unknown type"));
        assert!(check_shape(&json!("object")).is_err());
    }
}
