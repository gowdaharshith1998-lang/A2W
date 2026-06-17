//! # a2w-templates
//!
//! A small **golden corpus** of valid [`a2w_ir::Workflow`]s. Each template is a
//! complete, *validator-clean* workflow plus human metadata (`id`, `name`,
//! `description`, `tags`).
//!
//! The corpus has two jobs:
//! - It is the **few-shot example library** for text-to-workflow authoring: the
//!   author crate serializes one or two of these workflows into the LLM system
//!   prompt so the model has a concrete, correct shape to imitate.
//! - It is a **starter gallery** an agent can search ([`search`]) and fetch
//!   ([`get`]) directly, skipping generation entirely when a template already
//!   fits.
//!
//! Every template returned by [`all`] is guaranteed (by the crate's own tests)
//! to pass [`a2w_validator::validate`] with `is_valid == true`. That invariant
//! is load-bearing: a broken example would teach the model the wrong shape.
//!
//! Templates favour the four *executable* node kinds
//! (`webhook_trigger` / `schedule_trigger` / `http_request` / `transform`) so
//! they are realistic; a couple add `merge` to demonstrate fan-out/fan-in. (The
//! `schedule_trigger` and `merge` kinds validate structurally even though the
//! M2 engine does not yet execute them, which is why templates are checked
//! against the *validator*, not the engine.)

#![forbid(unsafe_code)]

use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
use serde::Serialize;
use serde_json::json;

/// A named, described, tagged workflow example.
#[derive(Debug, Clone, Serialize)]
pub struct Template {
    /// Stable identifier (also the workflow's `id`).
    pub id: String,
    /// Short human-readable name.
    pub name: String,
    /// One- or two-sentence description of what the workflow does.
    pub description: String,
    /// Free-form keyword tags for [`search`].
    pub tags: Vec<String>,
    /// The workflow itself — a complete, valid IR document.
    pub workflow: Workflow,
}

/// Convenience: build a [`Node`] with the given JSON `params`.
fn node(id: &str, kind: NodeKind, params: serde_json::Value) -> Node {
    let mut n = Node::new(id, kind);
    n.params = params;
    n
}

/// Convenience: build a [`Template`] from its parts.
fn template(
    id: &str,
    name: &str,
    description: &str,
    tags: &[&str],
    nodes: Vec<Node>,
    connections: Vec<Connection>,
) -> Template {
    Template {
        id: id.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        tags: tags.iter().map(|t| (*t).to_string()).collect(),
        workflow: Workflow {
            schema_version: SCHEMA_VERSION,
            id: id.to_string(),
            name: name.to_string(),
            nodes,
            connections,
        },
    }
}

/// `webhook_trigger -> http_request(GET) -> transform -> http_request(POST)`.
fn webhook_to_slack() -> Template {
    template(
        "webhook_to_slack",
        "Webhook to Slack notification",
        "On an inbound webhook, fetch a record from an API, build a summary \
         field, and POST it to a Slack-style incoming webhook URL.",
        &["slack", "notification", "http", "webhook", "alert"],
        vec![
            node("trigger", NodeKind::WebhookTrigger, json!({})),
            node(
                "fetch",
                NodeKind::HttpRequest,
                json!({
                    "method": "GET",
                    "url": "https://api.example.com/records/{{json.id}}"
                }),
            ),
            node(
                "summarize",
                NodeKind::Transform,
                json!({ "set": { "summary": "New event for {{json.id}}" } }),
            ),
            node(
                "notify",
                NodeKind::HttpRequest,
                json!({
                    "method": "POST",
                    "url": "https://hooks.slack.com/services/T000/B000/XXXX",
                    "json": { "text": "{{json.summary}}" }
                }),
            ),
        ],
        vec![
            Connection::new("trigger", 0, "fetch"),
            Connection::new("fetch", 0, "summarize"),
            Connection::new("summarize", 0, "notify"),
        ],
    )
}

