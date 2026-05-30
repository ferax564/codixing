//! Deterministic per-file *signature fingerprint* used by [`Engine::sync`] to
//! classify a content change as **COSMETIC** or **STRUCTURAL**.
//!
//! ## Why
//!
//! Embedding is the expensive path during sync — dense embeds dominate wall
//! time on large repositories (hours). A content change that only touches
//! function bodies, comments, or whitespace does **not** alter retrieval-relevant
//! structure, so the cached embedding vectors can be reused instead of being
//! recomputed.
//!
//! The fingerprint is a stable hash over the file's *signatures* only:
//! symbol names + signatures + visibility + kind, plus import sources and export
//! names. Bodies, comments, and formatting are excluded **by construction**
//! (the AST extractor never puts a function body into [`SemanticEntity::signature`]).
//!
//! ## Conservatism
//!
//! Correctness beats savings. [`signature_fingerprint`] returns `None` whenever
//! a stable fingerprint cannot be computed (no AST entities — e.g. config / doc
//! / plain-text files, or an unsupported language). A `None` fingerprint is
//! treated by the caller as **STRUCTURAL** (full re-embed). A wrong COSMETIC
//! classification would silently produce stale embeddings, which is far worse
//! than the redundant embed work a conservative STRUCTURAL classification costs.

use crate::language::{EntityKind, Language, SemanticEntity};

/// Languages eligible for COSMETIC reuse — an explicit **opt-in allowlist**.
///
/// Cosmetic reuse trusts that the signature fingerprint captures a file's entire
/// retrieval-relevant surface. That trust must be vetted per language, because
/// each language's extractor represents structure differently (e.g. struct
/// fields, class members, and config values are not all emitted as entities, and
/// some signatures truncate long values). A language earns a place here only with
/// a regression test pinning "a member rename is detected (STRUCTURAL) while a
/// body-only edit is reused (COSMETIC)".
///
/// Anything not on this list — every other language, plus config / doc / notebook
/// files — is treated as STRUCTURAL (full re-embed). Start with Rust, the language
/// the empirical separation was measured on; add others one at a time.
fn cosmetic_eligible(language: Language) -> bool {
    matches!(language, Language::Rust)
}

/// Deterministic per-file fingerprint that ignores **only** `Function`/`Method`
/// bodies.
///
/// The hash is computed over the file with every function/method body span
/// removed. Two files therefore fingerprint identically iff they differ only
/// inside function bodies (body reformatting included, since the whole body is
/// removed). Any change to the non-body bytes — a renamed struct field, a
/// changed `const` value, an added import, a modified signature, a reordered or
/// re-commented declaration, or reformatting of the non-body layout — alters the
/// fingerprint and is classified STRUCTURAL. (Non-body reformatting re-embedding
/// is the safe direction: it never reuses a stale vector.)
///
/// This "exclude only bodies" formulation is the deliberate inverse of
/// enumerating which constructs to include: it cannot miss a structural change
/// in a construct nobody special-cased (the failure mode that repeatedly bit the
/// per-kind approach). The cost is mild conservatism — a comment edit or a
/// declaration reorder outside a body re-embeds — which is the safe direction.
///
/// Returns `None` (→ STRUCTURAL) for non-allowlisted languages and for files
/// with no AST entities.
pub fn signature_fingerprint(
    entities: &[SemanticEntity],
    source: &[u8],
    language: Language,
) -> Option<u64> {
    if !cosmetic_eligible(language) {
        // Language not vetted for cosmetic reuse — every change is STRUCTURAL.
        return None;
    }
    if entities.is_empty() {
        // No AST-derived structure to reason about. Be conservative — re-embed.
        return None;
    }

    // Collect the body spans of functions/methods — the only regions a COSMETIC
    // edit is allowed to touch. A function's `byte_range` ends at its closing
    // brace, so its body is `[first '{' .. range end]`. Bodyless declarations
    // (e.g. a trait method signature `fn f();`) have no brace and contribute
    // nothing, so a change to them stays STRUCTURAL.
    let mut bodies: Vec<(usize, usize)> = entities
        .iter()
        .filter(|e| matches!(e.kind, EntityKind::Function | EntityKind::Method))
        .filter_map(|e| body_span(source, &e.byte_range))
        .collect();
    bodies.sort_unstable();

    // Merge overlapping/nested spans (a closure or nested fn lives inside an
    // outer body) so masking is unambiguous.
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(bodies.len());
    for (s, e) in bodies {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }

    // Local items: Rust indexes items declared *inside* a function body (e.g.
    // `fn outer() { fn inner(a: u32) {} }`) as their own entities, with their
    // own chunks/vectors. Masking the outer body would hide a nested item's
    // signature, so a change to it could be wrongly classified COSMETIC. If any
    // entity begins inside a body span, bail to STRUCTURAL (re-embed the file) —
    // we can only safely mask bodies that contain no indexed structure.
    let has_nested_item = entities.iter().any(|e| {
        merged
            .iter()
            .any(|&(bs, be)| e.byte_range.start > bs && e.byte_range.start < be)
    });
    if has_nested_item {
        return None;
    }

    Some(xxhash_rust::xxh3::xxh3_64(&masked(source, &merged)))
}

