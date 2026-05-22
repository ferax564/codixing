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
/// removed and the remaining bytes whitespace-normalized. Two files therefore
/// fingerprint identically iff they differ only inside function bodies (or in
/// pure formatting). Any other change — a renamed struct field, a changed
/// `const` value, an added import, a modified signature, a reordered or
/// re-commented declaration — alters the fingerprint and is classified
/// STRUCTURAL.
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

    Some(xxhash_rust::xxh3::xxh3_64(&masked_normalized(
        source, &merged,
    )))
}

/// Body span of a function/method: from its first `{` to the end of its range.
/// `None` when the range contains no brace (a bodyless declaration).
fn body_span(source: &[u8], range: &std::ops::Range<usize>) -> Option<(usize, usize)> {
    let end = range.end.min(source.len());
    let start = range.start.min(end);
    let brace = source[start..end].iter().position(|&b| b == b'{')?;
    Some((start + brace, end))
}

/// `source` with the given (sorted, merged) byte spans removed and every run of
/// ASCII whitespace collapsed to a single space. Whitespace folding makes pure
/// reformatting (the canonical cosmetic edit) hash identically, while any real
/// token change outside a function body still surfaces.
fn masked_normalized(source: &[u8], remove: &[(usize, usize)]) -> Vec<u8> {
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

    let mut norm: Vec<u8> = Vec::with_capacity(kept.len());
    let mut prev_ws = false;
    for &b in &kept {
        if b.is_ascii_whitespace() {
            if !prev_ws {
                norm.push(b' ');
                prev_ws = true;
            }
        } else {
            norm.push(b);
            prev_ws = false;
        }
    }
    norm
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
    fn reformatting_outside_body_is_cosmetic() {
        // Differs only by whitespace runs outside the body → normalized away.
        let a = b"fn  f()   {x}";
        let b = b"fn f() {x}";
        assert_eq!(
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
}
