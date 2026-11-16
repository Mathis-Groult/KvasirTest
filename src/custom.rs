// Copyright 2026 Mathis Groult
// SPDX-License-Identifier: GPL-3.0-or-later (KvasirTest; see NOTICE)

//! Hand-written mutants from spec files (`--custom-mutants`), tested in the same schema build as
//! the generated ones.
//!
//! A spec is TOML, one table per mutant:
//!
//! ```toml
//! [[mutant]]
//! name = "swap_xy"                    # unique across all specs; shown in results
//! file = "src/display.rs"             # relative to the workspace root
//! old = "Vec3(t.x, t.y, t.z)"         # exact text, found exactly once in the file
//! new = "Vec3(t.y, t.x, t.z)"
//! ```
//!
//! A custom mutant is a plain textual edit. Its mutated span is the edit trimmed to the tokens it
//! really changes (whole tokens, so the classic marker comment never lands inside a literal), and
//! its schema site is the smallest enclosing expression or statement list that can hold a runtime
//! switch. The variant is the site's text with the edit applied, so the schema runs exactly the
//! code a classic build of the edit would. Edits with no such site (const contexts, items, code
//! outside functions) and edits the schema cannot compile run the classic way.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context as _, bail, ensure};
use camino::{Utf8Path, Utf8PathBuf};
use proc_macro2::{Spacing, TokenStream, TokenTree};
use serde::Deserialize;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{Block, Expr, Stmt};

