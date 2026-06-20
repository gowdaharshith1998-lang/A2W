//! Best-effort translation of n8n expressions into A2W template syntax.
//!
//! n8n string parameters that are expressions start with a leading `=` and
//! contain one or more `{{ ... }}` mustache segments, e.g.
//! `"={{ $json.field }}"` or `"={{ $json[\"field\"] }}"`. A2W uses
//! `{{json.field}}` (no `$`, no inner spaces).
//!
//! [`translate_expr`] handles the common `$json` cases and reports whether the
//! string was *fully* translated. Anything it cannot cleanly map (references to
//! `$node`, `$items`, `$(...)`, function calls, JMESPath, etc.) is left exactly
//! as it was, and the second tuple element is `false` so the caller can emit an
//! `ExpressionNotTranslated` warning.

/// Translate an n8n expression string to A2W template syntax.
///
/// Returns `(translated, fully_translated)`.
///
/// - If `s` does not begin with `=`, it is not an n8n expression: it is
///   returned unchanged with `fully_translated = true` (nothing to translate).
/// - Otherwise the leading `=` is stripped and each `{{ ... }}` segment is
///   examined. A segment is translatable iff its inner expression references
///   only `$json` (optionally with `.field` / `["field"]` / `[0]` accessors) or
///   is the bare `$json`. Translatable segments are rewritten to `{{json...}}`
///   with inner whitespace collapsed. If *every* segment translates, the result
///   is returned with `fully_translated = true`.
/// - If any segment cannot be translated, the **original** string is returned
///   unchanged with `fully_translated = false`.
#[must_use]
pub fn translate_expr(s: &str) -> (String, bool) {
    let Some(body) = s.strip_prefix('=') else {
        // Not an n8n expression at all.
        return (s.to_string(), true);
    };

    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    let mut all_ok = true;

    while let Some(open) = rest.find("{{") {
        // Copy the literal text before the mustache verbatim.
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 2..];
        let Some(close_rel) = after_open.find("}}") else {
            // Unterminated mustache: cannot translate cleanly.
            all_ok = false;
            break;
        };
        let inner = &after_open[..close_rel];
        match translate_segment(inner) {
            Some(translated) => {
                out.push_str("{{");
                out.push_str(&translated);
                out.push_str("}}");
            }
            None => {
                all_ok = false;
                break;
            }
        }
        rest = &after_open[close_rel + 2..];
    }

    if all_ok {
        out.push_str(rest);
        (out, true)
    } else {
        // Leave the original string untouched for the caller to flag.
        (s.to_string(), false)
    }
}

/// Try to translate the inside of a single `{{ ... }}` segment.
///
/// Returns `Some(normalized)` (no `$`, inner whitespace collapsed) when the
/// segment references only `$json`, otherwise `None`.
fn translate_segment(inner: &str) -> Option<String> {
    let trimmed = inner.trim();

    // Bare `$json` → `json`.
    if trimmed == "$json" {
        return Some("json".to_string());
    }

    // Must start with `$json` followed by an accessor (`.` or `[`).
    let after = trimmed.strip_prefix("$json")?;
    let mut chars = after.chars();
    match chars.next() {
        Some('.') | Some('[') => {}
        _ => return None,
    }

    // Only allow a "safe" accessor chain: identifiers, dots, brackets, quotes,
    // digits, whitespace. Reject anything that looks like a function call,
    // operator, or another `$`-reference embedded in the expression.
    for ch in after.chars() {
        let ok =
            ch.is_alphanumeric() || matches!(ch, '.' | '[' | ']' | '"' | '\'' | '_' | ' ' | '\t');
        if !ok {
            return None;
        }
    }

    // Collapse all whitespace inside the accessor chain.
    let collapsed: String = after.chars().filter(|c| !c.is_whitespace()).collect();
    Some(format!("json{collapsed}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_string_unchanged() {
        let (out, ok) = translate_expr("https://example.com");
        assert_eq!(out, "https://example.com");
        assert!(ok);
    }

    #[test]
    fn simple_json_field() {
        let (out, ok) = translate_expr("={{ $json.repo }}");
        assert_eq!(out, "{{json.repo}}");
        assert!(ok);
    }

    #[test]
    fn bare_json() {
        let (out, ok) = translate_expr("={{ $json }}");
        assert_eq!(out, "{{json}}");
        assert!(ok);
    }

    #[test]
    fn bracket_accessor() {
        let (out, ok) = translate_expr("={{ $json[\"field name\"] }}");
        assert_eq!(out, "{{json[\"fieldname\"]}}");
        assert!(ok);
    }

    #[test]
    fn surrounding_literals() {
        let (out, ok) = translate_expr("=https://api/{{ $json.id }}/end");
        assert_eq!(out, "https://api/{{json.id}}/end");
        assert!(ok);
    }

    #[test]
    fn node_reference_not_translated() {
        let src = "={{ $node[\"X\"].json.value }}";
        let (out, ok) = translate_expr(src);
        assert_eq!(out, src, "untranslatable expr must be left as-is");
        assert!(!ok);
    }

    #[test]
    fn function_call_not_translated() {
        let src = "={{ $json.items.map(x => x.id) }}";
        let (out, ok) = translate_expr(src);
        assert_eq!(out, src);
        assert!(!ok);
    }
}
