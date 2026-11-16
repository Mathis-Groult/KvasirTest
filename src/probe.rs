// Copyright 2026 Mathis Groult
// KvasirTest: see docs/decisions/0002-test-mode-reach-map.md.

//! Functions of a source file for `test` mode: a stable key and a hash per function, a hash of
//! everything else in the file, and the probes that record which functions a test executes.
//!
//! Hashes are taken over token streams, so comments and formatting never count as a change.

use quote::ToTokens;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;

use crate::schema::{LineIndex, REACH_FILE_VAR};
use crate::span::LineColumn;

/// A function that can hold a probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FnInfo {
    /// `path/to/file.rs::module::<impl Trait for Type>::name`, with `#2`, `#3` for repeats.
    pub key: String,
    /// Hash of the whole item: attributes, signature and body.
    pub hash: u64,
    /// Byte offset just after the body's opening brace, where the probe goes.
    pub probe_at: usize,
}

/// What `test` mode knows about one source file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileFns {
    /// Hash of the file with every probed function item taken out.
    pub outside_hash: u64,
    pub fns: Vec<FnInfo>,
}

/// FNV-1a, 64 bits: stable across Rust releases (unlike `DefaultHasher`).
pub fn fnv64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The probe statement for function `id`: when [`REACH_FILE_VAR`] names a file, the first call
/// in the process appends the id to it. Afterwards (or when not recording) it costs one atomic
/// load. Written for every edition and without imports.
pub fn probe(id: u64) -> String {
    format!(
        "{{ static KVASIR_TEST_PROBE: ::std::sync::atomic::AtomicBool = \
         ::std::sync::atomic::AtomicBool::new(false); \
         if !KVASIR_TEST_PROBE.load(::std::sync::atomic::Ordering::Relaxed) \
         && !KVASIR_TEST_PROBE.swap(true, ::std::sync::atomic::Ordering::Relaxed) {{ \
         if let Some(path) = ::std::env::var_os(\"{REACH_FILE_VAR}\") {{ \
         if let Ok(mut file) = ::std::fs::OpenOptions::new().create(true).append(true).open(path) {{ \
         let _ = ::std::io::Write::write_all(&mut file, b\"{id}\\n\"); }} }} }} }} "
    )
}

/// Probe ids in a reach file written by probes (one per line; others are ignored).
pub fn parse_probe_ids(text: &str) -> std::collections::BTreeSet<u64> {
    text.lines().filter_map(|l| l.trim().parse().ok()).collect()
}

/// Find the probe-able functions of `code` (the file at `rel_path`).
pub fn analyse(rel_path: &str, code: &str) -> Result<FileFns, syn::Error> {
    let file = syn::parse_file(code)?;
    let mut finder = Finder {
        rel_path,
        code,
        index: LineIndex::new(code),
        path: Vec::new(),
        fns: Vec::new(),
    };
    finder.visit_file(&file);
    let mut outside = file;
    StripFns.visit_file_mut(&mut outside);
    Ok(FileFns {
        outside_hash: fnv64(outside.to_token_stream().to_string().as_bytes()),
        fns: finder.fns,
    })
}

/// `code` with the probe for `ids[i]` inserted into function `fns[i]`.
pub fn instrument(code: &str, fns: &[FnInfo], ids: &[u64]) -> String {
    let mut at: Vec<(usize, u64)> = fns
        .iter()
        .map(|f| f.probe_at)
        .zip(ids.iter().copied())
        .collect();
    at.sort_unstable();
    let mut out = String::with_capacity(code.len() + at.len() * 400);
    let mut last = 0;
    for (offset, id) in at {
        out.push_str(&code[last..offset]);
        out.push_str(&probe(id));
        last = offset;
    }
    out.push_str(&code[last..]);
    out
}

fn is_probed(sig: &syn::Signature) -> bool {
    // A const fn may run at compile time, where a probe cannot: it stays part of the outside.
    sig.constness.is_none()
}

