use tree_sitter::{Node, Tree};

use super::c_family::{macro_defined_function_name, mask_attribute_macros};
use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, Visibility, extract_preceding_comments,
    find_name_node, node_line_range, node_text,
};

/// C language support.
pub struct CLanguage;

const ENTITY_KINDS: &[&str] = &[
    "function_definition",
    "struct_specifier",
    "enum_specifier",
    "type_definition",
    "declaration",
    "preproc_include",
    "preproc_function_def",
];

impl LanguageSupport for CLanguage {
    fn language(&self) -> Language {
        Language::C
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_c::LANGUAGE.into()
    }

    fn entity_node_kinds(&self) -> &[&str] {
        ENTITY_KINDS
    }

    fn extract_entities(&self, tree: &Tree, source: &[u8]) -> Vec<SemanticEntity> {
        let mut entities = Vec::new();
        let root = tree.root_node();
        collect_entities(self, &root, source, &[], &mut entities);
        entities
    }

    fn extract_signature(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_c_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_preceding_comments(node, source, "//")
    }

    fn prepare_source<'a>(&self, source: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        mask_attribute_macros(source)
    }
}

/// Node kinds whose children are file-level declarations.
pub(super) fn is_declaration_scope(kind: &str) -> bool {
    matches!(
        kind,
        "translation_unit"
            | "preproc_if"
            | "preproc_ifdef"
            | "preproc_else"
            | "preproc_elif"
            | "preproc_elifdef"
            | "linkage_specification"
            | "declaration_list"
    )
}

/// Recognise a function defined through a macro, which the grammar sees as a
/// call expression followed by a block:
///
/// ```c
/// SYSCALL_DEFINE3(read, unsigned int, fd, char __user *, buf, size_t, count)
/// {
/// ```
///
/// `node` must be a file-level child. Returns the entity spanning the macro
/// call and its body.
pub(super) fn macro_defined_function(node: &Node, source: &[u8]) -> Option<SemanticEntity> {
    if node.kind() != "expression_statement" {
        return None;
    }
    let body = node.next_sibling()?;
    if body.kind() != "compound_statement" {
        return None;
    }
    let call = node.named_child(0)?;
    if call.kind() != "call_expression" {
        return None;
    }
    let function = call.child_by_field_name("function")?;
    if function.kind() != "identifier" {
        return None;
    }
    let macro_name = node_text(&function, source);
    // Function-defining macros are spelled in capitals; anything else here
    // is a parse artefact.
    if !macro_name.bytes().any(|b| b.is_ascii_uppercase())
        || macro_name.bytes().any(|b| b.is_ascii_lowercase())
    {
        return None;
    }
    let arguments = call.child_by_field_name("arguments")?;
    let first = arguments.named_child(0)?;
    let name = macro_defined_function_name(macro_name, node_text(&first, source))?;
    let signature = node_text(&call, source)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Some(SemanticEntity {
        kind: EntityKind::Function,
        name,
        signature: Some(signature),
        doc_comment: extract_preceding_comments(node, source, "//"),
        byte_range: node.start_byte()..body.end_byte(),
        line_range: node.start_position().row..body.end_position().row + 1,
        scope: Vec::new(),
        visibility: Visibility::default(),
        type_relations: Vec::new(),
    })
}

/// Line range of a `#define`, whose node also covers the line break that
/// ends the directive.
pub(super) fn macro_line_range(node: &Node, source: &[u8]) -> std::ops::Range<usize> {
    let start = node.start_position().row;
    let lines = node_text(node, source).trim_end().lines().count().max(1);
    start..start + lines
}

/// `#define name(params)`, without the replacement text.
pub(super) fn macro_signature(node: &Node, source: &[u8]) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    let params = node
        .child_by_field_name("parameters")
        .map(|p| node_text(&p, source))
        .unwrap_or("()");
    Some(format!("#define {}{}", node_text(&name, source), params))
}

fn collect_entities(
    lang: &CLanguage,
    node: &Node,
    source: &[u8],
    scope: &[String],
    entities: &mut Vec<SemanticEntity>,
) {
    let kind_str = node.kind();

    if let Some(entity_kind) = match_entity_kind(kind_str, node, source) {
        let name = extract_entity_name(node, source, kind_str).unwrap_or_default();

        // Skip anonymous structs/enums unless they're part of a typedef
        if name.is_empty() && matches!(entity_kind, EntityKind::Struct | EntityKind::Enum) {
            // Don't add, but do recurse
        } else {
            let entity = SemanticEntity {
                kind: entity_kind,
                name,
                signature: lang.extract_signature(node, source),
                doc_comment: lang.extract_doc_comment(node, source),
                byte_range: node.start_byte()..node.end_byte(),
                line_range: if kind_str == "preproc_function_def" {
                    macro_line_range(node, source)
                } else {
                    node_line_range(node)
                },
                scope: scope.to_vec(),
                visibility: Visibility::default(),
                type_relations: Vec::new(),
            };
            entities.push(entity);
        }
    }

    // A macro body is unparsed text: nothing to recurse into.
    if kind_str == "preproc_function_def" {
        return;
    }

    // Recurse into children
    let declaration_scope = is_declaration_scope(kind_str);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if declaration_scope && let Some(entity) = macro_defined_function(&child, source) {
            entities.push(entity);
        }
        collect_entities(lang, &child, source, scope, entities);
    }
}

