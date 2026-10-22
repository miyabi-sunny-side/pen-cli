use serde_yaml::Value;
use std::fs;

fn workflow(path: &str) -> (String, Value) {
    let source = fs::read_to_string(path).expect("workflow file is readable from the crate root");
    let yaml = serde_yaml::from_str(&source).expect("workflow file is valid YAML");
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
        "cargo build --release --locked --target",
        "pen LICENSE",
        "softprops/action-gh-release@3d0d9888cb7fd7b750713d6e236d1fcb99157228",
        "generate_release_notes: true",
    ] {
        assert!(
            source.contains(required),
            "release workflow is missing {required}"
        );
    }
}

#[test]
fn release_builds_can_be_dispatched_without_publishing_a_release() {
    let (source, _) = workflow(".github/workflows/release.yml");

    assert!(source.contains("on:\n  workflow_dispatch:\n  push:"));
    assert!(
        source.contains("if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/')")
    );
}

#[test]
fn release_workflow_is_valid_pinned_and_minimally_privileged() {
    let (source, release) = workflow(".github/workflows/release.yml");
    for line in source.lines() {
        let Some(action) = line.trim().strip_prefix("- uses: ") else {
            continue;
        };
        let action = action.split_whitespace().next().unwrap();
        let (_, reference) = action.split_once('@').unwrap();
        assert_eq!(reference.len(), 40, "{action}");
        assert!(
            reference.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{action}"
        );
    }

    assert_eq!(release["permissions"]["contents"], "read");
    assert_eq!(
        release["jobs"]["release"]["permissions"]["contents"],
        "write"
    );
    assert!(release["jobs"]["build"]["permissions"].is_null());
}

#[test]
fn repository_has_the_rust_cli_foundation() {
    let manifest = fs::read_to_string("Cargo.toml").unwrap();
    let toolchain = fs::read_to_string("rust-toolchain.toml").unwrap();

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
}
