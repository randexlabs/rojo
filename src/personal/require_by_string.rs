//! Lossless Luau parsing primitives used by the require-by-string transformer.

use std::{collections::HashMap, ops::Range, path::PathBuf, sync::Arc};

use anyhow::{anyhow, Result};
use full_moon::{
    ast::{Call, Expression, FunctionArgs, FunctionCall, Prefix, Suffix},
    node::Node,
    tokenizer::{Lexer, LexerResult, StringLiteralQuoteType, Symbol, TokenReference, TokenType},
    visitors::Visitor,
};
use memofs::Vfs;
use rbx_dom_weak::{types::Variant, ustr};

mod config;
mod module_index;

use crate::{
    snapshot::InstanceSnapshot,
    transformer::{TransformChange, TransformContext, Transformer},
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RequireCall {
    pub(crate) request: String,
    pub(crate) source_range: Range<usize>,
    pub(crate) bare_string: bool,
}

/// Translates alias-based Luau require-by-string calls into Roblox instance
/// expressions while leaving Roblox-native path prefixes untouched.
pub(crate) struct RequireByStringTransformer {
    vfs: Arc<Vfs>,
    module_index: Option<module_index::ModuleIndex>,
    alias_configs: HashMap<PathBuf, config::AliasConfig>,
}

impl RequireByStringTransformer {
    pub(crate) fn new(vfs: Arc<Vfs>) -> Self {
        Self {
            vfs,
            module_index: None,
            alias_configs: HashMap::new(),
        }
    }
}

impl Transformer for RequireByStringTransformer {
    fn transform(
        &mut self,
        snapshot: InstanceSnapshot,
        context: &TransformContext<'_>,
    ) -> Result<InstanceSnapshot> {
        if self.module_index.is_none() {
            let root = context.root.unwrap_or(&snapshot);
            self.module_index = Some(module_index::ModuleIndex::from_snapshot(root, &self.vfs)?);
        }

        let module_index = self
            .module_index
            .as_mut()
            .expect("require-by-string module index was not initialized");
        let instance_parent_path = context
            .instance_path
            .as_deref()
            .map(|path| path[..path.len().saturating_sub(1)].to_vec())
            .unwrap_or_default();
        module_index.replace_snapshot(&snapshot, &self.vfs, context.instance_path.as_deref())?;
        self.transform_snapshot(snapshot, instance_parent_path)
    }

    fn handle_change(&mut self, change: &TransformChange) {
        let changed_path = change.path();
        self.alias_configs.retain(|_, config| {
            !config
                .relevant_paths
                .iter()
                .any(|path| path == changed_path || path.starts_with(changed_path))
        });

        if change.is_removed() {
            if let Some(module_index) = &mut self.module_index {
                module_index.remove_source_subtree(changed_path);
            }
        }
    }

    fn remove_instance(&mut self, context: &TransformContext<'_>) {
        if let (Some(module_index), Some(instance_path)) =
            (&mut self.module_index, context.instance_path.as_deref())
        {
            module_index.remove_instance_subtree(instance_path);
        }
    }
}

impl RequireByStringTransformer {
    fn transform_snapshot(
        &mut self,
        snapshot: InstanceSnapshot,
        instance_path: Vec<String>,
    ) -> Result<InstanceSnapshot> {
        enum Task {
            Visit(InstanceSnapshot, Vec<String>),
            Assemble(InstanceSnapshot, usize),
        }

        let mut tasks = vec![Task::Visit(snapshot, instance_path)];
        let mut results = Vec::new();

        while let Some(task) = tasks.pop() {
            match task {
                Task::Visit(mut snapshot, mut instance_path) => {
                    instance_path.push(snapshot.name.to_string());
                    snapshot = self.transform_source(snapshot, &instance_path)?;

                    let children = std::mem::take(&mut snapshot.children);
                    let child_count = children.len();
                    tasks.push(Task::Assemble(snapshot, child_count));
                    for child in children.into_iter().rev() {
                        tasks.push(Task::Visit(child, instance_path.clone()));
                    }
                }
                Task::Assemble(mut snapshot, child_count) => {
                    let children_start = results.len() - child_count;
                    snapshot.children = results.split_off(children_start);
                    results.push(snapshot);
                }
            }
        }

        Ok(results
            .pop()
            .expect("transformer did not produce a snapshot"))
    }

    fn transform_source(
        &mut self,
        mut snapshot: InstanceSnapshot,
        instance_path: &[String],
    ) -> Result<InstanceSnapshot> {
        let source = match snapshot.properties.get(&ustr("Source")) {
            Some(Variant::String(source)) => source.as_str().to_owned(),
            _ => return Ok(snapshot),
        };
        let Some(source_path) = module_index::source_path(&snapshot) else {
            return Ok(snapshot);
        };
        if module_index::luau_module_path(&self.vfs, &source_path)?.is_none() {
            return Ok(snapshot);
        }
        if !source.contains("require") || !has_static_require_candidate(&source)? {
            return Ok(snapshot);
        }

        let calls = find_require_calls(&source)?;
        if calls.is_empty() {
            return Ok(snapshot);
        }

        let needs_alias_config = calls
            .iter()
            .any(|call| call.request.starts_with('@') && !is_reserved_request(&call.request));
        if !needs_alias_config {
            return Ok(snapshot);
        }

        let importer_instance_path = instance_path;

        let directory = config::source_directory(&self.vfs, &source_path)?;
        let alias_config = match self.alias_configs.entry(directory.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(config::AliasConfig::load(&self.vfs, &source_path)?)
            }
        };

        let module_index = self
            .module_index
            .as_ref()
            .expect("require-by-string module index was not initialized");

        let mut edits = Vec::new();
        let mut relevant_paths = Vec::new();

        for call in calls {
            let Some(module_path) = resolve_request(alias_config, &call.request, &directory)?
            else {
                continue;
            };

            let Some(target) =
                module_index.target(&module_path, &self.vfs, importer_instance_path)?
            else {
                anyhow::bail!(
                    "cannot resolve require(\"{}\") from {}: module path {} is not present in the Rojo tree",
                    call.request,
                    source_path.display(),
                    module_path.display()
                );
            };

            relevant_paths.push(target.source_path.clone());
            relevant_paths.extend(target.relevant_paths.clone());

            let replacement = module_index::format_relative_instance_path(
                importer_instance_path,
                &target.instance_path,
            )?;

            edits.push((
                call.source_range,
                if call.bare_string {
                    format!("({replacement})")
                } else {
                    replacement
                },
            ));
        }

        if edits.is_empty() {
            return Ok(snapshot);
        }

        let mut transformed_source = source;
        edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        for (range, replacement) in edits {
            transformed_source.replace_range(range, &replacement);
        }

        snapshot
            .properties
            .insert(ustr("Source"), transformed_source.into());

        snapshot.metadata.relevant_paths.extend(relevant_paths);
        snapshot
            .metadata
            .relevant_paths
            .extend(alias_config.relevant_paths.iter().cloned());
        snapshot.metadata.relevant_paths.sort();
        snapshot.metadata.relevant_paths.dedup();

        Ok(snapshot)
    }
}

