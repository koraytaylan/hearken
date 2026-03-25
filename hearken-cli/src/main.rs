use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use hearken_core::{LogReader, LogTemplate, tokenize};
use hearken_ml::LogParser;
use hearken_storage::Storage;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(short, long, default_value = "hearken.db")]
    database: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Process a log file
    Process {
        /// Path to the log file
        file: String,
        /// Similarity threshold for pattern matching (0.0 to 1.0)
        #[arg(long, default_value_t = 0.5)]
        threshold: f64,
        /// Number of lines to process in each batch
        #[arg(long, default_value_t = 500000)]
        batch_size: usize,
    },
    /// Search for patterns in the database
    Search {
        /// Query string for full-text search
        query: String,
    },
    /// Generate an HTML report from the database
    Report {
        /// Output HTML file path
        #[arg(long, default_value = "report.html")]
        output: String,
        /// Number of sample occurrences per pattern
        #[arg(long, default_value_t = 5)]
        samples: usize,
        /// Maximum number of patterns to include (by occurrence count)
        #[arg(long, default_value_t = 500)]
        top: usize,
        /// Only include patterns containing ANY of these substrings (comma-separated)
        #[arg(long, value_delimiter = ',')]
        filter: Option<Vec<String>>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let storage = Storage::open(&cli.database)
        .context("Failed to open database")?;

    match cli.command {
        Commands::Process { file, threshold, batch_size } => {
            let mut storage = storage;
            process_log(&mut storage, &file, threshold, batch_size)?;
        }
        Commands::Search { query } => {
            search_patterns(&storage, &query)?;
        }
        Commands::Report { output, samples, top, filter } => {
            generate_report(&storage, &output, samples, top, filter)?;
        }
    }

    Ok(())
}

/// Computes a collapsed structural shape for a token.
/// Digits → 'd', letters → 'a', other chars kept as-is.
/// Consecutive same-class chars are collapsed: "23.10.2024" → "d.d.d"
fn token_shape(token: &str) -> String {
    let mut shape = String::with_capacity(16);
    let mut last_class = '\0';
    for c in token.chars().take(20) {
        let class = if c.is_ascii_digit() { 'd' }
                    else if c.is_ascii_alphabetic() { 'a' }
                    else { c };
        if class != last_class {
            shape.push(class);
            last_class = class;
        }
    }
    shape
}

/// Computes a structural fingerprint from a line's first two tokens.
/// Returns (has_leading_whitespace, fingerprint_string).
fn line_prefix_fingerprint(line: &str) -> (bool, String) {
    let has_leading_ws = line.as_bytes().first().map_or(false, |&b| b == b' ' || b == b'\t');
    let mut fp = String::with_capacity(32);
    for (i, tok) in line.split_whitespace().take(2).enumerate() {
        if i > 0 { fp.push('|'); }
        fp.push_str(&token_shape(tok));
    }
    (has_leading_ws, fp)
}

/// Auto-detects entry-start fingerprints from a sample of lines.
/// Finds the dominant prefix shapes covering ≥90% of non-whitespace-leading lines.
fn detect_entry_fingerprints(lines: &[(u64, &str, u64)]) -> HashSet<String> {
    let mut freq: HashMap<String, usize> = HashMap::new();
    let mut total_non_ws: usize = 0;

    for (_, line, _) in lines {
        let (has_ws, fp) = line_prefix_fingerprint(line);
        if has_ws || fp.is_empty() { continue; }
        total_non_ws += 1;
        *freq.entry(fp).or_insert(0) += 1;
    }

    if total_non_ws == 0 { return HashSet::new(); }

    let mut ranked: Vec<(String, usize)> = freq.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));

    let threshold = (total_non_ws as f64 * 0.90) as usize;
    let mut entry_fps = HashSet::new();
    let mut covered: usize = 0;

    for (fp, count) in &ranked {
        entry_fps.insert(fp.clone());
        covered += *count;
        if covered >= threshold { break; }
    }

    entry_fps
}

/// Checks if a line is an entry start based on auto-detected fingerprints.
fn is_entry_start(line: &str, entry_fps: &HashSet<String>) -> bool {
    if entry_fps.is_empty() { return true; } // fallback: every line is an entry
    let (has_ws, fp) = line_prefix_fingerprint(line);
    if has_ws { return false; }
    entry_fps.contains(&fp)
}

/// A grouped log entry: primary line plus any continuation lines.
struct GroupedEntry<'a> {
    start_pos: u64,
    primary_line: &'a str,
    continuation_lines: Vec<&'a str>,
    next_pos: u64,
}

