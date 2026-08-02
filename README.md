# pen

`pen` is a small Linux x86_64 companion CLI for [herdr](https://herdr.dev). It saves a workspace
layout as a machine-local TOML definition, closes the running workspace, and restores it when
needed.

This project is at v0.1.0: the core experience works, while rough edges are intentionally left for
later polishing.

## Usage

Start herdr before running `pen`; its Unix socket must be reachable. Install `fzf` to use the
picker.

```sh
pen save
pen close
pen picker
```

Definitions are stored in `~/.config/pen/*.toml`. `pen picker` uses `fzf`; Space toggles the
selected workspace and Enter restores or focuses it.

The defaults can be overridden for testing or nonstandard installations:

| Variable | Default | Purpose |
| --- | --- | --- |
| `PEN_CONFIG_DIR` | `~/.config/pen` | Workspace definition directory |
| `PEN_SOCKET` | `~/.config/herdr/herdr.sock` | herdr Unix socket |
| `PEN_FZF` | `fzf` | fzf executable path |

## Install a release

Download the Linux x86_64 archive from the GitHub Release, verify its adjacent SHA-256 file, and
place `pen` somewhere on `PATH`.

Release assets:

- `pen-linux-x86_64.tar.gz`
- `pen-linux-x86_64.tar.gz.sha256`

Each archive contains the `pen` executable and the MIT `LICENSE`.

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
match the version in `Cargo.toml`, builds a static Linux x86_64 archive, generates its checksum, and
attaches both files to a GitHub Release with generated notes.

`bump-tag` is the release authority. This repository's development workflow does not create tags,
push commits, or publish releases by itself.

## License

MIT. See [LICENSE](LICENSE).