fn resolve_request(
    alias_config: &config::AliasConfig,
    request: &str,
    importer_directory: &std::path::Path,
) -> Result<Option<PathBuf>> {
    let Some((alias_name, suffix)) = config::split_alias(request) else {
        return Ok(None);
    };

    if alias_name.is_empty() || is_reserved_alias(alias_name) {
        return Ok(None);
    }

    let path = alias_config.resolve(alias_name, suffix, importer_directory)?;
    Ok(Some(path))
}

fn is_reserved_request(request: &str) -> bool {
    let Some((alias_name, _)) = config::split_alias(request) else {
        return false;
    };
    is_reserved_alias(alias_name)
}

fn is_reserved_alias(alias_name: &str) -> bool {
    alias_name.eq_ignore_ascii_case("self") || alias_name.eq_ignore_ascii_case("game")
}

fn has_static_require_candidate(source: &str) -> Result<bool> {
    let tokens = match Lexer::new(source, full_moon::LuaVersion::luau()).collect() {
        LexerResult::Ok(tokens) | LexerResult::Recovered(tokens, _) => tokens,
        LexerResult::Fatal(errors) => {
            anyhow::bail!("failed to tokenize Luau source: {errors:?}");
        }
    };
    let tokens = tokens
        .into_iter()
        .filter(|token| !token.token_type().is_trivia())
        .collect::<Vec<_>>();

    for (index, token) in tokens.iter().enumerate() {
        if !matches!(
            token.token_type(),
            TokenType::Identifier { identifier } if identifier.as_str() == "require"
        ) {
            continue;
        }

        let mut next_index = index + 1;
        while matches!(
            tokens.get(next_index).map(|token| token.token_type()),
            Some(TokenType::Symbol {
                symbol: Symbol::LeftParen
            })
        ) {
            next_index += 1;
        }
        if matches!(
            tokens.get(next_index).map(|token| token.token_type()),
            Some(TokenType::StringLiteral { .. })
        ) {
            return Ok(true);
        }
    }

    Ok(false)
}

