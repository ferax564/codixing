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

use crate::language::{EntityKind, SemanticEntity, Visibility};

/// Compute a deterministic signature fingerprint for a file from the semantic
/// entities its parser extracted.
///
/// Returns `None` when no fingerprint can be computed (no entities), signalling
/// the caller to treat any change as STRUCTURAL (re-embed). When `Some(hash)` is
/// returned, two files with the same public/internal *shape* (identical symbol
/// signatures, imports, and exports) hash identically regardless of body,
/// comment, or whitespace differences.
///
/// The hash is stable across runs and platforms: entity descriptors are sorted
/// before hashing so extraction order does not affect the result.
pub fn signature_fingerprint(entities: &[SemanticEntity], source: &[u8]) -> Option<u64> {
    if entities.is_empty() {
        // No AST-derived structure to fingerprint (config, docs, plain text,
        // or unsupported language). Be conservative — let the caller re-embed.
        return None;
    }

    // Build one canonical descriptor line per entity. We intentionally include
    // only signature-level information:
    //   kind | scope::name | signature | visibility | sorted type relations
    // and exclude byte/line ranges, doc comments, and function bodies — EXCEPT
    // for aggregate kinds (struct/enum/interface) whose members are not emitted
    // as separate entities (see `entity_descriptor`).
    let mut descriptors: Vec<String> = entities
        .iter()
        .map(|e| entity_descriptor(e, source))
        .collect();

    // Sort so the fingerprint is independent of extraction order. A reordering
    // of declarations within a file is itself a cosmetic change.
    descriptors.sort_unstable();

    // Hash the joined descriptors with a domain separator between entries so
    // that "ab" + "c" cannot collide with "a" + "bc".
    let joined = descriptors.join("\u{1f}"); // unit separator
    Some(xxhash_rust::xxh3::xxh3_64(joined.as_bytes()))
}

/// Render a single entity into a canonical descriptor string.
///
/// Function/method bodies are excluded (their `signature` stops at the body),
/// so a body-only edit does not change the descriptor. But aggregate kinds —
/// `struct`, `enum`, `interface` — keep their members (field/variant names and
/// types) *inside* the braces, and those members are NOT emitted as separate
/// entities, so the signature line alone (everything before `{`) would miss a
/// field/variant rename. For those kinds we fold a hash of the entity's full
/// source span into the descriptor. These kinds carry no executable body, so
/// this does not reintroduce body-sensitivity for functions.
fn entity_descriptor(e: &SemanticEntity, source: &[u8]) -> String {
    let scoped_name = if e.scope.is_empty() {
        e.name.clone()
    } else {
        format!("{}::{}", e.scope.join("::"), e.name)
    };

    let signature = e.signature.as_deref().unwrap_or("");
    let visibility = visibility_tag(&e.visibility);

    // Type relations (implements / extends / returns / contains) are part of a
    // symbol's structural surface — a changed return type or base class must be
    // STRUCTURAL. Sort them for order independence.
    let mut relations: Vec<String> = e
        .type_relations
        .iter()
        .map(|r| format!("{}:{}", r.kind, r.target))
        .collect();
    relations.sort_unstable();

    // Member-bearing aggregates: hash the full span so member renames/additions
    // (invisible to the pre-brace signature) change the fingerprint.
    let members = match e.kind {
        EntityKind::Struct | EntityKind::Enum | EntityKind::Interface => source
            .get(e.byte_range.clone())
            .map(|b| xxhash_rust::xxh3::xxh3_64(b).to_string())
            .unwrap_or_default(),
        _ => String::new(),
    };

    format!(
        "{kind}|{scoped_name}|{signature}|{visibility}|{relations}|{members}",
        kind = entity_kind_tag(&e.kind),
        relations = relations.join(","),
    )
}

