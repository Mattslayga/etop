use std::{
    io,
    process::{Command, Stdio},
};

use crate::persistence;

const TOP_BIN: &str = "top";
const TOP_ARGS: [&str; 8] = [
    "-l",
    "2",
    "-s",
    "0",
    "-o",
    "power",
    "-stats",
    "pid,command,power",
];
const MAX_REASONABLE_POWER: f64 = 10_000.0;

#[derive(Debug, Clone)]
pub(crate) struct ProcRow {
    pub(crate) pid: i32,
    pub(crate) process: String,
    pub(crate) process_lc: String,
    pub(crate) power: String,
    pub(crate) power_num: f64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Snapshot {
    pub(crate) rows: Vec<ProcRow>,
    pub(crate) total_power: f64,
}

#[derive(Default)]
pub(crate) struct TopStreamParser {
    excluded_pids: Vec<i32>,
    in_table: bool,
    skipped_warmup: bool,
    rows: Vec<ProcRow>,
}

impl TopStreamParser {
    pub(crate) fn new(excluded_pids: Vec<i32>) -> Self {
        Self {
            excluded_pids,
            in_table: false,
            skipped_warmup: false,
            rows: Vec::new(),
        }
    }

    pub(crate) fn push_line(&mut self, line: &str) -> Result<Option<Snapshot>, String> {
        let trimmed = line.trim();

        if trimmed.starts_with("PID") {
            let finished = self.finish_frame();
            self.in_table = true;
            return Ok(finished);
        }

        if !self.in_table {
            return Ok(None);
        }

        if trimmed.is_empty() {
            return Ok(self.finish_frame());
        }

        let first = trimmed.chars().next().unwrap_or(' ');
        if !first.is_ascii_digit() {
            return Ok(self.finish_frame());
        }

        let row =
            parse_row(trimmed).ok_or_else(|| format!("unable to parse top row: {trimmed}"))?;

        if !self.excluded_pids.contains(&row.pid) {
            self.rows.push(row);
        }

        Ok(None)
    }

    pub(crate) fn finish_stream(&mut self) -> Option<Snapshot> {
        self.finish_frame()
    }

    fn finish_frame(&mut self) -> Option<Snapshot> {
        if !self.in_table {
            return None;
        }

        self.in_table = false;
        let rows = std::mem::take(&mut self.rows);

        if !self.skipped_warmup {
            self.skipped_warmup = true;
            return None;
        }

        Some(snapshot_from_rows(rows))
    }
}

pub(crate) fn snapshot_from_live_snapshot(live_snapshot: &persistence::LiveSnapshot) -> Snapshot {
    let rows = live_snapshot
        .samples
        .iter()
        .map(|sample| ProcRow {
            pid: sample.pid,
            process: sample.process.clone(),
            process_lc: sample.process.to_lowercase(),
            power: format!("{:.1}", sample.power),
            power_num: sample.power,
        })
        .collect();

    snapshot_from_rows(rows)
}

pub(crate) fn snapshot_from_rows(rows: Vec<ProcRow>) -> Snapshot {
    let total_power = rows.iter().map(|r| r.power_num).sum::<f64>();
    Snapshot { rows, total_power }
}

pub(crate) fn fetch_snapshot() -> io::Result<Snapshot> {
    let child = Command::new(TOP_BIN)
        .args(TOP_ARGS)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut excluded_pids = Vec::with_capacity(2);
    if let Ok(top_pid) = i32::try_from(child.id()) {
        excluded_pids.push(top_pid);
    }
    if let Ok(self_pid) = i32::try_from(std::process::id()) {
        excluded_pids.push(self_pid);
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "top command failed: {}",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let rows = parse_second_sample_excluding(&stdout, &excluded_pids);
    Ok(snapshot_from_rows(rows))
}

fn parse_second_sample_excluding(raw: &str, excluded_pids: &[i32]) -> Vec<ProcRow> {
    // Keep macOS top "second sample" semantics by resetting rows each time we hit a PID header.
    let mut rows = Vec::new();
    let mut in_table = false;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with("PID") {
            rows.clear();
            in_table = true;
            continue;
        }

        if !in_table {
            continue;
        }

        let first = trimmed.chars().next().unwrap_or(' ');
        if !first.is_ascii_digit() {
            continue;
        }

        if let Some(row) = parse_row(trimmed) {
            if excluded_pids.contains(&row.pid) {
                continue;
            }
            rows.push(row);
        }
    }

    rows
}

fn parse_row(line: &str) -> Option<ProcRow> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 3 {
        return None;
    }

    let pid = parts[0].parse::<i32>().ok()?;
    let power_num = sanitize_power_value(parse_numeric_value(parts.last()?));
    let power = format!("{power_num:.1}");
    let process = parts[1..parts.len().saturating_sub(1)].join(" ");
    let process_lc = process.to_lowercase();

