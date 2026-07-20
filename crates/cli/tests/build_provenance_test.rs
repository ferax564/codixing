use std::process::Command;

#[allow(dead_code)]
#[path = "../../../build-support/provenance.rs"]
mod provenance;

#[test]
fn hidden_build_provenance_is_machine_readable_and_side_effect_free() {
    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .arg("--build-provenance-json")
        .output()
        .expect("failed to run codixing provenance command");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["origin"], "embedded-build-v1");
    assert!(value["dirty"].is_boolean());
    for field in ["revision", "tree"] {
        let object_id = value[field]
            .as_str()
            .expect("Git checkout must emit an object ID");
        assert!(matches!(object_id.len(), 40 | 64));
        assert!(object_id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}

#[test]
fn build_provenance_tracks_core_workspace_sources() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let tracked = provenance::tracked_workspace_paths(&workspace).unwrap();
    assert!(tracked.contains(&workspace.join("crates/core/src/lib.rs")));
}

#[test]
fn build_support_git_lookup_ignores_poisoned_environment() {
    assert!(provenance::is_git_environment_key(std::ffi::OsStr::new(
        "gIt_Config_Count"
    )));
    assert!(!provenance::is_git_environment_key(std::ffi::OsStr::new(
        "GITHUB_SHA"
    )));
    let poison = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "build_provenance_tracks_core_workspace_sources",
            "--nocapture",
        ])
        .env("GIT_DIR", poison.path().join("redirected-git-dir"))
        .env("GIT_WORK_TREE", poison.path().join("redirected-work-tree"))
        .env("GIT_INDEX_FILE", poison.path().join("redirected-index"))
        .env(
            "GIT_CONFIG_GLOBAL",
            poison.path().join("redirected-global-config"),
        )
        .env(
            "GIT_CONFIG_SYSTEM",
            poison.path().join("redirected-system-config"),
        )
        .env("GIT_CEILING_DIRECTORIES", poison.path())
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.repositoryformatversion")
        .env("GIT_CONFIG_VALUE_0", "999")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
