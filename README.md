# pen

`pen` is a small Linux x86_64 and macOS Apple Silicon companion CLI for
[herdr](https://herdr.dev). It saves a workspace layout as a machine-local TOML definition, closes
the running workspace, and restores it when needed. Intel Macs are not supported.

This project is at v0.1.0: the core experience works, while rough edges are intentionally left for
later polishing.

## Usage

[Install Herdr](https://herdr.dev/docs/install/) and start it before running `pen`; its Unix socket
must be reachable. Install `fzf` to use the picker.

```sh
pen save
pen close
pen picker
```

Definitions are stored in `~/.config/pen/*.toml`. `pen picker` uses `fzf`; Space toggles the
selected workspace and keeps the list open for the next toggle (restoring this way does not move
focus), Enter focuses a running workspace or restores a stopped one and closes the picker, and Esc
closes it.

`pen close` and closing a running workspace from the picker never discard state silently: an
unsaved workspace asks save / discard / cancel, and a workspace that differs from its saved
definition asks update / discard / cancel. A workspace that matches its saved definition closes
without questions.

The defaults can be overridden for testing or nonstandard installations:

| Variable | Default | Purpose |
| --- | --- | --- |
| `PEN_CONFIG_DIR` | `~/.config/pen` | Workspace definition directory |
| `PEN_SOCKET` | `~/.config/herdr/herdr.sock` | herdr Unix socket |
| `PEN_FZF` | `fzf` | fzf executable path |

## Install a release

Download the archive for Linux x86_64 or macOS Apple Silicon from the GitHub Release, verify its
adjacent SHA-256 file, and place `pen` somewhere on `PATH`.

Release assets:

- `pen-linux-x86_64.tar.gz`
- `pen-linux-x86_64.tar.gz.sha256`
- `pen-macos-aarch64.tar.gz`
- `pen-macos-aarch64.tar.gz.sha256`

Each archive contains the `pen` executable and the MIT `LICENSE`.

On macOS, if Gatekeeper reports that the downloaded binary cannot be opened, remove the quarantine
attribute before moving it onto `PATH`:

```sh
xattr -d com.apple.quarantine pen
```

## Development

Rust 1.96.0 is selected by `rust-toolchain.toml`.

```sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
cargo build --locked --release
```

## Releases

The release workflow runs whenever `bump-tag` pushes a `v*` tag. It rejects a tag that does not
match the version in `Cargo.toml`, builds Linux x86_64 and macOS Apple Silicon archives, generates
their checksums, and attaches the files to a GitHub Release with generated notes.

`bump-tag` is the release authority. This repository's development workflow does not create tags,
push commits, or publish releases by itself.

## License

MIT. See [LICENSE](LICENSE).
