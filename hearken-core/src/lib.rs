use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::File;
use thiserror::Error;
use memmap2::Mmap;

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

pub struct LogReader {
    mmap: Mmap,
}

impl LogReader {
    pub fn new(file_path: &str) -> Result<Self, CoreError> {
        let file = File::open(file_path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        #[cfg(unix)]
        mmap.advise(memmap2::Advice::Sequential)?;
        Ok(Self {
            mmap,
        })
    }

    /// Absolute fastest zero-copy batch reading using memory mapping.
    /// Returns (start_pos, truncated_line_content, next_line_start_pos)
    pub fn read_batch(&self, start_pos: u64, batch_size: usize) -> Result<Vec<(u64, &str, u64)>, CoreError> {
        if start_pos >= self.mmap.len() as u64 {
            return Ok(vec![]);
        }

        let mut lines = Vec::with_capacity(batch_size);
        let content = &self.mmap[start_pos as usize..];
        
        let mut current_offset = 0;
        for _ in 0..batch_size {
            if current_offset >= content.len() { break; }
            
            let remaining = &content[current_offset..];
            match remaining.iter().position(|&b| b == b'\n') {
                Some(pos) => {
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
                }
                None => {
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
        }
        
        Ok(lines)
    }

    pub fn len(&self) -> u64 {
        self.mmap.len() as u64
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
        assert_eq!(tokens, vec!["at", "com.example.reflect.Accessor.invoke0(Native Method)"]);

        let tokens = tokenize("\tat com.example.proxy.GeneratedAccessor42.invoke(Unknown Source)");
        assert_eq!(tokens, vec!["at", "com.example.proxy.GeneratedAccessor42.invoke(Unknown Source)"]);

        // Normal frame without internal spaces — no change
        let tokens = tokenize("\tat com.example.app.Widget.process(Widget.java:538) [module:2.3.1]");
        assert_eq!(tokens, vec!["at", "com.example.app.Widget.process(Widget.java:538)", "[module:2.3.1]"]);

        // Bracket-delimited token with spaces stays as one token
        let tokens = tokenize("*INFO* [Background Worker Pool Thread] ServiceImpl");
        assert_eq!(tokens, vec!["*INFO*", "[Background Worker Pool Thread]", "ServiceImpl"]);
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
}