/// Body span of a function/method: the final brace-balanced block, found by
/// matching backward from the entity's last `}`. Matching from the end (rather
/// than taking the first `{`) skips braces that belong to the *signature* —
/// const-generic bounds, `const { .. }` blocks in return types — so an edit
/// inside those is correctly treated as STRUCTURAL.
///
/// `None` when the range contains no brace (a bodyless declaration such as a
/// trait method signature `fn f();`).
///
/// Note: this is a byte-level brace match that does not track string/char
/// literals or comments inside the signature. That is acceptable: an unbalanced
/// brace inside a signature literal is exceedingly rare, and a mismatch only
/// shifts the masked region, which the surrounding STRUCTURAL bytes still guard.
fn body_span(source: &[u8], range: &std::ops::Range<usize>) -> Option<(usize, usize)> {
    let end = range.end.min(source.len());
    let start = range.start.min(end);
    let slice = &source[start..end];
    let last_close = slice.iter().rposition(|&b| b == b'}')?;
    let mut depth = 0i32;
    let mut i = last_close as isize;
    while i >= 0 {
        match slice[i as usize] {
            b'}' => depth += 1,
            b'{' => {
                depth -= 1;
                if depth == 0 {
                    return Some((start + i as usize, end));
                }
            }
            _ => {}
        }
        i -= 1;
    }
    None
}