/// True when a struct/enum/class specifier carries a body, i.e. it defines
/// the type rather than merely naming it.
fn has_body(node: &Node) -> bool {
    node.child_by_field_name("body").is_some()
}

fn match_entity_kind(kind: &str, node: &Node, source: &[u8]) -> Option<EntityKind> {
    match kind {
        "function_definition" => Some(EntityKind::Function),
        "struct_specifier" => {
            // Only at top level (not inside a typedef)
            if let Some(parent) = node.parent()
                && parent.kind() == "type_definition"
            {
                return None; // Will be handled by type_definition
            }
            // A specifier without a body (`struct foo *p;`, `struct foo;`) is a
            // reference or forward declaration, not a definition.
            if !has_body(node) {
                return None;
            }
            Some(EntityKind::Struct)
        }
        "enum_specifier" => {
            if let Some(parent) = node.parent()
                && parent.kind() == "type_definition"
            {
                return None;
            }
            // A specifier without a body (`struct foo *p;`, `struct foo;`) is a
            // reference or forward declaration, not a definition.
            if !has_body(node) {
                return None;
            }
            Some(EntityKind::Enum)
        }
        "type_definition" => Some(EntityKind::TypeAlias),
        "declaration" => {
            // Only include function declarations (prototypes), not variable declarations
            if is_function_declaration(node, source) {
                Some(EntityKind::Function)
            } else {
                None
            }
        }
        "preproc_include" => Some(EntityKind::Import),
        // Function-like macros are called like functions and looked up like
        // functions. Object-like macros are left to text search: a kernel
        // tree has millions of them.
        "preproc_function_def" => Some(EntityKind::Function),
        _ => None,
    }
}

/// Check if a `declaration` node is a function prototype.
fn is_function_declaration(node: &Node, _source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "function_declarator" {
            return true;
        }
        // Handle pointer declarators containing function_declarator
        if child.kind() == "pointer_declarator" || child.kind() == "init_declarator" {
            let mut inner_cursor = child.walk();
            for inner in child.children(&mut inner_cursor) {
                if inner.kind() == "function_declarator" {
                    return true;
                }
            }
        }
    }
    false
}

fn extract_entity_name(node: &Node, source: &[u8], kind: &str) -> Option<String> {
    match kind {
        "function_definition" => {
            // Function name is inside the declarator
            if let Some(declarator) = node.child_by_field_name("declarator") {
                return find_declarator_name(&declarator, source);
            }
            None
        }
        "struct_specifier" | "enum_specifier" => find_name_node(node, source),
        "type_definition" => {
            // The typedef name is the last identifier (the declarator)
            if let Some(declarator) = node.child_by_field_name("declarator") {
                return Some(node_text(&declarator, source).to_string());
            }
            None
        }
        "declaration" => {
            // Function prototype name
            if let Some(declarator) = node.child_by_field_name("declarator") {
                return find_declarator_name(&declarator, source);
            }
            None
        }
        "preproc_include" => {
            let text = node_text(node, source);
            Some(text.trim().to_string())
        }
        "preproc_function_def" => node
            .child_by_field_name("name")
            .map(|n| node_text(&n, source).to_string()),
        _ => find_name_node(node, source),
    }
}

/// Recursively find the identifier name inside a declarator chain.
fn find_declarator_name(node: &Node, source: &[u8]) -> Option<String> {
    if node.kind() == "identifier" {
        return Some(node_text(node, source).to_string());
    }
    // Check the "declarator" field first
    if let Some(decl) = node.child_by_field_name("declarator") {
        return find_declarator_name(&decl, source);
    }
    // Fallback: check children for identifier
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            return Some(node_text(&child, source).to_string());
        }
        if (child.kind() == "function_declarator"
            || child.kind() == "pointer_declarator"
            || child.kind() == "array_declarator")
            && let Some(name) = find_declarator_name(&child, source)
        {
            return Some(name);
        }
    }
    None
}

