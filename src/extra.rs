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

use std::collections::{BTreeMap, BTreeSet};

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
    SwapArgs,
    EnumVariant,
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

/// Adjacent call arguments that look like the same type (`t.x, t.y`; `a[0], a[1]`; two literals
/// of one kind; two plain variables; the same method on such receivers) and differ: the index
/// of the first of each pair.
#[cfg(test)]
fn swappable_pairs(args: &syn::punctuated::Punctuated<Expr, syn::Token![,]>) -> Vec<usize> {
    swappable_pairs_of(&args.iter().collect::<Vec<_>>())
}

/// [`swappable_pairs`] over a slice.
pub fn swappable_pairs_of(args: &[&Expr]) -> Vec<usize> {
    (0..args.len().saturating_sub(1))
        .filter(|&i| same_shape(args[i], args[i + 1]) && text(args[i]) != text(args[i + 1]))
        .collect()
}

fn text(e: &Expr) -> String {
    quote::ToTokens::to_token_stream(e).to_string()
}

fn same_shape(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Field(a), Expr::Field(b)) => text(&a.base) == text(&b.base),
        (Expr::Index(a), Expr::Index(b)) => text(&a.expr) == text(&b.expr),
        (Expr::Lit(a), Expr::Lit(b)) => {
            std::mem::discriminant(&a.lit) == std::mem::discriminant(&b.lit)
                && !matches!(a.lit, Lit::Bool(_))
        }
        (Expr::Path(a), Expr::Path(b)) => {
            a.path.get_ident().is_some() && b.path.get_ident().is_some()
        }
        (Expr::MethodCall(a), Expr::MethodCall(b)) => {
            a.method == b.method
                && a.args.is_empty()
                && b.args.is_empty()
                && same_shape(&a.receiver, &b.receiver)
        }
        (Expr::Reference(a), Expr::Reference(b)) => {
            a.mutability.is_some() == b.mutability.is_some() && same_shape(&a.expr, &b.expr)
        }
        _ => false,
    }
}

/// `Type::Variant` paths used as values in one file, by type key (`Self` keyed by its impl), so a
/// use can be swapped for a sibling the file itself uses.
#[derive(Debug, Default)]
pub struct VariantUses(BTreeMap<String, BTreeSet<String>>);

impl VariantUses {
    /// Collect the uses in `file` (values only: not patterns, types or call callees).
    pub fn of(file: &syn::File) -> Self {
        let mut c = VariantCollector::default();
        syn::visit::Visit::visit_file(&mut c, file);
        c.uses
    }

    /// The next sibling (cyclic, in name order) of `variant` under `key`, if the file uses one.
    pub fn sibling(&self, key: &str, variant: &str) -> Option<&str> {
        let set = self.0.get(key)?;
        if set.len() < 2 {
            return None;
        }
        set.range::<str, _>((
            std::ops::Bound::Excluded(variant),
            std::ops::Bound::Unbounded,
        ))
        .next()
        .or_else(|| set.iter().next())
        .map(String::as_str)
        .filter(|s| *s != variant)
    }
}

/// Atomic orderings: a swap is invisible to single-threaded tests.
const ATOMIC: [&str; 5] = ["Relaxed", "Acquire", "Release", "AcqRel", "SeqCst"];

/// The type key and variant of `Type::Variant` (both CamelCase, at least two segments, no
/// generic arguments), with `Self` keyed by the enclosing impl; `None` for anything else.
pub fn variant_path(path: &syn::Path, impl_ty: Option<&str>) -> Option<(String, String)> {
    let n = path.segments.len();
    if n < 2 || path.segments.iter().any(|s| !s.arguments.is_none()) {
        return None;
    }
    let ty = path.segments[n - 2].ident.to_string();
    let variant = path.segments[n - 1].ident.to_string();
    // CamelCase or one capital (`Axis::X`); `ALL_CAPS` is a constant, not a variant.
    let camel = |s: &str| {
        s.starts_with(|c: char| c.is_ascii_uppercase())
            && (s.len() == 1 || s.chars().any(|c| c.is_ascii_lowercase()))
    };
    if !camel(&ty) || !camel(&variant) || ATOMIC.contains(&variant.as_str()) {
        return None;
    }
    let prefix: Vec<String> = path
        .segments
        .iter()
        .take(n - 1)
        .map(|s| s.ident.to_string())
        .collect();
    let key = if ty == "Self" {
        format!("Self in {}", impl_ty?)
    } else {
        prefix.join("::")
    };
    Some((key, variant))
}

