# etop

Local-only macOS TUI process viewer focused on power usage.

## Data source

`etop` keeps the sample-primed semantics:

- `top -l 2 -s 0 -o power -stats pid,command,cpu,mem,power`
- Parses the **second** sample

## UI

OneDark-inspired multi-pane layout:

- Header/status bar (live/paused, loading, sort)
- Stats panel (mode, totals, filter, visible rows)
- History sparklines:
  - aggregate power
  - aggregate CPU
- Scrollable/selectable process table with highlighted selected row
- Selected-process detail pane (PID, process, power, CPU, mem)
- Controls footer

Palette cues used in the TUI:

- background: `#282c34`
- foreground/title: `#abb2bf`
- accent: `#61afef`
- muted/borders: `#5c6370`
- selected row background: `#2c313c`

## Controls

- `q` quit
- `j/k` or `↑/↓` move selection
- `g/G` jump to top/bottom
- `/` start filter input
  - type to edit filter
  - `Enter` apply
  - `Esc` cancel edit and keep current filter
- `p/c/m` sort by power/cpu/mem (desc)
- `space` pause/resume refresh

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
