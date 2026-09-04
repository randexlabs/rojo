//! Maps Luau module paths to their Roblox instance paths.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::Result;
use memofs::{IoResultExt, Vfs};

use crate::snapshot::{InstanceSnapshot, InstigatingSource};

#[derive(Debug, Clone)]
pub(super) struct ModuleTarget {
    pub(super) source_path: PathBuf,
    pub(super) instance_path: Vec<String>,
    pub(super) relevant_paths: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub(super) struct ModuleIndex {
    modules: HashMap<PathBuf, Vec<ModuleTarget>>,
}

impl ModuleIndex {
    pub(super) fn from_snapshot(root: &InstanceSnapshot, vfs: &Vfs) -> Result<Self> {
        let mut index = Self::default();
        index.visit_snapshot(root, vfs, Vec::new())?;
        Ok(index)
    }

    pub(super) fn replace_snapshot(
        &mut self,
        snapshot: &InstanceSnapshot,
        vfs: &Vfs,
        instance_path: Option<&[String]>,
    ) -> Result<()> {
        let instance_parent_path = instance_path
            .map(|path| &path[..path.len().saturating_sub(1)])
            .unwrap_or_default();
        let removed_instance_path = instance_path.map_or_else(
            || {
                let mut path = instance_parent_path.to_vec();
                path.push(snapshot.name.to_string());
                path
            },
            ToOwned::to_owned,
        );
        self.remove_instance_subtree(&removed_instance_path);
        self.visit_snapshot(snapshot, vfs, instance_parent_path.to_vec())
    }

    pub(super) fn target(
        &self,
        module_path: &Path,
        vfs: &Vfs,
        importer_instance_path: &[String],
    ) -> Result<Option<&ModuleTarget>> {
        if matches!(
            module_path
                .extension()
                .and_then(|extension| extension.to_str()),
            Some("lua" | "luau")
        ) {
            return self.target_at_path(module_path, vfs, importer_instance_path);
        }

        let luau_path = path_with_extension(module_path, "luau");
        if let Some(target) = self.target_at_path(&luau_path, vfs, importer_instance_path)? {
            return Ok(Some(target));
        }

        let lua_path = path_with_extension(module_path, "lua");
        if let Some(target) = self.target_at_path(&lua_path, vfs, importer_instance_path)? {
            return Ok(Some(target));
        }

        self.target_at_path(module_path, vfs, importer_instance_path)
    }

    fn target_at_path(
        &self,
        module_path: &Path,
        vfs: &Vfs,
        importer_instance_path: &[String],
    ) -> Result<Option<&ModuleTarget>> {
        if let Some(target) = self.target_at(module_path, importer_instance_path) {
            return Ok(Some(target));
        }

        let Some(canonical_path) = vfs.canonicalize(module_path).with_not_found()? else {
            return Ok(None);
        };

        Ok(self.target_at(&canonical_path, importer_instance_path))
    }

    fn target_at(
        &self,
        module_path: &Path,
        importer_instance_path: &[String],
    ) -> Option<&ModuleTarget> {
        self.modules.get(module_path)?.iter().max_by_key(|target| {
            importer_instance_path
                .iter()
                .zip(&target.instance_path)
                .take_while(|(left, right)| left == right)
                .count()
        })
    }

    pub(super) fn visit_snapshot(
        &mut self,
        snapshot: &InstanceSnapshot,
        vfs: &Vfs,
        instance_path: Vec<String>,
    ) -> Result<()> {
        // The plugin tree and user projects can be deeply nested. An explicit
        // work stack avoids making project depth a limit on synchronization.
        let mut pending = vec![(snapshot, instance_path)];
        while let Some((snapshot, mut instance_path)) = pending.pop() {
            instance_path.push(snapshot.name.to_string());

            if let Some(source_path) = source_path(snapshot) {
                if snapshot.class_name.as_str() == "ModuleScript" {
                    if let Some(module_path) = luau_module_path(vfs, &source_path)? {
                        let target = ModuleTarget {
                            source_path,
                            instance_path: instance_path.clone(),
                            relevant_paths: snapshot.metadata.relevant_paths.clone(),
                        };

                        let targets = self.modules.entry(module_path.clone()).or_default();
                        if let Some(existing) = targets
                            .iter()
                            .find(|existing| existing.source_path != target.source_path)
                        {
                            anyhow::bail!(
                                "ambiguous Luau module path `{}`: {} and {}",
                                module_path.display(),
                                existing.source_path.display(),
                                target.source_path.display()
                            );
                        }
                        targets.push(target);
                    }
                }
            }

            for child in snapshot.children.iter().rev() {
                pending.push((child, instance_path.clone()));
            }
        }

        Ok(())
    }

    pub(super) fn remove_instance_subtree(&mut self, instance_path: &[String]) {
        self.modules.retain(|_, targets| {
            targets.retain(|target| !target.instance_path.starts_with(instance_path));
            !targets.is_empty()
        });
    }

    pub(super) fn remove_source_subtree(&mut self, source_path: &Path) {
        self.modules.retain(|_, targets| {
            targets.retain(|target| !target.source_path.starts_with(source_path));
            !targets.is_empty()
        });
    }
}

fn path_with_extension(path: &Path, extension: &str) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push(".");
    path.push(extension);
    path.into()
}

pub(super) fn luau_module_path(vfs: &Vfs, source_path: &Path) -> Result<Option<PathBuf>> {
    let is_directory = vfs
        .metadata(source_path)
        .with_not_found()?
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false);

    if is_directory {
        return Ok(Some(source_path.to_path_buf()));
    }

    Ok(matches!(
        source_path
            .extension()
            .and_then(|extension| extension.to_str()),
        Some("lua" | "luau")
    )
    .then(|| source_path.to_path_buf()))
}

pub(super) fn format_relative_instance_path(
    importer_instance_path: &[String],
    target_instance_path: &[String],
) -> Result<String> {
    let common_length = importer_instance_path
        .iter()
        .zip(target_instance_path)
        .take_while(|(left, right)| left == right)
        .count();

    if common_length == 0 {
        anyhow::bail!("Luau module paths do not share a Roblox instance root");
    }

    let mut expression = String::from("script");
    for _ in common_length..importer_instance_path.len() {
        expression.push_str(".Parent");
    }
    for component in &target_instance_path[common_length..] {
        append_instance_component(&mut expression, component);
    }

    Ok(expression)
}

pub(super) fn source_path(snapshot: &InstanceSnapshot) -> Option<PathBuf> {
    match snapshot.metadata.instigating_source.as_ref()? {
        InstigatingSource::Path(path) => Some(path.clone()),
        InstigatingSource::ProjectNode { path, node, .. } => {
            let path_node = node.path.as_ref()?;
            let source_path = path_node.path();

            Some(if source_path.is_absolute() {
                source_path.to_path_buf()
            } else {
                path.parent()?.join(source_path)
            })
        }
    }
}

fn append_instance_component(expression: &mut String, component: &str) {
    // Dot access is ambiguous in Roblox: an Instance property takes
    // precedence over a child with the same name. WaitForChild also handles
    // names that are not valid Luau identifiers or that are keywords, while
    // waiting for client-side replication to provide the child.
    expression.push_str(":WaitForChild(");
    expression.push_str(&serde_json::to_string(component).unwrap());
    expression.push(')');
}