#[derive(Default)]
struct RequireVisitor {
    calls: Vec<RequireCall>,
    errors: Vec<String>,
}

impl Visitor for RequireVisitor {
    fn visit_function_call(&mut self, function_call: &FunctionCall) {
        let Prefix::Name(name) = function_call.prefix() else {
            return;
        };

        let TokenType::Identifier { identifier } = name.token_type() else {
            return;
        };

        if identifier.as_str() != "require" {
            return;
        }

        let Some(Suffix::Call(Call::AnonymousCall(arguments))) = function_call.suffixes().next()
        else {
            return;
        };

        let (token, bare_string) = match arguments {
            FunctionArgs::String(token) => (token, true),
            FunctionArgs::Parentheses { arguments, .. } => {
                let Some(expression) = arguments.iter().next() else {
                    return;
                };

                let Some(token) = string_expression_token(expression) else {
                    return;
                };

                if arguments.iter().count() != 1 {
                    return;
                }

                (token, false)
            }
            FunctionArgs::TableConstructor(_) => return,
            _ => return,
        };

        let request = match string_literal_value(token) {
            Ok(request) => request,
            Err(error) => {
                self.errors.push(error.to_string());
                return;
            }
        };

        let Some(start) = token.start_position() else {
            return;
        };
        let Some(end) = token.end_position() else {
            return;
        };

        self.calls.push(RequireCall {
            request,
            source_range: start.bytes()..end.bytes(),
            bare_string,
        });
    }
}

pub(crate) fn find_require_calls(source: &str) -> Result<Vec<RequireCall>> {
    let ast = full_moon::parse_fallible(source, full_moon::LuaVersion::luau())
        .into_result()
        .map_err(|errors| anyhow!("failed to parse Luau source: {errors:?}"))?;

    let mut visitor = RequireVisitor::default();
    visitor.visit_ast(&ast);

    if !visitor.errors.is_empty() {
        return Err(anyhow!(visitor.errors.join("; ")));
    }

    Ok(visitor.calls)
}

pub(super) fn string_expression_token(expression: &Expression) -> Option<&TokenReference> {
    match expression {
        Expression::String(token) => Some(token),
        Expression::Parentheses { expression, .. } => string_expression_token(expression),
        _ => None,
    }
}

pub(super) fn string_literal_value(token: &TokenReference) -> Result<String> {
    let TokenType::StringLiteral {
        literal,
        quote_type,
        ..
    } = token.token_type()
    else {
        anyhow::bail!("expected a string literal token");
    };

    let value = match quote_type {
        StringLiteralQuoteType::Brackets => decode_long_string(literal.as_str()),
        StringLiteralQuoteType::Double | StringLiteralQuoteType::Single => {
            decode_lua_string(literal.as_str())?
        }
        _ => anyhow::bail!("unsupported string literal quote type"),
    };

    Ok(value)
}

