//! # a2w-search — search / evolution over the IR (M5)
//!
//! With candidates valid-by-construction (M1) and cheaply scorable (M3), the
//! IR becomes a searchable space. This crate runs a **deterministic beam
//! search** over [correctness-preserving operators](operators), using the M3
//! [`ConfidenceReport`](a2w_verify::ConfidenceReport) score as the **fitness
//! function** — ranking by how many relations, fixtures and assertions hold,
//! not a binary pass/fail.
//!
//! The structural advantage A2W exploits: scoring a candidate is zero-token and
//! deterministic, so evaluating a whole beam per generation is nearly free
//! compared to an LLM-in-the-loop competitor.
//!
//! Determinism: operators enumerate in a fixed order, candidates are
//! deduplicated and ranked by a total order (score, then parsimony, then
//! canonical form). No RNG is used anywhere — the search is fully reproducible.

#![forbid(unsafe_code)]

pub mod operators;

use std::collections::BTreeSet;

use a2w_ir::Workflow;
use a2w_verify::{verify, ConfidenceReport, VerificationHarness, VerificationPlan, VerifyError};

pub use operators::{InsertPassthrough, Mutation, RemovePassthrough, SetTransformField};

/// Search configuration.
#[derive(Debug, Clone, Copy)]
pub struct SearchConfig {
    /// Maximum generations to run.
    pub generations: usize,
    /// How many candidates survive to seed the next generation.
    pub beam_width: usize,
    /// Hard cap on candidates scored per generation (defends against operator
    /// blow-up). `0` means unlimited.
    pub max_candidates_per_gen: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            generations: 6,
            beam_width: 4,
            max_candidates_per_gen: 64,
        }
    }
}

/// The result of a search run.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    /// Fitness of the seed workflow.
    pub initial_score: f64,
    /// Fitness of the best workflow found.
    pub best_score: f64,
    /// The best workflow found (the seed itself if nothing improved on it).
    pub best_workflow: Workflow,
    /// The confidence report for the best workflow (the evidence behind the
    /// score — pass this straight to M4 promotion).
    pub best_report: ConfidenceReport,
    /// Total distinct candidates scored across all generations.
    pub candidates_evaluated: usize,
    /// Generations actually run (may stop early on a perfect score).
    pub generations_run: usize,
}

impl SearchOutcome {
    /// Whether the search strictly improved on the seed.
    #[must_use]
    pub fn improved(&self) -> bool {
        self.best_score > self.initial_score
    }
}

/// A scored candidate, ordered for deterministic beam selection.
struct Scored {
    workflow: Workflow,
    report: ConfidenceReport,
    score: f64,
    node_count: usize,
    canonical: String,
}

/// Run a deterministic beam search to improve `seed` against `plan`.
///
/// `plan.observe_node` must remain a node in every candidate — the supplied
/// operators are expected to preserve it (freeze it where needed). The fitness
/// is `plan`-derived M3 confidence; ties prefer fewer nodes (parsimony), then
/// the lexicographically-smallest canonical form (full determinism).
///
/// # Errors
/// [`VerifyError`] if the seed cannot be scored (e.g. the observe node is
/// absent, or the seed is unrunnable).
pub async fn evolve(
    harness: &VerificationHarness,
    seed: &Workflow,
    plan: &VerificationPlan,
    operators: &[Box<dyn Mutation>],
    config: SearchConfig,
) -> Result<SearchOutcome, VerifyError> {
    let seed_scored = score(harness, seed, plan).await?;
    let initial_score = seed_scored.score;

    let mut best = seed_scored;
    let mut beam: Vec<Workflow> = vec![seed.clone()];
    let mut seen: BTreeSet<String> = BTreeSet::new();
    seen.insert(best.canonical.clone());
    let mut candidates_evaluated = 1usize;
    let mut generations_run = 0usize;

    for _gen in 0..config.generations {
        generations_run += 1;

        // 1. Generate candidates from the whole beam, deduplicated.
        let mut raw: Vec<Workflow> = Vec::new();
        let mut raw_seen: BTreeSet<String> = BTreeSet::new();
        for parent in &beam {
            for op in operators {
                for cand in op.apply(parent) {
                    // Valid-by-construction guard: discard any non-M1 candidate.
                    if !a2w_validator::validate(&cand).is_valid {
                        continue;
                    }
                    // The observe node must survive.
                    if !cand.nodes.iter().any(|n| n.id == plan.observe_node) {
                        continue;
                    }
                    let canon = canonical_of(&cand);
                    if seen.contains(&canon) || !raw_seen.insert(canon) {
                        continue;
                    }
                    raw.push(cand);
                }
            }
        }

        // Deterministic order before capping, so the cap is reproducible.
        raw.sort_by_key(canonical_of);
        if config.max_candidates_per_gen > 0 {
            raw.truncate(config.max_candidates_per_gen);
        }
        if raw.is_empty() {
            break; // converged: no new candidates
        }

        // 2. Score every candidate (zero-token, deterministic).
        let mut scored: Vec<Scored> = Vec::with_capacity(raw.len());
        for cand in raw {
            let s = score(harness, &cand, plan).await?;
            seen.insert(s.canonical.clone());
            candidates_evaluated += 1;
            scored.push(s);
        }

        // 3. Rank by fitness (desc), then parsimony (fewer nodes), then
        //    canonical form (asc) — a total, reproducible order.
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.node_count.cmp(&b.node_count))
                .then_with(|| a.canonical.cmp(&b.canonical))
        });

        // 4. Update the global best.
        if let Some(top) = scored.first() {
            if better(top, &best) {
                best = clone_scored(top);
            }
        }

        // 5. Seed the next generation from the best `beam_width` candidates.
        beam = scored
            .into_iter()
            .take(config.beam_width.max(1))
            .map(|s| s.workflow)
            .collect();

        // Early stop on a perfect score.
        if best.score >= 1.0 {
            break;
        }
    }

    Ok(SearchOutcome {
        initial_score,
        best_score: best.score,
        best_workflow: best.workflow,
        best_report: best.report,
        candidates_evaluated,
        generations_run,
    })
}

/// Strict betterness with the same total order used for ranking.
fn better(a: &Scored, b: &Scored) -> bool {
    if a.score != b.score {
        return a.score > b.score;
    }
    if a.node_count != b.node_count {
        return a.node_count < b.node_count;
    }
    a.canonical < b.canonical
}

async fn score(
    harness: &VerificationHarness,
    wf: &Workflow,
    plan: &VerificationPlan,
) -> Result<Scored, VerifyError> {
    let report = verify(harness, wf, plan).await?;
    Ok(Scored {
        score: report.score(),
        node_count: wf.nodes.len(),
        canonical: canonical_of(wf),
        report,
        workflow: wf.clone(),
    })
}

fn clone_scored(s: &Scored) -> Scored {
    Scored {
        workflow: s.workflow.clone(),
        report: s.report.clone(),
        score: s.score,
        node_count: s.node_count,
        canonical: s.canonical.clone(),
    }
}

/// Canonical string form of a workflow for dedup/ordering. Node and connection
/// order is normalized so two structurally-identical workflows hash the same.
fn canonical_of(wf: &Workflow) -> String {
    let mut nodes: Vec<String> = wf
        .nodes
        .iter()
        .map(|n| serde_json::to_string(n).unwrap_or_default())
        .collect();
    nodes.sort();
    let mut conns: Vec<String> = wf
        .connections
        .iter()
        .map(|c| format!("{}:{}->{}", c.from_node, c.from_port, c.to_node))
        .collect();
    conns.sort();
    format!("N[{}]C[{}]", nodes.join("|"), conns.join("|"))
}