    Some(ProcRow {
        pid,
        process,
        process_lc,
        power_num,
        power,
    })
}

fn parse_numeric_value(s: &str) -> f64 {
    let mut cleaned = String::new();
    let mut seen_digit = false;
    let mut seen_decimal = false;

    for ch in s.trim().chars() {
        if ch.is_ascii_digit() {
            cleaned.push(ch);
            seen_digit = true;
            continue;
        }

        if ch == ',' {
            if seen_digit {
                continue;
            }
            break;
        }

        if ch == '.' && !seen_decimal {
            cleaned.push(ch);
            seen_decimal = true;
            continue;
        }

        if seen_digit || seen_decimal {
            break;
        }
    }

    cleaned.parse::<f64>().unwrap_or(0.0)
}

fn sanitize_power_value(value: f64) -> f64 {
    if value.is_finite() && (0.0..=MAX_REASONABLE_POWER).contains(&value) {
        value
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_second_sample_keeps_only_last_pid_block() {
        let raw = r#"
Processes: 123 total
PID COMMAND POWER
1 launchd 0.2
2 Finder 1.9

PID COMMAND POWER
99 Safari 12.3
100 Google Chrome Helper 4.6
"#;

        let rows = parse_second_sample_excluding(raw, &[]);
        let pids: Vec<i32> = rows.iter().map(|r| r.pid).collect();

        assert_eq!(pids, vec![99, 100]);
        assert_eq!(rows[1].process, "Google Chrome Helper");
    }

    #[test]
    fn parse_second_sample_excludes_internal_pids() {
        let raw = r#"
PID COMMAND POWER
10 etop 5.0
20 top 3.0
30 Safari 1.0
"#;

        let rows = parse_second_sample_excluding(raw, &[10, 20]);
        let pids: Vec<i32> = rows.iter().map(|r| r.pid).collect();

        assert_eq!(pids, vec![30]);
    }

    #[test]
    fn top_stream_parser_skips_warmup_frame() {
        let mut parser = TopStreamParser::new(vec![]);
        let mut snapshots = Vec::new();

        let lines = [
            "PID COMMAND POWER",
            "10 Safari 1.0",
            "",
            "PID COMMAND POWER",
            "20 Finder 2.5",
            "",
        ];

        for line in lines {
            if let Some(snapshot) = parser.push_line(line).expect("stream line should parse") {
                snapshots.push(snapshot);
            }
        }

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].rows.len(), 1);
        assert_eq!(snapshots[0].rows[0].pid, 20);
        assert!((snapshots[0].total_power - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn top_stream_parser_excludes_internal_pids() {
        let mut parser = TopStreamParser::new(vec![30]);

        parser
            .push_line("PID COMMAND POWER")
            .expect("header should parse");
        parser
            .push_line("10 warmup 0.1")
            .expect("warmup row should parse");
        parser.push_line("").expect("warmup end should parse");

        parser
            .push_line("PID COMMAND POWER")
            .expect("header should parse");
        parser
            .push_line("30 top 7.0")
            .expect("excluded row should parse");
        let snapshot = parser
            .push_line("31 Safari 1.0")
            .expect("row should parse")
            .or_else(|| parser.push_line("").expect("frame boundary should parse"))
            .expect("second frame should emit");

        let pids: Vec<i32> = snapshot.rows.iter().map(|row| row.pid).collect();
        assert_eq!(pids, vec![31]);
    }

    #[test]
    fn top_stream_parser_reports_row_parse_errors() {
        let mut parser = TopStreamParser::new(vec![]);
        parser
            .push_line("PID COMMAND POWER")
            .expect("header should parse");

        let err = parser
            .push_line("123")
            .expect_err("malformed row should fail parsing");
        assert!(err.contains("unable to parse top row"));
    }

    #[test]
    fn parse_row_supports_multi_word_process_name() {
        let row = parse_row("4242 Google Chrome Helper 9.1").expect("row should parse");

        assert_eq!(row.pid, 4242);
        assert_eq!(row.process, "Google Chrome Helper");
        assert_eq!(row.power, "9.1");
        assert!((row.power_num - 9.1).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_row_rejects_missing_columns() {
        assert!(parse_row("123 onlypid").is_none());
    }

    #[test]
    fn parse_numeric_value_handles_grouping_separators_and_suffix_junk() {
        assert_eq!(parse_numeric_value("1,234.5"), 1234.5);
        assert_eq!(parse_numeric_value("12.3+"), 12.3);
    }

    #[test]
    fn parse_row_sanitizes_implausibly_large_power_values() {
        let row = parse_row("336 powerd 55443258.0").expect("row should parse");
        assert_eq!(row.power_num, 0.0);
        assert_eq!(row.power, "0.0");
    }
}
