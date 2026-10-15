use serde_json::Value;

/// Numeric leaves keyed by JSON pointer. Arrays of objects are skipped: a
/// ranked list's index is not an identity, so it cannot be a series.
pub fn flatten(value: &Value, path: &mut String, out: &mut Vec<(String, f64)>) {
    match value {
        Value::Number(n) => {
            if let Some(v) = n.as_f64() {
                out.push((path.clone(), v));
            }
        }
        Value::Bool(b) => out.push((path.clone(), f64::from(u8::from(*b)))),
        Value::Object(map) => {
            for (k, v) in map {
                let len = path.len();
                path.push('/');
                path.push_str(&escape(k));
                flatten(v, path, out);
                path.truncate(len);
            }
        }
        Value::Array(items) if items.iter().all(|i| i.is_number() || i.is_null()) => {
            for (i, v) in items.iter().enumerate() {
                let len = path.len();
                path.push('/');
                path.push_str(&i.to_string());
                flatten(v, path, out);
                path.truncate(len);
            }
        }
        Value::Array(_) | Value::String(_) | Value::Null => {}
    }
}

/// RFC 6901: chip keys such as `k10temp/0000:00:18.3` contain `/`.
pub fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn series_paths_escape_slashes_in_keys() {
        let mut out = Vec::new();
        flatten(
            &json!({"k10temp/0000:00:18.3": {"a~b": 1}}),
            &mut "/hwmon".into(),
            &mut out,
        );
        assert_eq!(out, [("/hwmon/k10temp~10000:00:18.3/a~0b".to_string(), 1.0)]);
    }
    #[test]
    fn ranked_lists_are_not_series_but_number_arrays_are() {
        let mut out = Vec::new();
        let v = json!({"core_pct": [5, null, 7], "top_cpu": [{"pid": 1, "cpu_pct": 50}], "name": "x", "on": true});
        flatten(&v, &mut "/m".into(), &mut out);
        let paths: Vec<_> = out.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, ["/m/core_pct/0", "/m/core_pct/2", "/m/on"]);
    }
}
