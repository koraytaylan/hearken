use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Utc};
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::fs::File;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CoreError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Parse error: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogSource {
    pub id: Option<i64>,
    pub file_path: String,
    pub last_processed_position: u64,
    pub file_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogTemplate {
    pub id: Option<i64>,
    pub template: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogOccurrence {
    pub id: Option<i64>,
    pub log_source_id: i64,
    pub pattern_id: i64,
    pub timestamp: DateTime<Utc>,
    pub variables: Vec<String>,
    pub raw_message: String,
}

/// Whitespace tokenizer that keeps paired delimiters together.
/// Does not split inside `()` or `[]`, so `invoke0(Native Method)` stays
/// as one token instead of being split into `invoke0(Native` and `Method)`.
pub fn tokenize(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut tokens = Vec::new();
    let mut start: Option<usize> = None;
    let mut depth: i32 = 0;

    for i in 0..len {
        let b = bytes[i];
        if b.is_ascii_whitespace() && depth <= 0 {
            if let Some(s) = start {
                tokens.push(&line[s..i]);
                start = None;
            }
            depth = 0;
        } else {
            if start.is_none() {
                start = Some(i);
            }
            match b {
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                _ => {}
            }
        }
    }
    if let Some(s) = start {
        tokens.push(&line[s..]);
    }
    tokens
}

thread_local! {
    static LAST_TS_FORMAT: Cell<usize> = const { Cell::new(0) };
}

const SYSLOG_MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn parse_syslog_month(s: &str) -> Option<u32> {
    SYSLOG_MONTHS
        .iter()
        .position(|&m| m == s)
        .map(|i| i as u32 + 1)
}

/// Attempts to extract a Unix timestamp (seconds) from the beginning of a log line.
///
/// Tries these formats in order (with thread-local caching of last successful format):
/// 0. ISO 8601 with `T` separator: `2026-01-15T08:00:00.123Z`
/// 1. ISO 8601 with space + comma frac: `2026-01-15 08:00:00,123`
/// 2. ISO 8601 with space + dot frac: `2026-01-15 08:00:00.123`
/// 3. Syslog-style: `Mar 15 08:00:00` (assumes current year)
/// 4. Common log format: `15/Mar/2026:08:00:00 +0000`
/// 5. Unix epoch: 10-digit number at line start
pub fn extract_timestamp(line: &str) -> Option<i64> {
    const NUM_FORMATS: usize = 6;
    let start = LAST_TS_FORMAT.with(std::cell::Cell::get);

    for offset in 0..NUM_FORMATS {
        let idx = (start + offset) % NUM_FORMATS;
        let result = match idx {
            0 => try_iso8601_t(line),
            1 => try_iso8601_space_comma(line),
            2 => try_iso8601_space_dot(line),
            3 => try_syslog(line),
            4 => try_common_log(line),
            5 => try_unix_epoch(line),
            _ => None,
        };
        if let Some(ts) = result {
            LAST_TS_FORMAT.with(|c| c.set(idx));
            return Some(ts);
        }
    }
    None
}

// ISO 8601 with T: 2026-01-15T08:00:00.123Z or 2026-01-15T08:00:00Z
fn try_iso8601_t(line: &str) -> Option<i64> {
    // Minimum: YYYY-MM-DDTHH:MM:SS (19 chars)
    if line.len() < 19 || line.as_bytes()[4] != b'-' || line.as_bytes()[10] != b'T' {
        return None;
    }
    // Find the end of the timestamp portion
    let ts_part = &line[..line.len().min(35)];
    // Try parsing with chrono's DateTime parser which handles Z, +00:00, fractional seconds
    if let Ok(dt) =
        DateTime::parse_from_rfc3339(ts_part.split_whitespace().next().unwrap_or(ts_part))
    {
        return Some(dt.timestamp());
    }
    // Try without timezone (assume UTC)
    let candidate = ts_part.split_whitespace().next().unwrap_or(ts_part);
    // Strip trailing non-datetime chars
    let candidate = candidate.trim_end_matches(|c: char| !c.is_ascii_digit());
    NaiveDateTime::parse_from_str(candidate, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|ndt| ndt.and_utc().timestamp())
}

// ISO 8601 with space and comma fraction: 2026-01-15 08:00:00,123
fn try_iso8601_space_comma(line: &str) -> Option<i64> {
    if line.len() < 19
        || line.as_bytes()[4] != b'-'
        || line.as_bytes()[10] != b' '
        || line.as_bytes()[13] != b':'
    {
        return None;
    }
    // Replace comma with dot for chrono parsing
    let end = line.len().min(23);
    let candidate = &line[..end];
    if !candidate.contains(',') {
        return None;
    }
    let fixed = candidate.replace(',', ".");
    NaiveDateTime::parse_from_str(&fixed, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .map(|ndt| ndt.and_utc().timestamp())
}

// ISO 8601 with space and dot fraction: 2026-01-15 08:00:00.123
fn try_iso8601_space_dot(line: &str) -> Option<i64> {
    if line.len() < 19
        || line.as_bytes()[4] != b'-'
        || line.as_bytes()[10] != b' '
        || line.as_bytes()[13] != b':'
    {
        return None;
    }
    let end = line.len().min(23);
    let candidate = &line[..end];
    NaiveDateTime::parse_from_str(candidate, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| {
            NaiveDateTime::parse_from_str(&line[..line.len().min(19)], "%Y-%m-%d %H:%M:%S")
        })
        .ok()
        .map(|ndt| ndt.and_utc().timestamp())
}

// Syslog: Mar 15 08:00:00
fn try_syslog(line: &str) -> Option<i64> {
    // Format: "Mon DD HH:MM:SS" or "Mon  D HH:MM:SS" — minimum 15 chars
    if line.len() < 15 {
        return None;
    }
    let month_str = &line[..3];
    let month = parse_syslog_month(month_str)?;

    // Day can be space-padded: "Mar  5" or "Mar 15"
    let day_and_rest = &line[3..];
    let day_str = day_and_rest[..3].trim();
    let day: u32 = day_str.parse().ok()?;

    if line.as_bytes().get(6)? != &b' ' && line.as_bytes().get(5)? != &b' ' {
        return None;
    }
    // Find time portion (HH:MM:SS)
    let time_start = if line.as_bytes()[5] == b' ' { 6 } else { 7 };
    if line.len() < time_start + 8 {
        return None;
    }
    let time_str = &line[time_start..time_start + 8];
    if time_str.as_bytes()[2] != b':' || time_str.as_bytes()[5] != b':' {
        return None;
    }
    let hour: u32 = time_str[..2].parse().ok()?;
    let min: u32 = time_str[3..5].parse().ok()?;
    let sec: u32 = time_str[6..8].parse().ok()?;

    let year = Utc::now().year();
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let dt = date.and_hms_opt(hour, min, sec)?;
    Some(dt.and_utc().timestamp())
}

// Common log format: 15/Mar/2026:08:00:00 +0000
fn try_common_log(line: &str) -> Option<i64> {
    // May start with [ from CLF: [15/Mar/2026:08:00:00 +0000]
    let s = line.strip_prefix('[').unwrap_or(line);
    if s.len() < 26 || s.as_bytes()[2] != b'/' || s.as_bytes()[6] != b'/' {
        return None;
    }
    let candidate = &s[..26];
    NaiveDateTime::parse_from_str(candidate, "%d/%b/%Y:%H:%M:%S %z")
        .ok()
        .map(|ndt| ndt.and_utc().timestamp())
        .or_else(|| {
            DateTime::parse_from_str(candidate, "%d/%b/%Y:%H:%M:%S %z")
                .ok()
                .map(|dt| dt.timestamp())
        })
}

// Unix epoch: 10-digit number at line start
fn try_unix_epoch(line: &str) -> Option<i64> {
    let bytes = line.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    // Verify first 10 chars are digits and the 11th (if present) is not a digit
    for &b in &bytes[..10] {
        if !b.is_ascii_digit() {
            return None;
        }
    }
    if bytes.len() > 10 && bytes[10].is_ascii_digit() {
        return None;
    }
    let epoch: i64 = line[..10].parse().ok()?;
    // Sanity: between 2000-01-01 and 2100-01-01
    if (946_684_800..=4_102_444_800).contains(&epoch) {
        Some(epoch)
    } else {
        None
    }
}

pub struct LogReader {
    mmap: Mmap,
}

impl LogReader {
    pub fn new(file_path: &str) -> Result<Self, CoreError> {
        let file = File::open(file_path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        #[cfg(unix)]
        mmap.advise(memmap2::Advice::Sequential)?;
        Ok(Self { mmap })
    }

    /// Absolute fastest zero-copy batch reading using memory mapping.
    /// Returns (start_pos, truncated_line_content, next_line_start_pos)
    pub fn read_batch(
        &self,
        start_pos: u64,
        batch_size: usize,
    ) -> Result<Vec<(u64, &str, u64)>, CoreError> {
        if start_pos >= self.mmap.len() as u64 {
            return Ok(vec![]);
        }

        let mut lines = Vec::with_capacity(batch_size);
        let content = &self.mmap[start_pos as usize..];

        let mut current_offset = 0;
        for _ in 0..batch_size {
            if current_offset >= content.len() {
                break;
            }

            let remaining = &content[current_offset..];
            if let Some(pos) = remaining.iter().position(|&b| b == b'\n') {
                let mut line_bytes = &remaining[..pos];

                // Strip CR if present
                if line_bytes.last() == Some(&b'\r') {
                    line_bytes = &line_bytes[..line_bytes.len() - 1];
                }

                // Fast byte-level truncation
                if line_bytes.len() > 64 * 1024 {
                    line_bytes = &line_bytes[..64 * 1024];
                }

                // Safe, zero-allocation UTF-8 conversion.
                // If it fails (e.g., binary dump or cut in middle of multibyte char), we just get ""
                let line = std::str::from_utf8(line_bytes).unwrap_or("");

                let next_pos = start_pos + current_offset as u64 + pos as u64 + 1;
                lines.push((start_pos + current_offset as u64, line, next_pos));
                current_offset += pos + 1;
            } else {
                let mut line_bytes = remaining;
                if line_bytes.last() == Some(&b'\r') {
                    line_bytes = &line_bytes[..line_bytes.len() - 1];
                }
                if line_bytes.len() > 64 * 1024 {
                    line_bytes = &line_bytes[..64 * 1024];
                }

                let line = std::str::from_utf8(line_bytes).unwrap_or("");
                let next_pos = start_pos + content.len() as u64;
                lines.push((start_pos + current_offset as u64, line, next_pos));
                break;
            }
        }

        Ok(lines)
    }

    pub fn len(&self) -> u64 {
        self.mmap.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_tokenize_keeps_parens_together() {
        // Parenthesised token with internal space stays as one token
        let tokens = tokenize("\tat com.example.reflect.Accessor.invoke0(Native Method)");
        assert_eq!(
            tokens,
            vec!["at", "com.example.reflect.Accessor.invoke0(Native Method)"]
        );

        let tokens = tokenize("\tat com.example.proxy.GeneratedAccessor42.invoke(Unknown Source)");
        assert_eq!(
            tokens,
            vec![
                "at",
                "com.example.proxy.GeneratedAccessor42.invoke(Unknown Source)"
            ]
        );

        // Normal frame without internal spaces — no change
        let tokens =
            tokenize("\tat com.example.app.Widget.process(Widget.java:538) [module:2.3.1]");
        assert_eq!(
            tokens,
            vec![
                "at",
                "com.example.app.Widget.process(Widget.java:538)",
                "[module:2.3.1]"
            ]
        );

        // Bracket-delimited token with spaces stays as one token
        let tokens = tokenize("*INFO* [Background Worker Pool Thread] ServiceImpl");
        assert_eq!(
            tokens,
            vec!["*INFO*", "[Background Worker Pool Thread]", "ServiceImpl"]
        );
    }

    #[test]
    fn test_mmap_reader() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "line 1").unwrap();
        writeln!(file, "line 2").unwrap();
        let path = file.path().to_str().unwrap();

        let reader = LogReader::new(path).unwrap();
        let lines = reader.read_batch(0, 10).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].1, "line 1");
        assert_eq!(lines[1].1, "line 2");
    }

    #[test]
    fn test_extract_timestamp_iso8601_t_zulu() {
        let ts = extract_timestamp("2026-01-15T08:00:00.123Z some log message");
        assert_eq!(ts, Some(1_768_464_000));
    }

    #[test]
    fn test_extract_timestamp_iso8601_t_no_frac() {
        let ts = extract_timestamp("2026-01-15T08:00:00Z INFO starting up");
        assert_eq!(ts, Some(1_768_464_000));
    }

    #[test]
    fn test_extract_timestamp_iso8601_space_comma() {
        let ts = extract_timestamp("2026-01-15 08:00:00,123 INFO starting up");
        assert_eq!(ts, Some(1_768_464_000));
    }

    #[test]
    fn test_extract_timestamp_iso8601_space_dot() {
        let ts = extract_timestamp("2026-01-15 08:00:00.123 INFO starting up");
        assert_eq!(ts, Some(1_768_464_000));
    }

    #[test]
    fn test_extract_timestamp_syslog() {
        let ts = extract_timestamp("Mar 15 08:00:00 myhost sshd[1234]: message");
        assert!(ts.is_some());
        // Verify the month/day/time portion using current year
        let year = Utc::now().year();
        let expected = NaiveDate::from_ymd_opt(year, 3, 15)
            .unwrap()
            .and_hms_opt(8, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        assert_eq!(ts, Some(expected));
    }

    #[test]
    fn test_extract_timestamp_common_log() {
        let ts = extract_timestamp("15/Mar/2026:08:00:00 +0000 \"GET / HTTP/1.1\"");
        assert_eq!(ts, Some(1_773_561_600));
    }

    #[test]
    fn test_extract_timestamp_common_log_bracketed() {
        let ts = extract_timestamp("[15/Mar/2026:08:00:00 +0000] \"GET / HTTP/1.1\"");
        assert_eq!(ts, Some(1_773_561_600));
    }

    #[test]
    fn test_extract_timestamp_unix_epoch() {
        let ts = extract_timestamp("1742000000 some event");
        assert_eq!(ts, Some(1_742_000_000));
    }

    #[test]
    fn test_extract_timestamp_unix_epoch_exact() {
        let ts = extract_timestamp("1742000000");
        assert_eq!(ts, Some(1_742_000_000));
    }

    #[test]
    fn test_extract_timestamp_no_match() {
        assert_eq!(extract_timestamp("just a regular log line"), None);
        assert_eq!(extract_timestamp("ERROR: something failed"), None);
        assert_eq!(extract_timestamp(""), None);
    }

    #[test]
    fn test_extract_timestamp_thread_local_cache() {
        // Call with syslog first, then again — the cache should make the second call fast
        let _ = extract_timestamp("Mar 15 08:00:00 host msg");
        let ts = extract_timestamp("Mar 16 09:00:00 host msg2");
        assert!(ts.is_some());
    }
}
