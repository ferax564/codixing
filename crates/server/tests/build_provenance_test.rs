use std::process::Command;

#[test]
fn hidden_build_provenance_exits_before_server_startup() {
    let output = Command::new(env!("CARGO_BIN_EXE_codixing-server"))
        .arg("--build-provenance-json")
        .output()
        .expect("failed to run server provenance command");
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
