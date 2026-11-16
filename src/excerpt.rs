// Copyright 2026 Mathis Groult
// SPDX-License-Identifier: GPL-3.0-or-later (KvasirTest; see NOTICE)

//! The mutated source of a mutant, as the compiler sees it.
//!
//! A name like `replace && with || ... :122:9` is not enough to rebuild the mutant by hand: the
//! AST mutant of the middle `&&` in `a && b && c && d` is `((a && b) || c) && d`, not the textual
//! `a && b || c && d`. The excerpt shows the source lines the mutant touches with the mutation
//! applied, and an expression site in parentheses, so the grouping is explicit.

use crate::mutant::Mutant;
use crate::schema::{LineIndex, SiteKind};
use crate::span::{LineColumn, Span};

/// Longest excerpt kept (chars); longer ones are cut with `..`.
const MAX_CHARS: usize = 400;

/// The lines a mutant touches, with the mutation applied, on one line; `None` when the name says
/// it all (a whole function body replaced).
pub fn excerpt(mutant: &Mutant) -> Option<String> {
    excerpt_of(mutant, true)
}

/// [`excerpt`], extended to the enclosing statement only if `whole_statement`.
fn excerpt_of(mutant: &Mutant, whole_statement: bool) -> Option<String> {
    let code = mutant.source_file.code();
    let index = LineIndex::new(code);
    let byte = |at: LineColumn| index.offset(code, at);
    let (edit_start, edit_end) = (byte(mutant.span.start)?, byte(mutant.span.end)?);
    // Only an expression site gets parentheses; other sites show the plain edit.
    let group = match mutant.site {
        Some(site) if site.kind == SiteKind::FnBody => return None,
        Some(site) if site.kind == SiteKind::Expr => {
            Some((byte(site.span.start)?, byte(site.span.end)?))
        }
        _ => None,
    };
    let (from, to) = group.unwrap_or((edit_start, edit_end));
    // Show the whole statement around the site: an inner site alone (`(a && b)`) hides the rest
    // of the condition it is grouped into.
    let (from, to) = if whole_statement {
        enclosing_statement(code, &index, from..to)
            .filter(|r| line_of(code, r.end) - line_of(code, r.start) < MAX_LINES)
            .map_or((from, to), |r| (r.start.min(from), r.end.max(to)))
    } else {
        (from, to)
    };
    let first_line = mutant.span.start.line.min(line_of(code, from));
    let last_line = mutant.span.end.line.max(line_of(code, to));
    let lines = Span {
        start: LineColumn {
            line: first_line,
            column: 1,
        },
        end: LineColumn {
            line: last_line + 1,
            column: 1,
        },
    };
    let line_start = byte(lines.start)?;
    let line_end = byte(lines.end).unwrap_or(code.len());

    let mut out = String::new();
    let mut push = |s: &str| out.push_str(s);
    match group {
        Some((g0, g1)) if g0 <= edit_start && edit_end <= g1 => {
            push(&code[line_start..g0]);
            push("(");
            push(&code[g0..edit_start]);
            push(&mutant.replacement);
            push(&code[edit_end..g1]);
            push(")");
            push(&code[g1..line_end]);
        }
        _ => {
            push(&code[line_start..edit_start]);
            push(&mutant.replacement);
            push(&code[edit_end..line_end]);
        }
    }
    let squashed = squash(&out);
    if squashed.is_empty() {
        // A deleted line: the name already shows what went.
        return None;
    }
    Some(if squashed.chars().count() > MAX_CHARS {
        let cut: String = squashed.chars().take(MAX_CHARS).collect();
        format!("{cut}..")
    } else {
        squashed
    })
}

/// The text that identifies a mutant across a refactor that moves code: the lines of its site
/// with the mutation applied (not the whole statement: a split shortens the `if` or `match` a
/// site sits in, which must not change the key), or for a deletion that leaves nothing to show,
/// the deleted text. `None` for whole-body replacements, which belong to their function.
pub fn source_key(mutant: &Mutant) -> Option<String> {
    if mutant.genre == crate::Genre::FnValue {
        return None;
    }
    excerpt_of(mutant, false)
        .or_else(|| Some(format!("delete {}", squash(&mutant.original_text()))))
}

/// Statements longer than this many lines are not shown whole.
const MAX_LINES: usize = 12;

/// Byte range of the innermost statement containing `target`, if the file parses.
fn enclosing_statement(
    code: &str,
    index: &LineIndex,
    target: std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    struct Finder<'a> {
        code: &'a str,
        index: &'a LineIndex,
        target: std::ops::Range<usize>,
        best: Option<std::ops::Range<usize>>,
    }
    impl<'ast> syn::visit::Visit<'ast> for Finder<'_> {
        fn visit_stmt(&mut self, i: &'ast syn::Stmt) {
            let span = syn::spanned::Spanned::span(i);
            if let (Some(start), Some(end)) = (
                self.index.offset(self.code, span.start().into()),
                self.index.offset(self.code, span.end().into()),
            ) && start <= self.target.start
                && self.target.end <= end
            {
                self.best = Some(start..end);
                syn::visit::visit_stmt(self, i);
            }
        }
    }
    let file = syn::parse_str::<syn::File>(code).ok()?;
    let mut finder = Finder {
        code,
        index,
        target,
        best: None,
    };
    syn::visit::Visit::visit_file(&mut finder, &file);
    finder.best
}

/// 1-based line of a byte offset.
fn line_of(code: &str, byte: usize) -> usize {
    code[..byte].matches('\n').count() + 1
}

/// Whitespace runs to one space, trimmed.
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::Options;
    use crate::visit::mutate_source_str;

    fn excerpts(code: &str, extra: &[&str]) -> Vec<(String, Option<String>)> {
        let mut args = vec!["mutants"];
        args.extend(extra);
        mutate_source_str(code, &Options::from_arg_strs(args))
            .unwrap()
            .iter()
            .map(|m| (m.describe_change(), excerpt(m)))
            .collect()
    }

    #[test]
    fn the_middle_and_of_a_chain_shows_its_real_grouping() {
        let code = "fn f(a: bool, b: bool, c: bool, d: bool) -> bool {\n    let x = a\n        && b\n        && c\n        && d;\n    x\n}\n";
        let all = excerpts(code, &[]);
        let middle: Vec<_> = all
            .iter()
            .filter(|(d, _)| d.starts_with("replace && with ||"))
            .map(|(_, e)| e.clone().unwrap())
            .collect();
        assert_eq!(
            middle,
            [
                "let x = (a && b && c || d);",
                "let x = (a && b || c) && d;",
                "let x = (a || b) && c && d;",
            ]
        );
        // A whole-body replacement is described by its name.
        assert!(
            all.iter()
                .any(|(d, e)| d.starts_with("replace f") && e.is_none())
        );
    }

    #[test]
    fn deletions_show_nothing_and_literals_their_line() {
        let code = "fn f(s: &S) {\n    s.set(\n        1,\n        2,\n    );\n    s.go();\n}\n";
        let all = excerpts(code, &["--extra-mutants", "delete-call,literal"]);
        let get = |d: &str| {
            all.iter()
                .find(|(x, _)| x.starts_with(d))
                .and_then(|(_, e)| e.clone())
        };
        assert_eq!(get("delete s.go();"), None);
        assert_eq!(
            get("replace 1 with 0").as_deref(),
            Some("s.set( (0), 2, );")
        );
        assert_eq!(
            get("delete s.set").as_deref(),
            None,
            "a deleted multi-line statement leaves nothing to show"
        );
    }
}