/// `source` with the given (sorted, merged) byte spans removed.
///
/// No whitespace normalization: collapsing whitespace would also fold runs
/// *inside string/char literals* outside function bodies, so a literal value
/// change like `"foo  bar"` → `"foo bar"` would hash identically and be
/// wrongly classified COSMETIC. The trade-off is that pure reformatting of the
/// non-body parts (signatures, struct layout) re-embeds — the safe direction.
/// Body reformatting stays COSMETIC because the entire body span is removed.
fn masked(source: &[u8], remove: &[(usize, usize)]) -> Vec<u8> {
    let mut kept: Vec<u8> = Vec::with_capacity(source.len());
    let mut i = 0usize;
    for &(s, e) in remove {
        if s > i {
            kept.extend_from_slice(&source[i..s]);
        }
        i = e.max(i);
    }
    if i < source.len() {
        kept.extend_from_slice(&source[i..]);
    }
    kept
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Visibility;

    /// Build an entity of `kind` spanning `range` in some source. Only the kind
    /// and byte_range matter for the masked-body fingerprint; the rest are dummy.
    fn ent(kind: EntityKind, range: std::ops::Range<usize>) -> SemanticEntity {
        SemanticEntity {
            kind,
            name: "x".to_string(),
            signature: None,
            doc_comment: None,
            byte_range: range,
            line_range: 0..0,
            scope: Vec::new(),
            visibility: Visibility::Public,
            type_relations: Vec::new(),
        }
    }

    fn fp(entities: &[SemanticEntity], src: &[u8]) -> Option<u64> {
        signature_fingerprint(entities, src, Language::Rust)
    }

    #[test]
    fn empty_entities_yield_none() {
        assert_eq!(fp(&[], b"fn f() {}"), None);
    }

    #[test]
    fn non_allowlisted_language_is_structural() {
        // Even with entities and identical source, a non-allowlisted language is
        // never fingerprinted — always STRUCTURAL.
        let s = b"def f(): pass";
        let e = [ent(EntityKind::Function, 0..s.len())];
        assert_eq!(signature_fingerprint(&e, s, Language::Python), None);
        assert!(signature_fingerprint(&e, s, Language::Rust).is_some());
    }

    #[test]
    fn function_body_edit_is_cosmetic() {
        // Same signature, different body → identical fingerprint (the win).
        let a = b"fn f(a: u32) -> u32 { let x = 1; x }";
        let b = b"fn f(a: u32) -> u32 { compute(a) + 99 }";
        assert_eq!(
            fp(&[ent(EntityKind::Function, 0..a.len())], a),
            fp(&[ent(EntityKind::Function, 0..b.len())], b),
        );
    }

    #[test]
    fn signature_change_is_structural() {
        // The change is before the body brace → kept → STRUCTURAL.
        let a = b"fn f(a: u32) { body }";
        let b = b"fn f(a: u64) { body }";
        assert_ne!(
            fp(&[ent(EntityKind::Function, 0..a.len())], a),
            fp(&[ent(EntityKind::Function, 0..b.len())], b),
        );
    }

    #[test]
    fn body_reformatting_is_cosmetic() {
        // The whole body span is removed, so reformatting *inside* the body is
        // cosmetic regardless of whitespace.
        let a = b"fn f() {let x=1;x}";
        let b = b"fn f() {\n    let x = 1;\n    x\n}";
        assert_eq!(
            fp(&[ent(EntityKind::Function, 0..a.len())], a),
            fp(&[ent(EntityKind::Function, 0..b.len())], b),
        );
    }

    #[test]
    fn outside_body_whitespace_is_structural() {
        // No whitespace normalization: a reformat of the non-body bytes re-embeds
        // (safe direction). This is the deliberate trade for not corrupting
        // whitespace inside string/char literals.
        let a = b"fn  f()   {x}";
        let b = b"fn f() {x}";
        assert_ne!(
            fp(&[ent(EntityKind::Function, 0..a.len())], a),
            fp(&[ent(EntityKind::Function, 0..b.len())], b),
        );
    }

    #[test]
    fn string_literal_whitespace_change_is_structural() {
        // Collapsing whitespace would fold runs inside string literals too; a
        // const string value change must stay STRUCTURAL (codex round-4 P2).
        let a = b"const H: &str = \"foo  bar\";";
        let b = b"const H: &str = \"foo bar\";";
        assert_ne!(
            fp(&[ent(EntityKind::Constant, 0..a.len())], a),
            fp(&[ent(EntityKind::Constant, 0..b.len())], b),
        );
    }

    #[test]
    fn signature_brace_change_is_structural() {
        // The body brace is found by matching backward from the last `}`, so a
        // brace in the *signature* (a const block in the return type) is not
        // mistaken for the body; editing inside it is STRUCTURAL (codex round-4 P2).
        let a = b"fn f() -> [u8; { 1 }] { body }";
        let b = b"fn f() -> [u8; { 2 }] { body }";
        assert_ne!(
            fp(&[ent(EntityKind::Function, 0..a.len())], a),
            fp(&[ent(EntityKind::Function, 0..b.len())], b),
        );
    }

    #[test]
    fn const_value_change_is_structural() {
        // const/static are not bodies — a value change is kept (the codex round-3
        // multiline-const finding is covered by construction now).
        let a = b"const X: u32 = 1;";
        let b = b"const X: u32 = 2;";
        assert_ne!(
            fp(&[ent(EntityKind::Constant, 0..a.len())], a),
            fp(&[ent(EntityKind::Constant, 0..b.len())], b),
        );
    }

    #[test]
    fn struct_field_rename_is_structural() {
        // Struct bodies are not masked (only fn/method) → field rename is kept.
        let a = b"struct S { verbose: bool }";
        let b = b"struct S { debug: bool }";
        assert_ne!(
            fp(&[ent(EntityKind::Struct, 0..a.len())], a),
            fp(&[ent(EntityKind::Struct, 0..b.len())], b),
        );
    }

    #[test]
    fn comment_outside_body_is_structural() {
        // Conservative-by-design: a comment edit outside a body re-embeds (safe).
        let a = b"// old\nfn f() {x}";
        let b = b"// new\nfn f() {x}";
        assert_ne!(
            fp(&[ent(EntityKind::Function, 7..a.len())], a),
            fp(&[ent(EntityKind::Function, 7..b.len())], b),
        );
    }

    #[test]
    fn bodyless_declaration_change_is_structural() {
        // A trait method signature has no brace → no body span → STRUCTURAL.
        let a = b"fn f(a: u32);";
        let b = b"fn f(a: u64);";
        assert_ne!(
            fp(&[ent(EntityKind::Function, 0..a.len())], a),
            fp(&[ent(EntityKind::Function, 0..b.len())], b),
        );
    }

    #[test]
    fn nested_local_item_forces_structural() {
        // A function with a local item: the inner fn is indexed as its own
        // entity but sits inside the outer body. Masking the outer body would
        // hide inner's signature, so the file must be STRUCTURAL (codex round-6).
        let s = b"fn outer() { fn inner(a: u32) {} }";
        let outer = ent(EntityKind::Function, 0..s.len());
        let inner = ent(EntityKind::Function, 13..32); // `fn inner...` inside outer body
        assert_eq!(fp(&[outer, inner], s), None);
    }
}
