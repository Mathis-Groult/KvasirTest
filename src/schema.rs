// Copyright 2026 Mathis Groult
// KvasirTest addition to cargo-mutants (MIT): see docs/decisions/0001-mutant-schemata.md.

//! Mutant schemata: one source text holding every mutant of a file, each behind a runtime guard.
//!
//! [`render`] is pure: it takes the original code and the mutants (with the syntax site each one
//! sits in) and returns the schema text plus [`Region`]s that map schema byte ranges back to mutant
//! ids, so a compiler error can be blamed on the mutants that caused it ([`Schema::blame`]).

use std::ops::Range;

use serde::Serialize;

use crate::span::{LineColumn, Span};

/// Environment variable naming the active mutant (`0` or absent: none).
pub const ACTIVE_MUTANT_VAR: &str = "KVASIR_TEST_MUTANT";

/// Environment variable naming a file: when set, no mutant is active and every guard appends its
/// id to that file the first time it is evaluated in the process (the reach of a test).
pub const REACH_FILE_VAR: &str = "KVASIR_TEST_REACH";

/// The value guards hold in reach-recording mode (no mutant has this id).
const RECORDING: u32 = u32::MAX;

/// Where a mutant sits in the syntax tree, which decides how the schema switches it on.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub enum SiteKind {
    /// A function body: replacements become `if G { return value; }` before the statements.
    FnBody,
    /// An expression: replacements become an `if G { variant } else { original }` chain.
    Expr,
    /// A match arm pattern: deleting the arm becomes `pattern if !G`.
    ArmPattern,
}

/// The syntax site of a mutant: the region the schema rewrites to hold it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Site {
    pub kind: SiteKind,
    pub span: Span,
}

/// One mutant as the renderer needs it.
#[derive(Clone, Debug)]
pub struct SchemaMutant<'a> {
    /// Non-zero id; the guard compares the environment variable with it.
    pub id: u32,
    /// The text the mutant replaces (inside `site.span` for expression sites).
    pub span: Span,
    pub replacement: &'a str,
    pub site: Site,
}

/// What a [`Region`] of the schema text holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegionKind {
    /// The guard expression of one mutant.
    Guard,
    /// The mutated code of one mutant.
    Variant,
    /// A whole site, with every mutant switched there.
    Site,
}

/// A byte range of the schema text and the mutants it belongs to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Region {
    pub kind: RegionKind,
    pub range: Range<usize>,
    pub ids: Vec<u32>,
}

/// The schema of one file.
#[derive(Debug)]
pub struct Schema {
    pub code: String,
    pub regions: Vec<Region>,
    /// Mutants left out because their site overlaps another site without nesting in it.
    pub rejected: Vec<u32>,
}

impl Schema {
    /// The mutants to blame for an error at `range` of the schema text: those of the innermost
    /// region containing it. Empty when the error lies outside every region.
    pub fn blame(&self, range: &Range<usize>) -> &[u32] {
        self.regions
            .iter()
            .filter(|r| r.range.start <= range.start && range.end <= r.range.end)
            .min_by_key(|r| r.range.len())
            .map_or(&[], |r| r.ids.as_slice())
    }
}

/// Errors that make a whole file unusable as a schema.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum SchemaError {
    #[error("mutant {id}: span {span:?} is outside the file")]
    SpanOutsideFile { id: u32, span: Span },
    #[error("mutant {id}: mutated text is not inside its site")]
    SpanOutsideSite { id: u32 },
    #[error("mutant id 0 is reserved for \"no mutant\"")]
    ZeroId,
}

