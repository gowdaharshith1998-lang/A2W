//! Task signatures: how skills are indexed and retrieved.
//!
//! A signature blends two deterministic features of a task:
//! 1. the **query tokens** (normalized keywords from the natural-language
//!    request), and
//! 2. the **structural fingerprint** (a histogram of node kinds in the
//!    workflow that solved it).
//!
//! Retrieval ranks stored skills by signature similarity to a new query, so a
//! workflow proven for "summarize and tag incoming alerts" can be surfaced for
//! "tag and summarize alert events".

use std::collections::{BTreeMap, BTreeSet};

use a2w_ir::Workflow;
use serde::{Deserialize, Serialize};

/// Tokens too generic to carry task meaning; dropped during normalization.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "the", "to", "of", "for", "in", "on", "with", "is", "are", "be", "that",
    "this", "it", "as", "by", "or", "from", "into", "then", "when", "each", "all", "any", "my",
    "our", "your",
];

/// A deterministic signature of a task: query tokens + node-kind histogram.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSignature {
    /// Normalized keyword set extracted from the query.
    pub tokens: BTreeSet<String>,
    /// Histogram of node kinds (wire names) in the solving workflow. Empty for
    /// a query-only signature used at retrieval time.
    pub kind_histogram: BTreeMap<String, usize>,
}

impl TaskSignature {
    /// Build a signature from a free-text query alone (no workflow). Used to
    /// retrieve candidate skills before any workflow exists.
    #[must_use]
    pub fn from_query(query: &str) -> Self {
        Self {
            tokens: tokenize(query),
            kind_histogram: BTreeMap::new(),
        }
    }

    /// Build a full signature from the query and the workflow that solved it.
    #[must_use]
    pub fn from_query_and_workflow(query: &str, wf: &Workflow) -> Self {
        let mut kind_histogram = BTreeMap::new();
        for node in &wf.nodes {
            let name = node_kind_wire_name(node.kind);
            *kind_histogram.entry(name.to_string()).or_insert(0) += 1;
        }
        Self {
            tokens: tokenize(query),
            kind_histogram,
        }
    }

    /// Similarity to another signature in `[0.0, 1.0]`.
    ///
    /// Combines Jaccard over tokens (the dominant signal) with cosine over kind
    /// histograms. When either histogram is empty (e.g. a query-only retrieval
    /// signature), similarity is the token Jaccard alone.
    #[must_use]
    pub fn similarity(&self, other: &TaskSignature) -> f64 {
        let token_sim = jaccard(&self.tokens, &other.tokens);
        if self.kind_histogram.is_empty() || other.kind_histogram.is_empty() {
            return token_sim;
        }
        let struct_sim = histogram_cosine(&self.kind_histogram, &other.kind_histogram);
        0.7 * token_sim + 0.3 * struct_sim
    }
}

/// Normalize free text into a deterministic keyword set: lowercase, split on
/// non-alphanumerics, drop stopwords and 1-char tokens.
fn tokenize(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|s| s.len() > 1 && !STOPWORDS.contains(&s.as_str()))
        .collect()
}

/// Jaccard similarity of two sets: |A∩B| / |A∪B|. Two empty sets → 1.0
/// (vacuously identical), one empty → 0.0.
fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Cosine similarity of two kind histograms.
fn histogram_cosine(a: &BTreeMap<String, usize>, b: &BTreeMap<String, usize>) -> f64 {
    let mut dot = 0.0;
    for (k, av) in a {
        if let Some(bv) = b.get(k) {
            dot += (*av as f64) * (*bv as f64);
        }
    }
    let norm = |m: &BTreeMap<String, usize>| -> f64 {
        m.values()
            .map(|v| (*v as f64) * (*v as f64))
            .sum::<f64>()
            .sqrt()
    };
    let denom = norm(a) * norm(b);
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

/// The wire (snake_case) name of a node kind, used as a stable histogram key.
fn node_kind_wire_name(kind: a2w_ir::NodeKind) -> &'static str {
    use a2w_ir::NodeKind as K;
    match kind {
        K::WebhookTrigger => "webhook_trigger",
        K::ScheduleTrigger => "schedule_trigger",
        K::HttpRequest => "http_request",
        K::McpToolCall => "mcp_tool_call",
        K::Transform => "transform",
        K::Branch => "branch",
        K::Switch => "switch",
        K::Loop => "loop",
        K::Merge => "merge",
        K::Wait => "wait",
        K::SubWorkflow => "sub_workflow",
        K::LlmCall => "llm_call",
        K::CodeStep => "code_step",
        K::Approval => "approval",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_drops_stopwords_and_short_tokens() {
        let t = tokenize("Summarize and tag the incoming Alert events!");
        assert!(t.contains("summarize"));
        assert!(t.contains("tag"));
        assert!(t.contains("incoming"));
        assert!(t.contains("alert"));
        assert!(t.contains("events"));
        assert!(!t.contains("and"));
        assert!(!t.contains("the"));
    }

    #[test]
    fn similar_queries_score_higher_than_dissimilar() {
        let base = TaskSignature::from_query("tag and summarize alert events");
        let close = TaskSignature::from_query("summarize incoming alert and tag them");
        let far = TaskSignature::from_query("convert currency exchange rates daily");
        assert!(base.similarity(&close) > base.similarity(&far));
    }

    #[test]
    fn identical_query_is_max_similar() {
        let a = TaskSignature::from_query("fetch user profile data");
        let b = TaskSignature::from_query("fetch user profile data");
        assert!((a.similarity(&b) - 1.0).abs() < 1e-9);
    }
}