/// Stable string tag for an entity kind (decoupled from `Display`, which may
/// change for human-facing output).
fn entity_kind_tag(kind: &EntityKind) -> &'static str {
    match kind {
        EntityKind::Function => "fn",
        EntityKind::Method => "method",
        EntityKind::Class => "class",
        EntityKind::Struct => "struct",
        EntityKind::Enum => "enum",
        EntityKind::Interface => "interface",
        EntityKind::Trait => "trait",
        EntityKind::TypeAlias => "typealias",
        EntityKind::Constant => "const",
        EntityKind::Static => "static",
        EntityKind::Module => "module",
        EntityKind::Import => "import",
        EntityKind::Impl => "impl",
        EntityKind::Namespace => "namespace",
        EntityKind::Variable => "var",
        EntityKind::Type => "type",
    }
}

/// Stable string tag for a visibility level.
fn visibility_tag(v: &Visibility) -> &'static str {
    match v {
        Visibility::Public => "pub",
        Visibility::CrateInternal => "crate",
        Visibility::Private => "priv",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::{TypeRelation, TypeRelationKind};

    fn entity(kind: EntityKind, name: &str, sig: Option<&str>, vis: Visibility) -> SemanticEntity {
        SemanticEntity {
            kind,
            name: name.to_string(),
            signature: sig.map(|s| s.to_string()),
            doc_comment: None,
            byte_range: 0..0,
            line_range: 0..0,
            scope: Vec::new(),
            visibility: vis,
            type_relations: Vec::new(),
        }
    }

    #[test]
    fn empty_entities_yield_no_fingerprint() {
        // A file with no AST entities (config / docs) must not be fingerprinted —
        // the caller treats this as STRUCTURAL.
        assert_eq!(signature_fingerprint(&[], b""), None);
    }

    #[test]
    fn identical_signatures_hash_identically() {
        let a = vec![
            entity(
                EntityKind::Function,
                "foo",
                Some("fn foo(a: u32) -> u32"),
                Visibility::Public,
            ),
            entity(
                EntityKind::Struct,
                "Bar",
                Some("struct Bar"),
                Visibility::Public,
            ),
        ];
        let b = a.clone();
        assert_eq!(
            signature_fingerprint(&a, b""),
            signature_fingerprint(&b, b"")
        );
        assert!(signature_fingerprint(&a, b"").is_some());
    }

    #[test]
    fn declaration_order_does_not_matter() {
        let a = vec![
            entity(
                EntityKind::Function,
                "foo",
                Some("fn foo()"),
                Visibility::Public,
            ),
            entity(
                EntityKind::Function,
                "bar",
                Some("fn bar()"),
                Visibility::Public,
            ),
        ];
        let mut b = a.clone();
        b.reverse();
        // Reordering declarations is a cosmetic change → same fingerprint.
        assert_eq!(
            signature_fingerprint(&a, b""),
            signature_fingerprint(&b, b"")
        );
    }

    #[test]
    fn changed_parameter_changes_fingerprint() {
        let before = vec![entity(
            EntityKind::Function,
            "foo",
            Some("fn foo(a: u32) -> u32"),
            Visibility::Public,
        )];
        let after = vec![entity(
            EntityKind::Function,
            "foo",
            Some("fn foo(a: u32, b: u32) -> u32"),
            Visibility::Public,
        )];
        assert_ne!(
            signature_fingerprint(&before, b""),
            signature_fingerprint(&after, b"")
        );
    }

    #[test]
    fn renamed_symbol_changes_fingerprint() {
        let before = vec![entity(
            EntityKind::Function,
            "foo",
            Some("fn foo()"),
            Visibility::Public,
        )];
        let after = vec![entity(
            EntityKind::Function,
            "renamed",
            Some("fn renamed()"),
            Visibility::Public,
        )];
        assert_ne!(
            signature_fingerprint(&before, b""),
            signature_fingerprint(&after, b"")
        );
    }

    #[test]
    fn changed_visibility_changes_fingerprint() {
        let before = vec![entity(
            EntityKind::Function,
            "foo",
            Some("fn foo()"),
            Visibility::Private,
        )];
        let after = vec![entity(
            EntityKind::Function,
            "foo",
            Some("fn foo()"),
            Visibility::Public,
        )];
        assert_ne!(
            signature_fingerprint(&before, b""),
            signature_fingerprint(&after, b"")
        );
    }

    #[test]
    fn changed_import_changes_fingerprint() {
        let before = vec![entity(
            EntityKind::Import,
            "std::collections::HashMap",
            None,
            Visibility::Private,
        )];
        let after = vec![entity(
            EntityKind::Import,
            "std::collections::BTreeMap",
            None,
            Visibility::Private,
        )];
        assert_ne!(
            signature_fingerprint(&before, b""),
            signature_fingerprint(&after, b"")
        );
    }

    #[test]
    fn changed_return_type_relation_changes_fingerprint() {
        let mut before = entity(
            EntityKind::Function,
            "foo",
            Some("fn foo()"),
            Visibility::Public,
        );
        before.type_relations = vec![TypeRelation {
            kind: TypeRelationKind::Returns,
            target: "u32".to_string(),
        }];
        let mut after = before.clone();
        after.type_relations = vec![TypeRelation {
            kind: TypeRelationKind::Returns,
            target: "u64".to_string(),
        }];
        assert_ne!(
            signature_fingerprint(&[before], b""),
            signature_fingerprint(&[after], b"")
        );
    }

    #[test]
    fn scope_disambiguates_same_name() {
        // Two methods named `new` in different scopes must not collide.
        let mut a = entity(
            EntityKind::Method,
            "new",
            Some("fn new()"),
            Visibility::Public,
        );
        a.scope = vec!["Foo".to_string()];
        let mut b = entity(
            EntityKind::Method,
            "new",
            Some("fn new()"),
            Visibility::Public,
        );
        b.scope = vec!["Bar".to_string()];
        assert_ne!(
            signature_fingerprint(&[a], b""),
            signature_fingerprint(&[b], b"")
        );
    }

    /// Helper: a struct entity whose `byte_range` spans the whole `src`.
    fn struct_over(name: &str, src: &[u8]) -> SemanticEntity {
        SemanticEntity {
            kind: EntityKind::Struct,
            name: name.to_string(),
            signature: Some(format!("pub struct {name}")),
            doc_comment: None,
            byte_range: 0..src.len(),
            line_range: 0..0,
            scope: Vec::new(),
            visibility: Visibility::Public,
            type_relations: Vec::new(),
        }
    }

    #[test]
    fn struct_field_rename_changes_fingerprint() {
        // Same pre-brace signature, renamed field with an unchanged type. Field
        // names live inside the braces and are not separate entities, so without
        // the member-span hash this would be misclassified COSMETIC and reuse a
        // stale vector. Regression for the codex P2.
        let a = b"pub struct Config { verbose: bool }";
        let b = b"pub struct Config { debug: bool }";
        assert_ne!(
            signature_fingerprint(&[struct_over("Config", a)], a),
            signature_fingerprint(&[struct_over("Config", b)], b),
            "a struct field rename must change the fingerprint (STRUCTURAL)"
        );
    }

    #[test]
    fn struct_body_identical_keeps_fingerprint() {
        // Identical struct text → identical fingerprint (the member hash is stable).
        let s = b"pub struct Config { verbose: bool }";
        assert_eq!(
            signature_fingerprint(&[struct_over("Config", s)], s),
            signature_fingerprint(&[struct_over("Config", s)], s),
        );
    }

    #[test]
    fn function_fingerprint_ignores_source_body() {
        // Functions exclude bodies (only struct/enum/interface fold member text),
        // so the same function signature fingerprints identically regardless of
        // the surrounding source bytes — a body edit stays COSMETIC.
        let f = || {
            vec![entity(
                EntityKind::Function,
                "f",
                Some("fn f(a: u32) -> u32"),
                Visibility::Public,
            )]
        };
        assert_eq!(
            signature_fingerprint(&f(), b"fn f(a: u32) -> u32 { a + 1 }"),
            signature_fingerprint(&f(), b"fn f(a: u32) -> u32 { a + 2 }"),
        );
    }
}
