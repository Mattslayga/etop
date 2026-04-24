# etop

Local-only macOS TUI process viewer focused on power usage and energy impact.

## Data source

`etop` keeps macOS `top` power semantics:

- Interactive TUI: long-lived stream from `top -l 0 -s 2 -o power -stats pid,command,power`
  - skips the first emitted table as warmup
- `--dump-once`: `top -l 2 -s 0 -o power -stats pid,command,power`
  - parses the **second** sample

## UI

OneDark-inspired multi-pane layout:

- Thin single-line status strip (mode, load, rows, power, filter, pinned PID when active)
- Aggregate **multi-row braille-style** power history area (roughly top third), rendered as a rolling right-edge trace with green→yellow→orange→red power-threshold bands
- Process table area (roughly bottom two thirds)
- Optional pinned-process detail pane that opens above rows within the table region
- Details are locked to the pinned PID/process until unpinned
- Cursor highlight is hidden while pinned so the detail view feels held/fixed

Palette cues used in the TUI:

- background: `#282c34`
- foreground/title: `#abb2bf`
- accent: `#61afef`
- muted/borders: `#5c6370`
- selected row background: `#2c313c`

## Controls

- `q` quit
- `j/k` or `↑/↓` move selection (when not pinned)
- `g/G` jump to top/bottom (when not pinned)
- `f` start filter input
  - type to edit filter
  - `Enter` apply
  - `Esc` cancel edit and keep current filter
- `d` clear the active filter
- `p` pause/resume refresh
- `s` cycle process sort
- `r` cycle pinned history range
- `Enter` pin/unpin process details
- `m` open graph-threshold settings modal
  - `↑/↓` or `j/k` move fields
  - `Enter` edit/confirm field value
  - `m` apply settings and close
  - `Esc` cancel field edit or close without applying

## CLI

```bash
etop --help
etop --version
etop --dump-once
etop update
```

## Platform support

- Runtime/data semantics are macOS-specific (`top -l ...`), so `etop` is a macOS tool.
- Release artifacts are currently **Apple Silicon macOS only** (`aarch64-apple-darwin`).
- Linux artifacts are intentionally **not** published yet.

## Install with Homebrew (Apple Silicon macOS)

```bash
brew install Mattslayga/etop/etop
```

This installs the current Apple Silicon macOS release from the `Mattslayga/homebrew-etop` tap.

## Update with Homebrew

```bash
brew update
brew upgrade etop
```

If Homebrew has cached a stale local tap checkout, refresh it explicitly:

```bash
brew untap Mattslayga/etop
brew tap Mattslayga/etop
brew reinstall etop
```

## Install / run from source

```bash
# interactive TUI
cargo run

# non-interactive smoke mode (prints rows once and exits)
cargo run -- --dump-once

# optional: optimized local binary
cargo build --release
./target/release/etop
```

## Install from the repo installer (Apple Silicon macOS)

Review the script, then run it locally:

```bash
curl -fsSL -o install-etop.sh \
  https://raw.githubusercontent.com/Mattslayga/etop/main/install.sh

sh install-etop.sh
```

By default this installs into `~/.local/bin`. To install a specific release:

```bash
sh install-etop.sh --version vX.Y.Z
```

## Install / run from GitHub Releases directly (Apple Silicon macOS)

```bash
# replace vX.Y.Z
curl -L -o etop.tar.gz \
  https://github.com/Mattslayga/etop/releases/download/vX.Y.Z/etop-vX.Y.Z-macos-arm64.tar.gz

tar -xzf etop.tar.gz
./etop
```

## License

MIT

## Contributors

- Matt Slayga
- OpenAI Codex
- Anthropic Claude

## Notes

- Internal collector processes (`etop` and sampling `top`) are hidden by PID
- No network calls; local command execution only