/// The guard expression that is true only when mutant `id` is active.
///
/// Self-contained (its own `static`s), so it works in any module of any crate without a helper
/// item, and written for every edition (no let-chains). In recording mode ([`REACH_FILE_VAR`]) it is false and reports its id once per process:
/// a test that never evaluates a guard cannot observe that mutant, which makes the reach exact.
pub fn guard(id: u32) -> String {
    format!(
        "({{ static KVASIR_TEST_ACTIVE: ::std::sync::OnceLock<u32> = ::std::sync::OnceLock::new(); \
         static KVASIR_TEST_SEEN: ::std::sync::atomic::AtomicBool = \
         ::std::sync::atomic::AtomicBool::new(false); \
         let active = *KVASIR_TEST_ACTIVE.get_or_init(|| \
         if ::std::env::var_os(\"{REACH_FILE_VAR}\").is_some() {{ {RECORDING} }} \
         else {{ ::std::env::var(\"{ACTIVE_MUTANT_VAR}\").ok().and_then(|v| v.parse().ok()).unwrap_or(0) }}); \
         if active == {RECORDING} && !KVASIR_TEST_SEEN.swap(true, ::std::sync::atomic::Ordering::Relaxed) {{ \
         if let Some(path) = ::std::env::var_os(\"{REACH_FILE_VAR}\") {{ \
         if let Ok(mut file) = ::std::fs::OpenOptions::new().create(true).append(true).open(path) {{ \
         let _ = ::std::io::Write::write_all(&mut file, b\"{id}\\n\"); }} }} }} \
         active == {id} }})"
    )
}

/// Mutant ids in a reach file written by guards (one per line; unparsable lines are ignored).
pub fn parse_reach(text: &str) -> std::collections::BTreeSet<u32> {
    text.lines().filter_map(|l| l.trim().parse().ok()).collect()
}

/// Render the schema of `code` holding all `mutants`.
pub fn render(code: &str, mutants: &[SchemaMutant<'_>]) -> Result<Schema, SchemaError> {
    let index = LineIndex::new(code);
    let mut sites = group_sites(code, &index, mutants)?;
    sites.sort_by(|a, b| {
        (a.range.start, std::cmp::Reverse(a.range.end), a.kind).cmp(&(
            b.range.start,
            std::cmp::Reverse(b.range.end),
            b.kind,
        ))
    });
    let (roots, rejected) = nest(sites);
    let mut out = Renderer {
        code,
        out: String::with_capacity(code.len() * 2),
        regions: Vec::new(),
    };
    out.emit_range(0..code.len(), &roots);
    Ok(Schema {
        code: out.out,
        regions: out.regions,
        rejected,
    })
}

struct Variant<'a> {
    id: u32,
    inner: Range<usize>,
    replacement: &'a str,
}

struct SiteNode<'a> {
    kind: SiteKind,
    range: Range<usize>,
    variants: Vec<Variant<'a>>,
    children: Vec<SiteNode<'a>>,
}

impl SiteNode<'_> {
    fn ids(&self) -> Vec<u32> {
        self.variants.iter().map(|v| v.id).collect()
    }
}

fn group_sites<'a>(
    code: &str,
    index: &LineIndex,
    mutants: &[SchemaMutant<'a>],
) -> Result<Vec<SiteNode<'a>>, SchemaError> {
    let mut sites: Vec<SiteNode<'a>> = Vec::new();
    for m in mutants {
        if m.id == 0 {
            return Err(SchemaError::ZeroId);
        }
        let outside = |span| SchemaError::SpanOutsideFile { id: m.id, span };
        let range = index.range(code, m.site.span).ok_or(outside(m.site.span))?;
        let inner = index.range(code, m.span).ok_or(outside(m.span))?;
        if m.site.kind == SiteKind::Expr && !(range.start <= inner.start && inner.end <= range.end)
        {
            return Err(SchemaError::SpanOutsideSite { id: m.id });
        }
        let variant = Variant {
            id: m.id,
            inner,
            replacement: m.replacement,
        };
        match sites
            .iter_mut()
            .find(|s| s.kind == m.site.kind && s.range == range)
        {
            Some(site) => site.variants.push(variant),
            None => sites.push(SiteNode {
                kind: m.site.kind,
                range,
                variants: vec![variant],
                children: Vec::new(),
            }),
        }
    }
    Ok(sites)
}

/// Build the containment forest of sorted sites; a site that crosses another is rejected.
fn nest(sorted: Vec<SiteNode<'_>>) -> (Vec<SiteNode<'_>>, Vec<u32>) {
    let mut rejected = Vec::new();
    let mut stack: Vec<SiteNode<'_>> = Vec::new();
    let mut roots = Vec::new();
    for site in sorted {
        while let Some(top) = stack.last() {
            if site.range.start >= top.range.end {
                let done = stack.pop().expect("stack is not empty");
                attach(&mut stack, &mut roots, done);
            } else {
                break;
            }
        }
        if let Some(top) = stack.last()
            && site.range.end > top.range.end
        {
            rejected.extend(site.ids());
            continue;
        }
        stack.push(site);
    }
    while let Some(done) = stack.pop() {
        attach(&mut stack, &mut roots, done);
    }
    (roots, rejected)
}