/// Groups raw lines into logical log entries by merging continuation lines
/// with their parent entry using auto-detected structural fingerprints.
fn group_entries<'a>(lines: &[(u64, &'a str, u64)], entry_fps: &HashSet<String>) -> Vec<GroupedEntry<'a>> {
    if lines.is_empty() { return vec![]; }

    let mut entries: Vec<GroupedEntry> = Vec::with_capacity(lines.len());
    let mut i = 0;

    // Skip leading continuation lines (orphaned from previous batch)
    while i < lines.len() && !is_entry_start(lines[i].1, entry_fps) {
        i += 1;
    }
    if i >= lines.len() { return entries; }

    let mut cur_start = lines[i].0;
    let mut cur_line = lines[i].1;
    let mut cur_conts: Vec<&str> = Vec::new();
    let mut cur_next = lines[i].2;
    i += 1;

    while i < lines.len() {
        if is_entry_start(lines[i].1, entry_fps) {
            entries.push(GroupedEntry {
                start_pos: cur_start,
                primary_line: cur_line,
                continuation_lines: std::mem::take(&mut cur_conts),
                next_pos: cur_next,
            });
            cur_start = lines[i].0;
            cur_line = lines[i].1;
        } else {
            cur_conts.push(lines[i].1);
        }
        cur_next = lines[i].2;
        i += 1;
    }
    entries.push(GroupedEntry {
        start_pos: cur_start,
        primary_line: cur_line,
        continuation_lines: cur_conts,
        next_pos: cur_next,
    });
    entries
}

