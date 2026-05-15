use std::process::Command;

fn no_embed_index(root: &std::path::Path) {
    let mut cfg = codixing_core::IndexConfig::new(root);
    cfg.embedding.enabled = false;
    let engine = codixing_core::Engine::init(root, cfg).unwrap();
    engine.save().unwrap();
}

#[test]
fn agent_context_pack_cli_emits_stable_json() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/greeting.rs"),
        "pub fn greeting(name: &str) -> String {\n    format!(\"hello {name}\")\n}\n",
    )
    .unwrap();
    no_embed_index(root);

    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args([
            "agent_context_pack",
            "change greeting output",
            "--mode",
            "edit",
            "--token-budget",
            "3000",
            "--changed-file",
            "src/greeting.rs",
            "--branch",
            "codex/test-pack",
            "--risk-level",
            "high",
        ])
        .current_dir(root)
        .output()
        .expect("failed to run codixing agent-context-pack");

    assert!(
        output.status.success(),
        "agent-context-pack failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let pack: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("CLI output should be JSON");
    assert_eq!(pack["schema_version"], 1);
    assert_eq!(pack["mode"], "edit");
    assert_eq!(pack["branch"], "codex/test-pack");
    assert!(
        pack["must_read"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["path"] == "src/greeting.rs"),
        "changed file should be pinned into must_read: {pack:#?}"
    );
}
