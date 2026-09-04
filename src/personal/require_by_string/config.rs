//! Configuration loading for Luau string-module resolution.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use memofs::{IoResultExt, Vfs};
use mlua::{Lua, Table, Value, VmState};
use serde::Deserialize;

const LUAURC_FILE_NAME: &str = ".luaurc";
const CONFIG_LUAU_FILE_NAME: &str = ".config.luau";

#[derive(Debug, Default, Deserialize)]
struct Luaurc {
    #[serde(default)]
    aliases: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub(super) struct AliasDefinition {
    pub(super) value: String,
    pub(super) base_directory: PathBuf,
    pub(super) source: PathBuf,
}

#[derive(Debug, Default)]
pub(super) struct AliasConfig {
    aliases: HashMap<String, AliasDefinition>,
    pub(super) relevant_paths: Vec<PathBuf>,
}

impl AliasConfig {
    pub(super) fn load(vfs: &Vfs, source_path: &Path) -> Result<Self> {
        let source_directory = source_directory(vfs, source_path)?;

        let mut directories = Vec::new();
        let mut directory = source_directory.as_path();
        loop {
            directories.push(directory.to_path_buf());
            let Some(parent) = directory.parent() else {
                break;
            };
            if parent == directory {
                break;
            }
            directory = parent;
        }

        let mut config = Self::default();
        for directory in directories.into_iter().rev() {
            let luaurc_path = directory.join(LUAURC_FILE_NAME);
            let config_luau_path = directory.join(CONFIG_LUAU_FILE_NAME);
            let has_luaurc = vfs.metadata(&luaurc_path).with_not_found()?.is_some();
            let has_config_luau = vfs.metadata(&config_luau_path).with_not_found()?.is_some();

            // Register both candidates even when one is absent. A later
            // creation, removal, or replacement of either file must invalidate
            // scripts that inherited configuration from this directory.
            config.relevant_paths.push(luaurc_path.clone());
            config.relevant_paths.push(config_luau_path.clone());

            if has_luaurc && has_config_luau {
                anyhow::bail!(
                    "both {} and {} exist in {}; Luau requires only one configuration file",
                    LUAURC_FILE_NAME,
                    CONFIG_LUAU_FILE_NAME,
                    directory.display()
                );
            }

            let (path, aliases) = if has_luaurc {
                let contents = vfs.read_to_string_lf_normalized(&luaurc_path)?;
                let parsed: Luaurc = crate::json::from_str_with_context(contents.as_str(), || {
                    luaurc_path.display().to_string()
                })?;
                (luaurc_path, parsed.aliases)
            } else if has_config_luau {
                let contents = vfs.read_to_string_lf_normalized(&config_luau_path)?;
                (
                    config_luau_path.clone(),
                    parse_config_luau(&config_luau_path, contents.as_str())?,
                )
            } else {
                continue;
            };

            for (name, value) in aliases {
                if !is_valid_alias_name(&name) {
                    anyhow::bail!("invalid Luau alias name `{name}` in {}", path.display());
                }

                config.aliases.insert(
                    name.to_ascii_lowercase(),
                    AliasDefinition {
                        value,
                        base_directory: directory.clone(),
                        source: path.clone(),
                    },
                );
            }
        }

        Ok(config)
    }

    pub(super) fn resolve(
        &self,
        alias_name: &str,
        suffix: &str,
        importer_directory: &Path,
    ) -> Result<PathBuf> {
        let mut stack = Vec::new();
        self.resolve_inner(alias_name, suffix, importer_directory, &mut stack)
    }

