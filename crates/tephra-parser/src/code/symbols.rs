//! Symbols and block IDs of a parsed code file (G4, G13).
//!
//! A symbol is a function, method, class, struct, enum, trait,
//! interface, impl, module, type alias, or a top-level variable,
//! const or static (a YAML/TOML key or TOML table header, for those
//! two grammars). Each has a kind, a name, the enclosing symbol's
//! name (`parent`), its **first line** (`line`: the definition line,
//! the one that carries a block ID) and its **range**
//! (`start_line..=end_line`). All lines are 1-based.
//!
//! # Ranges
//!
//! The range starts above the definition line when attributes
//! (`#[derive]`), decorators, doc comments (`///`, `//!`, `/** */`,
//! Go's `//` doc comments, Lua's `---`) or `^id` comments sit on the
//! lines directly above it with no blank line between. It ends on the
//! last line of the definition. `line` is never the attribute's or the
//! doc comment's line: it is the `fn` / `def` / `class` line.
//!
//! # What is a symbol, and what is not
//!
//! - Methods are functions found directly inside a class, impl, trait,
//!   interface, struct or extension body: kind `method`, parent = the
//!   type's name.
//! - Nested named functions and classes (inside a function body) are
//!   recorded, with the enclosing function as parent.
//! - Variables, consts and statics are recorded only at the top level
//!   of a file or module (including inside a Python `if` or a
//!   namespace). A class field, a local, a closure assigned inside a
//!   function: never. An arrow function or function expression bound to
//!   a top-level name is a `function`.
//! - Nothing is deduplicated by name: an `enum X` and an `impl X`, or
//!   two overloads, each keep a row.
//!
//! # Block IDs (G13)
//!
//! Only comment nodes are read, never string literals. A comment line
//! that *ends* in `^id` (the same `[A-Za-z0-9-]+` rule as prose) is a
//! block ID. It belongs to a symbol when
//!
//! 1. it sits on the symbol's first line (`fn f() { // ^k3f9a2`), or
//! 2. it sits on its own line directly above the symbol, with nothing
//!    but attributes, decorators, doc comments or other ID comments in
//!    between and no blank line.
//!
//! Otherwise it is a block of its own on the comment's line, with no
//! symbol. The block's line is the symbol's first line in the first
//! two cases.
//!
//! # Errors
//!
//! The walk descends into tree-sitter `ERROR` nodes, so a half-written
//! file still gives every symbol tree-sitter could recover. Symbols
//! are read with an explicit stack, not recursion, so a deeply nested
//! file cannot overflow the stack.

use std::collections::HashMap;

use serde::Serialize;
use tree_sitter::{Node, Tree};

use super::languages::Language;
use crate::blocks::{is_block_id_char, BlockAnchor};
use crate::diagnostics::{Diagnostic, DiagnosticKind};

/// One symbol of a code file. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Symbol {
    /// `function`, `method`, `class`, `struct`, `enum`, `trait`,
    /// `interface`, `impl`, `mod`, `type`, `const`, `static`,
    /// `variable`, plus `key`, `table` and `array_table` for YAML and
    /// TOML.
    pub kind: &'static str,
    /// The name as written, UTF-8.
    pub name: String,
    /// The enclosing symbol's name, `None` at the top level.
    pub parent: Option<String>,
    /// First line: the definition line (1-based).
    pub line: usize,
    /// First line of the range: the first attribute, decorator or doc
    /// comment line directly above the definition, else `line`.
    pub start_line: usize,
    /// Last line of the range.
    pub end_line: usize,
}

/// What [`extract`] found.
pub(crate) struct Extracted {
    pub symbols: Vec<Symbol>,
    pub blocks: Vec<BlockAnchor>,
}

