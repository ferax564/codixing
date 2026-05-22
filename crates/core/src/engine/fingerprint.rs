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
pub fn signature_fingerprint(entities: &[SemanticEntity]) -> Option<u64> {
    if entities.is_empty() {
        // No AST-derived structure to fingerprint (config, docs, plain text,
        // or unsupported language). Be conservative — let the caller re-embed.
        return None;
    }

    // Build one canonical descriptor line per entity. We intentionally include
    // only signature-level information:
    //   kind | scope::name | signature | visibility | sorted type relations
    // and exclude byte/line ranges, doc comments, and bodies.
    let mut descriptors: Vec<String> = entities.iter().map(entity_descriptor).collect();

    // Sort so the fingerprint is independent of extraction order. A reordering
    // of declarations within a file is itself a cosmetic change.
    descriptors.sort_unstable();

    // Hash the joined descriptors with a domain separator between entries so
    // that "ab" + "c" cannot collide with "a" + "bc".
    let joined = descriptors.join("\u{1f}"); // unit separator
    Some(xxhash_rust::xxh3::xxh3_64(joined.as_bytes()))
}

/// Render a single entity into a canonical, body-free descriptor string.
fn entity_descriptor(e: &SemanticEntity) -> String {
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

    format!(
        "{kind}|{scoped_name}|{signature}|{visibility}|{relations}",
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
        assert_eq!(signature_fingerprint(&[]), None);
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
        assert_eq!(signature_fingerprint(&a), signature_fingerprint(&b));
        assert!(signature_fingerprint(&a).is_some());
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
        assert_eq!(signature_fingerprint(&a), signature_fingerprint(&b));
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
            signature_fingerprint(&before),
            signature_fingerprint(&after)
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
            signature_fingerprint(&before),
            signature_fingerprint(&after)
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
            signature_fingerprint(&before),
            signature_fingerprint(&after)
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
            signature_fingerprint(&before),
            signature_fingerprint(&after)
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
            signature_fingerprint(&[before]),
            signature_fingerprint(&[after])
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
        assert_ne!(signature_fingerprint(&[a]), signature_fingerprint(&[b]));
    }
}