fn decode_long_string(value: &str) -> String {
    let value = value
        .strip_prefix("\r\n")
        .or_else(|| value.strip_prefix("\n\r"))
        .or_else(|| value.strip_prefix('\r'))
        .or_else(|| value.strip_prefix('\n'))
        .unwrap_or(value);
    let mut characters = value.chars().peekable();

    let mut decoded = String::with_capacity(value.len());
    while let Some(character) = characters.next() {
        if character == '\r' {
            if characters.peek() == Some(&'\n') {
                characters.next();
            }
            decoded.push('\n');
        } else if character == '\n' {
            if characters.peek() == Some(&'\r') {
                characters.next();
            }
            decoded.push('\n');
        } else {
            decoded.push(character);
        }
    }

    decoded
}

fn decode_lua_string(value: &str) -> Result<String> {
    let mut decoded = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();

    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }

        let Some(escape) = characters.next() else {
            anyhow::bail!("string literal ended with an escape character")
        };

        match escape {
            'a' => decoded.push('\u{0007}'),
            'b' => decoded.push('\u{0008}'),
            'f' => decoded.push('\u{000c}'),
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            't' => decoded.push('\t'),
            'v' => decoded.push('\u{000b}'),
            '\\' => decoded.push('\\'),
            '"' => decoded.push('"'),
            '\'' => decoded.push('\''),
            'z' => {
                while matches!(characters.peek(), Some(character) if character.is_whitespace()) {
                    characters.next();
                }
            }
            '\n' => decoded.push('\n'),
            '\r' => {
                decoded.push('\n');
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
            }
            'x' => {
                let first = characters.next();
                let second = characters.next();
                let Some(first) = first.and_then(|character| character.to_digit(16)) else {
                    anyhow::bail!("invalid hexadecimal escape in string literal")
                };
                let Some(second) = second.and_then(|character| character.to_digit(16)) else {
                    anyhow::bail!("invalid hexadecimal escape in string literal")
                };
                decoded.push(char::from_u32(first * 16 + second).unwrap());
            }
            digit if digit.is_ascii_digit() => {
                let mut number = digit.to_digit(10).unwrap();
                for _ in 0..2 {
                    let Some(next) = characters.peek().copied() else {
                        break;
                    };
                    let Some(next) = next.to_digit(10) else {
                        break;
                    };
                    characters.next();
                    number = number * 10 + next;
                }
                let character = char::from_u32(number)
                    .ok_or_else(|| anyhow!("invalid decimal escape in string literal"))?;
                if number > 255 {
                    anyhow::bail!("invalid decimal escape in string literal");
                }
                decoded.push(character);
            }
            'u' => {
                if characters.next() != Some('{') {
                    anyhow::bail!("invalid unicode escape in string literal");
                }

                let mut number = 0;
                let mut digits = 0;
                loop {
                    let Some(character) = characters.next() else {
                        anyhow::bail!("unterminated unicode escape in string literal");
                    };
                    if character == '}' {
                        break;
                    }
                    let Some(value) = character.to_digit(16) else {
                        anyhow::bail!("invalid unicode escape in string literal");
                    };
                    digits += 1;
                    if digits > 6 {
                        anyhow::bail!("unicode escape is too long in string literal");
                    }
                    number = number * 16 + value;
                }

                if digits == 0 {
                    anyhow::bail!("empty unicode escape in string literal");
                }
                let character = char::from_u32(number)
                    .ok_or_else(|| anyhow!("invalid unicode escape in string literal"))?;
                decoded.push(character);
            }
            other => decoded.push(other),
        }
    }

    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use memofs::{InMemoryFs, Vfs, VfsSnapshot};
    use rbx_dom_weak::types::Variant;

    use crate::{
        snapshot::{InstanceMetadata, InstanceSnapshot},
        transformer::TransformContext,
    };

    use super::*;

    fn source_instance(path: &str, name: &str, class_name: &str, source: &str) -> InstanceSnapshot {
        InstanceSnapshot::new()
            .name(name)
            .class_name(class_name)
            .property("Source", source)
            .metadata(
                InstanceMetadata::new()
                    .instigating_source(PathBuf::from(path))
                    .relevant_paths(vec![PathBuf::from(path)]),
            )
    }

    fn project_vfs(files: impl IntoIterator<Item = (&'static str, VfsSnapshot)>) -> Arc<Vfs> {
        let mut filesystem = InMemoryFs::new();
        filesystem
            .load_snapshot("/project", VfsSnapshot::dir(files))
            .unwrap();
        Arc::new(Vfs::new(filesystem))
    }

    fn module_context<'a>(root: &'a InstanceSnapshot, name: &str) -> TransformContext<'a> {
        TransformContext::with_instance_path(root, vec!["Project".to_owned(), name.to_owned()])
    }

    #[test]
    fn finds_parenthesized_string_requires() {
        let calls =
            find_require_calls("local one = require(\"@shared/one\")\nlocal two = require('two')")
                .unwrap();

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].request, "@shared/one");
        assert!(!calls[0].bare_string);
        assert_eq!(calls[1].request, "two");
        assert!(!calls[1].bare_string);
    }

    #[test]
    fn finds_bare_string_requires() {
        let source = "local module = require \"./module\"";
        let calls = find_require_calls(source).unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].request, "./module");
        assert!(calls[0].bare_string);
        assert_eq!(&source[calls[0].source_range.clone()], "\"./module\"");
    }

    #[test]
    fn finds_bracket_string_requires() {
        let source = "local module = require [[\n@shared/module]]";
        let calls = find_require_calls(source).unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].request, "@shared/module");
        assert!(calls[0].bare_string);
    }

    #[test]
    fn ignores_non_static_or_multi_argument_requires() {
        let calls =
            find_require_calls(
                "require(moduleName)\nrequire(\"one\", \"two\")\nrequire({})\nrequire.foo(\"not a require\")",
            )
            .unwrap();

        assert!(calls.is_empty());
    }

    #[test]
    fn preserves_source_offsets_after_comments_and_unicode() {
        let source = "-- comentário\nlocal module = require(\"./module\")";
        let calls = find_require_calls(source).unwrap();

        assert_eq!(&source[calls[0].source_range.clone()], "\"./module\"");
    }

    #[test]
    fn leaves_native_require_paths_unchanged() {
        let vfs = project_vfs([
            (
                "main.luau",
                VfsSnapshot::file("local dep = require(\"./dep\")"),
            ),
            ("dep.luau", VfsSnapshot::file("return {}")),
        ]);
        let main = source_instance(
            "/project/main.luau",
            "main",
            "ModuleScript",
            "local dep = require(\"./dep\")",
        );
        let root = InstanceSnapshot::new()
            .name("Project")
            .class_name("Folder")
            .children(vec![
                main.clone(),
                source_instance("/project/dep.luau", "dep", "ModuleScript", "return {}"),
            ]);
        let mut transformer = RequireByStringTransformer::new(vfs);

        let transformed = transformer
            .transform(main, &module_context(&root, "main"))
            .unwrap();

        let Variant::String(source) = transformed.properties.get(&ustr("Source")).unwrap() else {
            panic!("expected Source to be a string");
        };
        assert_eq!(source.as_str(), "local dep = require(\"./dep\")");
    }

    #[test]
    fn transforms_luaurc_aliases() {
        let vfs = project_vfs([
            (
                ".luaurc",
                VfsSnapshot::file(r#"{ "aliases": { "src": "./src" } }"#),
            ),
            (
                "main.luau",
                VfsSnapshot::file("return require(\"@SRC/dep\")"),
            ),
            (
                "src",
                VfsSnapshot::dir([("dep.luau", VfsSnapshot::file("return {}"))]),
            ),
        ]);
        let main = source_instance(
            "/project/main.luau",
            "main",
            "ModuleScript",
            "return require(\"@SRC/dep\")",
        );
        let root = InstanceSnapshot::new()
            .name("Project")
            .class_name("Folder")
            .children(vec![
                main.clone(),
                InstanceSnapshot::new()
                    .name("src")
                    .class_name("Folder")
                    .children(vec![source_instance(
                        "/project/src/dep.luau",
                        "dep",
                        "ModuleScript",
                        "return {}",
                    )]),
            ]);
        let mut transformer = RequireByStringTransformer::new(vfs);

        let transformed = transformer
            .transform(main, &module_context(&root, "main"))
            .unwrap();
        let Variant::String(source) = transformed.properties.get(&ustr("Source")).unwrap() else {
            panic!("expected Source to be a string");
        };
        assert_eq!(
            source.as_str(),
            "return require(script.Parent:WaitForChild(\"src\"):WaitForChild(\"dep\"))"
        );
    }

    #[test]
    fn keeps_transformed_require_inline_and_preserves_source_lines() {
        let vfs = project_vfs([
            (
                ".luaurc",
                VfsSnapshot::file(r#"{ "aliases": { "src": "./src" } }"#),
            ),
            (
                "main.luau",
                VfsSnapshot::file(
                    "local before = true\nlocal dep = require(\"@src/dep\") -- keep this line\nreturn dep",
                ),
            ),
            (
                "src",
                VfsSnapshot::dir([("dep.luau", VfsSnapshot::file("return {}"))]),
            ),
        ]);
        let main = source_instance(
            "/project/main.luau",
            "main",
            "ModuleScript",
            "local before = true\nlocal dep = require(\"@src/dep\") -- keep this line\nreturn dep",
        );
        let root = InstanceSnapshot::new()
            .name("Project")
            .class_name("Folder")
            .children(vec![
                main.clone(),
                InstanceSnapshot::new()
                    .name("src")
                    .class_name("Folder")
                    .children(vec![source_instance(
                        "/project/src/dep.luau",
                        "dep",
                        "ModuleScript",
                        "return {}",
                    )]),
            ]);

        let transformed = RequireByStringTransformer::new(vfs)
            .transform(main, &module_context(&root, "main"))
            .unwrap();
        let Variant::String(source) = transformed.properties.get(&ustr("Source")).unwrap() else {
            panic!("expected Source to be a string");
        };

        assert_eq!(source.lines().count(), 3);
        assert_eq!(
            source.as_str(),
            "local before = true\nlocal dep = require(script.Parent:WaitForChild(\"src\"):WaitForChild(\"dep\")) -- keep this line\nreturn dep"
        );
    }

    #[test]
    fn follows_luau_file_extension_precedence() {
        let vfs = project_vfs([
            (
                ".luaurc",
                VfsSnapshot::file(
                    r#"{ "aliases": { "preferred": "./dep.v1", "legacy": "./dep.v1.lua" } }"#,
                ),
            ),
            (
                "main.luau",
                VfsSnapshot::file("return { require(\"@preferred\"), require(\"@legacy\") }"),
            ),
            ("dep.v1.luau", VfsSnapshot::file("return {}")),
            ("dep.v1.lua", VfsSnapshot::file("return {}")),
        ]);
        let main = source_instance(
            "/project/main.luau",
            "main",
            "ModuleScript",
            "return { require(\"@preferred\"), require(\"@legacy\") }",
        );
        let root = InstanceSnapshot::new()
            .name("Project")
            .class_name("Folder")
            .children(vec![
                main.clone(),
                source_instance(
                    "/project/dep.v1.luau",
                    "Preferred",
                    "ModuleScript",
                    "return {}",
                ),
                source_instance("/project/dep.v1.lua", "Legacy", "ModuleScript", "return {}"),
            ]);
        let mut transformer = RequireByStringTransformer::new(vfs);

        let transformed = transformer
            .transform(main, &module_context(&root, "main"))
            .unwrap();
        let Variant::String(source) = transformed.properties.get(&ustr("Source")).unwrap() else {
            panic!("expected Source to be a string");
        };
        assert_eq!(
            source.as_str(),
            "return { require(script.Parent:WaitForChild(\"Preferred\")), require(script.Parent:WaitForChild(\"Legacy\")) }"
        );
    }

    #[test]
    fn resolves_aliases_chained_to_self_from_the_importer_directory() {
        let vfs = project_vfs([
            (
                ".luaurc",
                VfsSnapshot::file(r#"{ "aliases": { "local": "@self" } }"#),
            ),
            (
                "main.luau",
                VfsSnapshot::file("return require(\"@local/dep\")"),
            ),
            ("dep.luau", VfsSnapshot::file("return {}")),
        ]);
        let main = source_instance(
            "/project/main.luau",
            "main",
            "ModuleScript",
            "return require(\"@local/dep\")",
        );
        let root = InstanceSnapshot::new()
            .name("Project")
            .class_name("Folder")
            .children(vec![
                main.clone(),
                source_instance("/project/dep.luau", "dep", "ModuleScript", "return {}"),
            ]);
        let mut transformer = RequireByStringTransformer::new(vfs);

        let transformed = transformer
            .transform(main, &module_context(&root, "main"))
            .unwrap();
        let Variant::String(source) = transformed.properties.get(&ustr("Source")).unwrap() else {
            panic!("expected Source to be a string");
        };
        assert_eq!(
            source.as_str(),
            "return require(script.Parent:WaitForChild(\"dep\"))"
        );
    }

    #[test]
    fn resolves_duplicate_mounts_to_the_nearest_module_instance() {
        let vfs = project_vfs([
            (
                ".luaurc",
                VfsSnapshot::file(r#"{ "aliases": { "dep": "./dep" } }"#),
            ),
            ("main.luau", VfsSnapshot::file("return require(\"@dep\")")),
            ("dep.luau", VfsSnapshot::file("return {}")),
        ]);
        let main = source_instance(
            "/project/main.luau",
            "main",
            "ModuleScript",
            "return require(\"@dep\")",
        );
        let dep = source_instance("/project/dep.luau", "dep", "ModuleScript", "return {}");
        let mount = |name: &str| {
            InstanceSnapshot::new()
                .name(name)
                .class_name("Folder")
                .children(vec![main.clone(), dep.clone()])
        };
        let root = InstanceSnapshot::new()
            .name("Project")
            .class_name("Folder")
            .children(vec![mount("First"), mount("Second")]);
        let context_root = root.clone();
        let transformed = RequireByStringTransformer::new(vfs)
            .transform(root, &TransformContext::new(&context_root))
            .unwrap();

        for mount in &transformed.children {
            let main = &mount.children[0];
            let Variant::String(source) = main.properties.get(&ustr("Source")).unwrap() else {
                panic!("expected Source to be a string");
            };
            assert_eq!(
                source.as_str(),
                "return require(script.Parent:WaitForChild(\"dep\"))"
            );
        }
    }

    #[test]
    fn rejects_modules_removed_from_an_incremental_snapshot() {
        let vfs = project_vfs([
            (
                ".luaurc",
                VfsSnapshot::file(r#"{ "aliases": { "dep": "./dep" } }"#),
            ),
            ("main.luau", VfsSnapshot::file("return require(\"@dep\")")),
            ("dep.luau", VfsSnapshot::file("return {}")),
        ]);
        let main = source_instance(
            "/project/main.luau",
            "main",
            "ModuleScript",
            "return require(\"@dep\")",
        );
        let dep = source_instance("/project/dep.luau", "dep", "ModuleScript", "return {}");
        let old_root = InstanceSnapshot::new()
            .name("Project")
            .class_name("Folder")
            .children(vec![main.clone(), dep]);
        let new_root = InstanceSnapshot::new()
            .name("Project")
            .class_name("Folder")
            .children(vec![main]);
        let mut transformer = RequireByStringTransformer::new(vfs);
        transformer
            .transform(old_root.clone(), &TransformContext::new(&old_root))
            .unwrap();

        let context = TransformContext::with_optional_root(None, vec!["Project".to_owned()]);
        let error = transformer.transform(new_root, &context).unwrap_err();

        assert!(error
            .to_string()
            .contains("is not present in the Rojo tree"));
    }

    #[test]
    fn transforms_dynamic_config_luau_aliases() {
        let vfs = project_vfs([
            (
                ".config.luau",
                VfsSnapshot::file(
                    "local aliases = {}\nfor _, name in ipairs({\"source\"}) do\n    aliases[name] = \"./source\"\nend\nreturn { luau = { aliases = aliases } }",
                ),
            ),
            (
                "main.luau",
                VfsSnapshot::file("return require(\"@source/dep\")"),
            ),
            (
                "source",
                VfsSnapshot::dir([("dep.luau", VfsSnapshot::file("return {}"))]),
            ),
        ]);
        let main = source_instance(
            "/project/main.luau",
            "main",
            "ModuleScript",
            "return require(\"@source/dep\")",
        );
        let root = InstanceSnapshot::new()
            .name("Project")
            .class_name("Folder")
            .children(vec![
                main.clone(),
                InstanceSnapshot::new()
                    .name("source")
                    .class_name("Folder")
                    .children(vec![source_instance(
                        "/project/source/dep.luau",
                        "dep",
                        "ModuleScript",
                        "return {}",
                    )]),
            ]);
        let mut transformer = RequireByStringTransformer::new(vfs);

        let transformed = transformer
            .transform(main, &module_context(&root, "main"))
            .unwrap();
        let Variant::String(source) = transformed.properties.get(&ustr("Source")).unwrap() else {
            panic!("expected Source to be a string");
        };
        assert_eq!(
            source.as_str(),
            "return require(script.Parent:WaitForChild(\"source\"):WaitForChild(\"dep\"))"
        );
    }

    #[test]
    fn leaves_native_require_paths_unchanged_in_nested_init_modules() {
        let vfs = project_vfs([
            (
                "foo.luau",
                VfsSnapshot::file("return {}"),
            ),
            (
                "package",
                VfsSnapshot::dir([
                    (
                        "init.luau",
                        VfsSnapshot::file(
                            "local outer = require(\"./foo\")\nlocal parent = require(\"../foo\")\nlocal inner = require(\"@self/foo\")\nlocal game = require(\"@game/foo\")",
                        ),
                    ),
                    ("foo.luau", VfsSnapshot::file("return {}")),
                ]),
            ),
        ]);
        let package = source_instance(
            "/project/package",
            "package",
            "ModuleScript",
            "local outer = require(\"./foo\")\nlocal parent = require(\"../foo\")\nlocal inner = require(\"@self/foo\")\nlocal game = require(\"@game/foo\")",
        )
        .children(vec![source_instance(
            "/project/package/foo.luau",
            "foo",
            "ModuleScript",
            "return {}",
        )]);
        let root = InstanceSnapshot::new()
            .name("Project")
            .class_name("Folder")
            .children(vec![
                source_instance("/project/foo.luau", "foo", "ModuleScript", "return {}"),
                package,
            ]);
        let mut transformer = RequireByStringTransformer::new(vfs);

        let context_root = root.clone();
        let transformed = transformer
            .transform(root, &TransformContext::new(&context_root))
            .unwrap();
        let package = transformed
            .children
            .iter()
            .find(|child| child.name == "package")
            .unwrap();
        let Variant::String(source) = package.properties.get(&ustr("Source")).unwrap() else {
            panic!("expected Source to be a string");
        };
        assert_eq!(
            source.as_str(),
            "local outer = require(\"./foo\")\nlocal parent = require(\"../foo\")\nlocal inner = require(\"@self/foo\")\nlocal game = require(\"@game/foo\")"
        );
    }
}
