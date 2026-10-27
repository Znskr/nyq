//! A small JSON Schema check for MCP tool arguments and elicitation answers:
//! `type`, `required`, `enum`/`const`, nested `properties` and `items`, and
//! string/number bounds. Keywords it does not know are ignored, so a valid
//! call is never rejected; it only catches the common mistakes early, with a
//! message the model can act on.

use serde_json::Value;

/// Problems with `value` against `schema`, as `path: message` lines.
pub fn check(schema: &Value, value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(schema, value, "", &mut out, 0);
    out
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_type(v: &Value, ty: &str) -> bool {
    match ty {
        "integer" => match v {
            Value::Number(n) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        },
        "number" => v.is_number(),
        other => type_name(v) == other,
    }
}

fn walk(schema: &Value, value: &Value, path: &str, out: &mut Vec<String>, depth: usize) {
    let Some(s) = schema.as_object() else { return };
    if depth > 32 || out.len() >= 20 {
        return;
    }
    let at = if path.is_empty() {
        "arguments".to_string()
    } else {
        format!("`{path}`")
    };
    let types: Vec<&str> = match s.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !types.is_empty() && !types.iter().any(|t| is_type(value, t)) {
        out.push(format!(
            "{at} must be {} (got {})",
            types.join(" or "),
            type_name(value)
        ));
        return;
    }
    if let Some(Value::Array(options)) = s.get("enum") {
        if !options.contains(value) {
            let shown: Vec<String> = options.iter().take(20).map(Value::to_string).collect();
            out.push(format!("{at} must be one of {}", shown.join(", ")));
        }
    }
    if let Some(c) = s.get("const") {
        if c != value {
            out.push(format!("{at} must be {c}"));
        }
    }
    match value {
        Value::Object(m) => {
            for name in s
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !m.contains_key(name) {
                    out.push(format!("`{}` is required", join(path, name)));
                }
            }
            let props = s.get("properties").and_then(Value::as_object);
            for (k, v) in m {
                match props.and_then(|p| p.get(k)) {
                    Some(sub) => walk(sub, v, &join(path, k), out, depth + 1),
                    None if s.get("additionalProperties") == Some(&Value::Bool(false)) => {
                        let known: Vec<&str> = props
                            .map(|p| p.keys().map(String::as_str).collect())
                            .unwrap_or_default();
                        out.push(format!(
                            "`{}` is not a known property (expected: {})",
                            join(path, k),
                            known.join(", ")
                        ));
                    }
                    None => {}
                }
            }
        }
        Value::Array(items) => {
            if let Some(sub) = s.get("items").filter(|i| i.is_object()) {
                for (i, v) in items.iter().enumerate() {
                    walk(sub, v, &format!("{path}[{i}]"), out, depth + 1);
                }
            }
            bound(
                s,
                "minItems",
                "maxItems",
                items.len() as f64,
                &at,
                "items",
                out,
            );
        }
        Value::String(text) => bound(
            s,
            "minLength",
            "maxLength",
            text.chars().count() as f64,
            &at,
            "characters",
            out,
        ),
        Value::Number(n) => {
            if let Some(n) = n.as_f64() {
                bound(s, "minimum", "maximum", n, &at, "", out);
            }
        }
        _ => {}
    }
}

fn bound(
    s: &serde_json::Map<String, Value>,
    min: &str,
    max: &str,
    n: f64,
    at: &str,
    unit: &str,
    out: &mut Vec<String>,
) {
    let unit = if unit.is_empty() {
        String::new()
    } else {
        format!(" {unit}")
    };
    if let Some(lo) = s.get(min).and_then(Value::as_f64).filter(|lo| n < *lo) {
        out.push(format!("{at} must be at least {lo}{unit}"));
    }
    if let Some(hi) = s.get(max).and_then(Value::as_f64).filter(|hi| n > *hi) {
        out.push(format!("{at} must be at most {hi}{unit}"));
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reports_common_mistakes() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "minLength": 1},
                "count": {"type": "integer", "minimum": 1},
                "mode": {"enum": ["fast", "slow"]},
                "tags": {"type": "array", "items": {"type": "string"}},
                "opts": {"type": "object", "properties": {"deep": {"type": "boolean"}}, "required": ["deep"]},
                "any": {}
            },
            "required": ["path"]
        });
        assert!(check(
            &schema,
            &json!({"path": "a", "count": 2, "mode": "fast", "tags": ["x"], "any": [1]})
        )
        .is_empty());
        assert!(check(&schema, &json!({"path": "a", "count": 2.0})).is_empty());
        let errors = check(
            &schema,
            &json!({"count": "2", "mode": "medium", "opts": {}, "tags": [1]}),
        );
        assert_eq!(
            errors,
            [
                "`path` is required",
                "`count` must be integer (got string)",
                "`mode` must be one of \"fast\", \"slow\"",
                "`opts.deep` is required",
                "`tags[0]` must be string (got integer)",
            ]
        );
        assert_eq!(
            check(&schema, &json!({"path": ""})),
            ["`path` must be at least 1 characters"]
        );
        assert_eq!(
            check(&schema, &json!([])),
            ["arguments must be object (got array)"]
        );
        let strict =
            json!({"type": "object", "properties": {"a": {}}, "additionalProperties": false});
        assert_eq!(
            check(&strict, &json!({"b": 1})),
            ["`b` is not a known property (expected: a)"]
        );
        // Unknown keywords and non-object schemas never reject.
        assert!(check(&json!({"$ref": "#/defs/x", "oneOf": []}), &json!(1)).is_empty());
        assert!(check(&json!(true), &json!(1)).is_empty());
        assert!(check(&json!({"type": ["string", "null"]}), &Value::Null).is_empty());
    }
}