struct Finder<'a> {
    rel_path: &'a str,
    code: &'a str,
    index: LineIndex,
    /// Enclosing modules, impls, traits and functions.
    path: Vec<String>,
    fns: Vec<FnInfo>,
}

impl Finder<'_> {
    fn add(&mut self, name: &syn::Ident, item: &impl ToTokens, block: &syn::Block) {
        let open = block.brace_token.span.open().end();
        let Some(probe_at) = self.index.offset(
            self.code,
            LineColumn {
                line: open.line,
                column: open.column + 1,
            },
        ) else {
            return;
        };
        let mut key = format!("{}::", self.rel_path);
        for part in &self.path {
            key.push_str(part);
            key.push_str("::");
        }
        key.push_str(&name.to_string());
        let base = key.clone();
        let mut n = 1;
        while self.fns.iter().any(|f| f.key == key) {
            n += 1;
            key = format!("{base}#{n}");
        }
        self.fns.push(FnInfo {
            key,
            hash: fnv64(item.to_token_stream().to_string().as_bytes()),
            probe_at,
        });
    }

    fn nested<F: FnOnce(&mut Self)>(&mut self, part: String, f: F) {
        self.path.push(part);
        f(self);
        self.path.pop();
    }
}

impl<'ast> Visit<'ast> for Finder<'_> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.nested(i.ident.to_string(), |v| syn::visit::visit_item_mod(v, i));
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let ty = i.self_ty.to_token_stream().to_string();
        let part = match &i.trait_ {
            Some((_, path, _)) => format!("<impl {} for {ty}>", path.to_token_stream()),
            None => format!("<impl {ty}>"),
        };
        self.nested(part, |v| syn::visit::visit_item_impl(v, i));
    }

    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        self.nested(format!("<trait {}>", i.ident), |v| {
            syn::visit::visit_item_trait(v, i);
        });
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        if is_probed(&i.sig) {
            self.add(&i.sig.ident, i, &i.block);
        }
        self.nested(i.sig.ident.to_string(), |v| syn::visit::visit_item_fn(v, i));
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        if is_probed(&i.sig) {
            self.add(&i.sig.ident, i, &i.block);
        }
        self.nested(i.sig.ident.to_string(), |v| {
            syn::visit::visit_impl_item_fn(v, i);
        });
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        if let Some(block) = &i.default
            && is_probed(&i.sig)
        {
            self.add(&i.sig.ident, i, block);
        }
        self.nested(i.sig.ident.to_string(), |v| {
            syn::visit::visit_trait_item_fn(v, i);
        });
    }
}

/// Removes every probed function item, leaving what a function hash does not cover.
struct StripFns;

impl VisitMut for StripFns {
    fn visit_file_mut(&mut self, i: &mut syn::File) {
        i.items
            .retain(|item| !matches!(item, syn::Item::Fn(f) if is_probed(&f.sig)));
        syn::visit_mut::visit_file_mut(self, i);
    }

    fn visit_item_mod_mut(&mut self, i: &mut syn::ItemMod) {
        if let Some((_, items)) = &mut i.content {
            items.retain(|item| !matches!(item, syn::Item::Fn(f) if is_probed(&f.sig)));
        }
        syn::visit_mut::visit_item_mod_mut(self, i);
    }

    fn visit_item_impl_mut(&mut self, i: &mut syn::ItemImpl) {
        i.items
            .retain(|item| !matches!(item, syn::ImplItem::Fn(f) if is_probed(&f.sig)));
        syn::visit_mut::visit_item_impl_mut(self, i);
    }

    fn visit_item_trait_mut(&mut self, i: &mut syn::ItemTrait) {
        i.items.retain(|item| {
            !matches!(item, syn::TraitItem::Fn(f) if f.default.is_some() && is_probed(&f.sig))
        });
        syn::visit_mut::visit_item_trait_mut(self, i);
    }
}

#[cfg(test)]
mod test {
    use pretty_assertions::assert_eq;

    use super::*;

