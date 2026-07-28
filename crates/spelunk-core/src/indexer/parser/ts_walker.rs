use super::super::chunker::{Chunk, ChunkKind, chunk_token_cap, sliding_window};
use crate::error::IndexError;
use crate::search::tokens::estimate_tokens;
use anyhow::Result;

pub(super) fn ts_language(name: &str) -> Result<tree_sitter::Language> {
    use ast_grep_language::{LanguageExt, SupportLang};

    // Most grammars are sourced from `ast-grep-language`, which exposes each
    // `SupportLang` variant's raw `tree_sitter::Language` via `get_ts_language()`
    // on the unified tree-sitter 0.26 runtime (single `tree_sitter::Language`
    // type — no duplicate-runtime bloat). `proto` and `sql` are not shipped by
    // ast-grep-language, so they stay on their standalone grammar crates, which
    // expose a `LANGUAGE: LanguageFn` constant convertible via `.into()`.
    let support = match name {
        "rust" => SupportLang::Rust,
        "python" => SupportLang::Python,
        "javascript" | "jsx" => SupportLang::JavaScript,
        "typescript" => SupportLang::TypeScript,
        "tsx" => SupportLang::Tsx,
        "go" => SupportLang::Go,
        "java" => SupportLang::Java,
        "c" => SupportLang::C,
        "cpp" => SupportLang::Cpp,
        "json" => SupportLang::Json,
        "html" => SupportLang::Html,
        "css" => SupportLang::Css,
        "hcl" => SupportLang::Hcl,
        "php" => SupportLang::Php,
        "ruby" => SupportLang::Ruby,
        "csharp" => SupportLang::CSharp,
        "kotlin" => SupportLang::Kotlin,
        "swift" => SupportLang::Swift,
        "sql" => return Ok(tree_sitter_sequel::LANGUAGE.into()),
        "proto" => return Ok(tree_sitter_proto::LANGUAGE.into()),
        other => return Err(IndexError::UnsupportedLanguage(other.to_string()).into()),
    };
    Ok(support.get_ts_language())
}

// ---------------------------------------------------------------------------
// Per-language semantic node configurations
// ---------------------------------------------------------------------------

/// Describes a node type that should become a chunk.
pub(super) struct NodeSpec {
    /// tree-sitter node kind string
    pub kind: &'static str,
    /// The chunk kind to assign
    pub chunk_kind: ChunkKind,
    /// Field name to use for the symbol name (e.g. "name")
    pub name_field: Option<&'static str>,
}

pub(super) fn s(
    kind: &'static str,
    chunk_kind: ChunkKind,
    name_field: Option<&'static str>,
) -> NodeSpec {
    NodeSpec {
        kind,
        chunk_kind,
        name_field,
    }
}

