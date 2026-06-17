//! A deliberately tiny, interim string-templating helper.
//!
//! It substitutes two token forms against a single input item's JSON:
//! - `{{json}}`        → the whole item JSON, rendered compactly.
//! - `{{json.FIELD}}`  → the value at top-level object key `FIELD`.
//!
//! Substituted values render as their inner string when the value is a JSON
//! string (so a URL fragment doesn't gain quotes), and as compact JSON
//! otherwise. Unknown fields render as the empty string.
//!
//! NOTE: this is a stopgap. The full expression engine — jaq / proper `{{ }}`
//! interpolation over the item context — arrives in a later crate. Keep this
//! helper small and self-contained; do not grow it into a parser.

/// Substitute `{{json}}` and `{{json.FIELD}}` tokens in `template` using `item`.
///
/// Scans by `char` (UTF-8 safe): the braces are ASCII, but arbitrary text
/// between them may be multi-byte.
pub(crate) fn render(template: &str, item: &serde_json::Value) -> String {
    let mut out = String::with_capacity(template.len());
    let chars: Vec<char> = template.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Look for an opening "{{".
        if chars[i] == '{' && chars.get(i + 1) == Some(&'{') {
            if let Some(close) = find_close(&chars, i + 2) {
                let token: String = chars[i + 2..close].iter().collect();
                out.push_str(&resolve(token.trim(), item));
                i = close + 2; // skip past the closing "}}"
                continue;
            }
        }
        // Not a token start (or unterminated): copy the char through.
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Find the char index of the "}}" that closes a token opened at `from`.
fn find_close(chars: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < chars.len() {
        if chars[i] == '}' && chars[i + 1] == '}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Resolve a single token (without the surrounding braces) against `item`.
fn resolve(token: &str, item: &serde_json::Value) -> String {
    if token == "json" {
        return render_value(item);
    }
    if let Some(field) = token.strip_prefix("json.") {
        return match item.get(field) {
            Some(v) => render_value(v),
            None => String::new(),
        };
    }
    // Unrecognized token: leave it visibly empty rather than guessing.
    String::new()
}

/// Render a JSON value: bare string for strings, compact JSON otherwise.
fn render_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn substitutes_field() {
        let item = json!({ "id": 7, "name": "neo" });
        assert_eq!(
            render("https://x/{{json.name}}/{{json.id}}", &item),
            "https://x/neo/7"
        );
    }

    #[test]
    fn substitutes_whole_json() {
        let item = json!({ "a": 1 });
        assert_eq!(render("{{json}}", &item), "{\"a\":1}");
    }

    #[test]
    fn unknown_field_is_empty() {
        let item = json!({ "a": 1 });
        assert_eq!(render("x{{json.missing}}y", &item), "xy");
    }

    #[test]
    fn passes_through_plain_text() {
        let item = json!({});
        assert_eq!(render("no tokens here", &item), "no tokens here");
    }
}