use crate::Result;
use crate::mutant::{Function, Genre, Mutant, MutationTarget};
use crate::pretty::ToPrettyString;
use crate::schema::{LineIndex, Site, SiteKind};
use crate::source::SourceFile;
use crate::span::{LineColumn, Span};
use crate::workspace::Workspace;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpecFile {
    #[serde(default)]
    mutant: Vec<SpecMutant>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpecMutant {
    name: String,
    file: Utf8PathBuf,
    old: String,
    new: String,
}

/// Read every spec and build its mutants, in spec order.
pub fn load(spec_paths: &[Utf8PathBuf], workspace: &Workspace) -> Result<Vec<Mutant>> {
    let mut specs = Vec::new();
    for path in spec_paths {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read custom mutants {path}"))?;
        let spec: SpecFile =
            toml::from_str(&text).with_context(|| format!("parse custom mutants {path}"))?;
        specs.extend(spec.mutant);
    }
    let mut names = BTreeSet::new();
    for spec in &specs {
        ensure!(
            names.insert(spec.name.as_str()),
            "custom mutant name {:?} is used twice",
            spec.name
        );
    }
    let mut files: BTreeMap<&Utf8Path, SourceFile> = BTreeMap::new();
    let mut mutants = Vec::with_capacity(specs.len());
    for spec in &specs {
        if !files.contains_key(spec.file.as_path()) {
            files.insert(&spec.file, load_source(workspace, &spec.file)?);
        }
        let source_file = &files[spec.file.as_path()];
        mutants.push(
            custom_mutant(source_file, &spec.name, &spec.old, &spec.new)
                .with_context(|| format!("custom mutant {:?} in {}", spec.name, spec.file))?,
        );
    }
    Ok(mutants)
}

/// Load `path` as a source file of the package whose directory holds it (the deepest one).
fn load_source(workspace: &Workspace, path: &Utf8Path) -> Result<SourceFile> {
    let package = workspace
        .packages()
        .iter()
        .filter(|p| path.starts_with(&p.relative_dir))
        .max_by_key(|p| p.relative_dir.components().count())
        .with_context(|| format!("{path} is not inside any package of the workspace"))?;
    SourceFile::load(workspace.root(), path, package, false)?
        .with_context(|| format!("{path} is outside the workspace"))
}

/// Build one custom mutant replacing the single occurrence of `old` in the file with `new`.
fn custom_mutant(source_file: &SourceFile, name: &str, old: &str, new: &str) -> Result<Mutant> {
    let code = source_file.code();
    let mut found = code.match_indices(old);
    let (Some((at, _)), None) = (found.next(), found.next()) else {
        bail!("the old text must occur exactly once in the file");
    };
    ensure!(old != new, "old and new texts are the same");
    let tokens = token_ranges(code).context("tokenize the file")?;
    let (range, replacement) = edit_range(code, at, old, new, &tokens);
    let syn_file = syn::parse_str::<syn::File>(code).context("parse the file")?;
    let index = LineIndex::new(code);
    let mut finder = SiteFinder {
        code,
        index: &index,
        target: range.clone(),
        const_depth: 0,
        skip_next: false,
        namespace: Vec::new(),
        fns: Vec::new(),
        best: None,
        function: None,
    };
    finder.visit_file(&syn_file);
    let site = finder.best.map(|r| Site {
        kind: SiteKind::Expr,
        span: span_of(code, r),
    });
    Ok(Mutant::new_discovered(
        source_file.clone(),
        finder.function,
        span_of(code, range),
        None,
        replacement,
        Genre::Custom,
        Some(MutationTarget::Custom {
            name: name.to_owned(),
        }),
        site,
    ))
}

/// The byte range of the original code the edit changes, widened to whole tokens, and the text
/// that replaces it.
fn edit_range(
    code: &str,
    at: usize,
    old: &str,
    new: &str,
    tokens: &[Range<usize>],
) -> (Range<usize>, String) {
    let prefix = common_prefix(old, new);
    let suffix = common_suffix(&old[prefix..], &new[prefix..]);
    let mut range = at + prefix..at + old.len() - suffix;
    for token in tokens {
        let overlaps = token.start < range.end && range.start < token.end;
        let splits = token.start < range.start && range.start < token.end;
        if overlaps || splits {
            range.start = range.start.min(token.start);
            range.end = range.end.max(token.end);
        }
    }
    // The same range in the edited file: the code outside the edit is unchanged.
    let edited = format!("{}{new}{}", &code[..at], &code[at + old.len()..]);
    let replacement = edited[range.start..range.end + new.len() - old.len()].to_owned();
    (range, replacement)
}

/// Length in bytes of the longest common prefix, on a char boundary.
fn common_prefix(a: &str, b: &str) -> usize {
    a.char_indices()
        .zip(b.chars())
        .find(|((_, x), y)| x != y)
        .map_or(a.len().min(b.len()), |((i, _), _)| i)
}

/// Length in bytes of the longest common suffix, on a char boundary.
fn common_suffix(a: &str, b: &str) -> usize {
    a.chars()
        .rev()
        .zip(b.chars().rev())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum()
}

/// Byte ranges of every leaf token and group delimiter of `code`.
fn token_ranges(code: &str) -> Result<Vec<Range<usize>>> {
    fn walk(stream: TokenStream, index: &LineIndex, code: &str, out: &mut Vec<Range<usize>>) {
        let range = |span: proc_macro2::Span| {
            Some(index.offset(code, span.start().into())?..index.offset(code, span.end().into())?)
        };
        // A joint punctuation sequence (`!=`, `&&`, `..=`) is one operator: keep it whole.
        let mut joint: Option<Range<usize>> = None;
        for tree in stream {
            let Some(mut r) = range(tree.span()) else {
                continue;
            };
            if let Some(j) = joint.take() {
                r.start = j.start;
            }
            match tree {
                TokenTree::Group(g) => {
                    out.extend(range(g.span_open()));
                    out.extend(range(g.span_close()));
                    walk(g.stream(), index, code, out);
                }
                TokenTree::Punct(p) if p.spacing() == Spacing::Joint => joint = Some(r),
                _ => out.push(r),
            }
        }
        out.extend(joint);
    }
    let stream = TokenStream::from_str(code).map_err(|e| anyhow::anyhow!("{e}"))?;
    let index = LineIndex::new(code);
    let mut out = Vec::new();
    walk(stream, &index, code, &mut out);
    Ok(out)
}

/// The [`Span`] of a byte range (columns in chars, not counting `\r`, as [`LineIndex`] reads them).
fn span_of(code: &str, range: Range<usize>) -> Span {
    Span {
        start: position(code, range.start),
        end: position(code, range.end),
    }
}

fn position(code: &str, byte: usize) -> LineColumn {
    let before = &code[..byte];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    LineColumn {
        line: before.matches('\n').count() + 1,
        column: code[line_start..byte]
            .chars()
            .filter(|&c| c != '\r')
            .count()
            + 1,
    }
}

/// Finds the innermost site that contains the target range and can hold a runtime switch.
struct SiteFinder<'a> {
    code: &'a str,
    index: &'a LineIndex,
    target: Range<usize>,
    /// Const contexts, patterns, types and attributes enclosing the node: no switch possible.
    const_depth: usize,
    /// The next expression visited sits where `( .. )` would not parse (a block-like expression
    /// statement before other statements, a match arm body without a comma).
    skip_next: bool,
    namespace: Vec<String>,
    fns: Vec<Arc<Function>>,
    best: Option<Range<usize>>,
    function: Option<Arc<Function>>,
}