/// `schedule_trigger -> http_request(GET) -> transform`.
fn scheduled_fetch_transform() -> Template {
    template(
        "scheduled_fetch_transform",
        "Scheduled fetch and transform",
        "On a cron schedule, fetch JSON from an API endpoint and reshape it by \
         setting a couple of derived fields.",
        &["schedule", "cron", "fetch", "http", "transform", "etl"],
        vec![
            node(
                "every_5m",
                NodeKind::ScheduleTrigger,
                json!({ "cron": "*/5 * * * *" }),
            ),
            node(
                "fetch",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://api.example.com/metrics" }),
            ),
            node(
                "shape",
                NodeKind::Transform,
                json!({ "set": { "source": "metrics-api", "ingested": true } }),
            ),
        ],
        vec![
            Connection::new("every_5m", 0, "fetch"),
            Connection::new("fetch", 0, "shape"),
        ],
    )
}

/// One webhook trigger fanning out to two independent fetches, merged back in.
///
/// `trigger -> fetch_a`, `trigger -> fetch_b`, `fetch_a -> merge`,
/// `fetch_b -> merge`. Demonstrates parallel branches and a fan-in `merge`.
fn parallel_fan_out() -> Template {
    template(
        "parallel_fan_out",
        "Parallel fan-out then merge",
        "On a webhook, fetch two independent API resources in parallel branches \
         and merge their results back into a single stream.",
        &["parallel", "fan-out", "merge", "http", "concurrency", "webhook"],
        vec![
            node("trigger", NodeKind::WebhookTrigger, json!({})),
            node(
                "fetch_a",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://api.example.com/a" }),
            ),
            node(
                "fetch_b",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://api.example.com/b" }),
            ),
            node("merge", NodeKind::Merge, json!({})),
        ],
        vec![
            Connection::new("trigger", 0, "fetch_a"),
            Connection::new("trigger", 0, "fetch_b"),
            Connection::new("fetch_a", 0, "merge"),
            Connection::new("fetch_b", 0, "merge"),
        ],
    )
}

/// `webhook_trigger -> transform`: a minimal echo/shape workflow.
fn webhook_echo() -> Template {
    template(
        "webhook_echo",
        "Webhook echo and tag",
        "Accept a webhook payload and pass it through a transform that stamps a \
         constant tag onto every item. The simplest useful workflow.",
        &["webhook", "echo", "transform", "passthrough", "minimal"],
        vec![
            node("trigger", NodeKind::WebhookTrigger, json!({})),
            node(
                "tag",
                NodeKind::Transform,
                json!({ "set": { "received": true, "channel": "webhook" } }),
            ),
        ],
        vec![Connection::new("trigger", 0, "tag")],
    )
}

/// `webhook_trigger -> http_request -> transform -> http_request`: a linear
/// multi-step enrichment pipeline (look up, enrich, forward).
fn enrichment_pipeline() -> Template {
    template(
        "enrichment_pipeline",
        "Multi-step enrichment pipeline",
        "Receive a webhook, look up the entity by id, enrich the item with a \
         derived status field, then forward the enriched record to a downstream \
         service.",
        &["enrichment", "pipeline", "http", "transform", "webhook", "etl"],
        vec![
            node("trigger", NodeKind::WebhookTrigger, json!({})),
            node(
                "lookup",
                NodeKind::HttpRequest,
                json!({
                    "method": "GET",
                    "url": "https://api.example.com/entities/{{json.id}}"
                }),
            ),
            node(
                "enrich",
                NodeKind::Transform,
                json!({ "set": { "status": "enriched", "pipeline": "v1" } }),
            ),
            node(
                "forward",
                NodeKind::HttpRequest,
                json!({
                    "method": "POST",
                    "url": "https://downstream.example.com/ingest",
                    "json": { "entity": "{{json}}" }
                }),
            ),
        ],
        vec![
            Connection::new("trigger", 0, "lookup"),
            Connection::new("lookup", 0, "enrich"),
            Connection::new("enrich", 0, "forward"),
        ],
    )
}

