//! The item model: the unit of data that flows between nodes, carrying a
//! **guaranteed lineage edge** back to whatever produced it.
//!
//! Data in A2W moves as an *item array* (`Vec<Item>`). Every item records its
//! [`ItemSource`], so a downstream consumer can always answer "which upstream
//! node, and which index within that node's output, produced this?". The engine
//! is responsible for stamping that lineage (see the engine's re-stamping step);
//! nodes simply return items and the engine overwrites their `source` with the
//! producing node's identity.

use serde::{Deserialize, Serialize};

/// Where an [`Item`] came from: the guaranteed lineage edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemSource {
    /// A root item seeded directly from the trigger input. These have no
    /// upstream producer node.
    Trigger,
    /// Produced by an upstream node. Identifies exactly which node and which
    /// zero-based index within that node's output array created this item.
    Produced {
        /// `id` of the node that produced this item.
        node_id: String,
        /// Zero-based position of this item within that node's output array.
        item_index: usize,
    },
}

/// A single unit of data flowing through the workflow.
///
/// `json` is the payload; `source` is the lineage edge identifying its origin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// The JSON payload.
    pub json: serde_json::Value,
    /// Lineage: which node (or the trigger) produced this item.
    pub source: ItemSource,
}

impl Item {
    /// Construct a root item seeded from the trigger input.
    #[must_use]
    pub fn root(json: serde_json::Value) -> Self {
        Self {
            json,
            source: ItemSource::Trigger,
        }
    }

    /// Construct an item produced by `node_id` at position `item_index`.
    #[must_use]
    pub fn produced(
        json: serde_json::Value,
        node_id: impl Into<String>,
        item_index: usize,
    ) -> Self {
        Self {
            json,
            source: ItemSource::Produced {
                node_id: node_id.into(),
                item_index,
            },
        }
    }
}