impl SiteFinder<'_> {
    fn range(&self, span: proc_macro2::Span) -> Option<Range<usize>> {
        let start = self.index.offset(self.code, span.start().into())?;
        let end = self.index.offset(self.code, span.end().into())?;
        (start <= end).then_some(start..end)
    }

    fn consider(&mut self, range: Option<Range<usize>>) {
        if self.const_depth > 0 || self.fns.is_empty() {
            return;
        }
        if let Some(range) = range
            && range.start <= self.target.start
            && self.target.end <= range.end
        {
            self.best = Some(range);
            self.function = self.fns.last().cloned();
        }
    }

    fn in_const(&mut self, f: impl FnOnce(&mut Self)) {
        self.const_depth += 1;
        f(self);
        self.const_depth -= 1;
    }

    fn in_namespace(&mut self, name: String, f: impl FnOnce(&mut Self)) {
        self.namespace.push(name);
        f(self);
        self.namespace.pop();
    }

    fn in_fn(&mut self, sig: &syn::Signature, span: proc_macro2::Span, f: impl FnOnce(&mut Self)) {
        self.namespace.push(sig.ident.to_string());
        self.fns.push(Arc::new(Function {
            function_name: self.namespace.join("::"),
            return_type: sig.output.to_pretty_string(),
            span: span.into(),
        }));
        let constness = usize::from(sig.constness.is_some());
        self.const_depth += constness;
        f(self);
        self.const_depth -= constness;
        self.fns.pop();
        self.namespace.pop();
    }
}

/// Expressions a switch must not wrap: places (wrapping one copies it into a temporary, so writes
/// through it would be lost), `let` conditions, and expressions with attributes.
fn wrappable(expr: &Expr) -> bool {
    let has_attrs = match expr {
        Expr::Call(e) => !e.attrs.is_empty(),
        Expr::MethodCall(e) => !e.attrs.is_empty(),
        Expr::Lit(e) => !e.attrs.is_empty(),
        Expr::Binary(e) => !e.attrs.is_empty(),
        Expr::Block(e) => !e.attrs.is_empty(),
        Expr::Macro(e) => !e.attrs.is_empty(),
        _ => false,
    };
    !has_attrs
        && !matches!(
            expr,
            Expr::Path(_)
                | Expr::Field(_)
                | Expr::Index(_)
                | Expr::Let(_)
                | Expr::Paren(_)
                | Expr::Infer(_)
                | Expr::Verbatim(_)
                | Expr::Unary(syn::ExprUnary {
                    op: syn::UnOp::Deref(_),
                    ..
                })
        )
}

impl<'ast> Visit<'ast> for SiteFinder<'_> {
    fn visit_expr(&mut self, i: &'ast Expr) {
        let skip = std::mem::take(&mut self.skip_next);
        if !skip && wrappable(i) {
            self.consider(self.range(i.span()));
        }
        syn::visit::visit_expr(self, i);
    }

    fn visit_block(&mut self, i: &'ast Block) {
        // Everything between the braces, so edits of the whitespace around statements fit.
        let range = self
            .range(i.brace_token.span.open())
            .zip(self.range(i.brace_token.span.close()))
            .map(|(open, close)| open.end..close.start);
        self.consider(range);
        let n = i.stmts.len();
        for (k, stmt) in i.stmts.iter().enumerate() {
            if let Stmt::Expr(expr, None) = stmt
                && k + 1 < n
            {
                self.skip_next = true;
                self.visit_expr(expr);
            } else {
                self.visit_stmt(stmt);
            }
        }
    }

    fn visit_arm(&mut self, i: &'ast syn::Arm) {
        self.in_const(|v| v.visit_pat(&i.pat));
        if let Some((_, guard)) = &i.guard {
            v_expr(self, guard);
        }
        self.skip_next = i.comma.is_none();
        self.visit_expr(&i.body);
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.in_fn(&i.sig, i.span(), |v| v.visit_block(&i.block));
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.in_fn(&i.sig, i.span(), |v| v.visit_block(&i.block));
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        if let Some(block) = &i.default {
            self.in_fn(&i.sig, i.span(), |v| v.visit_block(block));
        }
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let type_name = i.self_ty.to_pretty_string();
        let name = match &i.trait_ {
            Some((_, path, _)) => format!(
                "<impl {trait_} for {type_name}>",
                trait_ = path.to_pretty_string()
            ),
            None => type_name,
        };
        self.in_namespace(name, |v| syn::visit::visit_item_impl(v, i));
    }

    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        self.in_namespace(i.ident.to_string(), |v| syn::visit::visit_item_trait(v, i));
    }

    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.in_namespace(i.ident.to_string(), |v| syn::visit::visit_item_mod(v, i));
    }

    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.in_const(|v| syn::visit::visit_item_const(v, i));
    }

    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.in_const(|v| syn::visit::visit_item_static(v, i));
    }

    fn visit_impl_item_const(&mut self, i: &'ast syn::ImplItemConst) {
        self.in_const(|v| syn::visit::visit_impl_item_const(v, i));
    }

    fn visit_trait_item_const(&mut self, i: &'ast syn::TraitItemConst) {
        self.in_const(|v| syn::visit::visit_trait_item_const(v, i));
    }

    fn visit_expr_const(&mut self, i: &'ast syn::ExprConst) {
        self.in_const(|v| syn::visit::visit_expr_const(v, i));
    }

    fn visit_expr_repeat(&mut self, i: &'ast syn::ExprRepeat) {
        self.visit_expr(&i.expr);
        self.in_const(|v| v.visit_expr(&i.len));
    }

    fn visit_variant(&mut self, i: &'ast syn::Variant) {
        self.in_const(|v| syn::visit::visit_variant(v, i));
    }

    fn visit_generic_argument(&mut self, i: &'ast syn::GenericArgument) {
        self.in_const(|v| syn::visit::visit_generic_argument(v, i));
    }

    fn visit_type(&mut self, i: &'ast syn::Type) {
        self.in_const(|v| syn::visit::visit_type(v, i));
    }

    fn visit_pat(&mut self, i: &'ast syn::Pat) {
        self.in_const(|v| syn::visit::visit_pat(v, i));
    }

    fn visit_attribute(&mut self, _i: &'ast syn::Attribute) {}
}

