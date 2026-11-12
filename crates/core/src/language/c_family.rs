//! Shared helpers for the C and C++ grammars.
//!
//! tree-sitter has no preprocessor, so a declaration decorated with an
//! attribute macro that takes arguments does not parse:
//!
//! ```c
//! static __always_inline __alloc_size(1) void *kmalloc(size_t size, gfp_t flags)
//! void die(const char *, struct pt_regs *) __noreturn;
//! ```
//!
//! The first line becomes an `ERROR` node and the function body is scattered
//! across its siblings, so `kmalloc` never reaches the symbol table. The second
//! loses its prototype.
//!
//! [`mask_attribute_macros`] blanks such macros with spaces before the parse.
//! The result has the same length and the same line breaks as the input, so
//! every byte offset and line number in the tree is valid for the original
//! source, which is what entity text, signatures and chunks are sliced from.

use std::borrow::Cow;

/// Keywords that mark a parenthesised group as a parameter list rather than
/// attribute arguments.
const TYPE_KEYWORDS: &[&[u8]] = &[
    b"void",
    b"char",
    b"short",
    b"int",
    b"long",
    b"float",
    b"double",
    b"signed",
    b"unsigned",
    b"struct",
    b"union",
    b"enum",
    b"const",
    b"volatile",
    b"bool",
    b"_Bool",
];

/// Names the grammar already understands, or that are never attribute macros.
/// They are skipped without changing the declaration state.
const NEVER_MASK: &[&[u8]] = &[
    b"__attribute__",
    b"__attribute",
    b"__declspec",
    b"_Alignas",
    b"alignas",
    b"__asm__",
    b"__asm",
    b"asm",
    b"__typeof__",
    b"__typeof",
    b"typeof",
    b"sizeof",
    b"_Static_assert",
    b"static_assert",
    b"__extension__",
    b"_Generic",
    b"decltype",
    b"noexcept",
    b"operator",
    b"throw",
    b"requires",
    b"__restrict",
    b"__inline",
    b"__inline__",
    b"__volatile__",
    b"__const",
    b"__signed__",
    b"__alignof__",
    b"_Atomic",
    b"__cdecl",
    b"__stdcall",
    b"__fastcall",
    b"__ptr64",
    b"__int64",
    b"_Noreturn",
    b"__int128",
];

/// Keywords after which `name(...)` is an expression or a construct of its
/// own, not a declaration prefix.
const CONTROL_KEYWORDS: &[&[u8]] = &[
    b"return",
    b"if",
    b"else",
    b"while",
    b"for",
    b"switch",
    b"case",
    b"do",
    b"goto",
    b"new",
    b"delete",
    b"template",
    b"using",
    b"namespace",
    b"class",
];

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn in_list(list: &[&[u8]], word: &[u8]) -> bool {
    list.contains(&word)
}

/// `__lower_snake` or `ALL_CAPS`: the two spellings attribute macros use.
fn has_attribute_shape(name: &[u8]) -> bool {
    if in_list(NEVER_MASK, name) || in_list(TYPE_KEYWORDS, name) {
        return false;
    }
    if name.starts_with(b"__") && name.len() > 2 {
        return true;
    }
    name.len() >= 2
        && name.iter().any(u8::is_ascii_uppercase)
        && !name.iter().any(u8::is_ascii_lowercase)
}

/// Skip whitespace, comments and line continuations.
fn skip_trivia(s: &[u8], mut i: usize) -> usize {
    let n = s.len();
    while i < n {
        match s[i] {
            b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c => i += 1,
            b'/' if i + 1 < n && s[i + 1] == b'*' => {
                i = find(s, b"*/", i + 2).map_or(n, |j| j + 2);
            }
            b'/' if i + 1 < n && s[i + 1] == b'/' => {
                i = find_byte(s, b'\n', i).unwrap_or(n);
            }
            b'\\' if i + 1 < n && matches!(s[i + 1], b'\r' | b'\n') => i += 2,
            _ => break,
        }
    }
    i
}

fn find(s: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= s.len() {
        return None;
    }
    s[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|j| from + j)
}

fn find_byte(s: &[u8], byte: u8, from: usize) -> Option<usize> {
    s.get(from..)?
        .iter()
        .position(|&b| b == byte)
        .map(|j| from + j)
}

