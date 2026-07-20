use std::process::Command;

#[test]
fn doctor_json_reports_missing_index_without_failure() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["doctor", dir.path().to_str().unwrap(), "--json"])
        .output()
        .expect("failed to run codixing doctor");

    assert!(
        output.status.success(),
        "doctor should succeed for an unindexed directory: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("doctor output should be JSON");
    assert_eq!(report["binary"]["name"], "codixing");
    assert_eq!(report["binary"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(report["index"]["status"], "missing");
    assert_eq!(report["index"]["dir_exists"], false);

    let daemon_endpoint = report["daemon"]["endpoint"]
        .as_str()
        .expect("doctor should report its default daemon endpoint");
    #[cfg(unix)]
    assert!(daemon_endpoint.ends_with("/.codixing/daemon-minimal.sock"));
    #[cfg(windows)]
    assert!(daemon_endpoint.ends_with("-minimal"));
}

#[test]
fn doctor_json_reports_active_generation_layout() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("lib.rs"),
        "pub fn doctor_generation_sentinel() -> usize { 1 }\n",
    )
    .unwrap();

    let init = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["init", dir.path().to_str().unwrap()])
        .output()
        .expect("failed to initialize doctor fixture");
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["doctor", dir.path().to_str().unwrap(), "--json"])
        .output()
        .expect("failed to run codixing doctor");
    assert!(output.status.success());

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["index"]["status"], "ok");
    assert_eq!(report["index"]["layout"]["kind"], "generational");
    assert_eq!(report["index"]["layout"]["generation_count"], 1);
    assert!(
        report["index"]["layout"]["active_generation"]
            .as_str()
            .is_some_and(|name| name.starts_with("gen-"))
    );
    assert_eq!(
        report["index"]["layout"]["abandoned_generations"],
        serde_json::json!([])
    );
}