/// Visit an expression that is a candidate in its own right (resets a pending skip).
fn v_expr(v: &mut SiteFinder<'_>, expr: &Expr) {
    v.skip_next = false;
    v.visit_expr(expr);
}

#[cfg(test)]
mod test {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::schema::{SchemaMutant, render};

    /// The mutant of one edit in `code`, with its site rendered as text (`None` = classic).
    fn mutant(code: &str, old: &str, new: &str) -> (Mutant, Option<String>) {
        let file = SourceFile::for_tests("src/main.rs", code, "custom-test", true);
        let m = custom_mutant(&file, "m", old, new).expect("mutant");
        let site = m.site.map(|s| s.span.extract(code));
        (m, site)
    }

    #[test]
    fn swapped_arguments_switch_the_innermost_call() {
        let code = "fn f(t: V) -> W {\n    let w = W::new(g(t.x, t.y, t.z), 2);\n    w\n}\n";
        let (m, site) = mutant(code, "g(t.x, t.y, t.z)", "g(t.y, t.x, t.z)");
        assert_eq!(m.original_text(), "x, t.y");
        assert_eq!(m.replacement, "y, t.x");
        assert_eq!(site.as_deref(), Some("g(t.x, t.y, t.z)"));
        assert_eq!(
            m.name,
            "src/main.rs:2:24: custom m: replace x, t.y with y, t.x in f"
        );
        assert_eq!(
            m.mutated_code()
                .replace(crate::MUTATION_MARKER_COMMENT, "/**/"),
            "fn f(t: V) -> W {\n    let w = W::new(g(t.y, t.x /**/, t.z), 2);\n    w\n}\n"
        );
    }

    #[test]
    fn edits_widen_to_whole_tokens_so_literals_stay_intact() {
        let code = "fn f(o: Option<f32>) -> f32 {\n    o.unwrap_or(1.0)\n}\n";
        let (m, site) = mutant(code, "unwrap_or(1.0)", "unwrap_or(0.0)");
        assert_eq!(m.original_text(), "1.0");
        assert_eq!(m.replacement, "0.0");
        assert_eq!(site.as_deref(), Some("1.0"));

        let code = "fn f() -> &'static str {\n    \"vertical\"\n}\n";
        let (m, _) = mutant(code, "\"vertical\"", "\"fixed\"");
        assert_eq!(m.original_text(), "\"vertical\"");
        assert_eq!(m.replacement, "\"fixed\"");
    }

    #[test]
    fn joint_operators_stay_whole() {
        let code = "fn f(a: i8) -> bool {\n    a != -1\n}\n";
        let (m, _) = mutant(code, "a != -1", "a == -1");
        assert_eq!(m.original_text(), "!=");
        assert_eq!(m.replacement, "==");
    }

    #[test]
    fn a_replaced_variable_goes_up_from_the_place_to_the_value_use() {
        let code = "fn f(e: &E, a: f32, b: f32) {\n    e.set(1, a);\n    e.set(2, b);\n}\n";
        let (m, site) = mutant(code, "e.set(1, a)", "e.set(1, b)");
        assert_eq!(m.original_text(), "a");
        assert_eq!(site.as_deref(), Some("e.set(1, a)"));
    }