fn attach<'a>(stack: &mut [SiteNode<'a>], roots: &mut Vec<SiteNode<'a>>, done: SiteNode<'a>) {
    match stack.last_mut() {
        Some(parent) => parent.children.push(done),
        None => roots.push(done),
    }
}

struct Renderer<'c> {
    code: &'c str,
    out: String,
    regions: Vec<Region>,
}

impl Renderer<'_> {
    /// Copy `range` of the original code, rendering the sites inside it.
    fn emit_range(&mut self, range: Range<usize>, sites: &[SiteNode<'_>]) {
        let mut at = range.start;
        for site in sites {
            self.out.push_str(&self.code[at..site.range.start]);
            self.emit_site(site);
            at = site.range.end;
        }
        self.out.push_str(&self.code[at..range.end]);
    }

    fn emit_site(&mut self, site: &SiteNode<'_>) {
        let start = self.out.len();
        match site.kind {
            SiteKind::Expr => {
                self.out.push('(');
                for v in &site.variants {
                    self.out.push_str("if ");
                    self.emit_guard(v.id);
                    self.out.push_str(" { ");
                    let text = format!(
                        "{}{}{}",
                        &self.code[site.range.start..v.inner.start],
                        v.replacement,
                        &self.code[v.inner.end..site.range.end]
                    );
                    self.emit_variant(v.id, &text);
                    self.out.push_str(" } else ");
                }
                self.out.push_str("{ ");
                self.emit_range(site.range.clone(), &site.children);
                self.out.push_str(" })");
            }
            SiteKind::FnBody => {
                for v in &site.variants {
                    self.out.push_str("if ");
                    self.emit_guard(v.id);
                    self.out.push_str(" { ");
                    self.emit_variant(v.id, &format!("return ({});", v.replacement));
                    self.out.push_str(" } ");
                }
                self.emit_range(site.range.clone(), &site.children);
            }
            SiteKind::ArmPattern => {
                self.emit_range(site.range.clone(), &site.children);
                for (i, v) in site.variants.iter().enumerate() {
                    self.out.push_str(if i == 0 { " if !" } else { " && !" });
                    self.emit_guard(v.id);
                }
            }
        }
        self.regions.push(Region {
            kind: RegionKind::Site,
            range: start..self.out.len(),
            ids: site.ids(),
        });
    }

    fn emit_guard(&mut self, id: u32) {
        let start = self.out.len();
        self.out.push_str(&guard(id));
        self.regions.push(Region {
            kind: RegionKind::Guard,
            range: start..self.out.len(),
            ids: vec![id],
        });
    }

    fn emit_variant(&mut self, id: u32, text: &str) {
        let start = self.out.len();
        self.out.push_str(text);
        self.regions.push(Region {
            kind: RegionKind::Variant,
            range: start..self.out.len(),
            ids: vec![id],
        });
    }
}

/// Converts the (line, char column) positions of [`Span`] into byte offsets.
struct LineIndex {
    /// Byte offset of the start of each line (index 0 = line 1).
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(code: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(code.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { starts }
    }

    /// Byte offset of a position, counting columns in chars and not counting `\r` (as
    /// [`Span::extract`] does). A column one past the end of the line is the line end.
    fn offset(&self, code: &str, at: LineColumn) -> Option<usize> {
        let line_start = *self.starts.get(at.line.checked_sub(1)?)?;
        let line_end = self.starts.get(at.line).map_or(code.len(), |&s| s);
        let line = &code[line_start..line_end];
        let mut column = 1;
        for (i, c) in line.char_indices() {
            if column == at.column {
                return Some(line_start + i);
            }
            if c != '\r' {
                column += 1;
            }
        }
        (column == at.column).then_some(line_end)
    }

    fn range(&self, code: &str, span: Span) -> Option<Range<usize>> {
        let start = self.offset(code, span.start)?;
        let end = self.offset(code, span.end)?;
        (start <= end).then_some(start..end)
    }
}

#[cfg(test)]
mod test {
    use pretty_assertions::assert_eq;

    use super::*;