pub(super) fn node_specs(language: &str) -> Vec<NodeSpec> {
    use ChunkKind::*;
    match language {
        "rust" => vec![
            s("function_item", Function, Some("name")),
            s("impl_item", Impl, None),
            s("struct_item", Struct, Some("name")),
            s("enum_item", Enum, Some("name")),
            s("trait_item", Trait, Some("name")),
            s("mod_item", Module, Some("name")),
            s("const_item", Constant, Some("name")),
            s("type_item", TypeAlias, Some("name")),
        ],
        "python" => vec![
            s("function_definition", Function, Some("name")),
            s("class_definition", Class, Some("name")),
        ],
        "javascript" | "jsx" => vec![
            s("function_declaration", Function, Some("name")),
            s("method_definition", Method, Some("name")),
            s("class_declaration", Class, Some("name")),
            s("generator_function_declaration", Function, Some("name")),
        ],
        "typescript" | "tsx" => vec![
            s("function_declaration", Function, Some("name")),
            s("method_definition", Method, Some("name")),
            s("class_declaration", Class, Some("name")),
            s("interface_declaration", Interface, Some("name")),
            s("type_alias_declaration", TypeAlias, Some("name")),
            s("generator_function_declaration", Function, Some("name")),
        ],
        "go" => vec![
            s("function_declaration", Function, Some("name")),
            s("method_declaration", Method, Some("name")),
            s("type_spec", Struct, Some("name")),
        ],
        "java" => vec![
            s("class_declaration", Class, Some("name")),
            s("interface_declaration", Interface, Some("name")),
            s("method_declaration", Method, Some("name")),
            s("constructor_declaration", Method, Some("name")),
            s("enum_declaration", Enum, Some("name")),
        ],
        "c" => vec![
            s("function_definition", Function, None),
            s("struct_specifier", Struct, Some("name")),
            s("enum_specifier", Enum, Some("name")),
        ],
        "cpp" => vec![
            s("function_definition", Function, None),
            s("class_specifier", Class, Some("name")),
            s("struct_specifier", Struct, Some("name")),
            s("function_declarator", Function, Some("declarator")),
        ],
        // JSON: no semantic node types — falls back to sliding-window automatically.
        "json" => vec![],
        // HTML: capture inline script and style blocks as code chunks.
        "html" => vec![
            s("script_element", Function, None),
            s("style_element", Module, None),
        ],
        // CSS: each rule set and named @-rule becomes its own chunk.
        "css" => vec![
            s("rule_set", Rule, None),
            s("media_statement", Module, None),
            s("keyframes_statement", Function, None),
            s("supports_statement", Module, None),
        ],
        // PHP: functions, methods, classes, interfaces, traits, enums. The grammar
        // exposes a direct `name` field on each, so no custom name walker is needed.
        "php" => vec![
            s("function_definition", Function, Some("name")),
            s("method_declaration", Method, Some("name")),
            s("class_declaration", Class, Some("name")),
            s("interface_declaration", Interface, Some("name")),
            s("trait_declaration", Trait, Some("name")),
            s("enum_declaration", Enum, Some("name")),
        ],
        // Ruby: methods (incl. `def self.x` singletons), classes, and modules.
        // Each exposes a direct `name` field (`name`/`constant`), so the field
        // path handles extraction with no custom walker.
        "ruby" => vec![
            s("method", Method, Some("name")),
            s("singleton_method", Method, Some("name")),
            s("class", Class, Some("name")),
            s("module", Module, Some("name")),
        ],
        // C#: classes, structs, interfaces, enums, records, methods, and
        // constructors. tree-sitter-c-sharp exposes a direct `name` field on each,
        // so no custom name walker is needed. (Properties/fields are not chunked,
        // matching the existing languages, which chunk types + callables only.)
        "csharp" => vec![
            s("class_declaration", Class, Some("name")),
            s("struct_declaration", Struct, Some("name")),
            s("interface_declaration", Interface, Some("name")),
            s("enum_declaration", Enum, Some("name")),
            s("record_declaration", Struct, Some("name")),
            s("method_declaration", Method, Some("name")),
            s("constructor_declaration", Method, Some("name")),
        ],
        // Kotlin: classes/interfaces/enums (all `class_declaration`), objects
        // (incl. named `object` singletons), and functions. The grammar does not
        // expose a `name` field — names are unnamed `type_identifier` /
        // `simple_identifier` children — so extraction is handled by kotlin_decl_name.
        "kotlin" => vec![
            s("class_declaration", Class, None),
            s("object_declaration", Class, None),
            s("function_declaration", Function, None),
        ],
        // Swift: `class_declaration` is a unified node covering class/struct/enum/
        // extension (distinguished by the `declaration_kind` field); protocols are a
        // separate `protocol_declaration`. Functions and initializers round it out.
        // Most expose a direct `name` field; `init_declaration` has none, handled by
        // swift_init_name.
        "swift" => vec![
            s("class_declaration", Class, Some("name")),
            s("protocol_declaration", Interface, Some("name")),
            s("function_declaration", Function, Some("name")),
            s("init_declaration", Method, None),
        ],
        // HCL/Terraform: top-level blocks (resource, data, module, locals, …).
        // Name extraction is handled by hcl_block_name (identifier + string labels).
        "hcl" => vec![s("block", Module, None)],
        // Protobuf: message, enum, service, and rpc definitions.
        // Name extraction finds the *_name child node.
        "proto" => vec![
            s("message", Struct, None),
            s("enum", Enum, None),
            s("service", Interface, None),
            s("rpc", Method, None),
        ],
        // SQL: major DDL statements.
        // Name extraction finds the object_reference child.
        "sql" => vec![
            s("create_table", Struct, None),
            s("create_view", TypeAlias, None),
            s("create_function", Function, None),
            s("create_index", Constant, None),
        ],
        _ => vec![],
    }
}

/// Maximum AST recursion depth.  Deeply-nested or pathological parse trees
/// (common with adversarial inputs) would otherwise overflow the stack.
const MAX_WALK_DEPTH: usize = 512;

/// Maximum number of chunks collected in a single walk.  A file with millions
/// of matched AST nodes (possible with adversarial input) would otherwise
/// allocate unbounded memory.
const MAX_CHUNKS: usize = 100_000;

/// Immutable per-file context threaded through the AST walk.
pub(super) struct WalkCtx<'a> {
    pub src: &'a [u8],
    pub file_path: &'a str,
    pub language: &'a str,
    pub specs: &'a [NodeSpec],
}