    const CODE: &str = "\
use std::fmt;

/// Doc.
pub fn top(x: u32) -> u32 {
    fn inner() {}
    x + 1
}

const fn fixed() -> u32 { 3 }

struct S;

impl S {
    fn method(&self) {}
}

impl fmt::Display for S {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { write!(f, \"s\") }
}

trait T {
    fn required(&self);
    fn provided(&self) {}
}

mod m {
    fn top() {}
}
";

    #[test]
    fn functions_get_qualified_keys_and_const_fns_are_left_out() {
        let file = analyse("src/lib.rs", CODE).unwrap();
        let keys: Vec<&str> = file.fns.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "src/lib.rs::top",
                "src/lib.rs::top::inner",
                "src/lib.rs::<impl S>::method",
                "src/lib.rs::<impl fmt :: Display for S>::fmt",
                "src/lib.rs::<trait T>::provided",
                "src/lib.rs::m::top",
            ]
        );
    }

    #[test]
    fn comments_and_formatting_change_no_hash() {
        let a = analyse("a.rs", CODE).unwrap();
        let reformatted = CODE
            .replace("    x + 1", "    // a comment\n    x   +   1")
            .replace("/// Doc.", "// plain comment");
        let b = analyse("a.rs", &reformatted).unwrap();
        // A doc comment is an attribute: removing it changes `top`; nothing else moves.
        assert_ne!(a.fns[0].hash, b.fns[0].hash);
        assert_eq!(
            a.fns[1..].iter().map(|f| f.hash).collect::<Vec<_>>(),
            b.fns[1..].iter().map(|f| f.hash).collect::<Vec<_>>()
        );
        assert_eq!(a.outside_hash, b.outside_hash);
        let only_comment =
            analyse("a.rs", &CODE.replace("    x + 1", "    // c\n    x + 1")).unwrap();
        let hashes = |f: &FileFns| f.fns.iter().map(|f| f.hash).collect::<Vec<_>>();
        assert_eq!(hashes(&a), hashes(&only_comment));
        assert_eq!(a.outside_hash, only_comment.outside_hash);
    }

    #[test]
    fn a_body_change_moves_only_that_function() {
        let a = analyse("a.rs", CODE).unwrap();
        let b = analyse("a.rs", &CODE.replace("x + 1", "x + 2")).unwrap();
        assert_ne!(a.fns[0].hash, b.fns[0].hash);
        assert_eq!(a.fns[1].hash, b.fns[1].hash);
        assert_eq!(a.outside_hash, b.outside_hash);
    }

    #[test]
    fn non_function_changes_move_the_outside_hash() {
        let a = analyse("a.rs", CODE).unwrap();
        for changed in [
            CODE.replace("struct S;", "struct S(u8);"),
            CODE.replace("{ 3 }", "{ 4 }"),
            CODE.replace("fn required(&self);", "fn required(&self, x: u8);"),
            CODE.replace("use std::fmt;", "use std::fmt::{self};"),
        ] {
            assert_ne!(
                a.outside_hash,
                analyse("a.rs", &changed).unwrap().outside_hash,
                "{changed}"
            );
        }
    }

    #[test]
    fn probes_go_right_after_the_opening_brace() {
        let code = "fn a() { 1; }\nfn b() {}\n";
        let file = analyse("a.rs", code).unwrap();
        let out = instrument(code, &file.fns, &[7, 8]);
        assert_eq!(
            out,
            format!("fn a() {{{} 1; }}\nfn b() {{{}}}\n", probe(7), probe(8))
        );
        assert!(syn::parse_file(&out).is_ok());
    }

    #[test]
    fn repeated_names_get_a_counter() {
        let code = "#[cfg(unix)]\nfn f() {}\n#[cfg(windows)]\nfn f() {}\n";
        let keys: Vec<String> = analyse("a.rs", code)
            .unwrap()
            .fns
            .into_iter()
            .map(|f| f.key)
            .collect();
        assert_eq!(keys, ["a.rs::f", "a.rs::f#2"]);
    }
}
