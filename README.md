# etop

Local-only macOS TUI process viewer focused on power usage.

## Data source

`etop` keeps the sample-primed semantics:

- `top -l 2 -o power -stats pid,command,cpu,mem,power`
- Parses the **second** sample

## UI

Multi-pane layout:

- Header/status
- Stats panel (mode, totals, filter, sort)
- History sparklines:
  - aggregate power
  - aggregate CPU
- Scrollable/selectable process table
- Controls footer

## Controls

- `q` quit
- `j/k` or `↑/↓` move selection
- `g/G` jump to top/bottom
- `/` start filter input
  - type to edit filter
  - `Enter` apply
  - `Esc` clear/cancel filter
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
