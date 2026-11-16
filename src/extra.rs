// Copyright 2026 Mathis Groult
// SPDX-License-Identifier: GPL-3.0-or-later (KvasirTest; see NOTICE)

//! Extra mutation kinds (`--extra-mutants`), off by default so mutant counts and ratchets of
//! existing runs stay the same.
//!
//! Every kind here is type-safe by construction (the variant has the type of the original), so a
//! schema holds it on the first build:
//! - `delete-call`: delete a call statement (`x.f(..);`, `f(..)?;`, `f(..).await;`); the schema
//!   switches it as `if !G { stmt }`.
//! - `literal`: integers `n` -> `n-1` and `n+1` (`0` -> `1`), floats -> `0.0` (`0.0` -> `1.0`),
//!   `true` <-> `false`, strings -> `""` (`""` -> `"xyzzy"`); suffixes are kept.
//! - `drop-operand`: `a && b` / `a || b` -> `a` and -> `b`.
//!
//! Mutants of these kinds are generated only where a runtime switch fits (not in const
//! contexts or patterns): a classic build per literal of every array length would cost more
//! than it finds.

use clap::ValueEnum;
use serde::Deserialize;
use syn::{Expr, Lit};

/// One extra mutation kind, as named on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtraKind {
    /// Every kind below.
    All,
    DeleteCall,
    Literal,
    DropOperand,
}

/// The extra kinds a run generates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extra(Vec<ExtraKind>);

impl Extra {
    pub fn new(kinds: &[ExtraKind]) -> Self {
        Extra(kinds.to_vec())
    }

    pub fn has(&self, kind: ExtraKind) -> bool {
        self.0.iter().any(|&k| k == kind || k == ExtraKind::All)
    }
}

/// True for a statement expression whose only effect is a call: `f(..)`, `x.m(..)`, optionally
/// behind `?` and `.await`.
pub fn is_call(expr: &Expr) -> bool {
    match expr {
        Expr::Call(_) | Expr::MethodCall(_) => true,
        Expr::Try(e) => is_call(&e.expr),
        Expr::Await(e) => is_call(&e.base),
        _ => false,
    }
}

/// Operand literals of `i` whose change rustc would evaluate at compile time: the divisor of
/// `/` and `%`, the amount of a shift, and both sides of an operation between two literals.
pub fn const_eval_literals(i: &syn::ExprBinary) -> impl Iterator<Item = &Expr> {
    use syn::BinOp::{Div, DivAssign, Rem, RemAssign, Shl, ShlAssign, Shr, ShrAssign};
    let is_lit = |e: &Expr| matches!(e, Expr::Lit(_));
    let divisor = matches!(
        i.op,
        Div(_)
            | DivAssign(_)
            | Rem(_)
            | RemAssign(_)
            | Shl(_)
            | ShlAssign(_)
            | Shr(_)
            | ShrAssign(_)
    );
    let both = is_lit(&i.left) && is_lit(&i.right);
    [(&*i.left, both), (&*i.right, both || divisor)]
        .into_iter()
        .filter(move |(e, skip)| *skip && is_lit(e))
        .map(|(e, _)| e)
}

