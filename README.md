# pen

pen saves and restores [Herdr](https://herdr.dev) workspaces from the terminal.
Local TOML files store tabs, splits, working directories, and foreground commands.
Use it to close a workspace and reopen its layout later.

## Install

Install and start [Herdr](https://herdr.dev/docs/install/) first.
Its Unix socket must be reachable. Install `fzf` to use the workspace picker.
Prebuilt pen binaries support Linux x86_64 and macOS Apple Silicon.
Intel Macs are not supported by the release binaries.

Download from [GitHub Releases](https://github.com/miyabi-sunny-side/pen-cli/releases/latest).
For Linux x86_64, use an empty directory:

```sh
curl -fLO https://github.com/miyabi-sunny-side/pen-cli/releases/latest/download/pen-linux-x86_64.tar.gz
curl -fLO https://github.com/miyabi-sunny-side/pen-cli/releases/latest/download/pen-linux-x86_64.tar.gz.sha256
sha256sum -c pen-linux-x86_64.tar.gz.sha256
```

After the checksum succeeds, extract and install:

```sh
tar -xzf pen-linux-x86_64.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 755 pen "$HOME/.local/bin/pen"
export PATH="$HOME/.local/bin:$PATH"
pen --version
```

Keep `~/.local/bin` in your shell's PATH for new terminals.
On macOS, use `pen-macos-aarch64.tar.gz` and its adjacent `.sha256` file.
Verify with `shasum -a 256 -c`, then extract and install as above.
Each archive contains `pen` and the MIT `LICENSE`.
If Gatekeeper blocks the verified binary, remove its quarantine attribute:

```sh
xattr -d com.apple.quarantine pen
```

## Save, close, and restore

Run this inside a pane of the Herdr workspace you want to save:

```sh
pen save
```

Success prints `saved <workspace label>`.
Definitions live in `~/.config/pen/*.toml`. Saving overwrites the matching filename.
The workspace label becomes the filename. Path separators become underscores.
Use distinct labels; labels that normalize to the same filename can overwrite each other.

When ready to close the workspace, run:

```sh
pen close
```

This closes the workspace and its processes, while keeping its saved definition.
An unsaved workspace asks save / discard / cancel.
A changed workspace asks update / discard / cancel.
If the current layout cannot be compared, you can discard or cancel.
A matching definition closes immediately; that does not mean editor buffers are saved.

From another Herdr pane, open the picker to restore it:

```sh
pen picker
```

| Key | Action |
| --- | --- |
| Enter | Focus a running workspace or restore a stopped one, then exit |
| Space | Close or restore the selection and keep the picker open; restore does not move focus |
| Esc | Exit |

Running workspaces are marked `●`; stopped definitions are marked `○`.
Closing from the picker uses the same checks as `pen close`.
The picker resets its search and cursor after each Space action.

## What is saved

pen captures all workspace tabs, their layout and active tab, plus pane metadata.
It captures the foreground process group's command arguments when Herdr can identify them.
Restore starts commands again. It does not resume process memory or unsaved buffers.
Idle shells and pen's own save command need no captured command.
If command inspection fails, saving stops before replacing the existing definition.

Definitions may contain paths, command arguments, and pane environment values.
Keep them private when those values are sensitive, and restore only definitions you trust.
A failed restore can leave an incomplete workspace. Close it manually as the error directs.
Existing single-tab definitions remain readable; new saves use the multi-tab format.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `PEN_CONFIG_DIR` | `~/.config/pen` | Workspace definition directory |
| `PEN_SOCKET` | `~/.config/herdr/herdr.sock` | Herdr Unix socket |
| `PEN_FZF` | `fzf` | fzf executable path |

Herdr supplies `HERDR_PANE_ID` so save and close target the calling pane's workspace.
Without it, the current workspace is resolved from Herdr's focused pane.

## Development

Clone the repository. Rust 1.96.0 is selected by [rust-toolchain.toml](rust-toolchain.toml).

```sh
git clone https://github.com/miyabi-sunny-side/pen-cli.git
cd pen-cli
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
cargo build --locked --release
```

The binary is `target/release/pen`.
The [release workflow](.github/workflows/release.yml) publishes when a `v*` tag is pushed.
The tag must match `Cargo.toml`; CI builds the two platform archives and their checksums.
Manual workflow runs build artifacts without publishing a GitHub release.

Licensed under [MIT](LICENSE).