/// Symbols and block IDs of `tree`, which was parsed from `src`.
/// Pushes a [`DiagnosticKind::SyntaxErrors`] and a
/// [`DiagnosticKind::DuplicateBlockId`] per repeated ID.
pub(crate) fn extract(
    lang: Language,
    tree: &Tree,
    src: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Extracted {
    let lines = Lines::new(src);
    let root = tree.root_node();
    if root.has_error() {
        let row = first_error_row(lang, root, &lines);
        diagnostics.push(Diagnostic::new(
            DiagnosticKind::SyntaxErrors,
            row,
            format!(
                "{} syntax error near line {row}; symbols and links that could be recovered are recorded",
                lang.name()
            ),
        ));
    }

    let marks = collect_marks(lang, root, src, &lines);
    let mut walker = Walker {
        lang,
        src,
        lines: &lines,
        trivia: &marks.trivia,
        symbols: Vec::new(),
    };
    walker.run(root);
    let symbols = walker.symbols;

    let blocks = own_block_ids(lang, &marks.comments, src, &symbols, diagnostics);
    Extracted { symbols, blocks }
}

// ------------------------------------------------------------ lines

/// Byte offset of each line start, for byte <-> 1-based line.
struct Lines {
    starts: Vec<usize>,
    len: usize,
}

impl Lines {
    fn new(src: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            src.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Lines {
            starts,
            len: src.len(),
        }
    }

    /// 1-based line of the byte at `offset`.
    fn row(&self, offset: usize) -> usize {
        self.starts.partition_point(|&s| s <= offset.min(self.len))
    }

    fn count(&self) -> usize {
        self.starts.len()
    }

    /// Byte offset of the start of `row` (1-based).
    fn start(&self, row: usize) -> usize {
        self.starts[row - 1]
    }

    /// Byte offset of the end of `row`, before its line break.
    fn end(&self, row: usize, src: &str) -> usize {
        let end = self.starts.get(row).copied().unwrap_or(self.len);
        let text = &src[self.start(row)..end];
        self.start(row) + text.trim_end_matches(['\n', '\r']).len()
    }

    /// First row of `node`.
    fn first(&self, node: Node<'_>) -> usize {
        self.row(node.start_byte())
    }

    /// Last row of `node`: the row of its last byte, so a node that
    /// ends with a line break does not claim the next line.
    fn last(&self, node: Node<'_>) -> usize {
        let end = node.end_byte();
        if end > node.start_byte() {
            self.row(end - 1).max(self.first(node))
        } else {
            self.first(node)
        }
    }
}

/// The first line tree-sitter could not make sense of. A bare `ERROR`
/// node can start at a comment far above the real fault (it swallows
/// the leading comments), so the row is that of its first child that
/// is not a comment; a `MISSING` token is where it should have been.
fn first_error_row(lang: Language, root: Node<'_>, lines: &Lines) -> usize {
    let mut stack = vec![root];
    let mut best: Option<usize> = None;
    while let Some(node) = stack.pop() {
        if !node.has_error() && !node.is_error() && !node.is_missing() {
            continue;
        }
        if node.is_missing() {
            let row = lines.first(node);
            best = Some(best.map_or(row, |b| b.min(row)));
            continue;
        }
        if node.is_error() {
            let mut cursor = node.walk();
            let first = node
                .children(&mut cursor)
                .find(|c| !lang.is_comment_kind(c.kind()));
            let row = lines.first(first.unwrap_or(node));
            best = Some(best.map_or(row, |b| b.min(row)));
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    best.unwrap_or(1)
}

// ------------------------------------------------------------ marks

/// A comment node of the file.
struct CommentInfo {
    start_byte: usize,
    end_byte: usize,
    first_row: usize,
}

struct Marks {
    comments: Vec<CommentInfo>,
    /// `trivia[row]`: the line belongs to the lines directly above a
    /// definition: an attribute or decorator line (only attributes and
    /// comments on it), an own-line doc comment or `^id` comment.
    trivia: Vec<bool>,
}

/// One pass over the whole tree: every comment, and the rows that are
/// attribute / decorator / doc comment / ID comment lines.
fn collect_marks(lang: Language, root: Node<'_>, src: &str, lines: &Lines) -> Marks {
    let mut comments = Vec::new();
    let mut trivia = vec![false; lines.count() + 2];
    let mut cursor = root.walk();
    'walk: loop {
        let node = cursor.node();
        let kind = node.kind();
        let mut descend = true;
        if lang.is_comment_kind(kind) {
            descend = false;
            if !super::comments::is_unterminated_heredoc_text(lang, node) {
                let info = CommentInfo {
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte().min(src.len()),
                    first_row: lines.first(node),
                };
                let text = src.get(info.start_byte..info.end_byte).unwrap_or("");
                let (first, last) = (lines.first(node), lines.last(node));
                if own_line(src, lines, node, true)
                    && (is_doc_comment(lang, text)
                        || text.lines().any(|l| trailing_id(lang, l).is_some()))
                {
                    trivia[first..=last].fill(true);
                }
                comments.push(info);
            }
        } else if is_attribute_kind(lang, kind)
            && own_line(src, lines, node, false)
            && only_attributes_and_comments_follow(lang, root, src, node)
        {
            trivia[lines.first(node)..=lines.last(node)].fill(true);
        }

        if descend && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    Marks { comments, trivia }
}

/// `node` starts its line (only whitespace before it), and, when
/// `whole_line`, nothing but whitespace follows it on its last line.
fn own_line(src: &str, lines: &Lines, node: Node<'_>, whole_line: bool) -> bool {
    let first = lines.first(node);
    let before = src
        .get(lines.start(first)..node.start_byte())
        .unwrap_or("x");
    if !before.trim().is_empty() {
        return false;
    }
    if !whole_line {
        return true;
    }
    let last = lines.last(node);
    let end = node.end_byte().min(src.len());
    let after = src.get(end..lines.end(last, src).max(end)).unwrap_or("x");
    after.trim().is_empty()
}

/// After `node` (an attribute or decorator), the rest of its last line
/// holds nothing but further attributes, comments and whitespace. When
/// code follows (`#[test] fn a() {}`, `@Input() name: string;`) the
/// line is a definition of its own, not a line above the next one.
fn only_attributes_and_comments_follow(
    lang: Language,
    root: Node<'_>,
    src: &str,
    node: Node<'_>,
) -> bool {
    let bytes = src.as_bytes();
    let mut pos = node.end_byte().min(bytes.len());
    loop {
        while pos < bytes.len() && bytes[pos] != b'\n' && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() || bytes[pos] == b'\n' {
            return true;
        }
        let Some(mut found) = root.descendant_for_byte_range(pos, pos + 1) else {
            return false;
        };
        // The outermost attribute or comment that holds this byte.
        let mut hit = None;
        loop {
            let kind = found.kind();
            if is_attribute_kind(lang, kind) || lang.is_comment_kind(kind) {
                hit = Some(found);
            }
            match found.parent() {
                Some(parent) if hit.is_none() => found = parent,
                _ => break,
            }
        }
        match hit {
            Some(h) if h.end_byte() > pos => pos = h.end_byte(),
            _ => return false,
        }
    }
}

/// An attribute or decorator node: it belongs to the definition below
/// it. (Rust's inner `#![...]` is not one.)
fn is_attribute_kind(lang: Language, kind: &str) -> bool {
    match lang {
        Language::Rust => kind == "attribute_item",
        Language::Python => kind == "decorator",
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx => {
            kind == "decorator"
        }
        Language::Java => kind == "annotation" || kind == "marker_annotation",
        Language::Swift => kind == "attribute",
        Language::Cpp => kind == "attribute_declaration",
        _ => false,
    }
}

/// A doc comment: one that documents the definition below it.
fn is_doc_comment(lang: Language, text: &str) -> bool {
    let slash_doc = text.starts_with("///") && !text.starts_with("////");
    let block_doc =
        (text.starts_with("/**") && !text.starts_with("/**/") && !text.starts_with("/***"))
            || text.starts_with("/*!");
    match lang {
        Language::Rust => slash_doc || text.starts_with("//!") || block_doc,
        Language::C | Language::Cpp => slash_doc || text.starts_with("//!") || block_doc,
        Language::Swift => slash_doc || block_doc,
        Language::TypeScript
        | Language::Tsx
        | Language::JavaScript
        | Language::Jsx
        | Language::Java => block_doc,
        // Go's doc comment is the plain comment block above a
        // declaration.
        Language::Go => true,
        Language::Lua => text.starts_with("---"),
        Language::Python | Language::Shell | Language::Yaml | Language::Toml => false,
    }
}

// ----------------------------------------------------------- walker

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// The file, or a module / namespace body.
    Top,
    /// The body of a class, impl, trait, interface, struct.
    Body,
    /// Inside a function or closure.
    Nested,
}

#[derive(Clone, Copy)]
struct Frame<'t> {
    node: Node<'t>,
    parent: Option<usize>,
    scope: Scope,
}

struct Walker<'a> {
    lang: Language,
    src: &'a str,
    lines: &'a Lines,
    trivia: &'a [bool],
    symbols: Vec<Symbol>,
}