pub(super) fn walk_node(
    node: tree_sitter::Node<'_>,
    ctx: &WalkCtx<'_>,
    parent_scope: Option<&str>,
    out: &mut Vec<Chunk>,
    depth: usize,
) {
    walk_node_inner(node, ctx, parent_scope, out, depth);
}

fn walk_node_inner(
    node: tree_sitter::Node<'_>,
    ctx: &WalkCtx<'_>,
    parent_scope: Option<&str>,
    out: &mut Vec<Chunk>,
    depth: usize,
) {
    if depth >= MAX_WALK_DEPTH || out.len() >= MAX_CHUNKS {
        return;
    }
    if let Some(spec) = ctx.specs.iter().find(|s| s.kind == node.kind()) {
        // Skip keyword leaf tokens: grammars like proto reuse the node kind
        // name for both the keyword token ("message") and the structural block.
        // Structural nodes always have named children; keyword leaves do not.
        if node.named_child_count() == 0 {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32) {
                    walk_node_inner(child, ctx, parent_scope, out, depth + 1);
                }
            }
            return;
        }

        let name = extract_name(&node, ctx.src, ctx.language, spec);

        let content = node.utf8_text(ctx.src).unwrap_or("").to_owned();

        // Look for a doc comment immediately before this node
        let docstring = preceding_comment(&node, ctx.src);

        // Build scope label for impl/class containers
        let scope_label: Option<String> = match spec.chunk_kind {
            ChunkKind::Impl | ChunkKind::Class => {
                name.clone().map(|n| format!("{} {}", spec.kind, n))
            }
            _ => parent_scope.map(str::to_owned),
        };

        let start_row = node.start_position().row;
        let is_container = matches!(
            spec.chunk_kind,
            ChunkKind::Module
                | ChunkKind::Impl
                | ChunkKind::Class
                | ChunkKind::Trait
                | ChunkKind::Interface
        );

        if estimate_tokens(&content) > chunk_token_cap() {
            if is_container {
                // Suppress the container's own chunk; its children already carry
                // fine-grained chunks framed by parent_scope. Re-window only if the
                // container matched no children.
                let before = out.len();
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i as u32) {
                        walk_node_inner(child, ctx, scope_label.as_deref(), out, depth + 1);
                    }
                }
                if out.len() == before {
                    push_windowed(
                        &content,
                        ctx,
                        start_row,
                        name.as_deref(),
                        docstring.as_deref(),
                        parent_scope,
                        out,
                    );
                }
            } else {
                push_windowed(
                    &content,
                    ctx,
                    start_row,
                    name.as_deref(),
                    docstring.as_deref(),
                    parent_scope,
                    out,
                );
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i as u32) {
                        walk_node_inner(child, ctx, scope_label.as_deref(), out, depth + 1);
                    }
                }
            }
            return;
        }

        out.push(Chunk {
            file_path: ctx.file_path.to_owned(),
            language: ctx.language.to_owned(),
            kind: spec.chunk_kind.clone(),
            name,
            start_line: start_row + 1,
            end_line: node.end_position().row + 1,
            content,
            docstring,
            parent_scope: parent_scope.map(str::to_owned),
            summary: None,
        });

        // Recurse into children with the updated scope
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                walk_node_inner(child, ctx, scope_label.as_deref(), out, depth + 1);
            }
        }
    } else {
        // Not a target node — recurse with same parent scope
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                walk_node_inner(child, ctx, parent_scope, out, depth + 1);
            }
        }
    }
}

/// Re-window an oversized node's text into sliding-window sub-chunks, offsetting
/// each sub-chunk's line span by the node's 0-based `start_row`. The node's
/// identity (`name`/`docstring`/`parent_scope`) is threaded onto every sub-chunk
/// so a re-windowed node keeps its symbol name and docstring in the embedding
/// text instead of degrading to `title: none`.
#[allow(clippy::too_many_arguments)]
fn push_windowed(
    content: &str,
    ctx: &WalkCtx<'_>,
    start_row: usize,
    name: Option<&str>,
    docstring: Option<&str>,
    parent_scope: Option<&str>,
    out: &mut Vec<Chunk>,
) {
    for mut sub in sliding_window(
        content,
        ctx.file_path,
        ctx.language,
        name,
        docstring,
        parent_scope,
    ) {
        sub.start_line += start_row;
        sub.end_line += start_row;
        out.push(sub);
    }
}

