//! Durable skill library (F4): the same promote/retrieve flow as
//! [`SkillLibrary`](crate::SkillLibrary), but backed by [`a2w_store::Store`] so
//! the generate → verify → promote → retrieve loop runs through the persisted
//! surface rather than only in memory.
//!
//! Promotion is gated on the **holdout** confidence report (the certified
//! evidence from the search's disjoint holdout, per F2/F3) and on M1 validity —
//! identical to the in-memory gate, reusing [`crate::library::vet_for_promotion`].

use a2w_ir::Workflow;
use a2w_store::{SkillRecord, Store};
use a2w_verify::{ConfidenceReport, Threshold};

use crate::library::{assemble_skill, vet_for_promotion};
use crate::signature::TaskSignature;
use crate::{EvidenceSnapshot, Skill, SkillError};

/// Errors from the persistent skill library.
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    /// A promotion-gate failure (validity / threshold / report mismatch).
    #[error(transparent)]
    Skill(#[from] SkillError),
    /// A storage-layer failure.
    #[error("store error: {0}")]
    Store(#[from] a2w_store::StoreError),
    /// A persisted skill row could not be deserialized back into a [`Skill`].
    #[error("corrupt skill row '{id}': {detail}")]
    Corrupt {
        /// The offending skill id.
        id: String,
        /// What failed.
        detail: String,
    },
}

/// A skill library persisted to an [`a2w_store::Store`].
///
/// Holds only a borrow of the store and the promotion threshold; all state
/// lives in the durable `skills` table (schema v6).
pub struct PersistentSkillLibrary<'a> {
    store: &'a Store,
    threshold: Threshold,
}

impl<'a> PersistentSkillLibrary<'a> {
    /// Build a persistent library over `store` with an explicit threshold.
    #[must_use]
    pub fn new(store: &'a Store, threshold: Threshold) -> Self {
        Self { store, threshold }
    }

    /// Build a persistent library with the default (strict) threshold.
    #[must_use]
    pub fn with_default_threshold(store: &'a Store) -> Self {
        Self::new(store, Threshold::default())
    }

    /// The promotion threshold in force.
    #[must_use]
    pub fn threshold(&self) -> &Threshold {
        &self.threshold
    }

    /// Promote a workflow into the durable library **iff** its (holdout)
    /// confidence report clears the threshold and the workflow is M1-valid.
    /// Returns the stable skill id (derived from the workflow fingerprint so a
    /// re-promotion upserts rather than duplicates).
    ///
    /// # Errors
    /// [`PersistError::Skill`] if the gate fails; [`PersistError::Store`] on a
    /// write failure.
    pub async fn promote(
        &self,
        query: &str,
        workflow: Workflow,
        observe_node: &str,
        report: &ConfidenceReport,
    ) -> Result<String, PersistError> {
        vet_for_promotion(&workflow, report, &self.threshold)?;
        let id = skill_id(&workflow);
        let skill = assemble_skill(id.clone(), query, workflow, observe_node, report);
        let rec = to_record(&skill)?;
        self.store.save_skill(&rec).await?;
        Ok(id)
    }

    /// Fetch a persisted skill by id.
    ///
    /// # Errors
    /// [`PersistError::Store`] on a read failure; [`PersistError::Corrupt`] if
    /// the stored row cannot be deserialized.
    pub async fn get(&self, id: &str) -> Result<Option<Skill>, PersistError> {
        match self.store.get_skill(id).await? {
            None => Ok(None),
            Some(rec) => Ok(Some(from_record(&rec)?)),
        }
    }

    /// Number of persisted skills.
    ///
    /// # Errors
    /// [`PersistError::Store`] on a read failure.
    pub async fn len(&self) -> Result<usize, PersistError> {
        Ok(self.store.list_skills().await?.len())
    }

    /// Whether the durable library is empty.
    ///
    /// # Errors
    /// [`PersistError::Store`] on a read failure.
    pub async fn is_empty(&self) -> Result<bool, PersistError> {
        Ok(self.len().await? == 0)
    }

    /// Retrieve the top-`k` persisted skills most similar to `query`, each
    /// paired with its similarity, sorted descending (ties broken by id for
    /// determinism). Loads all rows and ranks in memory.
    ///
    /// # Errors
    /// [`PersistError::Store`] on a read failure; [`PersistError::Corrupt`] if a
    /// stored row cannot be deserialized.
    pub async fn retrieve(&self, query: &str, k: usize) -> Result<Vec<(Skill, f64)>, PersistError> {
        let query_sig = TaskSignature::from_query(query);
        let mut scored: Vec<(Skill, f64)> = Vec::new();
        for rec in self.store.list_skills().await? {
            let skill = from_record(&rec)?;
            let sim = query_sig.similarity(&skill.signature);
            scored.push((skill, sim));
        }
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.id.cmp(&b.0.id))
        });
        scored.truncate(k);
        Ok(scored)
    }

    /// The single best match for `query`, if any skill is stored.
    ///
    /// # Errors
    /// As [`PersistentSkillLibrary::retrieve`].
    pub async fn best_match(&self, query: &str) -> Result<Option<(Skill, f64)>, PersistError> {
        Ok(self.retrieve(query, 1).await?.into_iter().next())
    }
}

/// A stable, content-derived skill id so re-promoting the same workflow upserts.
fn skill_id(wf: &Workflow) -> String {
    format!("skill_{}", a2w_store::workflow_fingerprint(wf))
}

/// Serialize a [`Skill`] into a durable [`SkillRecord`].
fn to_record(skill: &Skill) -> Result<SkillRecord, PersistError> {
    Ok(SkillRecord {
        id: skill.id.clone(),
        query: skill.query.clone(),
        observe_node: skill.observe_node.clone(),
        workflow_json: serde_json::to_string(&skill.workflow).map_err(|e| PersistError::Corrupt {
            id: skill.id.clone(),
            detail: format!("workflow serialize: {e}"),
        })?,
        signature_json: serde_json::to_string(&skill.signature).map_err(|e| {
            PersistError::Corrupt {
                id: skill.id.clone(),
                detail: format!("signature serialize: {e}"),
            }
        })?,
        evidence_json: serde_json::to_string(&skill.evidence).map_err(|e| PersistError::Corrupt {
            id: skill.id.clone(),
            detail: format!("evidence serialize: {e}"),
        })?,
        holdout_score: skill.evidence.score,
    })
}

/// Reconstruct a [`Skill`] from a durable [`SkillRecord`].
fn from_record(rec: &SkillRecord) -> Result<Skill, PersistError> {
    let workflow: Workflow =
        serde_json::from_str(&rec.workflow_json).map_err(|e| PersistError::Corrupt {
            id: rec.id.clone(),
            detail: format!("workflow deserialize: {e}"),
        })?;
    let signature: TaskSignature =
        serde_json::from_str(&rec.signature_json).map_err(|e| PersistError::Corrupt {
            id: rec.id.clone(),
            detail: format!("signature deserialize: {e}"),
        })?;
    let evidence: EvidenceSnapshot =
        serde_json::from_str(&rec.evidence_json).map_err(|e| PersistError::Corrupt {
            id: rec.id.clone(),
            detail: format!("evidence deserialize: {e}"),
        })?;
    Ok(Skill {
        id: rec.id.clone(),
        query: rec.query.clone(),
        workflow,
        observe_node: rec.observe_node.clone(),
        signature,
        evidence,
    })
}