type Stack<'t> = Vec<Frame<'t>>;

impl<'a> Walker<'a> {
    fn run(&mut self, root: Node<'_>) {
        let mut stack: Stack<'_> = vec![Frame {
            node: root,
            parent: None,
            scope: Scope::Top,
        }];
        while let Some(frame) = stack.pop() {
            self.visit(frame, &mut stack);
        }
    }

    fn text(&self, node: Node<'_>) -> &'a str {
        self.src
            .get(node.start_byte()..node.end_byte())
            .unwrap_or("")
    }

    fn field(&self, node: Node<'_>, name: &str) -> Option<&'a str> {
        let child = node.child_by_field_name(name)?;
        let text = self.text(child);
        (!text.is_empty()).then_some(text)
    }

    /// Pushes `node`'s named children (so the first pops first).
    fn push_children<'t>(
        &self,
        node: Node<'t>,
        parent: Option<usize>,
        scope: Scope,
        stack: &mut Stack<'t>,
    ) {
        let mut cursor = node.walk();
        let children: Vec<Node<'t>> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(Frame {
                node: child,
                parent,
                scope,
            });
        }
    }

    /// The wrapper that carries the decorators / `export` / template
    /// header of `node`: the node the range and first line are read
    /// from.
    fn outer<'t>(&self, node: Node<'t>) -> Node<'t> {
        let mut outer = node;
        while let Some(parent) = outer.parent() {
            let wraps = match parent.kind() {
                "decorated_definition"
                | "export_statement"
                | "ambient_declaration"
                | "template_declaration" => true,
                // `namespace N {}` is an expression statement in TS.
                "expression_statement" => outer.kind() == "internal_module",
                _ => false,
            };
            if !wraps {
                break;
            }
            outer = parent;
        }
        outer
    }

    /// The row of the first token of `node` that is not an attribute,
    /// decorator, comment or C++ `template <...>` header: the
    /// definition line.
    fn code_row(&self, node: Node<'_>) -> usize {
        self.code_row_of(node)
            .unwrap_or_else(|| self.lines.first(node))
    }

    /// `None` when every token under `node` is an attribute, comment
    /// or header (a Java `modifiers` that is only `@Override`).
    fn code_row_of(&self, node: Node<'_>) -> Option<usize> {
        if node.child_count() == 0 {
            return Some(self.lines.first(node));
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            let kind = child.kind();
            if is_attribute_kind(self.lang, kind)
                || self.lang.is_comment_kind(kind)
                || (self.lang == Language::Cpp
                    && matches!(kind, "template" | "template_parameter_list"))
            {
                continue;
            }
            if let Some(row) = self.code_row_of(child) {
                return Some(row);
            }
        }
        None
    }

    /// First row of the lines directly above `row` that belong to the
    /// definition on it.
    fn chain_start(&self, row: usize) -> usize {
        let mut start = row;
        while start > 1 && self.trivia[start - 1] {
            start -= 1;
        }
        start
    }

    /// Records a symbol for `node`. Returns its index.
    fn add(
        &mut self,
        kind: &'static str,
        name: &str,
        parent: Option<String>,
        node: Node<'_>,
    ) -> usize {
        let outer = self.outer(node);
        let line = self.code_row(outer);
        let (start, end) = self.range_of(outer, line);
        self.push_symbol(kind, name, parent, line, start, end)
    }

    /// `(start_line, end_line)` of the definition `node` whose first
    /// line is `line`. The lines above it only count when the
    /// definition starts its line: in `/** doc */ fn f() { fn g() {} }`
    /// the doc comment belongs to `f`, not to `g`, which shares the
    /// line.
    fn range_of(&self, node: Node<'_>, line: usize) -> (usize, usize) {
        let first = self.lines.first(node);
        let row_start = self.lines.start(first);
        let starts_row = self
            .src
            .get(row_start..node.start_byte())
            .is_some_and(|before| before.trim().is_empty());
        let start = if starts_row {
            self.chain_start(line).min(first).min(line)
        } else {
            line
        };
        (start, self.lines.last(node).max(line))
    }

    /// Records a symbol with explicit rows (a declarator that shares
    /// its statement with others).
    fn push_symbol(
        &mut self,
        kind: &'static str,
        name: &str,
        parent: Option<String>,
        line: usize,
        start_line: usize,
        end_line: usize,
    ) -> usize {
        self.symbols.push(Symbol {
            kind,
            name: name.to_string(),
            parent,
            line,
            start_line,
            end_line,
        });
        self.symbols.len() - 1
    }

    fn parent_name(&self, parent: Option<usize>) -> Option<String> {
        parent.map(|i| self.symbols[i].name.clone())
    }

    fn visit<'t>(&mut self, f: Frame<'t>, stack: &mut Stack<'t>) {
        let node = f.node;
        if node.is_missing() {
            return;
        }
        // A closure's body is code, never a top-level or a member
        // list: what it holds is nested.
        if is_closure_kind(self.lang, node.kind()) {
            self.push_children(node, f.parent, Scope::Nested, stack);
            return;
        }
        let handled = match self.lang {
            Language::Rust => self.rust(f, stack),
            Language::Python => self.python(f, stack),
            Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx => {
                self.ts(f, stack)
            }
            Language::Go => self.go(f, stack),
            Language::Shell => self.shell(f, stack),
            Language::Yaml => self.yaml(f),
            Language::Toml => self.toml(f),
            Language::C | Language::Cpp => self.c(f, stack),
            Language::Java => self.java(f, stack),
            Language::Swift => self.swift(f, stack),
            Language::Lua => self.lua(f, stack),
        };
        if !handled {
            self.push_children(node, f.parent, f.scope, stack);
        }
    }

    /// Pushes the children of a symbol node under the new `parent`.
    fn descend<'t>(&self, node: Node<'t>, parent: usize, scope: Scope, stack: &mut Stack<'t>) {
        self.push_children(node, Some(parent), scope, stack);
    }

    // ---- Rust

    fn rust<'t>(&mut self, f: Frame<'t>, stack: &mut Stack<'t>) -> bool {
        let n = f.node;
        let parent = self.parent_name(f.parent);
        match n.kind() {
            "function_item" | "function_signature_item" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let kind = if f.scope == Scope::Body {
                    "method"
                } else {
                    "function"
                };
                let idx = self.add(kind, name, parent, n);
                self.descend(n, idx, Scope::Nested, stack);
            }
            "struct_item" | "enum_item" | "union_item" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let kind = match n.kind() {
                    "enum_item" => "enum",
                    "union_item" => "union",
                    _ => "struct",
                };
                self.add(kind, name, parent, n);
            }
            "trait_item" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let idx = self.add("trait", name, parent, n);
                self.descend(n, idx, Scope::Body, stack);
            }
            "impl_item" => {
                let Some(ty) = n.child_by_field_name("type") else {
                    return false;
                };
                let name = self.rust_type_name(ty);
                if name.is_empty() {
                    return false;
                }
                let idx = self.add("impl", &name, parent, n);
                self.descend(n, idx, Scope::Body, stack);
            }
            "mod_item" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let idx = self.add("mod", name, parent, n);
                self.descend(n, idx, Scope::Top, stack);
            }
            "const_item" | "static_item" | "type_item" if f.scope == Scope::Top => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let kind = match n.kind() {
                    "const_item" => "const",
                    "static_item" => "static",
                    _ => "type",
                };
                self.add(kind, name, parent, n);
            }
            _ => return false,
        }
        true
    }

    /// `Foo` of `Foo`, `Foo<T>`, `a::Foo`, `&Foo`, `&mut Foo<T>`.
    fn rust_type_name(&self, ty: Node<'_>) -> String {
        match ty.kind() {
            "generic_type" | "reference_type" | "pointer_type" => ty
                .child_by_field_name("type")
                .map(|t| self.rust_type_name(t))
                .unwrap_or_default(),
            "scoped_type_identifier" => ty
                .child_by_field_name("name")
                .map(|t| self.text(t).to_string())
                .unwrap_or_default(),
            _ => self.text(ty).trim().to_string(),
        }
    }

    // ---- Python

    fn python<'t>(&mut self, f: Frame<'t>, stack: &mut Stack<'t>) -> bool {
        let n = f.node;
        let parent = self.parent_name(f.parent);
        match n.kind() {
            "function_definition" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let kind = if f.scope == Scope::Body {
                    "method"
                } else {
                    "function"
                };
                let idx = self.add(kind, name, parent, n);
                self.descend(n, idx, Scope::Nested, stack);
            }
            "class_definition" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let idx = self.add("class", name, parent, n);
                self.descend(n, idx, Scope::Body, stack);
            }
            "assignment" if f.scope == Scope::Top => {
                // `x = y = 5` nests the second assignment in `right`.
                let mut assignment = n;
                loop {
                    if let Some(left) = assignment.child_by_field_name("left") {
                        let mut names = Vec::new();
                        self.python_targets(left, &mut names);
                        for name in names {
                            self.add_statement_symbol("variable", &name, parent.clone(), n);
                        }
                    }
                    match assignment.child_by_field_name("right") {
                        Some(r) if r.kind() == "assignment" => assignment = r,
                        _ => break,
                    }
                }
                if let Some(right) = n.child_by_field_name("right") {
                    self.push_children(right, f.parent, Scope::Nested, stack);
                }
            }
            _ => return false,
        }
        true
    }

    fn python_targets(&self, node: Node<'_>, out: &mut Vec<String>) {
        match node.kind() {
            "identifier" => out.push(self.text(node).to_string()),
            "pattern_list" | "tuple_pattern" | "list_pattern" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.python_targets(child, out);
                }
            }
            _ => {}
        }
    }

    /// A symbol for a statement (`x = 1`) whose range is the statement
    /// the node sits in.
    fn add_statement_symbol(
        &mut self,
        kind: &'static str,
        name: &str,
        parent: Option<String>,
        node: Node<'_>,
    ) -> usize {
        let statement = match node.parent() {
            Some(p) if p.kind() == "expression_statement" => p,
            _ => node,
        };
        let line = self.lines.first(statement);
        let (start, end) = self.range_of(statement, line);
        self.push_symbol(kind, name, parent, line, start, end)
    }

    // ---- TypeScript / JavaScript

    fn ts<'t>(&mut self, f: Frame<'t>, stack: &mut Stack<'t>) -> bool {
        let n = f.node;
        let parent = self.parent_name(f.parent);
        match n.kind() {
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let idx = self.add("function", name, parent, n);
                self.descend(n, idx, Scope::Nested, stack);
            }
            "class_declaration" | "abstract_class_declaration" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let idx = self.add("class", name, parent, n);
                self.descend(n, idx, Scope::Body, stack);
            }
            "interface_declaration" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let idx = self.add("interface", name, parent, n);
                self.descend(n, idx, Scope::Body, stack);
            }
            "enum_declaration" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                self.add("enum", name, parent, n);
            }
            "type_alias_declaration" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                self.add("type", name, parent, n);
            }
            "internal_module" | "module" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let name = name.trim_matches(['"', '\'']);
                let idx = self.add("mod", name, parent, n);
                self.descend(n, idx, Scope::Top, stack);
            }
            "method_definition" | "method_signature" | "abstract_method_signature"
                if f.scope == Scope::Body =>
            {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let idx = self.add("method", name, parent, n);
                self.descend(n, idx, Scope::Nested, stack);
            }
            "lexical_declaration" | "variable_declaration" if f.scope == Scope::Top => {
                self.ts_declaration(n, parent);
                self.push_children(n, f.parent, Scope::Nested, stack);
            }
            _ => return false,
        }
        true
    }

    fn ts_declaration(&mut self, decl: Node<'_>, parent: Option<String>) {
        let is_const = decl.child(0).is_some_and(|c| c.kind() == "const");
        let mut cursor = decl.walk();
        let declarators: Vec<Node<'_>> = decl
            .named_children(&mut cursor)
            .filter(|c| c.kind() == "variable_declarator")
            .collect();
        let single = declarators.len() == 1;
        for declarator in declarators.iter() {
            let Some(name_node) = declarator.child_by_field_name("name") else {
                continue;
            };
            if name_node.kind() != "identifier" {
                continue; // destructuring pattern
            }
            let is_function = declarator.child_by_field_name("value").is_some_and(|v| {
                matches!(
                    v.kind(),
                    "arrow_function" | "function_expression" | "function" | "generator_function"
                )
            });
            let kind = if is_function {
                "function"
            } else if is_const {
                "const"
            } else {
                "variable"
            };
            let name = self.text(name_node).to_string();
            if single {
                self.add(kind, &name, parent.clone(), decl);
            } else {
                let line = self.lines.first(*declarator);
                let end = self.lines.last(*declarator).max(line);
                self.push_symbol(kind, &name, parent.clone(), line, line, end);
            }
        }
    }

    // ---- Go

    fn go<'t>(&mut self, f: Frame<'t>, stack: &mut Stack<'t>) -> bool {
        let n = f.node;
        let parent = self.parent_name(f.parent);
        match n.kind() {
            "function_declaration" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let idx = self.add("function", name, parent, n);
                self.descend(n, idx, Scope::Nested, stack);
            }
            "method_declaration" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let receiver = n
                    .child_by_field_name("receiver")
                    .and_then(|r| self.first_of_kind(r, "type_identifier"))
                    .map(|t| self.text(t).to_string());
                let idx = self.add("method", name, receiver, n);
                self.descend(n, idx, Scope::Nested, stack);
            }
            "type_declaration" => {
                let mut cursor = n.walk();
                let specs: Vec<Node<'t>> = n
                    .named_children(&mut cursor)
                    .filter(|c| matches!(c.kind(), "type_spec" | "type_alias"))
                    .collect();
                let grouped = n.children(&mut cursor).any(|c| c.kind() == "(");
                for spec in specs {
                    let Some(name) = self.field(spec, "name") else {
                        continue;
                    };
                    let kind = match spec.child_by_field_name("type").map(|t| t.kind()) {
                        Some("struct_type") if spec.kind() == "type_spec" => "struct",
                        Some("interface_type") if spec.kind() == "type_spec" => "interface",
                        _ => "type",
                    };
                    // `type T struct{}` is its declaration; in a
                    // `type ( ... )` group each spec is its own range.
                    let at = if grouped { spec } else { n };
                    self.add(kind, name, parent.clone(), at);
                }
            }
            "const_declaration" | "var_declaration" if f.scope == Scope::Top => {
                let kind = if n.kind() == "const_declaration" {
                    "const"
                } else {
                    "variable"
                };
                let mut cursor = n.walk();
                let mut specs: Vec<Node<'t>> = Vec::new();
                for child in n.named_children(&mut cursor) {
                    match child.kind() {
                        "const_spec" | "var_spec" => specs.push(child),
                        "var_spec_list" => {
                            let mut c2 = child.walk();
                            specs.extend(child.named_children(&mut c2));
                        }
                        _ => {}
                    }
                }
                let grouped = n
                    .children(&mut cursor)
                    .any(|c| matches!(c.kind(), "(" | "var_spec_list"));
                for spec in specs {
                    let mut c3 = spec.walk();
                    let names: Vec<&str> = spec
                        .children_by_field_name("name", &mut c3)
                        .map(|c| self.text(c))
                        .collect();
                    for name in names {
                        let at = if grouped { spec } else { n };
                        self.add(kind, name, parent.clone(), at);
                    }
                }
            }
            _ => return false,
        }
        true
    }

    /// The text of every descendant of `node` of `kind`, in order.
    fn all_of_kind(&self, node: Node<'_>, kind: &str, out: &mut Vec<String>) {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == kind {
                out.push(self.text(n).to_string());
                continue;
            }
            let mut cursor = n.walk();
            let children: Vec<Node<'_>> = n.named_children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
    }

    fn first_of_kind<'t>(&self, node: Node<'t>, kind: &str) -> Option<Node<'t>> {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == kind {
                return Some(n);
            }
            let mut cursor = n.walk();
            let children: Vec<Node<'t>> = n.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        None
    }

    // ---- Shell

    fn shell<'t>(&mut self, f: Frame<'t>, stack: &mut Stack<'t>) -> bool {
        let n = f.node;
        if n.kind() != "function_definition" {
            return false;
        }
        let Some(name) = self.field(n, "name") else {
            return false;
        };
        let parent = self.parent_name(f.parent);
        let idx = self.add("function", name, parent, n);
        self.descend(n, idx, Scope::Nested, stack);
        true
    }

    // ---- YAML

    /// The keys of each document's top-level mapping.
    fn yaml(&mut self, f: Frame<'_>) -> bool {
        let n = f.node;
        if n.kind() != "document" {
            return false;
        }
        let mut cursor = n.walk();
        let block = n
            .named_children(&mut cursor)
            .find(|c| c.kind() == "block_node");
        let top = block.and_then(|b| {
            let mut c = b.walk();
            let found = b
                .named_children(&mut c)
                .find(|m| m.kind() == "block_mapping");
            found
        });
        if let Some(mapping) = top {
            let mut c3 = mapping.walk();
            let pairs: Vec<Node<'_>> = mapping
                .named_children(&mut c3)
                .filter(|p| p.kind() == "block_mapping_pair")
                .collect();
            for pair in pairs {
                let Some(key) = pair.child_by_field_name("key") else {
                    continue;
                };
                let name = self.text(key).trim().trim_matches(['"', '\'']).to_string();
                if !name.is_empty() {
                    self.add("key", &name, None, pair);
                }
            }
        }
        true
    }

    // ---- TOML

    /// Top-level keys and `[table]` / `[[array-table]]` headers.
    fn toml(&mut self, f: Frame<'_>) -> bool {
        let n = f.node;
        match n.kind() {
            "pair" if n.parent().is_some_and(|p| p.kind() == "document") => {
                if let Some(key) = self.toml_key(n) {
                    self.add("key", &key, None, n);
                }
                true
            }
            "table" | "table_array_element" => {
                if let Some(key) = self.toml_key(n) {
                    let kind = if n.kind() == "table" {
                        "table"
                    } else {
                        "array_table"
                    };
                    self.add(kind, &key, None, n);
                }
                true
            }
            _ => false,
        }
    }

    fn toml_key(&self, node: Node<'_>) -> Option<String> {
        let mut cursor = node.walk();
        let key = node
            .named_children(&mut cursor)
            .find(|c| matches!(c.kind(), "bare_key" | "dotted_key" | "quoted_key"))?;
        let text = self.text(key).trim();
        Some(text.trim_matches(['"', '\'']).to_string()).filter(|t| !t.is_empty())
    }

    // ---- C / C++

    fn c<'t>(&mut self, f: Frame<'t>, stack: &mut Stack<'t>) -> bool {
        let n = f.node;
        let parent = self.parent_name(f.parent);
        let cpp = self.lang == Language::Cpp;
        match n.kind() {
            "function_definition" => {
                let Some(decl) = n.child_by_field_name("declarator") else {
                    return false;
                };
                let Some((name, scope_name)) = self.c_declarator_name(decl) else {
                    return false;
                };
                let (kind, parent) = match scope_name {
                    Some(s) => ("method", Some(s)),
                    None if f.scope == Scope::Body => ("method", parent),
                    None => ("function", parent),
                };
                let idx = self.add(kind, &name, parent, n);
                self.descend(n, idx, Scope::Nested, stack);
            }
            "struct_specifier" | "union_specifier" | "class_specifier" | "enum_specifier" => {
                let (Some(name), Some(_body)) =
                    (self.field(n, "name"), n.child_by_field_name("body"))
                else {
                    return false;
                };
                let kind = match n.kind() {
                    "enum_specifier" => "enum",
                    "union_specifier" => "union",
                    "class_specifier" => "class",
                    _ => "struct",
                };
                let idx = self.add(kind, name, parent, n);
                self.descend(n, idx, Scope::Body, stack);
            }
            "namespace_definition" if cpp => {
                let Some(name) = self.field(n, "name") else {
                    // An anonymous namespace holds top-level code.
                    self.push_children(n, f.parent, Scope::Top, stack);
                    return true;
                };
                let idx = self.add("mod", name, parent, n);
                self.descend(n, idx, Scope::Top, stack);
            }
            "alias_declaration" if cpp => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                self.add("type", name, parent, n);
            }
            "type_definition" => {
                if let Some((name, _)) = n
                    .child_by_field_name("declarator")
                    .and_then(|d| self.c_declarator_name(d))
                {
                    self.add("type", &name, parent, n);
                }
                // `typedef struct Named { ... } T;` also defines Named.
                self.push_children(n, f.parent, f.scope, stack);
            }
            "field_declaration" | "declaration" => {
                // A C++ constructor prototype is a `declaration`, not a
                // `field_declaration`, inside a class body.
                let in_body = f.scope == Scope::Body;
                if !in_body && !(f.scope == Scope::Top && n.kind() == "declaration") {
                    return false;
                }
                let mut cursor = n.walk();
                let storage: Vec<&str> = n
                    .children(&mut cursor)
                    .filter(|c| c.kind() == "storage_class_specifier")
                    .map(|c| self.text(c))
                    .collect();
                let is_const = n
                    .children(&mut cursor)
                    .any(|c| c.kind() == "type_qualifier" && self.text(c) == "const");
                let declarators: Vec<Node<'t>> = n
                    .children_by_field_name("declarator", &mut cursor)
                    .collect();
                for decl in declarators {
                    let (inner, is_function) = self.c_unwrap(decl);
                    let Some((name, scope_name)) = self.c_declarator_name(inner) else {
                        continue;
                    };
                    if is_function {
                        // A prototype: a method inside a class body,
                        // nothing at file level.
                        if in_body {
                            self.add("method", &name, scope_name.or_else(|| parent.clone()), n);
                        }
                    } else if !in_body && !storage.contains(&"extern") {
                        let kind = if is_const {
                            "const"
                        } else if storage.contains(&"static") {
                            "static"
                        } else {
                            "variable"
                        };
                        self.add(kind, &name, parent.clone(), n);
                    }
                }
                // A `struct S { ... } s;` declares its struct too.
                self.push_children(n, f.parent, f.scope, stack);
            }
            _ => return false,
        }
        true
    }

    /// Strips pointer / reference / array / init wrappers off a C
    /// declarator. The flag says whether a function declarator was
    /// found directly (a prototype or function), not a function
    /// pointer variable.
    fn c_unwrap<'t>(&self, mut node: Node<'t>) -> (Node<'t>, bool) {
        let mut is_function = false;
        loop {
            match node.kind() {
                "init_declarator"
                | "pointer_declarator"
                | "reference_declarator"
                | "array_declarator"
                | "attributed_declarator" => {
                    match node
                        .child_by_field_name("declarator")
                        .or_else(|| node.named_child(0))
                    {
                        Some(inner) => node = inner,
                        None => return (node, is_function),
                    }
                }
                "function_declarator" => {
                    let Some(inner) = node.child_by_field_name("declarator") else {
                        return (node, is_function);
                    };
                    // `(*fp)(int)` is a pointer variable.
                    is_function = inner.kind() != "parenthesized_declarator";
                    node = inner;
                }
                "parenthesized_declarator" => match node.named_child(0) {
                    Some(inner) => node = inner,
                    None => return (node, is_function),
                },
                _ => return (node, is_function),
            }
        }
    }

    /// `(name, scope)` of a C/C++ declarator: `scope` is the `C` of
    /// `C::m`.
    fn c_declarator_name(&self, decl: Node<'_>) -> Option<(String, Option<String>)> {
        let (node, _) = self.c_unwrap(decl);
        match node.kind() {
            "identifier" | "field_identifier" | "type_identifier" | "destructor_name"
            | "operator_name" => Some((self.text(node).to_string(), None)),
            "qualified_identifier" => {
                let scope = node
                    .child_by_field_name("scope")
                    .map(|s| self.text(s).trim_end_matches("::").to_string());
                let name = node.child_by_field_name("name")?;
                let (inner, _) = self.c_declarator_name(name)?;
                Some((inner, scope))
            }
            "template_function" => {
                let name = node.child_by_field_name("name")?;
                Some((self.text(name).to_string(), None))
            }
            _ => None,
        }
    }

    // ---- Java

    fn java<'t>(&mut self, f: Frame<'t>, stack: &mut Stack<'t>) -> bool {
        let n = f.node;
        let parent = self.parent_name(f.parent);
        let (kind, scope) = match n.kind() {
            "class_declaration" => ("class", Scope::Body),
            "interface_declaration" | "annotation_type_declaration" => ("interface", Scope::Body),
            "enum_declaration" => ("enum", Scope::Body),
            "record_declaration" => ("record", Scope::Body),
            "method_declaration" | "constructor_declaration" => ("method", Scope::Nested),
            _ => return false,
        };
        let Some(name) = self.field(n, "name") else {
            return false;
        };
        let idx = self.add(kind, name, parent, n);
        self.descend(n, idx, scope, stack);
        true
    }

    // ---- Swift

    fn swift<'t>(&mut self, f: Frame<'t>, stack: &mut Stack<'t>) -> bool {
        let n = f.node;
        let parent = self.parent_name(f.parent);
        match n.kind() {
            "class_declaration" => {
                let Some(name) = self.swift_type_name(n) else {
                    return false;
                };
                // The grammar uses one node for class / struct / enum /
                // actor / extension; the keyword says which.
                let kind = match self.swift_declaration_keyword(n) {
                    "struct" => "struct",
                    "enum" => "enum",
                    "extension" => "impl",
                    _ => "class",
                };
                let idx = self.add(kind, &name, parent, n);
                self.descend(n, idx, Scope::Body, stack);
            }
            "protocol_declaration" => {
                let Some(name) = self.swift_type_name(n) else {
                    return false;
                };
                let idx = self.add("interface", &name, parent, n);
                self.descend(n, idx, Scope::Body, stack);
            }
            "function_declaration" | "protocol_function_declaration" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                let kind = if f.scope == Scope::Body {
                    "method"
                } else {
                    "function"
                };
                let idx = self.add(kind, name, parent, n);
                self.descend(n, idx, Scope::Nested, stack);
            }
            "init_declaration" | "deinit_declaration" | "subscript_declaration" => {
                let name = match n.kind() {
                    "init_declaration" => "init",
                    "deinit_declaration" => "deinit",
                    _ => "subscript",
                };
                let idx = self.add("method", name, parent, n);
                self.descend(n, idx, Scope::Nested, stack);
            }
            "typealias_declaration" => {
                let Some(name) = self.field(n, "name") else {
                    return false;
                };
                self.add("type", name, parent, n);
            }
            "property_declaration" if f.scope == Scope::Top => {
                let is_const = {
                    let mut cursor = n.walk();
                    let found = n
                        .named_children(&mut cursor)
                        .find(|c| c.kind() == "value_binding_pattern")
                        .is_some_and(|c| self.text(c).trim_start().starts_with("let"));
                    found
                };
                let kind = if is_const { "const" } else { "variable" };
                let mut cursor = n.walk();
                let patterns: Vec<Node<'t>> =
                    n.children_by_field_name("name", &mut cursor).collect();
                let mut names: Vec<String> = Vec::new();
                for pattern in patterns {
                    self.all_of_kind(pattern, "simple_identifier", &mut names);
                }
                for name in names {
                    self.add(kind, &name, parent.clone(), n);
                }
                self.push_children(n, f.parent, Scope::Nested, stack);
            }
            _ => return false,
        }
        true
    }

    fn swift_declaration_keyword(&self, n: Node<'_>) -> &'a str {
        n.child_by_field_name("declaration_kind")
            .map(|k| self.text(k))
            .unwrap_or("class")
    }

    fn swift_type_name(&self, n: Node<'_>) -> Option<String> {
        let name = n.child_by_field_name("name")?;
        // `extension Array where ...` names a `user_type`.
        let id = if name.kind() == "type_identifier" || name.kind() == "simple_identifier" {
            Some(name)
        } else {
            self.first_of_kind(name, "type_identifier")
        };
        let text = self.text(id.unwrap_or(name)).trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    // ---- Lua

    fn lua<'t>(&mut self, f: Frame<'t>, stack: &mut Stack<'t>) -> bool {
        let n = f.node;
        let parent = self.parent_name(f.parent);
        match n.kind() {
            "function_declaration" => {
                let Some(name_node) = n.child_by_field_name("name") else {
                    return false;
                };
                let full = self.text(name_node);
                let split = full.rfind(['.', ':']);
                let (kind, name, owner) = match split {
                    Some(i) => ("method", &full[i + 1..], Some(full[..i].to_string())),
                    None => ("function", full, parent),
                };
                let idx = self.add(kind, name, owner, n);
                self.descend(n, idx, Scope::Nested, stack);
            }
            "variable_declaration" | "assignment_statement" if f.scope == Scope::Top => {
                self.lua_assignment(n, parent);
                self.push_children(n, f.parent, Scope::Nested, stack);
            }
            _ => return false,
        }
        true
    }

    fn lua_assignment(&mut self, n: Node<'_>, parent: Option<String>) {
        // `local a, b = 1, 2` wraps an assignment_statement.
        let assignment = if n.kind() == "variable_declaration" {
            let mut cursor = n.walk();
            let found = n
                .named_children(&mut cursor)
                .find(|c| c.kind() == "assignment_statement");
            found
        } else {
            Some(n)
        };
        let Some(assignment) = assignment else {
            // `local x` with no value: the names are in a `variable_list`.
            let mut cursor = n.walk();
            let mut names: Vec<String> = Vec::new();
            for list in n
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "variable_list")
            {
                let mut c2 = list.walk();
                names.extend(
                    list.named_children(&mut c2)
                        .filter(|c| c.kind() == "identifier")
                        .map(|c| self.text(c).to_string()),
                );
            }
            for name in names {
                self.add("variable", &name, parent.clone(), n);
            }
            return;
        };
        let mut cursor = assignment.walk();
        let mut targets: Vec<Node<'_>> = Vec::new();
        let mut values: Vec<Node<'_>> = Vec::new();
        for child in assignment.named_children(&mut cursor) {
            match child.kind() {
                "variable_list" => {
                    let mut c2 = child.walk();
                    targets.extend(child.named_children(&mut c2));
                }
                "expression_list" => {
                    let mut c2 = child.walk();
                    values.extend(child.named_children(&mut c2));
                }
                _ => {}
            }
        }
        for (i, target) in targets.iter().enumerate() {
            if target.kind() != "identifier" {
                continue; // `M.x = 1`, `t[1] = 2`
            }
            let is_function = values
                .get(i)
                .is_some_and(|v| v.kind() == "function_definition");
            let kind = if is_function { "function" } else { "variable" };
            let name = self.text(*target).to_string();
            self.add(kind, &name, parent.clone(), n);
        }
    }
}

