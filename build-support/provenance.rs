//! Shared build-time Git provenance for the CLI and HTTP server.
//!
//! This file intentionally lives at the workspace root and is included by both
//! binary build scripts. Codixing ships workspace-built binaries today; if the
//! binary crates are ever published independently, move this into a tiny build
//! dependency so the package archive carries it too.

use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

struct Identity {
    revision: Option<String>,
    tree: Option<String>,
    dirty: bool,
}

pub fn emit() {
    println!("cargo:rerun-if-changed=../../build-support/provenance.rs");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    let source_root = manifest_dir
        .join("../..")
        .canonicalize()
        .expect("Codixing workspace root must exist");
    // Every tracked workspace file must invalidate the attestation. Otherwise a
    // clean build-script result can be reused while dirty core code is linked;
    // after the checkout is reverted, that stale binary could appear to match a
    // clean revision and tree. Failure to establish complete tracking therefore
    // makes provenance unavailable instead of emitting a potentially false ID.
    let tracking_complete = emit_git_rerun_paths(&source_root)
        && match tracked_workspace_paths(&source_root) {
            Ok(paths) => {
                let mut complete = true;
                for path in paths {
                    complete &= emit_rerun_path(&path);
                }
                complete
            }
            Err(error) => {
                println!("cargo:warning={error}");
                false
            }
        };

    let identity = if tracking_complete {
        git_identity(&source_root)
    } else {
        unavailable_identity()
    };
    let generated = generated_module(&identity);
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));
    fs::write(out_dir.join("build_provenance.rs"), generated)
        .expect("failed to write generated build provenance module");
}

fn git_identity(source_root: &Path) -> Identity {
    let revision = git_output(source_root, ["rev-parse", "--verify", "HEAD"])
        .and_then(|value| optional_validated_oid("Git revision", &value));
    let tree = git_output(source_root, ["rev-parse", "--verify", "HEAD^{tree}"])
        .and_then(|value| optional_validated_oid("Git tree", &value));
    let dirty = git_output(
        source_root,
        ["status", "--porcelain=v1", "--untracked-files=normal"],
    )
    .is_none_or(|status| !status.is_empty());
    if revision.is_none() || tree.is_none() {
        println!(
            "cargo:warning=Git provenance is unavailable; strict benchmark evidence will fail closed"
        );
    }
    Identity {
        revision,
        tree,
        dirty,
    }
}

fn unavailable_identity() -> Identity {
    Identity {
        revision: None,
        tree: None,
        dirty: true,
    }
}

fn optional_validated_oid(label: &str, value: &str) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    if matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(value)
    } else {
        println!("cargo:warning={label} is not a valid Git object ID");
        None
    }
}

fn git_output<I, S>(source_root: &Path, arguments: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = sanitized_git_command(source_root)
        .args(arguments)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
}

fn emit_git_rerun_paths(source_root: &Path) -> bool {
    let mut complete = true;
    let dot_git = source_root.join(".git");
    if dot_git.is_file() {
        complete &= emit_rerun_path(&dot_git);
    }
    let head = git_path(source_root, "HEAD");
    let index = git_path(source_root, "index");
    let packed_refs = git_path(source_root, "packed-refs");
    if head.is_none() || index.is_none() || packed_refs.is_none() {
        println!(
            "cargo:warning=worktree Git metadata paths are unavailable; strict benchmark evidence will fail closed"
        );
        return false;
    }
    for path in [&head, &index, &packed_refs].into_iter().flatten() {
        complete &= emit_rerun_path(path);
    }
    if let Some(head) = head {
        let Ok(contents) = fs::read_to_string(head) else {
            println!(
                "cargo:warning=worktree Git HEAD is unreadable; strict benchmark evidence will fail closed"
            );
            return false;
        };
        if let Some(reference) = contents.trim().strip_prefix("ref: ") {
            let Some(path) = git_path(source_root, reference) else {
                println!(
                    "cargo:warning=worktree Git ref path is unavailable; strict benchmark evidence will fail closed"
                );
                return false;
            };
            complete &= emit_rerun_path(&path);
        }
    }
    complete
}

pub fn tracked_workspace_paths(source_root: &Path) -> Result<Vec<PathBuf>, String> {
    let output = sanitized_git_command(source_root)
        .args(["ls-files", "-z", "--cached"])
        .output()
        .map_err(|error| format!("failed to list tracked workspace files: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-files failed with {}; strict benchmark evidence will fail closed",
            output.status
        ));
    }

    let mut paths = Vec::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let relative = std::str::from_utf8(raw).map_err(|_| {
            "a tracked workspace path is not UTF-8; strict benchmark evidence will fail closed"
                .to_string()
        })?;
        if relative.contains('\n') || relative.contains('\r') {
            return Err(
                "a tracked workspace path cannot be represented in a Cargo directive; strict benchmark evidence will fail closed"
                    .to_string(),
            );
        }
        let relative = Path::new(relative);
        if relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(
                "git returned a non-relative tracked path; strict benchmark evidence will fail closed"
                    .to_string(),
            );
        }
        paths.push(source_root.join(relative));
    }
    if paths.is_empty() {
        return Err(
            "git returned no tracked workspace files; strict benchmark evidence will fail closed"
                .to_string(),
        );
    }
    Ok(paths)
}

fn sanitized_git_command(source_root: &Path) -> Command {
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if is_git_environment_key(&name) {
            command.env_remove(name);
        }
    }
    command.arg("-C").arg(source_root);
    command
}

pub fn is_git_environment_key(name: &OsStr) -> bool {
    name.to_string_lossy()
        .as_bytes()
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"GIT_"))
}

fn git_path(source_root: &Path, name: &str) -> Option<PathBuf> {
    let output = git_output(source_root, ["rev-parse", "--git-path", name])?;
    let path = PathBuf::from(output);
    Some(if path.is_absolute() {
        path
    } else {
        source_root.join(path)
    })
}

fn emit_rerun_path(path: &Path) -> bool {
    let Some(path) = path.to_str() else {
        println!(
            "cargo:warning=a provenance dependency path is not UTF-8; strict benchmark evidence will fail closed"
        );
        return false;
    };
    if path.contains('\n') || path.contains('\r') {
        println!(
            "cargo:warning=a provenance dependency path cannot be represented in a Cargo directive; strict benchmark evidence will fail closed"
        );
        return false;
    }
    println!("cargo:rerun-if-changed={path}");
    true
}

fn generated_module(identity: &Identity) -> String {
    let revision = option_literal(identity.revision.as_deref());
    let tree = option_literal(identity.tree.as_deref());
    format!(
        r#"mod build_provenance {{
    pub const SCHEMA_VERSION: u32 = 1;
    pub const REVISION: Option<&str> = {revision};
    pub const TREE: Option<&str> = {tree};
    pub const DIRTY: bool = {dirty};

    pub fn write_if_requested() -> Result<bool, serde_json::Error> {{
        let mut arguments = std::env::args_os();
        let _executable = arguments.next();
        if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--build-provenance-json"))
            || arguments.next().is_some()
        {{
            return Ok(false);
        }}
        serde_json::to_writer(
            std::io::stdout().lock(),
            &serde_json::json!({{
                "schema_version": SCHEMA_VERSION,
                "origin": "embedded-build-v1",
                "revision": REVISION,
                "tree": TREE,
                "dirty": DIRTY,
            }}),
        )?;
        Ok(true)
    }}
}}
"#,
        dirty = identity.dirty,
    )
}

fn option_literal(value: Option<&str>) -> String {
    value.map_or_else(|| "None".to_string(), |value| format!("Some({value:?})"))
}