fn extract_c_signature(node: &Node, source: &[u8]) -> Option<String> {
    let kind = node.kind();
    match kind {
        "function_definition" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "declaration" => {
            let text = node_text(node, source);
            Some(text.trim().trim_end_matches(';').to_string())
        }
        "struct_specifier" | "enum_specifier" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "type_definition" => {
            let text = node_text(node, source);
            Some(
                text.lines()
                    .next()
                    .unwrap_or(text)
                    .trim()
                    .trim_end_matches(';')
                    .to_string(),
            )
        }
        "preproc_function_def" => macro_signature(node, source),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_c(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let lang = CLanguage;
        let tree = lang.parse(source.as_bytes()).unwrap();
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    fn function<'a>(entities: &'a [SemanticEntity], name: &str) -> &'a SemanticEntity {
        entities
            .iter()
            .find(|e| e.kind == EntityKind::Function && e.name == name)
            .unwrap_or_else(|| {
                let names: Vec<_> = entities.iter().map(|e| e.name.as_str()).collect();
                panic!("no function `{name}` among {names:?}")
            })
    }

    #[test]
    fn function_behind_attribute_macro_with_arguments() {
        // Regression: `kmalloc` in the Linux kernel. The grammar cannot parse
        // `__alloc_size(1)` in a declaration prefix, so the definition was
        // lost and its body scattered.
        let src = r#"
static __always_inline __alloc_size(1) void *kmalloc(size_t size, gfp_t flags)
{
	if (__builtin_constant_p(size) && size)
		return kmalloc_large(size, flags);
	return __kmalloc(size, flags);
}

void *__kmalloc(size_t size, gfp_t flags) __assume_kmalloc_alignment __alloc_size(1);
extern void die(const char *, struct pt_regs *) __noreturn;
"#;
        let (_, entities) = parse_c(src);
        let kmalloc = function(&entities, "kmalloc");
        assert_eq!(kmalloc.line_range, 1..7, "spans the whole definition");
        assert!(
            kmalloc
                .signature
                .as_ref()
                .unwrap()
                .contains("__alloc_size(1) void *kmalloc(size_t size, gfp_t flags)"),
            "signature keeps the original text: {:?}",
            kmalloc.signature
        );
        function(&entities, "__kmalloc");
        function(&entities, "die");
        assert!(
            entities.iter().all(|e| e.name != "__alloc_size"),
            "the attribute macro is not a symbol"
        );
    }

    #[test]
    fn function_like_macro_is_a_symbol() {
        let src = r#"
#define PAGE_SIZE 4096
/* Cast a member of a structure out to the containing structure. */
#define container_of(ptr, type, member) ({ \
	void *__mptr = (void *)(ptr); \
	((type *)(__mptr - offsetof(type, member))); })
int after(void);
"#;
        let (_, entities) = parse_c(src);
        let mac = function(&entities, "container_of");
        assert_eq!(mac.line_range, 3..6);
        assert_eq!(
            mac.signature.as_deref(),
            Some("#define container_of(ptr, type, member)")
        );
        assert!(mac.doc_comment.as_ref().unwrap().contains("Cast a member"));
        assert!(
            entities.iter().all(|e| e.name != "PAGE_SIZE"),
            "object-like macros are not indexed"
        );
        function(&entities, "after");
    }

    #[test]
    fn macro_defined_function_is_a_symbol() {
        let src = r#"
SYSCALL_DEFINE3(read, unsigned int, fd, char __user *, buf, size_t, count)
{
	return ksys_read(fd, buf, count);
}

int caller(void)
{
	return 0;
}
EXPORT_SYMBOL(caller);
DEFINE_MUTEX(big_lock);
"#;
        let (_, entities) = parse_c(src);
        let sys_read = function(&entities, "sys_read");
        assert_eq!(sys_read.line_range, 1..5);
        assert!(
            sys_read
                .signature
                .as_ref()
                .unwrap()
                .starts_with("SYSCALL_DEFINE3(read, unsigned int, fd")
        );
        function(&entities, "caller");
        assert_eq!(
            entities.iter().filter(|e| e.name == "caller").count(),
            1,
            "EXPORT_SYMBOL does not define anything"
        );
        assert!(
            entities.iter().all(|e| e.name != "big_lock"),
            "a macro call that ends in `;` has no body and is not a function"
        );
    }

    #[test]
    fn extract_function() {
        let src = r#"
// Add two integers.
int add(int a, int b) {
    return a + b;
}
"#;
        let (_, entities) = parse_c(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "add");
        assert!(fns[0].signature.as_ref().unwrap().contains("int add"));
        assert!(
            fns[0]
                .doc_comment
                .as_ref()
                .unwrap()
                .contains("Add two integers")
        );
    }

    #[test]
    fn struct_references_are_not_definitions() {
        // Regression: every `struct task *p` field or parameter used to be
        // indexed as a struct definition, burying the real one on large C
        // codebases.
        let src = r#"
struct task;

struct task {
    int pid;
    struct task *parent;
    enum state st;
};

int run(struct task *t);
"#;
        let (_, entities) = parse_c(src);
        let structs: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Struct)
            .collect();
        assert_eq!(structs.len(), 1, "only the bodied struct is a definition");
        assert_eq!(structs[0].name, "task");
        assert!(
            entities.iter().all(|e| e.kind != EntityKind::Enum),
            "bodiless enum reference must not be a definition"
        );
    }

    #[test]
    fn extract_struct_and_typedef() {
        let src = r#"
struct Point {
    double x;
    double y;
};

typedef unsigned long size_t;
"#;
        let (_, entities) = parse_c(src);
        let structs: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Struct)
            .collect();
        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].name, "Point");

        let types: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::TypeAlias)
            .collect();
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name, "size_t");
    }

    #[test]
    fn extract_enum_and_include() {
        let src = r#"
#include <stdio.h>

enum Color {
    RED,
    GREEN,
    BLUE
};
"#;
        let (_, entities) = parse_c(src);
        let enums: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Enum)
            .collect();
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].name, "Color");

        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert_eq!(imports.len(), 1);
    }
}
