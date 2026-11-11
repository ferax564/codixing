//! Output contracts an agent depends on: compact listings, 1-based line
//! numbers that agree with `grep`, bounded output, and a quiet exit when the
//! reader (for example `head`) goes away.

use std::path::Path;
use std::process::{Command, Stdio};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn index(root: &Path) {
    let mut cfg = codixing_core::IndexConfig::new(root);
    cfg.embedding.enabled = false;
    let engine = codixing_core::Engine::init(root, cfg).unwrap();
    engine.save().unwrap();
}

fn project(root: &Path) {
    write(
        root,
        "src/a_impl.rs",
        "use crate::gadget::Gadget;\n\nimpl Gadget {\n    pub fn spin(&self) -> u32 {\n        make_gadget().size\n    }\n}\n",
    );
    write(
        root,
        "src/gadget.rs",
        "pub struct Gadget {\n    pub size: u32,\n}\n\npub fn make_gadget() -> Gadget {\n    Gadget { size: 1 }\n}\n",
    );
    write(
        root,
        "src/gadget_factory.rs",
        "pub struct GadgetFactory;\n\npub fn run() -> u32 {\n    crate::gadget::make_gadget().size\n}\n",
    );
}

fn run(root: &Path, args: &[&str]) -> (String, String, bool) {
    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(args)
        .current_dir(root)
        .output()
        .expect("failed to run codixing");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

#[test]
fn symbols_lists_exact_definition_first_without_imports() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    project(root);
    index(root);

    let (stdout, stderr, ok) = run(root, &["symbols", "Gadget"]);
    assert!(ok, "symbols failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();

    // `struct Gadget` is on line 1 of its file; `grep` agrees on the number.
    assert_eq!(
        lines.first().copied(),
        Some("Struct Gadget  src/gadget.rs:1-3"),
        "exact type definition must come first, as `Kind name  path:start-end`:\n{stdout}"
    );
    assert!(
        !stdout.contains("Import"),
        "imports are hidden unless --imports is passed:\n{stdout}"
    );
    // The impl block and the substring match follow the exact type.
    assert!(
        stdout.contains("Impl Gadget  src/a_impl.rs:3-7"),
        "{stdout}"
    );
    assert!(stdout.contains("GadgetFactory"), "{stdout}");

    let (with_imports, _, _) = run(root, &["symbols", "Gadget", "--imports"]);
    assert!(with_imports.contains("Import"), "{with_imports}");
}

#[test]
fn symbols_output_is_limited_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut source = String::new();
    for i in 0..120 {
        source.push_str(&format!("pub fn handler_{i}() {{}}\n"));
    }
    write(root, "src/handlers.rs", &source);
    index(root);

    let (stdout, _, ok) = run(root, &["symbols", "handler"]);
    assert!(ok);
    let rows = stdout
        .lines()
        .filter(|l| l.starts_with("Function "))
        .count();
    assert_eq!(rows, 50, "default listing shows 50 rows:\n{stdout}");
    assert!(
        stdout.contains("70 more (showing 50 of 120"),
        "a cut listing must state how much was left out:\n{stdout}"
    );

    let (limited, _, _) = run(root, &["symbols", "handler", "--limit", "3"]);
    assert_eq!(
        limited
            .lines()
            .filter(|l| l.starts_with("Function "))
            .count(),
        3
    );

    let (all, _, _) = run(root, &["symbols", "handler", "--limit", "0"]);
    assert_eq!(
        all.lines().filter(|l| l.starts_with("Function ")).count(),
        120
    );
}

#[test]
fn usages_prints_every_reference_as_path_line_kind() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    project(root);
    index(root);

    let (stdout, stderr, ok) = run(root, &["usages", "make_gadget"]);
    assert!(ok, "usages failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();

    assert_eq!(
        lines.first().copied(),
        Some("src/gadget.rs:5:definition: pub fn make_gadget() -> Gadget {"),
        "the definition leads, with a 1-based line:\n{stdout}"
    );
    assert!(
        lines.contains(&"src/a_impl.rs:5:call: make_gadget().size"),
        "{stdout}"
    );
    assert!(
        lines.contains(&"src/gadget_factory.rs:4:call: crate::gadget::make_gadget().size"),
        "{stdout}"
    );
    assert!(stderr.contains("3 usage(s) in 3 file(s)."), "{stderr}");

    // The numbers are the ones `grep` reports.
    let (grep, _, _) = run(root, &["grep", "make_gadget", "--literal"]);
    for expected in [
        "src/gadget.rs:5:",
        "src/a_impl.rs:5:",
        "src/gadget_factory.rs:4:",
    ] {
        assert!(
            grep.contains(expected),
            "grep disagrees on {expected}:\n{grep}"
        );
    }
}

#[test]
fn search_prints_one_based_ranges_and_a_focused_snippet() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    project(root);
    index(root);

    let (stdout, stderr, ok) = run(root, &["search", "Gadget", "--limit", "1"]);
    assert!(ok, "search failed: {stderr}");
    let first = stdout.lines().next().unwrap_or_default();
    assert!(
        first.starts_with("1. src/gadget.rs:1-"),
        "definition first, as `path:start-end` with 1-based lines:\n{stdout}"
    );
    assert!(
        stdout.contains("   1| pub struct Gadget {"),
        "snippet lines carry their real line number:\n{stdout}"
    );
    assert!(
        !stdout.contains("score="),
        "scores are not printed:\n{stdout}"
    );
}

#[cfg(unix)]
#[test]
fn closing_the_pipe_early_ends_the_command_quietly() {
    use std::io::Read;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Enough rows to overflow the pipe buffer several times over.
    let mut source = String::new();
    for i in 0..6000 {
        source.push_str(&format!(
            "pub fn generated_function_with_a_long_name_{i}() {{}}\n"
        ));
    }
    write(root, "src/many.rs", &source);
    index(root);

    let mut child = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["symbols", "generated_function", "--limit", "0"])
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn codixing");

    // Behave like `head -1`: read a little, then close the pipe.
    let mut stdout = child.stdout.take().unwrap();
    let mut first = [0u8; 64];
    stdout.read_exact(&mut first).unwrap();
    drop(stdout);

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked") && !stderr.contains("Broken pipe"),
        "a closed reader must not produce a panic message:\n{stderr}"
    );
    assert_ne!(
        output.status.code(),
        Some(101),
        "a closed reader must not look like a crash"
    );
}