/// Language-aware name extraction for a chunk node.
pub(super) fn extract_name(
    node: &tree_sitter::Node<'_>,
    src: &[u8],
    language: &str,
    spec: &NodeSpec,
) -> Option<String> {
    // Try the declared name field first.
    let from_field = spec
        .name_field
        .and_then(|field| node.child_by_field_name(field))
        .and_then(|n| n.utf8_text(src).ok())
        .map(|text| match language {
            // JSON keys are wrapped in quotes — strip them.
            "json" => text.trim_matches('"').to_owned(),
            _ => text.to_owned(),
        });

    if from_field.is_some() {
        return from_field;
    }

    // Language-specific fallbacks when no name field is declared.
    match language {
        "c" | "cpp" => c_function_name(node, src),
        "css" => css_chunk_name(node, src),
        "html" => html_chunk_name(node, src),
        "hcl" => hcl_block_name(node, src),
        "proto" => proto_named_child(node, src),
        "sql" => sql_object_name(node, src),
        "kotlin" => kotlin_decl_name(node, src),
        "swift" => swift_init_name(node),
        _ => None,
    }
}

/// Extract the declared name from a Kotlin declaration node. tree-sitter-kotlin
/// (the `-sg` grammar) does not expose a `name` field: classes/interfaces/enums
/// and named objects carry their name as a `type_identifier` child, and functions
/// as a `simple_identifier` child. A `companion object` has no name — returns None.
fn kotlin_decl_name(node: &tree_sitter::Node<'_>, src: &[u8]) -> Option<String> {
    let want = match node.kind() {
        "class_declaration" | "object_declaration" => "type_identifier",
        "function_declaration" => "simple_identifier",
        _ => return None,
    };
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32)
            && child.kind() == want
        {
            return child.utf8_text(src).ok().map(str::to_owned);
        }
    }
    None
}

/// Swift `init_declaration` nodes have no name field; label them `init`.
fn swift_init_name(node: &tree_sitter::Node<'_>) -> Option<String> {
    if node.kind() == "init_declaration" {
        Some("init".to_owned())
    } else {
        None
    }
}

/// Return the selector text from a CSS `rule_set` node, or the @-keyword for
/// at-rules, to use as the chunk name.
fn css_chunk_name(node: &tree_sitter::Node<'_>, src: &[u8]) -> Option<String> {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            if child.kind() == "selectors" {
                return child.utf8_text(src).ok().map(|s| s.trim().to_owned());
            }
            // @-rule keyword (e.g. "media", "keyframes")
            if matches!(child.kind(), "at_keyword" | "keyword") {
                return child.utf8_text(src).ok().map(|s| s.to_owned());
            }
        }
    }
    None
}

/// Return the `src`/`id` attribute value of an HTML chunk element as its name,
/// falling back to the tag name.  tree-sitter-html uses child kinds
/// (`attribute_name`, `attribute_value`) rather than named fields.
fn html_chunk_name(node: &tree_sitter::Node<'_>, src: &[u8]) -> Option<String> {
    for i in 0..node.child_count() {
        if let Some(start_tag) = node.child(i as u32) {
            if start_tag.kind() != "start_tag" {
                continue;
            }
            let mut tag_name: Option<String> = None;
            for j in 0..start_tag.child_count() {
                let child = match start_tag.child(j as u32) {
                    Some(c) => c,
                    None => continue,
                };
                if child.kind() == "tag_name" {
                    tag_name = child.utf8_text(src).ok().map(str::to_owned);
                }
                if child.kind() == "attribute" {
                    let mut name = "";
                    let mut value = "";
                    for k in 0..child.child_count() {
                        if let Some(attr_child) = child.child(k as u32) {
                            match attr_child.kind() {
                                "attribute_name" => name = attr_child.utf8_text(src).unwrap_or(""),
                                "attribute_value" | "quoted_attribute_value" => {
                                    value = attr_child.utf8_text(src).unwrap_or("")
                                }
                                _ => {}
                            }
                        }
                    }
                    if matches!(name, "src" | "id") && !value.is_empty() {
                        return Some(value.trim_matches('"').trim_matches('\'').to_owned());
                    }
                }
            }
            return tag_name;
        }
    }
    None
}

/// Extract the function name from a C/C++ `function_definition` node, which
/// nests the name inside a declarator rather than exposing a direct `name` field.
fn c_function_name<'a>(node: &tree_sitter::Node<'a>, src: &'a [u8]) -> Option<String> {
    // function_definition → declarator → … → identifier
    let decl = node.child_by_field_name("declarator")?;
    find_identifier(decl, src)
}

/// Maximum recursion depth for identifier search inside declarator subtrees.
const MAX_IDENT_DEPTH: usize = 64;

