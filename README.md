# etop (MVP)

Minimal local-only TUI process viewer for macOS power usage.

## Features

- Live refresh (2s)
- Process table with: PID, process, power, CPU, MEM
- Sorted by power descending
- Quit key: `q`
- Header/footer with source + refresh hints
- Uses:
  - `top -l 2 -o power -stats pid,command,cpu,mem,power`
  - Parses the **second sample**

## Run

```bash
cargo run
```

Then press `q` to quit.

## Non-interactive smoke mode

```bash
cargo run -- --dump-once
```

Prints top rows once and exits.

## Notes

- macOS only (`top -l` semantics)
- No network calls; local command execution only
