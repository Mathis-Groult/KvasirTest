// Copyright 2026 Mathis Groult
// SPDX-License-Identifier: GPL-3.0-or-later (KvasirTest; see NOTICE)

//! `--fn PATH`: keep only mutants inside the named functions.
//!
//! `--re` matches the whole printed name, which holds generic arguments
//! (`ProtoNode<'_>::write_to`) and the mutated text (a `delete self.x.write_nbt(..)` in a wrapper
//! matches `--re write_nbt`). `--fn` matches the enclosing function only, by path segments,
//! ignoring generic arguments: `--fn ProtoNode::write_to`, `--fn write_to`, and for trait impls
//! either `--fn '<impl EntityBase for TextDisplayEntity>::init_data_tracker'` or
//! `--fn TextDisplayEntity::init_data_tracker`.

/// The names `function` answers to: the path without generic arguments, and for a trait impl
/// (`<impl T for Y>::f`) also `Y::f`.
fn names(function: &str) -> Vec<String> {
    let plain = strip_generics(function);
    let mut out = vec![plain.clone()];
    if let Some(rest) = plain.strip_prefix("<impl ")
        && let Some((head, tail)) = rest.split_once(">::")
        && let Some((_, ty)) = head.rsplit_once(" for ")
    {
        out.push(format!("{ty}::{tail}"));
    }
    out
}

/// Remove `<..>` generic argument lists that follow a name (`A<'_>::f` -> `A::f`), keeping a
/// leading `<impl .. for ..>`.
fn strip_generics(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for c in s.chars() {
        let after_name = out
            .chars()
            .last()
            .is_some_and(|p| p.is_alphanumeric() || p == '_');
        match c {
            '<' if depth > 0 || after_name => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth > 0 => {}
            _ => out.push(c),
        }
    }
    out
}

/// True if `function` is one of `patterns` (a whole name or its trailing `::` segments).
pub fn matches(patterns: &[String], function: Option<&str>) -> bool {
    let Some(function) = function else {
        return false;
    };
    let names = names(function);
    patterns.iter().any(|p| {
        let p = strip_generics(p.trim());
        names
            .iter()
            .any(|n| *n == p || n.ends_with(&format!("::{p}")))
    })
}

#[cfg(test)]
mod test {
    use super::*;

    fn m(p: &str, f: &str) -> bool {
        matches(&[p.to_owned()], Some(f))
    }

    #[test]
    fn generic_arguments_are_ignored() {
        assert!(m("ProtoNode::write_to", "ProtoNode<'_>::write_to"));
        assert!(m("write_to", "ProtoNode<'_>::write_to"));
        assert!(m("ProtoNode<'a>::write_to", "ProtoNode<'_>::write_to"));
        assert!(!m("Node::write_to", "ProtoNode<'_>::write_to"));
        assert!(!m("write", "ProtoNode<'_>::write_to"));
    }

    #[test]
    fn trait_impls_answer_to_the_type_too() {
        let f = "<impl EntityBase for TextDisplayEntity>::init_data_tracker";
        assert!(m(f, f));
        assert!(m("TextDisplayEntity::init_data_tracker", f));
        assert!(m("init_data_tracker", f));
        assert!(!m("BlockDisplayEntity::init_data_tracker", f));
        assert!(m("Wrap::get", "<impl Deref<Target = T> for Wrap<T>>::get"));
    }

    #[test]
    fn free_and_nested_functions() {
        assert!(m("compute_wall_state", "compute_wall_state"));
        assert!(m("walls::compute_wall_state", "walls::compute_wall_state"));
        assert!(!matches(&["f".to_owned()], None));
    }
}