#[derive(Default)]
struct VariantCollector {
    uses: VariantUses,
    impls: Vec<String>,
    callees: BTreeSet<(usize, usize)>,
}

fn start_of(s: proc_macro2::Span) -> (usize, usize) {
    (s.start().line, s.start().column)
}

impl<'ast> syn::visit::Visit<'ast> for VariantCollector {
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.impls
            .push(quote::ToTokens::to_token_stream(&i.self_ty).to_string());
        syn::visit::visit_item_impl(self, i);
        self.impls.pop();
    }

    fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
        self.callees
            .insert(start_of(syn::spanned::Spanned::span(&*i.func)));
        syn::visit::visit_expr_call(self, i);
    }

    fn visit_expr_path(&mut self, i: &'ast syn::ExprPath) {
        if i.qself.is_none()
            && !self
                .callees
                .contains(&start_of(syn::spanned::Spanned::span(i)))
            && let Some((key, variant)) =
                variant_path(&i.path, self.impls.last().map(String::as_str))
        {
            self.uses.0.entry(key).or_default().insert(variant);
        }
    }

    fn visit_pat(&mut self, _: &'ast syn::Pat) {}

    fn visit_type(&mut self, _: &'ast syn::Type) {}

    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}
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
    fn swappable_arguments_have_the_same_shape() {
        let pairs = |s: &str| match syn::parse_str::<Expr>(s).unwrap() {
            Expr::Call(c) => swappable_pairs(&c.args),
            Expr::MethodCall(c) => swappable_pairs(&c.args),
            _ => panic!("not a call"),
        };
        assert_eq!(pairs("V(t.x, t.y, t.z)"), [0, 1]);
        assert_eq!(pairs("V(t.x, u.y)"), Vec::<usize>::new());
        assert_eq!(pairs("q(r[0], r[1], 2.0, 3.0)"), [0, 2]);
        assert_eq!(pairs("f(a, b, \"s\", 1)"), [0]);
        assert_eq!(pairs("s.set(w, h)"), [0]);
        assert_eq!(pairs("f(l[0].into(), l[1].into(), &a, &b)"), [0, 2]);
        assert_eq!(pairs("f(x, x, true, false)"), Vec::<usize>::new());
    }

    #[test]
    fn enum_variants_swap_with_a_sibling_the_file_uses() {
        let file: syn::File = syn::parse_str(
            r#"
            fn f(a: bool) -> Shape {
                let o = Ordering::Relaxed;
                let p = Ordering::SeqCst;
                match q { Shape::Pat => 1, _ => 2 };
                let w = Wrap::One(3);
                if a { Shape::Tall } else { Shape::Low }
            }
            fn g() -> Shape { Shape::None }
            impl Shape { fn h() -> Self { Self::Low } fn k() -> Self { Self::Tall } }
            "#,
        )
        .unwrap();
        let uses = VariantUses::of(&file);
        assert_eq!(uses.sibling("Shape", "Tall"), Some("Low"));
        assert_eq!(uses.sibling("Shape", "Low"), Some("None"));
        assert_eq!(uses.sibling("Shape", "None"), Some("Tall"));
        assert_eq!(uses.sibling("Self in Shape", "Low"), Some("Tall"));
        assert_eq!(uses.sibling("Ordering", "Relaxed"), None);
        assert_eq!(uses.sibling("Wrap", "One"), None);
        assert!(uses.0.get("Shape").is_some_and(|s| !s.contains("Pat")));
        let path = |s: &str| syn::parse_str::<syn::Path>(s).unwrap();
        assert!(variant_path(&path("Axis::X"), None).is_some());
        assert!(variant_path(&path("display::SCALE"), None).is_none());
        assert!(variant_path(&path("Tracked::SCALE"), None).is_none());
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
    fn the_visitor_swaps_enum_variant_values_only() {
        let code = r#"
fn f(a: bool, q: Shape) -> Shape {
    match q { Shape::Low => {} _ => {} }
    let w = Wrap::One(3);
    if a { Shape::Tall } else { Shape::Low }
}
"#;
        let options = crate::Options::from_arg_strs(["mutants", "--extra-mutants", "enum-variant"]);
        let names: Vec<String> = crate::visit::mutate_source_str(code, &options)
            .unwrap()
            .into_iter()
            .filter(|m| m.genre == crate::Genre::EnumVariant)
            .map(|m| m.name(false))
            .collect();
        assert_eq!(
            names,
            [
                "src/main.rs: replace Shape::Tall with Shape::Low in f",
                "src/main.rs: replace Shape::Low with Shape::Tall in f",
            ]
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