fn process_log(storage: &mut Storage, file_path: &str, threshold: f64, batch_size: usize) -> Result<()> {
    println!("Processing log file: {}", file_path);
    
    let source = storage.get_or_create_log_source(file_path)?;
    let reader = LogReader::new(file_path)?;
    let mut current_pos = source.last_processed_position;
    
    let mut parser = LogParser::new(15, threshold);
    
    // In-memory cache: template_index → DB pattern ID
    let mut pattern_id_cache: HashMap<usize, i64> = HashMap::new();
    // In-memory occurrence counts: template_index → count
    let mut occurrence_counts: HashMap<usize, u64> = HashMap::new();

    // Seed parser from DB and populate cache
    {
        let mut stmt = storage.conn.prepare("SELECT id, template FROM patterns")?;
        let rows = stmt.query_map([], |row| Ok(LogTemplate {
            id: Some(row.get(0)?),
            template: row.get(1)?,
        }))?;
        for row in rows {
            let template = row?;
            let db_id = template.id.unwrap();
            let idx = parser.templates.len();
            parser.add_template(template);
            pattern_id_cache.insert(idx, db_id);
        }
    }

    let file_size = reader.len();
    let mut total_lines: u64 = 0;
    let mut entry_fingerprints: HashSet<String> = HashSet::new();
    let mut fingerprints_detected = false;

    loop {
        let lines_with_pos = reader.read_batch(current_pos, batch_size)?;
        if lines_with_pos.is_empty() {
            println!("Finished processing all lines.");
            break;
        }

        let batch_len = lines_with_pos.len() as u64;
        total_lines += batch_len;

        // Auto-detect entry structure from first batch
        if !fingerprints_detected {
            entry_fingerprints = detect_entry_fingerprints(&lines_with_pos);
            fingerprints_detected = true;
            if entry_fingerprints.is_empty() {
                println!("No dominant line structure detected — treating each line as a separate entry.");
            } else {
                let entry_count = lines_with_pos.iter()
                    .filter(|(_, line, _)| is_entry_start(line, &entry_fingerprints))
                    .count();
                let cont = lines_with_pos.len() - entry_count;
                println!("Auto-detected entry structure ({} pattern(s)): {}/{} lines are entries, {} are continuations ({:.1}%)",
                    entry_fingerprints.len(), entry_count, lines_with_pos.len(),
                    cont, cont as f64 / lines_with_pos.len() as f64 * 100.0);
            }
        }

        // Group continuation lines with their parent entry
        let entries = group_entries(&lines_with_pos, &entry_fingerprints);
        if entries.is_empty() {
            // Entire batch was orphaned continuation lines — advance position and continue
            current_pos = lines_with_pos.last().unwrap().2;
            continue;
        }

        let start_pos = entries[0].start_pos;
        let progress = if file_size > 0 {
            (start_pos as f64 / file_size as f64) * 100.0
        } else {
            100.0
        };

        println!("Progress: {:.2}% (Position: {}, Lines: {}, Entries: {})",
            progress, start_pos, total_lines, entries.len());

        // 1. Parallel Transformation Phase (CPU HEAVY)
        // Tokenize primary line + continuation lines into a single token stream.
        let t_parallel = Instant::now();
        let parallel_results: Vec<(Vec<&str>, Option<usize>)> = entries.par_iter().map(|entry| {
            let mut tokens: Vec<&str> = tokenize(entry.primary_line);
            for cont_line in &entry.continuation_lines {
                if tokens.len() >= 1024 { break; }
                tokens.push("\n");
                tokens.extend(tokenize(cont_line));
            }
            tokens.truncate(1024);
            let matched = parser.find_match(&tokens);
            (tokens, matched)
        }).collect();
        let parallel_ms = t_parallel.elapsed().as_millis();

        // 2. Sequential Discovery Phase — pattern matching only, no DB writes
        let t_sequential = Instant::now();
        let mut new_patterns: Vec<(usize, String)> = Vec::new();
        let mut evolved_patterns: Vec<(i64, String)> = Vec::new();
        let mut occurrence_buffer: Vec<(usize, usize)> = Vec::with_capacity(entries.len());

        for (entry_idx, (tokens, maybe_match)) in parallel_results.iter().enumerate() {
            let template_idx = parser.parse_tokens(tokens, *maybe_match);
            if template_idx == usize::MAX { continue; }

            // Count occurrence in-memory
            *occurrence_counts.entry(template_idx).or_insert(0) += 1;
            occurrence_buffer.push((entry_idx, template_idx));

            let template = &mut parser.templates[template_idx];

            // Track new/evolved patterns for DB persistence
            let mut p_id = template.id;
            let evolved = p_id.is_some_and(|id| id < 0);
            if evolved { p_id = p_id.map(|id| -id); }

            if let Some(id) = p_id {
                if evolved {
                    template.id = Some(id);
                    let template_str = template.template_string();
                    evolved_patterns.push((id, template_str));
                }
                pattern_id_cache.insert(template_idx, id);
            } else if !pattern_id_cache.contains_key(&template_idx) {
                let template_str = template.template_string();
                new_patterns.push((template_idx, template_str));
            }
        }
        let sequential_ms = t_sequential.elapsed().as_millis();

        // 3. Minimal DB writes — only new/evolved patterns (not occurrences)
        let t_db = Instant::now();
        let tx = storage.conn.transaction()?;

        for (template_idx, template_str) in &new_patterns {
            let changes = tx.execute("INSERT OR IGNORE INTO patterns (template) VALUES (?)", rusqlite::params![template_str])?;
            let id: i64 = if changes > 0 {
                tx.last_insert_rowid()
            } else {
                tx.query_row("SELECT id FROM patterns WHERE template = ?", rusqlite::params![template_str], |row| row.get(0))?
            };
            pattern_id_cache.insert(*template_idx, id);
            parser.templates[*template_idx].id = Some(id);
        }

        if !evolved_patterns.is_empty() {
            let mut update_stmt = tx.prepare_cached("UPDATE patterns SET template = ? WHERE id = ?")?;
            for (id, template_str) in &evolved_patterns {
                update_stmt.execute(rusqlite::params![template_str, id])?;
            }
        }

        let last_entry_pos = entries.last().unwrap().next_pos;
        tx.execute(
            "UPDATE log_sources SET last_processed_position = ? WHERE id = ?",
            rusqlite::params![last_entry_pos as i64, source.id.unwrap()],
        )?;

        // Insert occurrences for every matched entry in this batch
        {
            let source_id = source.id.unwrap();
            let mut occ_stmt = tx.prepare_cached(
                "INSERT INTO occurrences (log_source_id, pattern_id, timestamp, variables) VALUES (?, ?, ?, ?)"
            )?;
            for &(entry_idx, template_idx) in &occurrence_buffer {
                if let Some(&pattern_id) = pattern_id_cache.get(&template_idx) {
                    let pos = entries[entry_idx].start_pos as i64;
                    let tmpl_tokens = &parser.templates[template_idx].tokens;
                    let entry_tokens = &parallel_results[entry_idx].0;
                    let variables: String = tmpl_tokens.iter().zip(entry_tokens.iter())
                        .filter_map(|(t, e)| if *t == "<*>" { Some(*e) } else { None })
                        .collect::<Vec<_>>()
                        .join("\t");
                    occ_stmt.execute(rusqlite::params![source_id, pattern_id, pos, variables])?;
                }
            }
        }

        tx.commit()?;
        let db_ms = t_db.elapsed().as_millis();
        current_pos = last_entry_pos;

        println!("  Batch: parallel={}ms, sequential={}ms, db={}ms, templates={}",
            parallel_ms, sequential_ms, db_ms, parser.templates.len());
    }

    // Write occurrence counts and rebuild FTS index in a single transaction
    println!("Writing pattern counts and rebuilding search index...");
    {
        let tx = storage.conn.transaction()?;
        {
            let mut update_stmt = tx.prepare("UPDATE patterns SET occurrence_count = ? WHERE id = ?")?;
            for (template_idx, count) in &occurrence_counts {
                if let Some(&pattern_id) = pattern_id_cache.get(template_idx) {
                    update_stmt.execute(rusqlite::params![*count as i64, pattern_id])?;
                }
            }
        }
        tx.execute("DELETE FROM patterns_fts", [])?;
        tx.execute(
            "INSERT INTO patterns_fts (pattern_id, template) SELECT id, template FROM patterns",
            [],
        )?;
        tx.commit()?;
    }

    println!("\nProcessed {} lines, discovered {} patterns.", total_lines, parser.templates.len());
    println!("\nAnalysis Summary:");
    let top_patterns = storage.get_top_patterns(10)?;
    println!("Top 10 Prevalent Patterns:");
    for (template, count) in top_patterns {
        println!("  - [{} occurrences] {}", count, template);
    }

    Ok(())
}