pub(super) fn find_identifier(node: tree_sitter::Node<'_>, src: &[u8]) -> Option<String> {
    find_identifier_inner(node, src, 0)
}

fn find_identifier_inner(node: tree_sitter::Node<'_>, src: &[u8], depth: usize) -> Option<String> {
    if depth >= MAX_IDENT_DEPTH {
        return None;
    }
    if node.kind() == "identifier" || node.kind() == "field_identifier" {
        return node.utf8_text(src).ok().map(str::to_owned);
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32)
            && let Some(name) = find_identifier_inner(child, src, depth + 1)
        {
            return Some(name);
        }
    }
    None
}

/// Build an HCL block name from its type identifier and string labels.
/// e.g. `resource "aws_instance" "main"` → `"resource.aws_instance.main"`.
fn hcl_block_name(node: &tree_sitter::Node<'_>, src: &[u8]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            match child.kind() {
                "identifier" => {
                    if let Ok(t) = child.utf8_text(src) {
                        parts.push(t.to_owned());
                    }
                }
                "string_lit" => {
                    if let Ok(t) = child.utf8_text(src) {
                        parts.push(t.trim_matches('"').to_owned());
                    }
                }
                _ => {}
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("."))
    }
}

/// Return the text of the first `*_name` child node (used for proto grammars).
fn proto_named_child(node: &tree_sitter::Node<'_>, src: &[u8]) -> Option<String> {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32)
            && child.kind().ends_with("_name")
        {
            return child.utf8_text(src).ok().map(str::to_owned);
        }
    }
    None
}

/// Return the text of the first `object_reference` child (used for SQL DDL nodes).
fn sql_object_name(node: &tree_sitter::Node<'_>, src: &[u8]) -> Option<String> {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32)
            && child.kind() == "object_reference"
        {
            return child.utf8_text(src).ok().map(str::to_owned);
        }
    }
    None
}

/// Return the text of the comment node that immediately precedes `node`
/// (skipping whitespace), if any.
///
/// Rust attributes (`#[derive(...)]`) are real siblings, skipped in the loop
/// below. Python wraps decorator+def in one `decorated_definition` node, so
/// the walk must start from that parent instead of `node`. TS/Java attach
/// decorators as a child, so neither case applies there. Ruby's
/// `private def foo; end` visibility idiom (and lookalikes like `memoize def
/// foo; end`) parses the def as a `method` node nested two levels inside a
/// `call` (`private(def foo; end)`), so the walk must start from that `call`
/// ancestor instead of the `method` node. Gated on the `method` being the
/// argument_list's only child so an unrelated comment above a multi-arg call
/// that merely happens to carry a `def` as one of several arguments (e.g.
/// `some_call(other_arg, def foo; end)`) doesn't get misattached to `foo`.
///
/// Some grammars (Python `class_definition`/`function_definition`, Ruby
/// `class`/`module`) attach a leading comment as a child of the enclosing
/// `block`'s own parent, immediately before the `body` field, rather than as
/// the first child inside the block. That bites only the first documented
/// member of a body: when `start` has no sibling of its own (nothing else in
/// its block precedes it), check one level up for that comment-as-child case.
pub(super) fn preceding_comment(node: &tree_sitter::Node<'_>, src: &[u8]) -> Option<String> {
    let start = match node.parent() {
        Some(parent) if parent.kind() == "decorated_definition" => parent,
        Some(parent) if parent.kind() == "argument_list" && parent.named_child_count() == 1 => {
            match parent.parent() {
                Some(call) if call.kind() == "call" => call,
                _ => *node,
            }
        }
        _ => *node,
    };
    match start.prev_sibling() {
        Some(prev) => scan_backward_for_comment(prev, src),
        None => scan_backward_for_comment(start.parent()?.prev_sibling()?, src),
    }
}

fn scan_backward_for_comment(mut prev: tree_sitter::Node<'_>, src: &[u8]) -> Option<String> {
    loop {
        let skip = prev.kind() == "\n"
            || prev.kind() == "newline"
            || prev.kind() == "attribute_item"
            || prev.is_extra()
                && prev.kind() != "comment"
                && prev.kind() != "line_comment"
                && prev.kind() != "block_comment"
                && prev.kind() != "doc_comment";
        if !skip {
            break;
        }
        prev = prev.prev_sibling()?;
    }
    if matches!(
        prev.kind(),
        "comment" | "line_comment" | "block_comment" | "doc_comment"
    ) {
        Some(prev.utf8_text(src).unwrap_or("").to_owned())
    } else {
        None
    }
}