    /// Span of the `n`th occurrence (0-based) of `needle` in `code`.
    fn span_of(code: &str, needle: &str, n: usize) -> Span {
        let start = code.match_indices(needle).nth(n).expect("needle present").0;
        Span {
            start: position(code, start),
            end: position(code, start + needle.len()),
        }
    }

    fn position(code: &str, byte: usize) -> LineColumn {
        let before = &code[..byte];
        let line = before.matches('\n').count() + 1;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        LineColumn {
            line,
            column: code[line_start..byte].chars().count() + 1,
        }
    }

    /// A mutant replacing the first `op` inside the first occurrence of `site`.
    fn expr_mutant<'a>(
        code: &str,
        id: u32,
        op: &str,
        site: &str,
        rep: &'a str,
    ) -> SchemaMutant<'a> {
        let site_start = code.find(site).expect("site present");
        let op_start = site_start + site.find(op).expect("op inside site");
        SchemaMutant {
            id,
            span: Span {
                start: position(code, op_start),
                end: position(code, op_start + op.len()),
            },
            replacement: rep,
            site: Site {
                kind: SiteKind::Expr,
                span: span_of(code, site, 0),
            },
        }
    }

    /// The schema with every guard written as `G<id>`, so expected texts stay readable.
    fn readable(schema: &Schema, ids: &[u32]) -> String {
        ids.iter().fold(schema.code.clone(), |s, &id| {
            s.replace(&guard(id), &format!("G{id}"))
        })
    }

    #[test]
    fn binary_operator_becomes_an_if_chain_with_the_original_last() {
        let code = "fn f(a: u32, b: u32) -> u32 { a + b }\n";
        let mutants = [
            expr_mutant(code, 1, "+", "a + b", "-"),
            expr_mutant(code, 2, "+", "a + b", "*"),
        ];
        let schema = render(code, &mutants).unwrap();
        assert_eq!(
            readable(&schema, &[1, 2]),
            "fn f(a: u32, b: u32) -> u32 { (if G1 { a - b } else if G2 { a * b } else { a + b }) }\n"
        );
        assert_eq!(schema.rejected, Vec::<u32>::new());
    }

    #[test]
    fn variants_hold_plain_code_and_only_the_original_branch_nests_inner_sites() {
        let code = "fn f(a: u32, b: u32, c: u32) -> u32 { a + b * c }\n";
        let mutants = [
            expr_mutant(code, 1, "+", "a + b * c", "-"),
            expr_mutant(code, 2, "*", "b * c", "+"),
        ];
        let schema = render(code, &mutants).unwrap();
        assert_eq!(
            readable(&schema, &[1, 2]),
            "fn f(a: u32, b: u32, c: u32) -> u32 { (if G1 { a - b * c } else { a + (if G2 { b + c } else { b * c }) }) }\n"
        );
    }

    #[test]
    fn function_body_gets_early_returns_before_its_statements() {
        let code = "fn f(a: u32) -> u32 {\n    let b = a;\n    b + 1\n}\n";
        let body = Span {
            start: position(code, code.find("let").unwrap()),
            end: position(code, code.find("b + 1").unwrap() + 5),
        };
        let fn_mutant = |id, rep| SchemaMutant {
            id,
            span: body,
            replacement: rep,
            site: Site {
                kind: SiteKind::FnBody,
                span: body,
            },
        };
        let mutants = [
            fn_mutant(1, "0"),
            fn_mutant(2, "1"),
            expr_mutant(code, 3, "+", "b + 1", "-"),
        ];
        let schema = render(code, &mutants).unwrap();
        assert_eq!(
            readable(&schema, &[1, 2, 3]),
            "fn f(a: u32) -> u32 {\n    if G1 { return (0); } if G2 { return (1); } let b = a;\n    (if G3 { b - 1 } else { b + 1 })\n}\n"
        );
    }

    #[test]
    fn deleted_match_arm_gets_a_negated_guard() {
        let code = "fn f(x: u8) -> u8 { match x { 1 => 10, _ => 0 } }\n";
        let pattern = span_of(code, "1", 0);
        let mutants = [SchemaMutant {
            id: 4,
            span: span_of(code, "1 => 10", 0),
            replacement: "",
            site: Site {
                kind: SiteKind::ArmPattern,
                span: pattern,
            },
        }];
        let schema = render(code, &mutants).unwrap();
        assert_eq!(
            readable(&schema, &[4]),
            "fn f(x: u8) -> u8 { match x { 1 if !G4 => 10, _ => 0 } }\n"
        );
    }

    #[test]
    fn struct_field_deletion_switches_the_whole_literal() {
        let code = "fn f() -> S { S { a: 1, ..S::default() } }\n";
        let mutants = [expr_mutant(
            code,
            5,
            "a: 1,",
            "S { a: 1, ..S::default() }",
            "",
        )];
        let schema = render(code, &mutants).unwrap();
        assert_eq!(
            readable(&schema, &[5]),
            "fn f() -> S { (if G5 { S {  ..S::default() } } else { S { a: 1, ..S::default() } }) }\n"
        );
    }

    #[test]
    fn mutants_of_one_expression_share_a_site_even_from_different_genres() {
        // A match guard `x > 0` mutated to true/false and its operator `>` mutated: one site.
        let code = "fn f(x: i8) -> u8 { match x { v if x > 0 => 1, _ => 0 } }\n";
        let guard_site = span_of(code, "x > 0", 0);
        let whole = |id, rep| SchemaMutant {
            id,
            span: guard_site,
            replacement: rep,
            site: Site {
                kind: SiteKind::Expr,
                span: guard_site,
            },
        };
        let mutants = [whole(1, "true"), expr_mutant(code, 2, ">", "x > 0", ">=")];
        let schema = render(code, &mutants).unwrap();
        assert_eq!(
            readable(&schema, &[1, 2]),
            "fn f(x: i8) -> u8 { match x { v if (if G1 { true } else if G2 { x >= 0 } else { x > 0 }) => 1, _ => 0 } }\n"
        );
        assert_eq!(
            schema
                .regions
                .iter()
                .filter(|r| r.kind == RegionKind::Site)
                .count(),
            1
        );
    }

    #[test]
    fn blame_picks_the_innermost_region() {
        let code = "fn f(a: u32, b: u32) -> u32 { a + b }\n";
        let mutants = [
            expr_mutant(code, 1, "+", "a + b", "-"),
            expr_mutant(code, 2, "+", "a + b", "*"),
        ];
        let schema = render(code, &mutants).unwrap();
        let variant_2 = schema.code.find("a * b").unwrap();
        assert_eq!(schema.blame(&(variant_2..variant_2 + 1)), &[2]);
        let original = schema.code.rfind("a + b").unwrap();
        assert_eq!(schema.blame(&(original..original + 5)), &[1, 2]);
        assert_eq!(schema.blame(&(0..2)), &[] as &[u32]);
    }

    #[test]
    fn crossing_sites_are_rejected_not_rendered() {
        let code = "fn f() -> u32 { 1 + 2 + 3 }\n";
        let crossing = |id, site: &str| SchemaMutant {
            id,
            span: span_of(code, site, 0),
            replacement: "0",
            site: Site {
                kind: SiteKind::Expr,
                span: span_of(code, site, 0),
            },
        };
        let mutants = [crossing(1, "1 + 2"), crossing(2, "2 + 3")];
        let schema = render(code, &mutants).unwrap();
        assert_eq!(schema.rejected, vec![2]);
        assert_eq!(
            readable(&schema, &[1]),
            "fn f() -> u32 { (if G1 { 0 } else { 1 + 2 }) + 3 }\n"
        );
    }

    #[test]
    fn columns_count_chars_and_skip_carriage_returns() {
        let code = "fn é() -> u32 {\r\n    1 + 2\r\n}\r\n";
        let mutants = [expr_mutant(code, 1, "+", "1 + 2", "-")];
        let schema = render(code, &mutants).unwrap();
        assert_eq!(
            readable(&schema, &[1]),
            "fn é() -> u32 {\r\n    (if G1 { 1 - 2 } else { 1 + 2 })\r\n}\r\n"
        );
    }

    #[test]
    fn reach_file_lines_are_ids() {
        assert_eq!(
            parse_reach("3\n17\n3\ngarbage\n\n"),
            [3, 17].into_iter().collect()
        );
    }

    #[test]
    fn id_zero_is_refused() {
        let code = "fn f() -> u32 { 1 + 2 }\n";
        let mutants = [expr_mutant(code, 0, "+", "1 + 2", "-")];
        assert_eq!(render(code, &mutants).unwrap_err(), SchemaError::ZeroId);
    }
}
