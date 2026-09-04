//! Defines the extension point for transforming snapshots before they are
//! diffed and synchronized.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::snapshot::InstanceSnapshot;

/// Read-only project context made available while a snapshot is transformed.
///
/// The context intentionally exposes a snapshot instead of Rojo's live tree,
/// so transformers can build indexes without depending on synchronization
/// internals.
pub struct TransformContext<'a> {
    /// The project snapshot used as context, when a transformer requested a
    /// materialized project view. Incremental transformers should keep their
    /// own indexes and work without this allocation.
    pub root: Option<&'a InstanceSnapshot>,

    /// The path of the snapshot root within `root`, when transforming an
    /// incremental subtree. Paths include the snapshot root itself.
    pub instance_path: Option<Vec<String>>,
}

impl<'a> TransformContext<'a> {
    /// Creates a context from a project snapshot.
    pub fn new(root: &'a InstanceSnapshot) -> Self {
        Self {
            root: Some(root),
            instance_path: None,
        }
    }

    /// Creates a context for a snapshot located at `instance_path` in `root`.
    pub fn with_instance_path(root: &'a InstanceSnapshot, instance_path: Vec<String>) -> Self {
        Self {
            root: Some(root),
            instance_path: Some(instance_path),
        }
    }

    /// Creates a context for an incremental snapshot with an optional project
    /// view. The optional form lets the synchronization loop avoid cloning the
    /// entire tree when no transformer needs it.
    pub fn with_optional_root(
        root: Option<&'a InstanceSnapshot>,
        instance_path: Vec<String>,
    ) -> Self {
        Self {
            root,
            instance_path: Some(instance_path),
        }
    }

    /// Creates a context without a materialized project view.
    pub fn without_root() -> Self {
        Self {
            root: None,
            instance_path: None,
        }
    }
}

/// Describes a filesystem change observed by the transformer pipeline.
#[derive(Debug, Clone)]
pub enum TransformChange {
    Created(PathBuf),
    Written(PathBuf),
    Removed(PathBuf),
}

impl TransformChange {
    /// Returns the canonicalized path associated with this change.
    pub fn path(&self) -> &Path {
        match self {
            Self::Created(path) | Self::Written(path) | Self::Removed(path) => path,
        }
    }

    /// Returns whether the change removed a path from the VFS.
    pub fn is_removed(&self) -> bool {
        matches!(self, Self::Removed(_))
    }
}

/// A transformation applied to a fully materialized instance snapshot before
/// Rojo computes patches for it.
///
/// Transformers deliberately operate on snapshots instead of `RojoTree`,
/// `PatchSet`, or Roblox objects. This keeps them independent from the live
/// synchronization machinery and makes the same pipeline usable by builds and
/// live-sync sessions.
pub trait Transformer: Send {
    /// Transforms an instance snapshot with access to the current project
    /// context.
    fn transform(
        &mut self,
        snapshot: InstanceSnapshot,
        context: &TransformContext<'_>,
    ) -> Result<InstanceSnapshot>;

    /// Handles a VFS change before affected snapshots are transformed.
    fn handle_change(&mut self, _change: &TransformChange) {}

    /// Handles an instance removed from the Rojo tree.
    fn remove_instance(&mut self, _context: &TransformContext<'_>) {}

    /// Indicates whether incremental contexts must include a materialized
    /// project snapshot. Stateful transformers should keep this false.
    fn needs_project_snapshot(&self) -> bool {
        false
    }
}

/// An ordered collection of snapshot transformations.
#[derive(Default)]
pub struct TransformerPipeline {
    transformers: Vec<Box<dyn Transformer>>,
}

impl TransformerPipeline {
    /// Creates an empty pipeline that preserves Rojo's default behavior.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a transformer to the end of the pipeline.
    pub fn add<T>(&mut self, transformer: T)
    where
        T: Transformer + 'static,
    {
        self.transformers.push(Box::new(transformer));
    }

    /// Returns a pipeline with the given transformer appended.
    pub fn with<T>(mut self, transformer: T) -> Self
    where
        T: Transformer + 'static,
    {
        self.add(transformer);
        self
    }

    /// Applies every transformer in registration order.
    pub fn transform(&mut self, snapshot: InstanceSnapshot) -> Result<InstanceSnapshot> {
        if self.needs_project_snapshot() {
            let context_snapshot = snapshot.clone();
            self.transform_with_context(snapshot, &TransformContext::new(&context_snapshot))
        } else {
            self.transform_with_context(snapshot, &TransformContext::without_root())
        }
    }

    /// Applies every transformer in registration order with project context.
    /// The context is shared unchanged; ordering applies to the snapshot being
    /// transformed, not to the contextual view.
    pub fn transform_with_context(
        &mut self,
        mut snapshot: InstanceSnapshot,
        context: &TransformContext<'_>,
    ) -> Result<InstanceSnapshot> {
        for transformer in &mut self.transformers {
            snapshot = transformer.transform(snapshot, context)?;
        }

        Ok(snapshot)
    }

    /// Notifies every transformer about a filesystem change.
    pub fn handle_change(&mut self, change: &TransformChange) {
        for transformer in &mut self.transformers {
            transformer.handle_change(change);
        }
    }

    /// Notifies every transformer about an instance removed from the tree.
    pub fn remove_instance(&mut self, context: &TransformContext<'_>) {
        for transformer in &mut self.transformers {
            transformer.remove_instance(context);
        }
    }

    /// Returns whether at least one transformer requires a full project view
    /// during incremental updates.
    pub fn needs_project_snapshot(&self) -> bool {
        self.transformers
            .iter()
            .any(|transformer| transformer.needs_project_snapshot())
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use rbx_dom_weak::ustr;

    use super::*;

    struct RenameTransformer(&'static str);

    impl Transformer for RenameTransformer {
        fn transform(
            &mut self,
            mut snapshot: InstanceSnapshot,
            _context: &TransformContext<'_>,
        ) -> Result<InstanceSnapshot> {
            snapshot.name = Cow::Borrowed(self.0);
            Ok(snapshot)
        }
    }

    #[test]
    fn transforms_are_applied_in_registration_order() {
        let snapshot = InstanceSnapshot::new().class_name(ustr("Folder"));
        let mut pipeline = TransformerPipeline::new()
            .with(RenameTransformer("first"))
            .with(RenameTransformer("second"));

        let transformed = pipeline.transform(snapshot).unwrap();

        assert_eq!(transformed.name, "second");
    }

    #[test]
    fn empty_pipeline_preserves_snapshot() {
        let snapshot = InstanceSnapshot::new().class_name(ustr("Folder"));

        let mut pipeline = TransformerPipeline::new();
        let transformed = pipeline.transform(snapshot.clone()).unwrap();

        assert_eq!(transformed, snapshot);
    }
}