/// Skip a string or character literal starting at `i`. An unterminated
/// literal ends at the line break.
fn skip_quoted(s: &[u8], mut i: usize) -> usize {
    let n = s.len();
    let quote = s[i];
    i += 1;
    while i < n {
        let c = s[i];
        if c == b'\\' {
            i += 2;
            continue;
        }
        if c == quote || c == b'\n' {
            return i + 1;
        }
        i += 1;
    }
    n
}

/// Index of the `)` matching the `(` at `i`, or `None` when the group is
/// unbalanced or runs into a statement boundary.
fn match_paren(s: &[u8], mut i: usize) -> Option<usize> {
    let n = s.len();
    let mut depth = 0usize;
    while i < n {
        let c = s[i];
        match c {
            b'"' | b'\'' => {
                i = skip_quoted(s, i);
                continue;
            }
            b'/' if i + 1 < n && matches!(s[i + 1], b'*' | b'/') => {
                i = skip_trivia(s, i);
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            b'{' | b'}' | b';' => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Classify the text between a macro's parentheses.
///
/// Returns `(attribute_like, has_literal)`. Attribute arguments are literals,
/// lone identifiers and a few operators; a parameter list has a type keyword,
/// two adjacent identifiers (`size_t size`), a `*`, or `...`.
fn classify_args(args: &[u8]) -> (bool, bool) {
    let n = args.len();
    let mut i = 0;
    let mut prev_ident = false;
    let mut has_literal = false;
    let mut any = false;
    while i < n {
        i = skip_trivia(args, i);
        if i >= n {
            break;
        }
        let c = args[i];
        any = true;
        if c == b'"' || c == b'\'' {
            i = skip_quoted(args, i);
            has_literal = true;
            prev_ident = false;
        } else if c.is_ascii_digit() {
            while i < n && (is_ident_char(args[i]) || args[i] == b'.') {
                i += 1;
            }
            has_literal = true;
            prev_ident = false;
        } else if is_ident_start(c) {
            let start = i;
            while i < n && is_ident_char(args[i]) {
                i += 1;
            }
            if prev_ident || in_list(TYPE_KEYWORDS, &args[start..i]) {
                return (false, false);
            }
            prev_ident = true;
        } else if matches!(
            c,
            b',' | b'&' | b'-' | b'>' | b'.' | b'|' | b'(' | b')' | b'+' | b':'
        ) {
            if c == b'.' && i + 1 < n && args[i + 1] == b'.' {
                return (false, false);
            }
            prev_ident = false;
            i += 1;
        } else {
            return (false, false);
        }
    }
    (any, has_literal)
}

/// True when another `name(` group follows before the declaration ends, which
/// means the group just seen cannot be the declarator's parameter list.
fn has_later_group(s: &[u8], mut i: usize) -> bool {
    let n = s.len();
    loop {
        i = skip_trivia(s, i);
        if i >= n {
            return false;
        }
        let c = s[i];
        if is_ident_start(c) {
            let start = i;
            while i < n && is_ident_char(s[i]) {
                i += 1;
            }
            let k = skip_trivia(s, i);
            if k < n && s[k] == b'(' {
                if in_list(NEVER_MASK, &s[start..i]) {
                    match match_paren(s, k) {
                        Some(close) => {
                            i = close + 1;
                            continue;
                        }
                        None => return false,
                    }
                }
                return true;
            }
        } else if c == b'*' {
            i += 1;
        } else {
            return false;
        }
    }
}

/// What the previous significant token at declaration level was.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prev {
    /// Start of a declaration: file start, or after `;`, `{` or `}`.
    DeclStart,
    Ident,
    CloseParen,
    Literal,
    Other,
}

/// Replace attribute macros that break the C and C++ grammars with spaces.
///
/// Only declaration level is touched: nothing inside a function body,
/// initializer, struct body or preprocessor directive changes, because there
/// `name(args) other` is ordinary code. `extern "C" { }` and `namespace { }`
/// blocks count as declaration level.
///
/// Three shapes are blanked:
///
/// - a macro with arguments in front of a declarator:
///   `__alloc_size(1) void *kmalloc(...)`, `SEC("tc") int prog(...)`;
/// - a macro with arguments after a parameter list:
///   `void f(void) __must_hold(lock);`;
/// - a bare attribute-shaped identifier after a parameter list:
///   `void die(void) __noreturn;`.
///
/// Returns the input unchanged (borrowed) when nothing matched, which is the
/// case for most files.
pub(crate) fn mask_attribute_macros(source: &[u8]) -> Cow<'_, [u8]> {
    let s = source;
    let n = s.len();
    let mut out: Option<Vec<u8>> = None;
    let blank = |out: &mut Option<Vec<u8>>, from: usize, to: usize| {
        let buf = out.get_or_insert_with(|| s.to_vec());
        for b in &mut buf[from..to] {
            if *b != b'\n' && *b != b'\r' {
                *b = b' ';
            }
        }
    };

    // One entry per open brace: true when the block is still declaration
    // level (`extern "C" {`, `namespace x {`).
    let mut braces: Vec<bool> = Vec::new();
    let mut opaque = 0usize;
    let mut brackets = 0usize;
    let mut prev = Prev::DeclStart;
    let mut prev_word: &[u8] = b"";
    let mut prev2_word: &[u8] = b"";
    let mut after_params = false;
    let mut in_initializer = false;
    let mut line_start = true;
    let mut i = 0;

    while i < n {
        let c = s[i];
        if c == b'\n' {
            line_start = true;
            i += 1;
            continue;
        }
        if matches!(c, b' ' | b'\t' | b'\r' | 0x0b | 0x0c) {
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < n && matches!(s[i + 1], b'*' | b'/') {
            i = skip_trivia(s, i);
            continue;
        }
        if c == b'#' && line_start {
            // Preprocessor directive, including continuation lines.
            loop {
                let Some(j) = find_byte(s, b'\n', i) else {
                    i = n;
                    break;
                };
                let mut k = j;
                if k > 0 && s[k - 1] == b'\r' {
                    k -= 1;
                }
                let continued = k > 0 && s[k - 1] == b'\\';
                i = j + 1;
                if !continued {
                    break;
                }
            }
            line_start = true;
            continue;
        }
        line_start = false;

        if c == b'"' || c == b'\'' {
            i = skip_quoted(s, i);
            if opaque == 0 {
                prev = Prev::Literal;
            }
            continue;
        }
        if c == b'{' {
            let transparent = opaque == 0
                && ((prev == Prev::Literal && prev_word == b"extern")
                    || prev_word == b"namespace"
                    || prev2_word == b"namespace");
            braces.push(transparent);
            if !transparent {
                opaque += 1;
            }
            prev = Prev::DeclStart;
            after_params = false;
            in_initializer = in_initializer && !transparent;
            i += 1;
            continue;
        }
        if c == b'}' {
            if let Some(transparent) = braces.pop()
                && !transparent
            {
                opaque = opaque.saturating_sub(1);
            }
            if opaque == 0 && !in_initializer {
                prev = Prev::DeclStart;
                after_params = false;
            }
            i += 1;
            continue;
        }
        if opaque > 0 {
            if is_ident_start(c) {
                while i < n && is_ident_char(s[i]) {
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }

        if is_ident_start(c) {
            let start = i;
            let mut j = i;
            while j < n && is_ident_char(s[j]) {
                j += 1;
            }
            let word = &s[start..j];
            let k = skip_trivia(s, j);
            let never = in_list(NEVER_MASK, word);

            if k < n && s[k] == b'(' && brackets == 0 {
                let Some(close) = match_paren(s, k) else {
                    prev2_word = prev_word;
                    prev_word = word;
                    prev = Prev::Ident;
                    i = j;
                    continue;
                };
                let next = skip_trivia(s, close + 1);
                let next_char = if next < n { s[next] } else { b';' };
                if !in_initializer
                    && !never
                    && !in_list(TYPE_KEYWORDS, word)
                    && !in_list(CONTROL_KEYWORDS, word)
                {
                    let (attribute_like, has_literal) = classify_args(&s[k + 1..close]);
                    if attribute_like {
                        let shaped = has_attribute_shape(word);
                        let before_declarator = !after_params
                            && (is_ident_start(next_char) || next_char == b'*')
                            && match prev {
                                Prev::DeclStart | Prev::CloseParen => true,
                                Prev::Ident => !in_list(CONTROL_KEYWORDS, prev_word),
                                Prev::Literal | Prev::Other => false,
                            }
                            && (has_literal || (shaped && has_later_group(s, close + 1)));
                        let after_declarator = after_params
                            && (shaped || has_literal)
                            && (matches!(next_char, b';' | b'{' | b',' | b'=')
                                || is_ident_start(next_char));
                        if before_declarator || after_declarator {
                            blank(&mut out, start, close + 1);
                            i = close + 1;
                            continue;
                        }
                    }
                }
                if never {
                    i = close + 1;
                    continue;
                }
                prev2_word = prev_word;
                prev_word = word;
                prev = Prev::CloseParen;
                after_params = true;
                i = close + 1;
                continue;
            }

            if after_params && !in_initializer && has_attribute_shape(word) {
                let next_char = if k < n { s[k] } else { b';' };
                let mut next_is_attribute = false;
                if is_ident_start(next_char) {
                    let mut m = k;
                    while m < n && is_ident_char(s[m]) {
                        m += 1;
                    }
                    let next_word = &s[k..m];
                    next_is_attribute = has_attribute_shape(next_word)
                        || matches!(next_word, b"__attribute__" | b"__asm__" | b"asm");
                }
                if matches!(next_char, b';' | b'{' | b',' | b'=') || next_is_attribute {
                    blank(&mut out, start, j);
                    i = j;
                    continue;
                }
            }
            if never {
                i = j;
                continue;
            }
            prev2_word = prev_word;
            prev_word = word;
            prev = Prev::Ident;
            i = j;
            continue;
        }

        if c.is_ascii_digit() {
            while i < n && (is_ident_char(s[i]) || s[i] == b'.') {
                i += 1;
            }
            prev = Prev::Literal;
            after_params = false;
            continue;
        }
        if c == b'(' {
            match match_paren(s, i) {
                Some(close) => {
                    i = close + 1;
                    prev = Prev::CloseParen;
                    after_params = true;
                }
                None => {
                    i += 1;
                    prev = Prev::Other;
                    after_params = false;
                }
            }
            continue;
        }
        match c {
            b'[' => brackets += 1,
            b']' => brackets = brackets.saturating_sub(1),
            b';' => {
                prev = Prev::DeclStart;
                after_params = false;
                in_initializer = false;
                i += 1;
                continue;
            }
            b'=' => in_initializer = true,
            b',' if !in_initializer && brackets == 0 => after_params = false,
            _ => {}
        }
        if !matches!(c, b'*' | b'&' | b'[' | b']') {
            after_params = false;
        }
        prev = Prev::Other;
        prev_word = b"";
        i += 1;
    }

    match out {
        Some(buf) => Cow::Owned(buf),
        None => Cow::Borrowed(source),
    }
}

/// The name a function-defining macro gives its function, when the macro is
/// written directly in front of a body:
///
/// ```c
/// SYSCALL_DEFINE3(read, unsigned int, fd, char __user *, buf, size_t, count)
/// {
/// ```
///
/// System-call macros define `sys_<name>` (and `compat_sys_<name>`), which is
/// the name callers and the syscall tables use. Every other macro is assumed
/// to name its function by its first argument.
pub(crate) fn macro_defined_function_name(macro_name: &str, first_arg: &str) -> Option<String> {
    let arg = first_arg.trim();
    if arg.is_empty() || !arg.bytes().all(is_ident_char) || !is_ident_start(arg.as_bytes()[0]) {
        return None;
    }
    if macro_name.starts_with("COMPAT_SYSCALL_DEFINE") {
        Some(format!("compat_sys_{arg}"))
    } else if macro_name.starts_with("SYSCALL_DEFINE") {
        Some(format!("sys_{arg}"))
    } else {
        Some(arg.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masked(src: &str) -> String {
        let out = mask_attribute_macros(src.as_bytes());
        assert_eq!(out.len(), src.len(), "masking must preserve length");
        let out = String::from_utf8(out.into_owned()).unwrap();
        assert_eq!(
            out.matches('\n').count(),
            src.matches('\n').count(),
            "masking must preserve line breaks"
        );
        out
    }

    #[test]
    fn untouched_source_is_borrowed() {
        let src = b"int add(int a, int b) { return foo(1) + a; }\n";
        assert!(matches!(mask_attribute_macros(src), Cow::Borrowed(_)));
    }

    #[test]
    fn masks_attribute_with_arguments_before_declarator() {
        let out = masked(
            "static __always_inline __alloc_size(1) void *kmalloc(size_t size, gfp_t flags)\n{\n}\n",
        );
        assert!(!out.contains("__alloc_size"));
        assert!(out.contains("static __always_inline"));
        assert!(out.contains("void *kmalloc(size_t size, gfp_t flags)"));
    }

    #[test]
    fn masks_attribute_on_its_own_line() {
        let out = masked("__printf(2, 3)\nvoid log_append(char *log, const char *fmt, ...);\n");
        assert!(!out.contains("__printf"));
        assert!(out.contains("void log_append(char *log, const char *fmt, ...);"));
    }

    #[test]
    fn masks_trailing_attributes() {
        let out = masked(
            "void *__kmalloc(size_t size, gfp_t flags) __assume_aligned __alloc_size(1);\n\
             extern void die(const char *, struct pt_regs *) __noreturn;\n\
             extern void lock_it(struct dev *d) __must_hold(local);\n",
        );
        assert!(out.contains("void *__kmalloc(size_t size, gfp_t flags)"));
        assert!(!out.contains("__assume_aligned"));
        assert!(!out.contains("__alloc_size"));
        assert!(!out.contains("__noreturn"));
        assert!(!out.contains("__must_hold"));
    }

    #[test]
    fn keeps_prototype_with_unnamed_typedef_parameters() {
        // `__foo(u32, u64)` looks like attribute arguments, but it is the
        // last group of the declaration, so it is the parameter list.
        let out = masked("int __foo(u32, u64) __cold;\n");
        assert!(out.contains("int __foo(u32, u64)"));
        assert!(!out.contains("__cold"));
    }

    #[test]
    fn masks_identifier_only_attribute_when_a_declarator_follows() {
        let out =
            masked("__success __flag(BPF_F_ANY_ALIGNMENT)\n__naked void test20(void)\n{\n}\n");
        assert!(!out.contains("__flag"));
        assert!(out.contains("__naked void test20(void)"));
    }

    #[test]
    fn leaves_code_inside_bodies_and_initializers_alone() {
        let src = "int f(void)\n{\n\tif (x) foo(1);\n\treturn bar(2) baz;\n}\n\
                   static int y = MAX(1, 2) * FOO;\n\
                   struct s { int a __aligned(8); };\n";
        assert_eq!(masked(src), src);
    }

    #[test]
    fn leaves_statement_macros_and_kr_definitions_alone() {
        let src = "EXPORT_SYMBOL(schedule);\n\
                   static DEFINE_MUTEX(foo_lock);\n\
                   int foo(a, b) FILE *a; char b; { return 1; }\n\
                   SYSCALL_DEFINE3(read, unsigned int, fd, char *, buf, size_t, count)\n{\n}\n";
        assert_eq!(masked(src), src);
    }

    #[test]
    fn leaves_preprocessor_lines_alone() {
        let src = "#define X(a) __printf(1, 2) a\n#define Y \\\n\t__section(\".x\") int\n";
        assert_eq!(masked(src), src);
    }

    #[test]
    fn extern_c_and_namespace_blocks_are_declaration_level() {
        let out = masked(
            "extern \"C\" {\nint bar(int) __THROW __nonnull ((1));\n}\n\
             namespace n {\nvoid f(int) Q_DECL_OVERRIDE;\n}\n",
        );
        assert!(!out.contains("__THROW"));
        assert!(!out.contains("__nonnull"));
        assert!(!out.contains("Q_DECL_OVERRIDE"));
    }

    #[test]
    fn grammar_attributes_are_not_masked() {
        let src = "void f(void) __attribute__((noreturn));\nint g(void) noexcept;\n";
        assert_eq!(masked(src), src);
    }

    #[test]
    fn syscall_macros_name_the_sys_function() {
        assert_eq!(
            macro_defined_function_name("SYSCALL_DEFINE3", "read").as_deref(),
            Some("sys_read")
        );
        assert_eq!(
            macro_defined_function_name("COMPAT_SYSCALL_DEFINE2", "open").as_deref(),
            Some("compat_sys_open")
        );
        assert_eq!(
            macro_defined_function_name("BPF_PROG", " handle_exit ").as_deref(),
            Some("handle_exit")
        );
        assert_eq!(macro_defined_function_name("FOO", "\"str\""), None);
    }
}