/// Replacement texts for a literal; empty when the kind has none.
pub fn literal_replacements(lit: &Lit) -> Vec<String> {
    match lit {
        Lit::Int(i) => {
            let suffix = i.suffix();
            match i.base10_parse::<u128>() {
                Ok(0) => vec![format!("1{suffix}")],
                Ok(n) => vec![format!("{}{suffix}", n - 1), format!("{}{suffix}", n + 1)],
                Err(_) => Vec::new(),
            }
        }
        Lit::Float(f) => {
            let suffix = f.suffix();
            match f.base10_parse::<f64>() {
                Ok(0.0) => vec![format!("1.0{suffix}")],
                Ok(_) => vec![format!("0.0{suffix}")],
                Err(_) => Vec::new(),
            }
        }
        Lit::Bool(b) => vec![(!b.value).to_string()],
        Lit::Str(s) if s.suffix().is_empty() => {
            if s.value().is_empty() {
                vec!["\"xyzzy\"".to_owned()]
            } else {
                vec!["\"\"".to_owned()]
            }
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn reps(code: &str) -> Vec<String> {
        literal_replacements(&syn::parse_str::<Lit>(code).unwrap())
    }

    #[test]
    fn literals_move_by_one_and_keep_their_suffix() {
        assert_eq!(reps("0"), ["1"]);
        assert_eq!(reps("1"), ["0", "2"]);
        assert_eq!(reps("3usize"), ["2usize", "4usize"]);
        assert_eq!(reps("0x10"), ["15", "17"]);
        assert_eq!(reps("1.5"), ["0.0"]);
        assert_eq!(reps("0.0f32"), ["1.0f32"]);
        assert_eq!(reps("true"), ["false"]);
        assert_eq!(reps("\"vertical\""), ["\"\""]);
        assert_eq!(reps("\"\""), ["\"xyzzy\""]);
        assert_eq!(reps("'c'"), Vec::<String>::new());
        assert_eq!(reps("b\"x\""), Vec::<String>::new());
    }

    #[test]
    fn call_statements() {
        let e = |s: &str| syn::parse_str::<Expr>(s).unwrap();
        assert!(is_call(&e("f(1)")));
        assert!(is_call(&e("a.b().c()?")));
        assert!(is_call(&e("x.go().await")));
        assert!(!is_call(&e("a = f()")));
        assert!(!is_call(&e("return f()")));
    }

    fn names(code: &str, kinds: &str) -> Vec<String> {
        let options = crate::Options::from_arg_strs(["mutants", "--extra-mutants", kinds]);
        crate::visit::mutate_source_str(code, &options)
            .unwrap()
            .into_iter()
            .filter(|m| {
                matches!(
                    m.genre,
                    crate::Genre::DeleteCall | crate::Genre::Literal | crate::Genre::DropOperand
                )
            })
            .map(|m| m.name(false))
            .collect()
    }

    #[test]
    fn the_visitor_generates_each_kind_only_where_a_switch_fits() {
        let code = r#"
const N: usize = 3;
fn f(s: &S, a: bool, b: bool) -> u8 {
    s.go(1);
    let n = [0u8; 2];
    match n[0] { 7 => {} _ => {} }
    if a && b { return 0; }
    s.label("x");
    let m = (n[1] % 4) << 2;
    let k = 3 + 4;
    2
}
"#;
        let all = names(code, "all");
        assert_eq!(
            all,
            [
                "src/main.rs: delete s.go(1); in f",
                "src/main.rs: replace 1 with 0 in f",
                "src/main.rs: replace 1 with 2 in f",
                "src/main.rs: replace 0u8 with 1u8 in f",
                "src/main.rs: replace 0 with 1 in f",
                "src/main.rs: replace a && b with a in f",
                "src/main.rs: replace a && b with b in f",
                "src/main.rs: replace 0 with 1 in f",
                "src/main.rs: delete s.label(\"x\"); in f",
                "src/main.rs: replace \"x\" with \"\" in f",
                "src/main.rs: replace 1 with 0 in f",
                "src/main.rs: replace 1 with 2 in f",
                "src/main.rs: replace 2 with 1 in f",
                "src/main.rs: replace 2 with 3 in f",
            ]
        );
        assert_eq!(names(code, "drop-operand").len(), 2);
        let options = crate::Options::from_arg_strs(["mutants"]);
        assert!(
            crate::visit::mutate_source_str(code, &options)
                .unwrap()
                .iter()
                .all(|m| !matches!(m.genre, crate::Genre::Literal))
        );
    }

    #[test]
    fn all_includes_every_kind() {
        let extra = Extra::new(&[ExtraKind::All]);
        assert!(extra.has(ExtraKind::Literal) && extra.has(ExtraKind::DeleteCall));
        let extra = Extra::new(&[ExtraKind::Literal]);
        assert!(extra.has(ExtraKind::Literal) && !extra.has(ExtraKind::DropOperand));
        assert!(!Extra::default().has(ExtraKind::Literal));
    }
}
