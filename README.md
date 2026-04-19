# etop

Local-only macOS TUI process viewer focused on power usage and energy impact.

## Data source

`etop` keeps sample-primed macOS semantics:

- `top -l 2 -s 0 -o power -stats pid,command,power`
- Parses the **second** sample

## UI

OneDark-inspired multi-pane layout:

- Thin single-line status strip (mode, load, rows, power, filter, pinned PID when active)
- Aggregate **multi-row braille-style** power history area (roughly top third)
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
- `/` start filter input
  - type to edit filter
  - `Enter` apply
  - `Esc` cancel edit and keep current filter
- `space` pause/resume refresh
- `Enter` pin/unpin process details

## Run

```bash
cargo run
```

## Non-interactive smoke mode

```bash
cargo run -- --dump-once
```

Prints top rows once and exits.

## Notes

- macOS only (`top -l` semantics)
- No network calls; local command execution only
