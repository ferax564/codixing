use std::process::Command;

#[test]
fn update_single_file_flag() {
    let dir = tempfile::tempdir().unwrap();
    // Canonicalize matches what `codixing init` does internally, ensuring
    // config.root and reindex_file path stripping agree (macOS /var vs /private/var).
    let root = dir.path().canonicalize().unwrap();
    let root = root.as_path();
    std::fs::write(root.join("foo.rs"), "pub fn hello() {}").unwrap();
    let engine = codixing_core::Engine::init(root, codixing_core::IndexConfig::new(root)).unwrap();
    engine.save().unwrap();
    drop(engine);

    // Mutate the file.
    std::fs::write(root.join("foo.rs"), "pub fn goodbye() {}").unwrap();

    // Run update --file.
    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["update", "--file", "foo.rs"])
        .current_dir(root)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "update failed\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Verify symbol changed.
    let engine = codixing_core::Engine::open(root).unwrap();
    let syms = engine.symbols("goodbye", None).unwrap();
    assert!(
        !syms.is_empty(),
        "goodbye should be indexed\nstdout: {stdout}\nstderr: {stderr}"
    );
    let old_syms = engine.symbols("hello", None).unwrap();
    assert!(
        old_syms.is_empty(),
        "hello should be removed (found {} entries)\nstdout: {stdout}\nstderr: {stderr}",
        old_syms.len()
    );
}
