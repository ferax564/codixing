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
}
