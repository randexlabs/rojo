//! Fork-owned integration points that are intentionally kept separate from
//! Rojo's upstream implementation.

use std::sync::Arc;

use memofs::Vfs;

use crate::transformer::TransformerPipeline;

mod require_by_string;

/// Builds the transformer pipeline used by the standard Rojo commands.
///
/// Personal transformers should be registered here or in modules called from
/// here. Keeping registration in this module limits future upstream merge
/// conflicts to this fork-owned boundary.
pub(crate) fn transformer_pipeline(vfs: Arc<Vfs>) -> TransformerPipeline {
    TransformerPipeline::new().with(require_by_string::RequireByStringTransformer::new(vfs))
}