    #[test]
    fn a_deleted_statement_switches_the_statement_list() {
        let code = "fn f(s: &S) {\n    s.a();\n    s.b();\n}\n";
        let (m, site) = mutant(code, "    s.a();\n", "");
        assert_eq!(m.replacement, "");
        assert_eq!(site.as_deref(), Some("\n    s.a();\n    s.b();\n"));
    }

    #[test]
    fn block_like_statements_before_others_are_not_wrapped_in_parens() {
        let code = "fn f(x: bool) -> u8 {\n    if x { g() } else { h() }\n    1\n}\n";
        let (_, site) = mutant(code, "if x {", "if !x {");
        // The `if` statement cannot become `( .. )` followed by more statements; its parent
        // statement list holds the switch.
        assert_eq!(
            site.as_deref(),
            Some("\n    if x { g() } else { h() }\n    1\n")
        );
    }

    #[test]
    fn match_arm_bodies_without_comma_go_to_the_match() {
        let code =
            "fn f(x: u8) -> u8 {\n    match x {\n        0 => { 1 }\n        _ => 2,\n    }\n}\n";
        let (_, site) = mutant(code, "{ 1 }", "{ 3 }");
        // `{ 1 }` itself is a block-like arm body without comma; the literal inside is fine.
        assert_eq!(site.as_deref(), Some("1"));
        let (_, site) = mutant(code, "0 => { 1 }", "1 => { 1 }");
        assert_eq!(
            site.as_deref().map(|s| s.starts_with("match x")),
            Some(true)
        );
    }

    #[test]
    fn const_contexts_and_code_outside_functions_get_no_site() {
        let code = "const N: usize = 3;\nfn f() -> [u8; 2] {\n    [0; 2]\n}\n";
        assert_eq!(mutant(code, "= 3", "= 4").1, None);
        // An array length is const, so the whole array expression holds the switch.
        assert_eq!(
            mutant(code, "[0; 2]", "[0; 3]").1.as_deref(),
            Some("[0; 2]")
        );
        let code = "const fn f() -> u8 {\n    1 + 2\n}\n";
        assert_eq!(mutant(code, "1 + 2", "1 - 2").1, None);
    }

    #[test]
    fn functions_are_named_like_generated_mutants() {
        let code =
            "struct S;\nimpl Tr for S {\n    fn go(&self) -> u8 {\n        1 + 2\n    }\n}\n";
        let (m, _) = mutant(code, "1 + 2", "1 - 2");
        assert_eq!(
            m.function.as_ref().map(|f| f.function_name.as_str()),
            Some("<impl Tr for S>::go")
        );
        assert!(
            m.name
                .ends_with("custom m: replace + with - in <impl Tr for S>::go")
        );
    }

    #[test]
    fn the_rendered_schema_variant_is_the_edited_site() {
        let code = "fn f(a: u32, b: u32) -> u32 {\n    let s = a.max(b);\n    s + 1\n}\n";
        let (m, _) = mutant(code, "a.max(b)", "a.min(b)");
        let schema = render(
            code,
            &[SchemaMutant {
                id: 1,
                span: m.span,
                replacement: &m.replacement,
                site: m.site.expect("site"),
            }],
        )
        .expect("schema");
        let guard = crate::schema::guard(1);
        assert_eq!(
            schema.code.replace(&guard, "G1"),
            "fn f(a: u32, b: u32) -> u32 {\n    let s = (if G1 { a.min(b) } else { a.max(b) });\n    s + 1\n}\n"
        );
    }

    #[test]
    fn old_text_must_be_unique_and_different() {
        let file = SourceFile::for_tests("src/main.rs", "fn f() -> u8 { 1 + 1 }\n", "t", true);
        assert!(custom_mutant(&file, "m", "1", "2").is_err());
        assert!(custom_mutant(&file, "m", "missing", "2").is_err());
        assert!(custom_mutant(&file, "m", "1 + 1", "1 + 1").is_err());
    }

    #[test]
    fn spec_files_parse_and_reject_unknown_keys() {
        let spec: SpecFile = toml::from_str(
            "[[mutant]]\nname = \"a\"\nfile = \"src/x.rs\"\nold = '''\nx\n  y'''\nnew = \"\"\n",
        )
        .unwrap();
        assert_eq!(spec.mutant[0].old, "x\n  y");
        assert!(
            toml::from_str::<SpecFile>(
                "[[mutant]]\nname=\"a\"\nfile=\"f\"\nold=\"\"\nnew=\"\"\nx=1\n"
            )
            .is_err()
        );
    }
}
