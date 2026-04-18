# etop

Local-only macOS TUI process viewer focused on power usage and energy impact.

## Data source

`etop` keeps sample-primed macOS semantics:

- `top -l 2 -s 0 -o power -stats pid,command,power`
- Parses the **second** sample

## UI

OneDark-inspired multi-pane layout:

- Header/status bar (live/paused, loading)
- Stats panel (rows, filter, aggregate power, details state)
- Aggregate power history sparkline
- Scrollable/selectable process table (PID, process, power)
- Selected-process detail pane (PID, process, power, rank/share)
- Inline control hints in panel titles/status text

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
- `space` pause/resume refresh
- `Enter` toggle selected-process details pane

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