/// A node whose body is code run later: what it holds is never a
/// top-level symbol or a member.
fn is_closure_kind(lang: Language, kind: &str) -> bool {
    match lang {
        Language::Rust => kind == "closure_expression",
        Language::Python => kind == "lambda",
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx => matches!(
            kind,
            "arrow_function" | "function_expression" | "function" | "generator_function" | "class"
        ),
        Language::Go => kind == "func_literal",
        Language::Java => kind == "lambda_expression",
        Language::Cpp => kind == "lambda_expression",
        Language::Swift => kind == "lambda_literal",
        Language::Lua => kind == "function_definition",
        Language::C | Language::Shell | Language::Yaml | Language::Toml => false,
    }
}

// --------------------------------------------------------- block IDs

/// The `^id` that ends a comment line, if any. `line` is one line of a
/// comment node's text.
pub(crate) fn trailing_id(lang: Language, line: &str) -> Option<&str> {
    let mut text = line.trim_end();
    // A block comment's closer is not part of the line's content.
    loop {
        let stripped = match lang {
            Language::Python | Language::Shell | Language::Yaml | Language::Toml => None,
            Language::Lua => lua_closer(text),
            _ => text.strip_suffix("*/"),
        };
        match stripped {
            Some(rest) => text = rest.trim_end(),
            None => break,
        }
    }
    let caret = text.rfind('^')?;
    let id = &text[caret + 1..];
    if id.is_empty() || !id.chars().all(is_block_id_char) {
        return None;
    }
    // A standalone token: whitespace (or nothing but comment markers,
    // as in `//^id`) before it.
    let before = &text[..caret];
    let ok = before.chars().last().is_none_or(|c| c.is_whitespace())
        || before.chars().all(|c| "/*#-!;<>=[]".contains(c));
    ok.then_some(id)
}

