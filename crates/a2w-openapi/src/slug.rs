//! Slugification helpers for deriving stable, agent-facing identifiers from
//! free-form OpenAPI strings (the spec `info.title` and, for operations that
//! lack an `operationId`, the HTTP method + path).
//!
//! The rule mirrors the rest of the A2W toolchain: lowercase, collapse every
//! run of non-alphanumeric characters to a single `_`, and trim leading and
//! trailing `_`. An empty or all-separator input yields `"api"` so a generated
//! id is never the empty string.

/// Slugify a single string: lowercase, non-alphanumeric runs become `_`, and
/// leading/trailing underscores are trimmed.
///
/// An empty or all-separator input yields `"api"`.
#[must_use]
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_underscore = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_underscore = false;
        } else if ch.is_alphanumeric() {
            // Non-ASCII alphanumerics (e.g. accented letters): keep lowercased.
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            prev_underscore = false;
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "api".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Synthesize an action name for an operation that has no `operationId`, by
/// slugging the path and joining it to the lowercased method, e.g.
/// `get` + `/users/{id}` -> `get_users_id`.
#[must_use]
pub fn synth_name(method: &str, path: &str) -> String {
    format!("{}_{}", method.to_ascii_lowercase(), slugify(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_slugify() {
        assert_eq!(slugify("Pet Store"), "pet_store");
        assert_eq!(slugify("My  Cool--API!!"), "my_cool_api");
        assert_eq!(slugify("  Acme (v2)  "), "acme_v2");
    }

    #[test]
    fn empty_becomes_api() {
        assert_eq!(slugify(""), "api");
        assert_eq!(slugify("***"), "api");
    }

    #[test]
    fn synthesized_names() {
        assert_eq!(synth_name("get", "/users"), "get_users");
        assert_eq!(synth_name("DELETE", "/users/{id}"), "delete_users_id");
        assert_eq!(synth_name("post", "/"), "post_api");
    }
}
