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
///
/// The search **selects** by fitness but is **certified** on the holdout.
/// `best_holdout_score` is the number a caller should trust and report;
/// `best_fitness_score` is what the search optimized against and must never be
/// presented as independent evidence of correctness (that would be Goodhart's
/// law: a measure that became a target stops measuring). `overfit_gap` makes
/// any divergence between the two explicit.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    /// Seed fitness score (selection metric).
    pub initial_fitness_score: f64,
    /// Seed holdout score (certification metric).
    pub initial_holdout_score: f64,
    /// Fitness score of the selected winner (what the search maximized).
    pub best_fitness_score: f64,
    /// **Certified** holdout score of the selected winner — the honest number.
    pub best_holdout_score: f64,
    /// `best_fitness_score - best_holdout_score`. `> 0` means the winner scored
    /// better on the metric it was optimized against than on the independent
    /// holdout — i.e. some of the gain was overfitting, not real correctness.
    pub overfit_gap: f64,
    /// The selected winner (the seed itself if nothing improved its fitness).
    pub best_workflow: Workflow,
    /// The **holdout** confidence report — the certification evidence. Pass
    /// THIS to M4 promotion (never the fitness report).
    pub best_holdout_report: ConfidenceReport,
    /// The fitness confidence report — selection evidence, for diagnostics only.
    pub best_fitness_report: ConfidenceReport,
    /// Total distinct candidates scored against the fitness plan.
    pub candidates_evaluated: usize,
    /// Generations actually run (may stop early on a perfect fitness score).
    pub generations_run: usize,
}

impl SearchOutcome {
    /// The number a caller should trust: the certified holdout score.
    #[must_use]
    pub fn certified_score(&self) -> f64 {
        self.best_holdout_score
    }

    /// Whether the search strictly improved the **certified** (holdout) score.
    #[must_use]
    pub fn improved_on_holdout(&self) -> bool {
        self.best_holdout_score > self.initial_holdout_score
    }

    /// Whether the search strictly improved the fitness (selection) score.
    #[must_use]
    pub fn improved_on_fitness(&self) -> bool {
        self.best_fitness_score > self.initial_fitness_score
    }

    /// Whether the winner overfit the fitness set (fitness gain not reflected
    /// in the holdout).
    #[must_use]
    pub fn overfit(&self) -> bool {
        self.overfit_gap > 1e-9
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

/// Run a deterministic beam search to improve `seed`, **selecting** by the
/// `fitness` plan and **certifying** the winner on a disjoint `holdout` plan.
///
/// This is the anti-Goodhart split (F2): the search optimizes the fitness
/// metric, so the fitness score is no longer independent evidence about the
/// winner. The winner is therefore re-scored on a holdout the fitness function
/// never saw during ranking, and the *holdout* score is the certified result.
///
/// Both plans' `observe_node`s must remain present in every candidate (the
/// supplied operators are expected to preserve them). Ranking is by fitness
/// (desc), then parsimony (fewer nodes), then canonical form (asc) — a total,
/// reproducible order. No RNG: reruns are byte-identical.
///
/// # Errors
/// [`VerifyError`] if the seed or the winner cannot be scored against either
/// plan (e.g. an observe node is absent, or the workflow is unrunnable).
pub async fn evolve(
    harness: &VerificationHarness,
    seed: &Workflow,
    fitness: &VerificationPlan,
    holdout: &VerificationPlan,
    operators: &[Box<dyn Mutation>],
    config: SearchConfig,
) -> Result<SearchOutcome, VerifyError> {
    // Seed baselines on BOTH plans (holdout for honest "did we improve?").
    let seed_fitness = score(harness, seed, fitness).await?;
    let initial_fitness_score = seed_fitness.score;
    let initial_holdout_score = verify(harness, seed, holdout).await?.score();

    let mut best = seed_fitness;
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
                    // BOTH observe nodes must survive so the winner is
                    // certifiable on the holdout.
                    if !cand.nodes.iter().any(|n| n.id == fitness.observe_node)
                        || !cand.nodes.iter().any(|n| n.id == holdout.observe_node)
                    {
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

        // 2. Score every candidate against the FITNESS plan only. The holdout
        //    is never consulted during ranking — that is the whole point.
        let mut scored: Vec<Scored> = Vec::with_capacity(raw.len());
        for cand in raw {
            let s = score(harness, &cand, fitness).await?;
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

        // 4. Update the global best (by fitness).
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

        // Early stop on a perfect FITNESS score.
        if best.score >= 1.0 {
            break;
        }
    }

    // Certify the fitness-winner on the holdout (the disjoint, never-optimized
    // evidence). This is the number we report.
    let best_fitness_score = best.score;
    let holdout_report = verify(harness, &best.workflow, holdout).await?;
    let best_holdout_score = holdout_report.score();

    Ok(SearchOutcome {
        initial_fitness_score,
        initial_holdout_score,
        best_fitness_score,
        best_holdout_score,
        overfit_gap: best_fitness_score - best_holdout_score,
        best_workflow: best.workflow,
        best_holdout_report: holdout_report,
        best_fitness_report: best.report,
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