    fn resolve_inner(
        &self,
        alias_name: &str,
        suffix: &str,
        importer_directory: &Path,
        stack: &mut Vec<String>,
    ) -> Result<PathBuf> {
        let normalized_name = alias_name.to_ascii_lowercase();
        if stack.contains(&normalized_name) {
            let chain = stack
                .iter()
                .chain(std::iter::once(&normalized_name))
                .cloned()
                .collect::<Vec<_>>()
                .join(" -> ");
            anyhow::bail!("cyclic Luau alias resolution: {chain}");
        }

        let Some(alias) = self.aliases.get(&normalized_name) else {
            anyhow::bail!("Luau alias `@{alias_name}` is not declared");
        };

        stack.push(normalized_name);
        let result = if let Some((nested_alias, nested_suffix)) = split_alias(&alias.value) {
            if nested_alias.eq_ignore_ascii_case("self") {
                let mut path = importer_directory.to_path_buf();
                append_module_components(&mut path, nested_suffix);
                append_module_components(&mut path, suffix);
                path
            } else if nested_alias.eq_ignore_ascii_case("game") {
                anyhow::bail!(
                    "alias `{}` in {} cannot target native alias @game",
                    alias_name,
                    alias.source.display()
                );
            } else {
                let combined_suffix = join_suffixes(nested_suffix, suffix);
                self.resolve_inner(nested_alias, &combined_suffix, importer_directory, stack)?
            }
        } else {
            let mut path = alias_base_path(alias);
            append_module_components(&mut path, suffix);
            path
        };
        stack.pop();

        Ok(result)
    }
}

pub(super) fn source_directory(vfs: &Vfs, source_path: &Path) -> Result<PathBuf> {
    if vfs
        .metadata(source_path)
        .with_not_found()?
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
    {
        Ok(source_path.to_path_buf())
    } else {
        Ok(source_path
            .parent()
            .context("source path had no parent directory")?
            .to_path_buf())
    }
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn alias_base_path(alias: &AliasDefinition) -> PathBuf {
    let home_suffix = alias
        .value
        .strip_prefix("~/")
        .or_else(|| alias.value.strip_prefix("~\\"));
    if let Some(value) = home_suffix {
        if let Some(mut path) = home_directory() {
            append_module_components(&mut path, value);
            return path;
        }
    }

    if Path::new(&alias.value).is_absolute() {
        PathBuf::from(&alias.value)
    } else {
        let mut path = alias.base_directory.clone();
        append_module_components(&mut path, &alias.value);
        path
    }
}

fn parse_config_luau(path: &Path, contents: &str) -> Result<BTreeMap<String, String>> {
    let lua = Lua::new();
    lua.sandbox(true)
        .map_err(|error| anyhow::anyhow!("{}: cannot sandbox Luau VM: {error}", path.display()))?;

    let deadline = Instant::now() + Duration::from_secs(2);
    lua.set_interrupt(move |_| {
        if Instant::now() >= deadline {
            Err(mlua::Error::RuntimeError(
                "configuration execution timed out".to_owned(),
            ))
        } else {
            Ok(VmState::Continue)
        }
    });

    let value: Value = lua
        .load(contents)
        .set_name(path.display().to_string())
        .eval()
        .map_err(|error| {
            anyhow::anyhow!(
                "{}: configuration execution failed: {error}",
                path.display()
            )
        })?;
    let Value::Table(root) = value else {
        anyhow::bail!(
            "{}: expected the configuration to return a table",
            path.display()
        );
    };
    let luau: Option<Table> = root
        .get("luau")
        .map_err(|error| anyhow::anyhow!("{}: invalid `luau` field: {error}", path.display()))?;
    let Some(luau) = luau else {
        return Ok(BTreeMap::new());
    };
    let aliases: Option<Table> = luau.get("aliases").map_err(|error| {
        anyhow::anyhow!("{}: invalid `luau.aliases` field: {error}", path.display())
    })?;
    let Some(aliases) = aliases else {
        return Ok(BTreeMap::new());
    };

    let mut result = BTreeMap::new();
    for pair in aliases.pairs::<String, String>() {
        let (name, value) = pair.map_err(|error| {
            anyhow::anyhow!(
                "{}: `luau.aliases` must contain string keys and values: {error}",
                path.display()
            )
        })?;
        result.insert(name, value);
    }

    Ok(result)
}

fn is_valid_alias_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".-_".contains(character))
}

pub(super) fn split_alias(value: &str) -> Option<(&str, &str)> {
    let value = value.strip_prefix('@')?;
    let (name, suffix) = match value.find(['/', '\\']) {
        Some(separator) => (&value[..separator], &value[separator + 1..]),
        None => (value, ""),
    };
    Some((name, suffix))
}

pub(super) fn join_suffixes(first: &str, second: &str) -> String {
    match (first.is_empty(), second.is_empty()) {
        (true, true) => String::new(),
        (true, false) => second.to_owned(),
        (false, true) => first.to_owned(),
        (false, false) => format!("{first}/{second}"),
    }
}

pub(super) fn append_module_components(path: &mut PathBuf, value: &str) {
    for component in value.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => {
                path.pop();
            }
            component => path.push(component),
        }
    }
}
