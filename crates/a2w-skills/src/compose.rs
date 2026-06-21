//! Composing and adapting stored skills for new queries.
//!
//! Two operations, both **validity-preserving** (the result passes M1):
//! - [`adapt`] re-ids a skill's workflow so it can be instantiated under a new
//!   id for a new query, with no structural change.
//! - [`compose_sequential`] splices skill A's output into skill B: A's terminal
//!   nodes feed B's trigger-successors, B's redundant trigger is dropped, and
//!   all node ids are namespaced to avoid collision. The result is a single,
//!   single-trigger DAG that runs in one engine pass (no sub-workflow needed).

use std::collections::HashSet;

use a2w_ir::{Connection, Node, Workflow, SCHEMA_VERSION};

use crate::library::Skill;
use crate::SkillError;

/// Re-id a skill's workflow under `new_id` (and optionally a new display name).
/// Node ids and connections are unchanged; only the workflow id changes.
///
/// # Errors
/// [`SkillError::Invalid`] if the result somehow fails validation (it should
/// not, since the source skill is valid).
pub fn adapt(skill: &Skill, new_id: &str, new_name: &str) -> Result<Workflow, SkillError> {
    let mut wf = skill.workflow.clone();
    wf.id = new_id.to_string();
    wf.name = new_name.to_string();
    let report = a2w_validator::validate(&wf);
    if !report.is_valid {
        return Err(SkillError::Invalid(report));
    }
    Ok(wf)
}

/// Splice two skills into a single sequential workflow: `a` then `b`.
///
/// All of `a`'s nodes are prefixed `a_`, all of `b`'s `b_`. `b`'s trigger is
/// dropped; for every edge `(b_trigger -> x)` and every terminal node `t` of
/// `a`, an edge `(a_t -> b_x)` is added. The composed workflow keeps `a`'s
/// trigger as the single entry point.
///
/// The returned tuple is `(workflow, observe_node)` where `observe_node` is the
/// prefixed id of `b`'s original observe node — the natural "result" of the
/// composition.
///
/// # Errors
/// - [`SkillError::Compose`] if `a` has no terminal node, or `b` has no
///   trigger-successor to attach to.
/// - [`SkillError::Invalid`] if the spliced graph fails M1 validation.
pub fn compose_sequential(
    a: &Skill,
    b: &Skill,
    new_id: &str,
    new_name: &str,
) -> Result<(Workflow, String), SkillError> {
    let a_pref = "a_";
    let b_pref = "b_";

    // --- A: copy every node + connection with the `a_` prefix. -------------
    let mut nodes: Vec<Node> = a
        .workflow
        .nodes
        .iter()
        .map(|n| prefixed_node(n, a_pref))
        .collect();
    let mut connections: Vec<Connection> = a
        .workflow
        .connections
        .iter()
        .map(|c| prefixed_conn(c, a_pref, a_pref))
        .collect();

    // A's terminal nodes: have no outgoing edge in A.
    let a_sources: HashSet<&str> = a
        .workflow
        .connections
        .iter()
        .map(|c| c.from_node.as_str())
        .collect();
    let a_terminals: Vec<String> = a
        .workflow
        .nodes
        .iter()
        .filter(|n| !a_sources.contains(n.id.as_str()))
        .map(|n| format!("{a_pref}{}", n.id))
        .collect();
    if a_terminals.is_empty() {
        return Err(SkillError::Compose(
            "left workflow has no terminal node to attach the right workflow to".to_string(),
        ));
    }

    // --- B: identify its trigger and the nodes it feeds. -------------------
    let b_trigger = b
        .workflow
        .nodes
        .iter()
        .find(|n| n.kind.is_trigger())
        .ok_or_else(|| SkillError::Compose("right workflow has no trigger".to_string()))?;
    let b_trigger_id = b_trigger.id.as_str();

    // B's trigger-successors (the entry points of B's real logic).
    let b_entry_targets: Vec<String> = b
        .workflow
        .connections
        .iter()
        .filter(|c| c.from_node == b_trigger_id)
        .map(|c| format!("{b_pref}{}", c.to_node))
        .collect();
    if b_entry_targets.is_empty() {
        return Err(SkillError::Compose(
            "right workflow's trigger feeds nothing; cannot splice".to_string(),
        ));
    }

    // --- B: copy every node EXCEPT its trigger, with the `b_` prefix. ------
    for n in &b.workflow.nodes {
        if n.id == b_trigger_id {
            continue;
        }
        nodes.push(prefixed_node(n, b_pref));
    }
    // B's connections, except those out of B's trigger (those get re-rooted to
    // A's terminals below).
    for c in &b.workflow.connections {
        if c.from_node == b_trigger_id {
            continue;
        }
        connections.push(prefixed_conn(c, b_pref, b_pref));
    }

    // --- Bridge: every A-terminal -> every B-entry-target. -----------------
    for t in &a_terminals {
        for entry in &b_entry_targets {
            connections.push(Connection::new(t.clone(), 0, entry.clone()));
        }
    }

    let wf = Workflow {
        schema_version: SCHEMA_VERSION,
        id: new_id.to_string(),
        name: new_name.to_string(),
        nodes,
        connections,
    };
    let report = a2w_validator::validate(&wf);
    if !report.is_valid {
        return Err(SkillError::Invalid(report));
    }
    let observe_node = format!("{b_pref}{}", b.observe_node);
    Ok((wf, observe_node))
}

fn prefixed_node(n: &Node, prefix: &str) -> Node {
    let mut copy = n.clone();
    copy.id = format!("{prefix}{}", n.id);
    copy
}

fn prefixed_conn(c: &Connection, from_prefix: &str, to_prefix: &str) -> Connection {
    Connection {
        from_node: format!("{from_prefix}{}", c.from_node),
        from_port: c.from_port,
        to_node: format!("{to_prefix}{}", c.to_node),
    }
}