fn search_patterns(storage: &Storage, query: &str) -> Result<()> {
    let results = storage.search_patterns(query)?;
    println!("Found {} patterns matching '{}':", results.len(), query);
    for (id, template) in results {
        println!("[Pattern ID: {}] {}", id, template);
    }
    Ok(())
}

fn generate_report(storage: &Storage, output_path: &str, samples_per_pattern: usize, top_n: usize, filter: Option<Vec<String>>) -> Result<()> {
    let start = Instant::now();
    println!("Generating report...");

    let (pattern_count, total_occurrences, sources) = storage.get_report_summary()
        .context("Failed to query report summary")?;

    if pattern_count == 0 {
        anyhow::bail!("No patterns found in database. Process a log file first.");
    }

    let patterns = storage.get_all_patterns_ranked(top_n, filter.as_deref())
        .context("Failed to query patterns")?;

    if let Some(ref f) = filter {
        println!("  Filter: patterns containing any of {:?}", f);
    }

    println!("  Fetching samples for {} patterns...", patterns.len());
    let mut pattern_data = Vec::with_capacity(patterns.len());
    for (id, template, count) in &patterns {
        let raw_samples = storage.get_pattern_samples(*id, samples_per_pattern)
            .unwrap_or_default();
        let samples: Vec<String> = raw_samples.iter().map(|vars| {
            let mut var_iter = vars.split('\t');
            let mut rebuilt = String::with_capacity(template.len() + vars.len());
            let mut chars = template.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '<' && chars.peek() == Some(&'*') {
                    chars.next(); // consume '*'
                    if chars.peek() == Some(&'>') {
                        chars.next(); // consume '>'
                        if let Some(val) = var_iter.next() {
                            rebuilt.push_str(val);
                        } else {
                            rebuilt.push_str("<*>");
                        }
                        continue;
                    }
                    rebuilt.push('<');
                    rebuilt.push('*');
                } else {
                    rebuilt.push(c);
                }
            }
            rebuilt
        }).collect();
        pattern_data.push(serde_json::json!({
            "id": id,
            "template": template,
            "count": count,
            "samples": samples,
        }));
    }

    let now: String = {
        let output = std::process::Command::new("date")
            .arg("+%Y-%m-%d %H:%M:%S")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        output
    };
    let command = std::env::args().collect::<Vec<_>>().join(" ");
    let report_json = serde_json::json!({
        "pattern_count": pattern_count,
        "total_occurrences": total_occurrences,
        "sources": sources,
        "generated_at": now,
        "command": command,
        "params": {
            "top": top_n,
            "samples": samples_per_pattern,
            "filter": filter,
            "output": output_path,
        },
        "patterns": pattern_data,
    });

    let json_str = serde_json::to_string(&report_json)
        .context("Failed to serialize report data")?;

    // Escape sequences that would break the <script> tag when embedded in HTML.
    // "</script>" or "</Script>" etc. inside a string literal terminates the tag.
    let json_str = json_str.replace("</", "<\\/");

    let template = include_str!("report_template.html");
    let html = template.replace("/*__REPORT_DATA__*/", &json_str);
    let file_size_bytes = html.len();

    // Inject the file size into the already-built HTML via a data attribute on the body
    let html = html.replace("<body>", &format!("<body data-file-size=\"{}\">", file_size_bytes));

    std::fs::write(output_path, &html)
        .with_context(|| format!("Failed to write report to {}", output_path))?;

    let elapsed = start.elapsed();
    println!("Report generated in {:.1}s", elapsed.as_secs_f64());
    println!("  Patterns: {}", pattern_count);
    println!("  Total occurrences: {}", total_occurrences);
    println!("  Output: {}", output_path);
    println!("  Size: {:.1} KB", html.len() as f64 / 1024.0);

    Ok(())
}