/// `schedule_trigger -> http_request -> transform -> http_request`: a scheduled
/// sync that reads from one system and writes to another.
fn scheduled_sync() -> Template {
    template(
        "scheduled_sync",
        "Scheduled cross-system sync",
        "On a daily schedule, read records from a source API, normalize them, \
         and POST the normalized batch to a destination API.",
        &["schedule", "cron", "sync", "http", "transform", "integration"],
        vec![
            node(
                "daily",
                NodeKind::ScheduleTrigger,
                json!({ "cron": "0 6 * * *" }),
            ),
            node(
                "read_source",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://source.example.com/records" }),
            ),
            node(
                "normalize",
                NodeKind::Transform,
                json!({ "set": { "normalized": true, "version": 2 } }),
            ),
            node(
                "write_dest",
                NodeKind::HttpRequest,
                json!({
                    "method": "POST",
                    "url": "https://dest.example.com/records",
                    "json": { "payload": "{{json}}" }
                }),
            ),
        ],
        vec![
            Connection::new("daily", 0, "read_source"),
            Connection::new("read_source", 0, "normalize"),
            Connection::new("normalize", 0, "write_dest"),
        ],
    )
}

/// All templates in the corpus, in a stable declaration order.
#[must_use]
pub fn all() -> Vec<Template> {
    vec![
        webhook_to_slack(),
        scheduled_fetch_transform(),
        parallel_fan_out(),
        webhook_echo(),
        enrichment_pipeline(),
        scheduled_sync(),
    ]
}

/// Case-insensitive keyword search over each template's `name`, `description`,
/// and `tags`.
///
/// The query is split on whitespace into words; a template matches if **any**
/// query word is a substring of **any** of its searchable fields. Results are
/// returned in declaration order (the order of [`all`]). An empty (or
/// whitespace-only) query matches nothing.
#[must_use]
pub fn search(query: &str) -> Vec<Template> {
    let words: Vec<String> = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    if words.is_empty() {
        return Vec::new();
    }

    all()
        .into_iter()
        .filter(|t| {
            let mut haystacks: Vec<String> =
                vec![t.name.to_lowercase(), t.description.to_lowercase()];
            haystacks.extend(t.tags.iter().map(|tag| tag.to_lowercase()));
            words
                .iter()
                .any(|w| haystacks.iter().any(|h| h.contains(w.as_str())))
        })
        .collect()
}

/// Fetch a single template by its `id`, if present.
#[must_use]
pub fn get(id: &str) -> Option<Template> {
    all().into_iter().find(|t| t.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_has_at_least_six_templates() {
        assert!(all().len() >= 6, "expected >= 6 templates, got {}", all().len());
    }

    #[test]
    fn every_template_validates() {
        for t in all() {
            let report = a2w_validator::validate(&t.workflow);
            assert!(
                report.is_valid,
                "template '{}' must be valid, findings: {:?}",
                t.id, report.findings
            );
        }
    }

    #[test]
    fn template_ids_are_unique() {
        let mut ids: Vec<String> = all().into_iter().map(|t| t.id).collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "template ids must be unique");
    }

    #[test]
    fn search_slack_finds_the_slack_template() {
        let hits = search("slack");
        assert!(
            hits.iter().any(|t| t.id == "webhook_to_slack"),
            "search('slack') should return the slack template, got: {:?}",
            hits.iter().map(|t| &t.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn search_is_case_insensitive_and_multi_word() {
        // "SCHEDULE etl" — either word matching any field is enough.
        let hits = search("SCHEDULE etl");
        assert!(
            hits.iter().any(|t| t.id == "scheduled_fetch_transform"),
            "case-insensitive multi-word search should match"
        );
    }

    #[test]
    fn search_empty_matches_nothing() {
        assert!(search("").is_empty());
        assert!(search("   ").is_empty());
    }

    #[test]
    fn search_returns_declaration_order() {
        // "http" appears in several templates; results must follow all()'s order.
        let hits = search("http");
        let all_ids: Vec<String> = all().into_iter().map(|t| t.id).collect();
        let hit_ids: Vec<String> = hits.iter().map(|t| t.id.clone()).collect();
        // hit_ids must be a subsequence of all_ids (i.e. same relative order).
        let mut it = all_ids.iter();
        for hid in &hit_ids {
            assert!(
                it.by_ref().any(|aid| aid == hid),
                "search results must be in declaration order"
            );
        }
    }

    #[test]
    fn get_known_id_is_some() {
        let t = get("webhook_to_slack").expect("webhook_to_slack present");
        assert_eq!(t.id, "webhook_to_slack");
        assert_eq!(t.workflow.id, "webhook_to_slack");
    }

    #[test]
    fn get_unknown_id_is_none() {
        assert!(get("nope").is_none());
    }
}