/// `text` without a trailing Lua long-bracket closer (`]]`, `]=]`).
fn lua_closer(text: &str) -> Option<&str> {
    let body = text.strip_suffix(']')?;
    let without_equals = body.trim_end_matches('=');
    without_equals.strip_suffix(']')
}

/// Every `^id` of every comment, attached to the symbol that owns it.
fn own_block_ids(
    lang: Language,
    comments: &[CommentInfo],
    src: &str,
    symbols: &[Symbol],
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<BlockAnchor> {
    // Row -> the first (outermost) symbol whose first line it is, and
    // row -> the symbol whose leading lines (attributes, doc comments)
    // it is one of.
    let mut by_first_line: HashMap<usize, usize> = HashMap::new();
    let mut by_leading_line: HashMap<usize, usize> = HashMap::new();
    for (i, s) in symbols.iter().enumerate() {
        by_first_line.entry(s.line).or_insert(i);
        for row in s.start_line..s.line {
            by_leading_line.entry(row).or_insert(i);
        }
    }

    let mut out: Vec<BlockAnchor> = Vec::new();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for comment in comments {
        let Some(text) = src.get(comment.start_byte..comment.end_byte) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            let Some(id) = trailing_id(lang, line) else {
                continue;
            };
            let row = comment.first_row + i;
            let owner = by_first_line
                .get(&row)
                .or_else(|| by_leading_line.get(&row))
                .copied();
            match seen.get(id) {
                Some(first) => diagnostics.push(Diagnostic::new(
                    DiagnosticKind::DuplicateBlockId,
                    row,
                    format!("duplicate block id \"^{id}\" (first seen on line {first})"),
                )),
                None => {
                    seen.insert(id, row);
                }
            }
            out.push(BlockAnchor {
                block_id: id.to_string(),
                line: owner.map_or(row, |o| symbols[o].line),
                symbol: owner,
                token_line: row,
            });
        }
    }
    // Comments were collected in tree order, which is file order.
    out.sort_by_key(|b| b.line);
    out
}
