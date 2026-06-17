//! Slugification of n8n node display names into stable A2W node ids.
//!
//! n8n addresses nodes by their human-readable *name*; the A2W IR references
//! nodes by a stable `id`. We derive an id from the name by lowercasing,
//! collapsing every run of non-alphanumeric characters to a single `_`, and
//! trimming leading/trailing `_`. A [`SlugAllocator`] then guarantees ids are
//! unique within a workflow by suffixing `_2`, `_3`, ... on collision.

use std::collections::{HashMap, HashSet};

/// Slugify a single string: lowercase, non-alphanumeric runs become `_`, and
/// leading/trailing underscores are trimmed.
///
/// An empty or all-separator input yields `"node"` so the result is never an
/// empty id.
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
        "node".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Allocates unique slugged ids and records the name→id mapping used to wire
/// connections (which n8n expresses by name).
#[derive(Debug, Default)]
pub struct SlugAllocator {
    used: HashSet<String>,
    by_name: HashMap<String, String>,
}

impl SlugAllocator {
    /// Create an empty allocator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a unique id for `name`, slugifying then disambiguating with a
    /// numeric suffix on collision. Records the name→id mapping.
    ///
    /// If the same `name` is allocated twice, a fresh unique id is produced for
    /// the second occurrence (n8n names are nominally unique, but we stay
    /// robust), and the stored mapping points at the most recent id.
    pub fn allocate(&mut self, name: &str) -> String {
        let base = slugify(name);
        let id = if self.used.insert(base.clone()) {
            base
        } else {
            let mut n: u32 = 2;
            loop {
                let candidate = format!("{base}_{n}");
                if self.used.insert(candidate.clone()) {
                    break candidate;
                }
                n += 1;
            }
        };
        self.by_name.insert(name.to_string(), id.clone());
        id
    }

    /// Look up the id previously allocated for an n8n node name, if any.
    #[must_use]
    pub fn id_for(&self, name: &str) -> Option<&str> {
        self.by_name.get(name).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_slugify() {
        assert_eq!(slugify("Webhook"), "webhook");
        assert_eq!(slugify("HTTP Request"), "http_request");
        assert_eq!(slugify("My  Cool--Node!!"), "my_cool_node");
        assert_eq!(slugify("  Edit Fields (set)  "), "edit_fields_set");
    }

    #[test]
    fn empty_becomes_node() {
        assert_eq!(slugify(""), "node");
        assert_eq!(slugify("***"), "node");
    }

    #[test]
    fn uniqueness_suffixing() {
        let mut a = SlugAllocator::new();
        assert_eq!(a.allocate("Set"), "set");
        assert_eq!(a.allocate("set"), "set_2");
        assert_eq!(a.allocate("SET"), "set_3");
        assert_eq!(a.id_for("set"), Some("set_2"));
    }
}
