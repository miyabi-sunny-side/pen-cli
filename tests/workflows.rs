use serde_yaml::Value;
use std::{fs, path::Path};

fn workflow(path: &str) -> (String, Value) {
    let source = fs::read_to_string(path).unwrap();
    let yaml = serde_yaml::from_str(&source).unwrap();
    (source, yaml)
}

#[test]
fn a_version_tag_builds_and_publishes_the_pen_release_assets() {
    let (source, _) = workflow(".github/workflows/release.yml");

    for required in [
        "tags:\n      - \"v*\"",
        "x86_64-unknown-linux-musl",
        "aarch64-apple-darwin",
        "macos-latest",
        "pen-linux-x86_64.tar.gz",
        "pen-macos-aarch64.tar.gz",
        "Verify tag matches Cargo version",
        "cargo test --locked",
        "cargo build --release --locked --target",
        "pen LICENSE",
        "softprops/action-gh-release@3bb12739c298aeb8a4eeaf626c5b8d85266b0e65",
        "generate_release_notes: true",
    ] {
        assert!(
            source.contains(required),
            "release workflow is missing {required}"
        );
    }
    assert!(!source.contains("workflow_dispatch"));
}

#[test]
fn ci_runs_for_main_pull_requests_but_not_pushes() {
    let (source, _) = workflow(".github/workflows/ci.yml");

    assert!(source.contains("on:\n  pull_request:\n    branches:\n      - main"));
    assert!(!source.contains("\n  push:"));
}

#[test]
fn workflows_are_valid_pinned_and_minimally_privileged() {
    for path in [
        Path::new(".github/workflows/ci.yml"),
        Path::new(".github/workflows/release.yml"),
    ] {
        let (source, _) = workflow(path.to_str().unwrap());
        for line in source.lines() {
            let Some(action) = line.trim().strip_prefix("- uses: ") else {
                continue;
            };
            let action = action.split_whitespace().next().unwrap();
            let (_, reference) = action.split_once('@').unwrap();
            assert_eq!(reference.len(), 40, "{action} in {}", path.display());
            assert!(
                reference.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "{action} in {}",
                path.display()
            );
        }
    }

    let (_, release) = workflow(".github/workflows/release.yml");
    assert_eq!(release["permissions"]["contents"], "read");
    assert_eq!(
        release["jobs"]["release"]["permissions"]["contents"],
        "write"
    );
    assert!(release["jobs"]["build"]["permissions"].is_null());
}

#[test]
fn repository_has_the_v0_1_0_rust_cli_foundation() {
    let manifest = fs::read_to_string("Cargo.toml").unwrap();
    let toolchain = fs::read_to_string("rust-toolchain.toml").unwrap();
    let license = fs::read_to_string("LICENSE").unwrap();
    let readme = fs::read_to_string("README.md").unwrap();
    let (ci, _) = workflow(".github/workflows/ci.yml");

    let version_line = format!("version = \"{}\"", env!("CARGO_PKG_VERSION"));
    for required in [
        version_line.as_str(),
        "rust-version = \"1.96\"",
        "license = \"MIT\"",
        "[lints.clippy]",
        "[profile.release]",
    ] {
        assert!(
            manifest.contains(required),
            "manifest is missing {required}"
        );
    }
    assert!(toolchain.contains("channel = \"1.96.0\""));
    assert!(license.starts_with("MIT License\n"));
    for required in [
        "Linux x86_64",
        "macOS Apple Silicon",
        "Intel Macs are not supported",
        "herdr",
        "fzf",
        "PEN_CONFIG_DIR",
        "PEN_SOCKET",
        "PEN_FZF",
        "xattr -d com.apple.quarantine pen",
    ] {
        assert!(readme.contains(required), "README is missing {required}");
    }
    for required in [
        "cargo fmt --check",
        "cargo clippy --all-targets --all-features --locked -- -D warnings",
        "cargo test --locked",
        "cargo build --locked --release",
    ] {
        assert!(ci.contains(required), "CI workflow is missing {required}");
    }
}
