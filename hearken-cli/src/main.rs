#![allow(clippy::collapsible_if, clippy::too_many_arguments)]

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use hearken_core::{LogReader, LogTemplate, extract_timestamp, tokenize};
use hearken_ml::{LogParser, template_similarity};
use hearken_storage::Storage;
use rayon::prelude::*;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

#[cfg(feature = "web")]
mod web;

#[derive(Deserialize, Default, Debug)]
struct HearkenConfig {
    database: Option<String>,
    threshold: Option<f64>,
    batch_size: Option<usize>,
    #[serde(default)]
    report: ReportConfig,
    #[serde(default)]
    export: ExportConfig,
    #[serde(default)]
    check: CheckConfig,
    #[cfg(feature = "jira")]
    #[serde(default)]
    jira: Option<hearken_jira::JiraTomlConfig>,
}

#[derive(Deserialize, Default, Debug)]
struct ReportConfig {
    output: Option<String>,
    top: Option<usize>,
    samples: Option<usize>,
    filter: Option<Vec<String>>,
    group: Option<Vec<String>>,
    tags_file: Option<String>,
    bucket: Option<String>,
}

#[derive(Deserialize, Default, Debug)]
struct ExportConfig {
    format: Option<String>,
    top: Option<usize>,
    samples: Option<usize>,
    filter: Option<Vec<String>>,
    group: Option<Vec<String>>,
    tags_file: Option<String>,
    bucket: Option<String>,
}

#[derive(Deserialize, Default, Debug)]
struct CheckConfig {
    max_anomaly_score: Option<f64>,
    max_new_patterns: Option<usize>,
    baseline: Option<String>,
    tags_file: Option<String>,
}

fn load_config() -> HearkenConfig {
    // Search for .hearken.toml in cwd, then parent dirs
    let mut dir = std::env::current_dir().ok();
    while let Some(ref d) = dir {
        let config_path = d.join(".hearken.toml");
        if config_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                if let Ok(config) = toml::from_str::<HearkenConfig>(&content) {
                    eprintln!("Using config: {}", config_path.display());
                    return config;
                }
            }
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }
    // Check ~/.config/hearken/config.toml
    if let Some(home) = std::env::var_os("HOME") {
        let config_path = Path::new(&home).join(".config/hearken/config.toml");
        if config_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                if let Ok(config) = toml::from_str::<HearkenConfig>(&content) {
                    eprintln!("Using config: {}", config_path.display());
                    return config;
                }
            }
        }
    }
    HearkenConfig::default()
}

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
    /// Process log file(s) — use "-" to read from stdin
    Process {
        /// Path(s) to log file(s) — use shell glob e.g. ~/logs/*.log, or "-" for stdin
        #[arg(required = true)]
        files: Vec<String>,
        /// Similarity threshold for pattern matching (0.0 to 1.0)
        #[arg(long, default_value_t = 0.5)]
        threshold: f64,
        /// Number of lines to process in each batch
        #[arg(long, default_value_t = 500000)]
        batch_size: usize,
        /// Override the derived group name (default for stdin: "stdin")
        #[arg(long)]
        group_name: Option<String>,
        /// After processing, sync patterns to JIRA
        #[cfg(feature = "jira")]
        #[arg(long)]
        jira_sync: bool,
    },
    /// Search for patterns in the database
    Search {
        /// Query string for full-text search
        query: String,
    },
    /// Show database statistics
    Stats {},
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
        /// Only include patterns from these file groups (comma-separated)
        #[arg(long, value_delimiter = ',')]
        group: Option<Vec<String>>,
        /// Path to tags JSON file for pattern tagging (created if absent)
        #[arg(long)]
        tags_file: Option<String>,
        /// Include suppressed patterns in output (default: excluded)
        #[arg(long)]
        include_suppressed: bool,
        /// Time bucket for trends: hour, day, or auto (auto-detect)
        #[arg(long, default_value = "auto")]
        bucket: String,
    },
    /// Export patterns as JSON or CSV
    Export {
        /// Output format
        #[arg(long, default_value = "json")]
        format: String,
        /// Output file path (defaults to stdout if not specified)
        #[arg(long)]
        output: Option<String>,
        /// Number of sample occurrences per pattern
        #[arg(long, default_value_t = 5)]
        samples: usize,
        /// Maximum number of patterns to include (by occurrence count)
        #[arg(long, default_value_t = 500)]
        top: usize,
        /// Only include patterns containing ANY of these substrings (comma-separated)
        #[arg(long, value_delimiter = ',')]
        filter: Option<Vec<String>>,
        /// Only include patterns from these file groups (comma-separated)
        #[arg(long, value_delimiter = ',')]
        group: Option<Vec<String>>,
        /// Path to tags JSON file for pattern suppression
        #[arg(long)]
        tags_file: Option<String>,
        /// Include suppressed patterns in output (default: excluded)
        #[arg(long)]
        include_suppressed: bool,
        /// Time bucket for trends: hour, day, or auto (auto-detect)
        #[arg(long, default_value = "auto")]
        bucket: String,
    },
    /// Compare two databases to find new, resolved, and changed patterns
    Diff {
        /// Path to the "before" database
        before: String,
        /// Path to the "after" database
        after: String,
        /// Output format (text or json)
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Find near-duplicate patterns within groups
    Dedup {
        /// Similarity threshold for considering patterns duplicates (0.0-1.0)
        #[arg(long, default_value_t = 0.95)]
        threshold: f64,
        /// Only check patterns from this file group
        #[arg(long)]
        group: Option<String>,
        /// Output format (text or json)
        #[arg(long, default_value = "text")]
        format: String,
        /// Matching mode: structural (same token count, position-based) or semantic (TF-IDF, any length)
        #[arg(long, default_value = "structural")]
        mode: String,
    },
    /// Detect anomalous patterns (single-source or statistical outliers)
    Anomalies {
        /// Only check patterns from this file group
        #[arg(long)]
        group: Option<String>,
        /// Maximum number of anomalies to display
        #[arg(long, default_value_t = 50)]
        top: usize,
        /// Output format (text or json)
        #[arg(long, default_value = "text")]
        format: String,
        /// Path to tags JSON file for pattern suppression
        #[arg(long)]
        tags_file: Option<String>,
        /// Include suppressed patterns
        #[arg(long)]
        include_suppressed: bool,
    },
    /// Save or compare database baselines for regression detection
    Baseline {
        #[command(subcommand)]
        action: BaselineAction,
    },
    /// Run CI/CD quality gate checks on processed logs
    Check {
        /// Maximum anomaly score before failing
        #[arg(long)]
        max_anomaly_score: Option<f64>,
        /// Maximum number of new patterns vs baseline before failing
        #[arg(long)]
        max_new_patterns: Option<usize>,
        /// Fail if any pattern contains this substring
        #[arg(long)]
        fail_on_pattern: Option<Vec<String>>,
        /// Path to baseline database for comparison
        #[arg(long)]
        baseline: Option<String>,
        /// Path to tags JSON file for pattern suppression
        #[arg(long)]
        tags_file: Option<String>,
        /// Only check patterns from this file group
        #[arg(long)]
        group: Option<String>,
        /// Output format (text or json)
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Find correlated patterns across file groups within time windows
    Correlate {
        /// Time window in seconds for co-occurrence detection
        #[arg(long, default_value_t = 60)]
        window: u64,
        /// Maximum number of correlations to display
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Minimum pattern occurrence count to include
        #[arg(long, default_value_t = 10)]
        min_count: i64,
        /// Output format (text or json)
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Cluster patterns by shared variable values (IPs, request IDs, threads)
    Cluster {
        /// Minimum number of shared variables to link patterns
        #[arg(long, default_value_t = 3)]
        min_shared: usize,
        /// Maximum number of clusters to display
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Only check patterns from this file group
        #[arg(long)]
        group: Option<String>,
        /// Maximum number of top patterns to analyze
        #[arg(long, default_value_t = 200)]
        pattern_limit: usize,
        /// Output format (text or json)
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Watch log files for changes and process new entries live
    Watch {
        /// Path(s) to log file(s) to watch
        #[arg(required = true)]
        files: Vec<String>,
        /// Similarity threshold for pattern matching (0.0 to 1.0)
        #[arg(long, default_value_t = 0.5)]
        threshold: f64,
        /// Number of lines to process in each batch
        #[arg(long, default_value_t = 500000)]
        batch_size: usize,
        /// Trigger OS notification when any anomaly score exceeds this value
        #[arg(long)]
        alert_score: Option<f64>,
        /// After processing, sync patterns to JIRA
        #[cfg(feature = "jira")]
        #[arg(long)]
        jira_sync: bool,
    },
    /// JIRA integration: create and manage JIRA tickets from patterns
    #[cfg(feature = "jira")]
    Jira {
        #[command(subcommand)]
        action: JiraAction,
    },
    /// Start a local HTTP server with a live web dashboard
    #[cfg(feature = "web")]
    Serve {
        /// Port to listen on
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
}

#[derive(Subcommand)]
enum BaselineAction {
    /// Save current database state as a baseline
    Save {
        /// Output baseline file path
        #[arg(long, default_value = "hearken-baseline.db")]
        output: String,
    },
    /// Compare current database against a baseline
    Compare {
        /// Path to baseline database
        baseline: String,
        /// Output format (text or json)
        #[arg(long, default_value = "text")]
        format: String,
    },
}

#[cfg(feature = "jira")]
#[derive(Subcommand)]
enum JiraAction {
    /// Show sync status: pattern counts, ticket counts, connection check
    Status {},
    /// Create new tickets and update existing ones
    Sync {
        #[arg(long)]
        anomalies_only: bool,
        #[arg(long, value_delimiter = ',')]
        tags: Option<Vec<String>>,
        #[arg(long, value_delimiter = ',')]
        exclude_tags: Option<Vec<String>>,
        #[arg(long)]
        min_occurrences: Option<i64>,
        #[arg(long)]
        new_only: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Update existing JIRA tickets only (no new ticket creation)
    Update {
        #[arg(long)]
        anomalies_only: bool,
        #[arg(long, value_delimiter = ',')]
        tags: Option<Vec<String>>,
        #[arg(long, value_delimiter = ',')]
        exclude_tags: Option<Vec<String>>,
        #[arg(long)]
        min_occurrences: Option<i64>,
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> Result<()> {
    let config = load_config();
    let mut cli = Cli::parse();

    // Apply config default for database (CLI override if user changed from clap default)
    if cli.database == "hearken.db" {
        if let Some(ref db) = config.database {
            cli.database = db.clone();
        }
    }

    let storage = Storage::open(&cli.database).context("Failed to open database")?;

    match cli.command {
        Commands::Process {
            files,
            mut threshold,
            mut batch_size,
            group_name,
            #[cfg(feature = "jira")]
            jira_sync,
        } => {
            if threshold == 0.5 {
                if let Some(t) = config.threshold {
                    threshold = t;
                }
            }
            if batch_size == 500000 {
                if let Some(b) = config.batch_size {
                    batch_size = b;
                }
            }
            if !(0.0..=1.0).contains(&threshold) {
                bail!("--threshold must be between 0.0 and 1.0, got {}", threshold);
            }

            // Handle stdin ("-") entries: read stdin into a temp file
            let mut _stdin_tempfiles: Vec<tempfile::NamedTempFile> = Vec::new();
            let mut resolved_files: Vec<String> = Vec::new();
            let mut stdin_paths: HashSet<String> = HashSet::new();

            for f in &files {
                if f == "-" {
                    let mut tmp = tempfile::NamedTempFile::new()
                        .context("Failed to create temp file for stdin")?;
                    std::io::copy(&mut std::io::stdin().lock(), &mut tmp)
                        .context("Failed to read stdin into temp file")?;
                    let path_str = tmp.path().to_string_lossy().to_string();
                    stdin_paths.insert(path_str.clone());
                    resolved_files.push(path_str);
                    _stdin_tempfiles.push(tmp);
                } else {
                    resolved_files.push(f.clone());
                }
            }

            let valid_files = validate_files(&resolved_files);
            if valid_files.is_empty() {
                bail!("No valid files to process. All provided paths were invalid or empty.");
            }
            let mut storage = storage;
            process_files(
                &mut storage,
                &valid_files,
                threshold,
                batch_size,
                group_name.as_deref(),
                &stdin_paths,
            )?;
            #[cfg(feature = "jira")]
            if jira_sync {
                if let Some(jira_toml) = config.jira {
                    let jira_config = hearken_jira::JiraConfig::from_toml_and_env(jira_toml)
                        .context("Failed to load JIRA configuration")?;
                    let db_name = Path::new(&cli.database)
                        .file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or(&cli.database)
                        .to_string();
                    let rt =
                        tokio::runtime::Runtime::new().context("Failed to create Tokio runtime")?;
                    let result = rt.block_on(hearken_jira::sync(
                        jira_config,
                        &storage,
                        &db_name,
                        &hearken_jira::filter::FilterOptions::default(),
                        None,
                        false,
                    ))?;
                    result.print_summary();
                } else {
                    eprintln!("Warning: --jira-sync specified but no [jira] section in config");
                }
            }
        }
        Commands::Search { query } => {
            search_patterns(&storage, &query)?;
        }
        Commands::Stats {} => {
            show_stats(&storage, &cli.database)?;
        }
        Commands::Report {
            mut output,
            mut samples,
            mut top,
            mut filter,
            mut group,
            mut tags_file,
            include_suppressed,
            mut bucket,
        } => {
            if output == "report.html" {
                if let Some(ref o) = config.report.output {
                    output = o.clone();
                }
            }
            if samples == 5 {
                if let Some(s) = config.report.samples {
                    samples = s;
                }
            }
            if top == 500 {
                if let Some(t) = config.report.top {
                    top = t;
                }
            }
            if filter.is_none() {
                filter = config.report.filter.clone();
            }
            if group.is_none() {
                group = config.report.group.clone();
            }
            if tags_file.is_none() {
                tags_file = config.report.tags_file.clone();
            }
            if bucket == "auto" {
                if let Some(ref b) = config.report.bucket {
                    bucket = b.clone();
                }
            }
            generate_report(
                &storage,
                &output,
                samples,
                top,
                filter,
                group,
                tags_file,
                include_suppressed,
                &bucket,
            )?;
        }
        Commands::Export {
            mut format,
            output,
            mut samples,
            mut top,
            mut filter,
            mut group,
            mut tags_file,
            include_suppressed,
            mut bucket,
        } => {
            if format == "json" {
                if let Some(ref f) = config.export.format {
                    format = f.clone();
                }
            }
            if samples == 5 {
                if let Some(s) = config.export.samples {
                    samples = s;
                }
            }
            if top == 500 {
                if let Some(t) = config.export.top {
                    top = t;
                }
            }
            if filter.is_none() {
                filter = config.export.filter.clone();
            }
            if group.is_none() {
                group = config.export.group.clone();
            }
            if tags_file.is_none() {
                tags_file = config.export.tags_file.clone();
            }
            if bucket == "auto" {
                if let Some(ref b) = config.export.bucket {
                    bucket = b.clone();
                }
            }
            export_patterns(
                &storage,
                &format,
                output.as_deref(),
                samples,
                top,
                filter,
                group,
                tags_file.as_deref(),
                include_suppressed,
                &bucket,
            )?;
        }
        Commands::Diff {
            before,
            after,
            format,
        } => {
            drop(storage);
            diff_databases(&before, &after, &format)?;
        }
        Commands::Dedup {
            threshold,
            group,
            format,
            mode,
        } => {
            if !(0.0..=1.0).contains(&threshold) {
                bail!("--threshold must be between 0.0 and 1.0");
            }
            find_duplicates(&storage, threshold, group.as_deref(), &format, &mode)?;
        }
        Commands::Anomalies {
            group,
            top,
            format,
            tags_file,
            include_suppressed,
        } => {
            detect_anomalies(
                &storage,
                group.as_deref(),
                top,
                &format,
                tags_file.as_deref(),
                include_suppressed,
            )?;
        }
        Commands::Check {
            mut max_anomaly_score,
            mut max_new_patterns,
            fail_on_pattern,
            mut baseline,
            mut tags_file,
            group,
            format,
        } => {
            if max_anomaly_score.is_none() {
                max_anomaly_score = config.check.max_anomaly_score;
            }
            if max_new_patterns.is_none() {
                max_new_patterns = config.check.max_new_patterns;
            }
            if baseline.is_none() {
                baseline = config.check.baseline.clone();
            }
            if tags_file.is_none() {
                tags_file = config.check.tags_file.clone();
            }
            let passed = run_check(
                &storage,
                max_anomaly_score,
                max_new_patterns,
                fail_on_pattern,
                baseline,
                tags_file.as_deref(),
                group.as_deref(),
                &format,
            )?;
            if !passed {
                std::process::exit(1);
            }
        }
        Commands::Correlate {
            window,
            top,
            min_count,
            format,
        } => {
            find_correlations(&storage, window, top, min_count, &format)?;
        }
        Commands::Cluster {
            min_shared,
            top,
            group,
            pattern_limit,
            format,
        } => {
            find_clusters(
                &storage,
                min_shared,
                top,
                group.as_deref(),
                pattern_limit,
                &format,
            )?;
        }
        Commands::Watch {
            files,
            mut threshold,
            mut batch_size,
            alert_score,
            #[cfg(feature = "jira")]
            jira_sync,
        } => {
            if threshold == 0.5 {
                if let Some(t) = config.threshold {
                    threshold = t;
                }
            }
            if batch_size == 500000 {
                if let Some(b) = config.batch_size {
                    batch_size = b;
                }
            }
            if !(0.0..=1.0).contains(&threshold) {
                bail!("--threshold must be between 0.0 and 1.0, got {}", threshold);
            }
            let valid_files = validate_files(&files);
            if valid_files.is_empty() {
                bail!("No valid files to watch. All provided paths were invalid or empty.");
            }
            let mut storage = storage;
            watch_files(
                &mut storage,
                &valid_files,
                threshold,
                batch_size,
                alert_score,
            )?;
            #[cfg(feature = "jira")]
            if jira_sync {
                if let Some(jira_toml) = config.jira {
                    let jira_config = hearken_jira::JiraConfig::from_toml_and_env(jira_toml)
                        .context("Failed to load JIRA configuration")?;
                    let db_name = Path::new(&cli.database)
                        .file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or(&cli.database)
                        .to_string();
                    let rt =
                        tokio::runtime::Runtime::new().context("Failed to create Tokio runtime")?;
                    let result = rt.block_on(hearken_jira::sync(
                        jira_config,
                        &storage,
                        &db_name,
                        &hearken_jira::filter::FilterOptions::default(),
                        None,
                        false,
                    ))?;
                    result.print_summary();
                } else {
                    eprintln!("Warning: --jira-sync specified but no [jira] section in config");
                }
            }
        }
        Commands::Baseline { action } => match action {
            BaselineAction::Save { output } => {
                save_baseline(&storage, &output)?;
            }
            BaselineAction::Compare { baseline, format } => {
                drop(storage);
                let diff = compute_diff(&baseline, &cli.database)?;
                format_diff(&diff, &baseline, &cli.database, &format)?;
            }
        },
        #[cfg(feature = "jira")]
        Commands::Jira { action } => {
            let jira_toml = config.jira.ok_or_else(|| {
                anyhow::anyhow!(
                    "No [jira] section found in .hearken.toml. See docs for configuration."
                )
            })?;
            let jira_config = hearken_jira::JiraConfig::from_toml_and_env(jira_toml)
                .context("Failed to load JIRA configuration")?;
            let db_name = Path::new(&cli.database)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or(&cli.database)
                .to_string();

            let rt = tokio::runtime::Runtime::new().context("Failed to create Tokio runtime")?;

            match action {
                JiraAction::Status {} => {
                    rt.block_on(hearken_jira::status(jira_config, &storage, &db_name))?;
                }
                JiraAction::Sync {
                    anomalies_only,
                    tags,
                    exclude_tags,
                    min_occurrences,
                    new_only,
                    dry_run,
                } => {
                    let filter_opts = hearken_jira::filter::FilterOptions {
                        anomalies_only,
                        tags,
                        exclude_tags,
                        min_occurrences,
                        new_only,
                    };
                    let anomaly_ids = if anomalies_only {
                        let anomalies =
                            compute_anomalies(&storage, None, usize::MAX, &HashSet::new())?;
                        Some(anomalies.iter().map(|a| a.id).collect::<HashSet<i64>>())
                    } else {
                        None
                    };
                    let result = rt.block_on(hearken_jira::sync(
                        jira_config,
                        &storage,
                        &db_name,
                        &filter_opts,
                        anomaly_ids.as_ref(),
                        dry_run,
                    ))?;
                    result.print_summary();
                }
                JiraAction::Update {
                    anomalies_only,
                    tags,
                    exclude_tags,
                    min_occurrences,
                    dry_run,
                } => {
                    let filter_opts = hearken_jira::filter::FilterOptions {
                        anomalies_only,
                        tags,
                        exclude_tags,
                        min_occurrences,
                        new_only: false,
                    };
                    let anomaly_ids = if anomalies_only {
                        let anomalies =
                            compute_anomalies(&storage, None, usize::MAX, &HashSet::new())?;
                        Some(anomalies.iter().map(|a| a.id).collect::<HashSet<i64>>())
                    } else {
                        None
                    };
                    let result = rt.block_on(hearken_jira::update(
                        jira_config,
                        &storage,
                        &db_name,
                        &filter_opts,
                        anomaly_ids.as_ref(),
                        dry_run,
                    ))?;
                    result.print_summary();
                }
            }
        }
        #[cfg(feature = "web")]
        Commands::Serve { port } => {
            drop(storage);
            let db_path = cli.database.clone();
            let rt = tokio::runtime::Runtime::new().context("Failed to create Tokio runtime")?;
            rt.block_on(web::run_server(&db_path, port))?;
        }
    }

    Ok(())
}

struct Correlation {
    pattern_a: i64,
    template_a: String,
    group_a: String,
    pattern_b: i64,
    template_b: String,
    group_b: String,
    co_occurrences: usize,
    confidence: f64,
    median_lag_secs: f64,
}

fn find_correlations(
    storage: &Storage,
    window_secs: u64,
    top: usize,
    min_count: i64,
    format: &str,
) -> Result<()> {
    println!(
        "Finding correlations (window={}s, min_count={})...",
        window_secs, min_count
    );

    let occurrences = storage.get_timed_occurrences(min_count)?;
    if occurrences.is_empty() {
        println!("No timestamped occurrences found. Process logs with timestamps first.");
        return Ok(());
    }
    println!(
        "  Analyzing {} timestamped occurrences...",
        occurrences.len()
    );

    // Build pattern metadata lookup: id -> (group, template)
    let mut pattern_meta: HashMap<i64, (String, String)> = HashMap::new();
    for (pid, _, group, tmpl) in &occurrences {
        pattern_meta
            .entry(*pid)
            .or_insert_with(|| (group.clone(), tmpl.clone()));
    }

    // Count total occurrences per pattern (for expected rate calculation)
    let mut pattern_counts: HashMap<i64, usize> = HashMap::new();
    for (pid, _, _, _) in &occurrences {
        *pattern_counts.entry(*pid).or_insert(0) += 1;
    }

    // Sliding window co-occurrence detection (only cross-group pairs)
    let window = window_secs as i64;
    let n = occurrences.len();

    // Cap analysis to avoid O(n²) blowup
    let max_occurrences = 500_000;
    let occ_slice = if n > max_occurrences {
        &occurrences[..max_occurrences]
    } else {
        &occurrences[..]
    };
    let n = occ_slice.len();

    let mut co_occur: HashMap<(i64, i64), Vec<i64>> = HashMap::new();

    for i in 0..n {
        let (pid_i, ts_i, group_i, _) = &occ_slice[i];
        for (pid_k, ts_k, group_k, _) in &occ_slice[i + 1..] {
            if *ts_k > ts_i + window {
                break;
            }
            if group_k == group_i {
                continue;
            }
            if pid_k == pid_i {
                continue;
            }

            let (a, b) = if pid_i < pid_k {
                (*pid_i, *pid_k)
            } else {
                (*pid_k, *pid_i)
            };
            let lag = (ts_k - ts_i).abs();
            let lags = co_occur.entry((a, b)).or_default();
            if lags.len() < 10000 {
                lags.push(lag);
            }
        }
    }

    if co_occur.is_empty() {
        println!(
            "No cross-group correlations found within {}s window.",
            window_secs
        );
        return Ok(());
    }

    // Compute total time span for expected rate
    let time_span = (occ_slice.last().unwrap().1 - occ_slice.first().unwrap().1).max(1) as f64;

    let mut correlations: Vec<Correlation> = Vec::new();
    for ((a, b), lags) in &co_occur {
        let count_a = *pattern_counts.get(a).unwrap_or(&1) as f64;
        let count_b = *pattern_counts.get(b).unwrap_or(&1) as f64;
        // Expected co-occurrences by chance: (count_a * count_b * window) / time_span
        let expected = (count_a * count_b * window as f64) / time_span;
        let observed = lags.len() as f64;
        let confidence = if expected > 0.0 {
            observed / expected
        } else {
            observed
        };

        if lags.len() >= 3 && confidence >= 1.5 {
            let (group_a, tmpl_a) = pattern_meta.get(a).cloned().unwrap_or_default();
            let (group_b, tmpl_b) = pattern_meta.get(b).cloned().unwrap_or_default();
            let mut sorted_lags = lags.clone();
            sorted_lags.sort();
            let median_lag = sorted_lags[sorted_lags.len() / 2] as f64;

            correlations.push(Correlation {
                pattern_a: *a,
                template_a: tmpl_a,
                group_a,
                pattern_b: *b,
                template_b: tmpl_b,
                group_b,
                co_occurrences: lags.len(),
                confidence,
                median_lag_secs: median_lag,
            });
        }
    }

    correlations.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap());
    correlations.truncate(top);

    if correlations.is_empty() {
        println!("No significant correlations found.");
        return Ok(());
    }

    if format == "json" {
        let items: Vec<serde_json::Value> = correlations
            .iter()
            .map(|c| {
                serde_json::json!({
                    "pattern_a": {"id": c.pattern_a, "group": c.group_a, "template": c.template_a},
                    "pattern_b": {"id": c.pattern_b, "group": c.group_b, "template": c.template_b},
                    "co_occurrences": c.co_occurrences,
                    "confidence": format!("{:.1}x", c.confidence),
                    "median_lag_secs": c.median_lag_secs,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        println!(
            "═══ Pattern Correlations (top {}, window={}s) ═══\n",
            correlations.len(),
            window_secs
        );
        for (i, c) in correlations.iter().enumerate() {
            println!(
                "  {}. [{:.1}x confidence, {} co-occurrences, median lag: {:.1}s]",
                i + 1,
                c.confidence,
                c.co_occurrences,
                c.median_lag_secs
            );
            let preview_a = if c.template_a.len() > 80 {
                format!("{}…", &c.template_a[..80])
            } else {
                c.template_a.clone()
            };
            let preview_b = if c.template_b.len() > 80 {
                format!("{}…", &c.template_b[..80])
            } else {
                c.template_b.clone()
            };
            println!("     A [{}]: {}", c.group_a, preview_a);
            println!("     B [{}]: {}\n", c.group_b, preview_b);
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
        let class = if c.is_ascii_digit() {
            'd'
        } else if c.is_ascii_alphabetic() {
            'a'
        } else {
            c
        };
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
    let has_leading_ws = line
        .as_bytes()
        .first()
        .is_some_and(|&b| b == b' ' || b == b'\t');
    let mut fp = String::with_capacity(32);
    for (i, tok) in line.split_whitespace().take(2).enumerate() {
        if i > 0 {
            fp.push('|');
        }
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
        if has_ws || fp.is_empty() {
            continue;
        }
        total_non_ws += 1;
        *freq.entry(fp).or_insert(0) += 1;
    }

    if total_non_ws == 0 {
        return HashSet::new();
    }

    let mut ranked: Vec<(String, usize)> = freq.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));

    let threshold = (total_non_ws as f64 * 0.90) as usize;
    let mut entry_fps = HashSet::new();
    let mut covered: usize = 0;

    for (fp, count) in &ranked {
        entry_fps.insert(fp.clone());
        covered += *count;
        if covered >= threshold {
            break;
        }
    }

    entry_fps
}

/// Checks if a line is an entry start based on auto-detected fingerprints.
fn is_entry_start(line: &str, entry_fps: &HashSet<String>) -> bool {
    if entry_fps.is_empty() {
        return true;
    } // fallback: every line is an entry
    let (has_ws, fp) = line_prefix_fingerprint(line);
    if has_ws {
        return false;
    }
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
fn group_entries<'a>(
    lines: &[(u64, &'a str, u64)],
    entry_fps: &HashSet<String>,
) -> Vec<GroupedEntry<'a>> {
    if lines.is_empty() {
        return vec![];
    }

    let mut entries: Vec<GroupedEntry> = Vec::with_capacity(lines.len());
    let mut i = 0;

    // Skip leading continuation lines (orphaned from previous batch)
    while i < lines.len() && !is_entry_start(lines[i].1, entry_fps) {
        i += 1;
    }
    if i >= lines.len() {
        return entries;
    }

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

/// Send a desktop notification for anomaly alerts.
#[allow(unused_variables)]
fn send_os_notification(title: &str, message: &str) {
    #[cfg(target_os = "macos")]
    {
        let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = std::process::Command::new("osascript")
            .args([
                "-e",
                &format!(
                    "display notification \"{}\" with title \"{}\"",
                    escaped, title
                ),
            ])
            .output();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("notify-send")
            .args([title, message])
            .output();
    }
}

fn watch_files(
    storage: &mut Storage,
    file_paths: &[String],
    threshold: f64,
    batch_size: usize,
    alert_score: Option<f64>,
) -> Result<()> {
    use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    // Phase 1: Initial processing of all files
    println!(
        "[watch] Initial processing of {} file(s)...",
        file_paths.len()
    );
    let stdin_paths = HashSet::new();
    process_files(
        storage,
        file_paths,
        threshold,
        batch_size,
        None,
        &stdin_paths,
    )?;
    println!("[watch] Initial processing complete.\n");

    // Phase 2: Set up file watching
    let (tx, rx) = mpsc::channel();

    let mut watcher = RecommendedWatcher::new(
        move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        },
        Config::default(),
    )
    .context("Failed to create file watcher")?;

    // Watch parent directories of all input files
    let mut watched_dirs: HashSet<String> = HashSet::new();
    for file_path in file_paths {
        let abs_path = std::fs::canonicalize(file_path)
            .with_context(|| format!("Failed to resolve path: {}", file_path))?;
        if let Some(parent) = abs_path.parent() {
            let dir_str = parent.to_string_lossy().to_string();
            if watched_dirs.insert(dir_str.clone()) {
                watcher
                    .watch(parent, RecursiveMode::NonRecursive)
                    .with_context(|| format!("Failed to watch directory: {}", parent.display()))?;
                println!("[watch] Watching directory: {}", parent.display());
            }
        }
    }

    // Build a set of canonical paths for quick lookup
    let canonical_paths: HashMap<String, String> = file_paths
        .iter()
        .filter_map(|fp| {
            std::fs::canonicalize(fp)
                .ok()
                .map(|cp| (cp.to_string_lossy().to_string(), fp.clone()))
        })
        .collect();

    // Set up Ctrl+C handler
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc_flag(&r);

    println!(
        "[watch] Watching {} file(s) for changes. Press Ctrl+C to stop.\n",
        file_paths.len()
    );

    while running.load(Ordering::Relaxed) {
        // Wait for events with a timeout so we can check the running flag
        let event = match rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(e) => e,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // Only care about Modify events
        if !matches!(event.kind, notify::EventKind::Modify(_)) {
            continue;
        }

        // Drain any additional pending events to batch-process
        let mut all_modified: HashSet<String> = HashSet::new();
        for path in &event.paths {
            let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            all_modified.insert(canon.to_string_lossy().to_string());
        }
        while let Ok(extra) = rx.try_recv() {
            if matches!(extra.kind, notify::EventKind::Modify(_)) {
                for path in &extra.paths {
                    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                    all_modified.insert(canon.to_string_lossy().to_string());
                }
            }
        }

        for canon_path in &all_modified {
            let original_path = match canonical_paths.get(canon_path) {
                Some(p) => p,
                None => continue, // Not one of our watched files
            };

            // Get current file size
            let current_size = match std::fs::metadata(original_path) {
                Ok(m) => m.len(),
                Err(_) => continue,
            };

            // Get last processed position from DB
            let groups = group_files(std::slice::from_ref(original_path), None, &HashSet::new());
            let (group_name, _) = match groups.iter().next() {
                Some(g) => g,
                None => continue,
            };
            let file_group_id = storage
                .get_or_create_file_group(group_name)
                .context("Failed to get file group")?;
            let source = storage.get_or_create_log_source(original_path, file_group_id)?;
            let last_pos = source.last_processed_position;

            if current_size == last_pos {
                continue; // No new data
            }

            // Handle log rotation (file truncated)
            if current_size < last_pos {
                println!(
                    "[watch] File truncated (rotation detected): {} — resetting position to 0",
                    original_path
                );
                storage.conn.execute(
                    "UPDATE log_sources SET last_processed_position = 0 WHERE id = ?",
                    rusqlite::params![source.id.unwrap()],
                )?;
            }

            let new_bytes = if current_size > last_pos {
                current_size - last_pos
            } else {
                current_size // After rotation reset
            };

            // Process new content incrementally
            let mut parser = LogParser::new(15, threshold);
            let mut pattern_id_cache: HashMap<usize, i64> = HashMap::new();
            let mut occurrence_counts: HashMap<usize, u64> = HashMap::new();

            // Seed parser from existing patterns
            {
                let mut stmt = storage
                    .conn
                    .prepare("SELECT id, template FROM patterns WHERE file_group_id = ?")?;
                let rows = stmt.query_map(rusqlite::params![file_group_id], |row| {
                    Ok(LogTemplate {
                        id: Some(row.get(0)?),
                        template: row.get(1)?,
                    })
                })?;
                for row in rows {
                    let template = row?;
                    let db_id = template.id.unwrap();
                    let idx = parser.templates.len();
                    parser.add_template(template);
                    pattern_id_cache.insert(idx, db_id);
                }
            }

            let pre_count: i64 = storage
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM occurrences WHERE log_source_id = ?",
                    rusqlite::params![source.id.unwrap()],
                    |row| row.get(0),
                )
                .unwrap_or(0);

            process_file(
                storage,
                original_path,
                file_group_id,
                &mut parser,
                &mut pattern_id_cache,
                &mut occurrence_counts,
                batch_size,
            )?;

            // Write occurrence counts
            {
                let tx = storage.conn.transaction()?;
                {
                    let mut update_stmt = tx.prepare(
                        "UPDATE patterns SET occurrence_count = occurrence_count + ? WHERE id = ?",
                    )?;
                    for (template_idx, count) in &occurrence_counts {
                        if let Some(&pattern_id) = pattern_id_cache.get(template_idx) {
                            update_stmt.execute(rusqlite::params![*count as i64, pattern_id])?;
                        }
                    }
                }
                tx.commit()?;
            }

            let post_count: i64 = storage
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM occurrences WHERE log_source_id = ?",
                    rusqlite::params![source.id.unwrap()],
                    |row| row.get(0),
                )
                .unwrap_or(0);

            let new_entries = post_count - pre_count;
            println!(
                "[watch] File modified: {} (+{} bytes, {} new entries)",
                original_path, new_bytes, new_entries
            );

            // Check anomaly alerts if --alert-score is set
            if let Some(alert_threshold) = alert_score {
                let anomalies = compute_anomalies(storage, None, 10, &HashSet::new())?;
                for a in &anomalies {
                    if a.score >= alert_threshold {
                        let msg = format!(
                            "Anomaly score {:.1} in group '{}': {}",
                            a.score,
                            a.group,
                            if a.template.len() > 80 {
                                &a.template[..80]
                            } else {
                                &a.template
                            }
                        );
                        println!("[watch] ALERT: {}", msg);
                        send_os_notification("Hearken Alert", &msg);
                    }
                }
            }
        }
    }

    println!("\n[watch] Stopped.");
    Ok(())
}

/// Set a flag to false on Ctrl+C (SIGINT).
#[cfg(unix)]
fn ctrlc_flag(flag: &Arc<std::sync::atomic::AtomicBool>) {
    let f = flag.clone();
    let _ = libc_sigint_handler(f);
}

#[cfg(not(unix))]
fn ctrlc_flag(_flag: &Arc<std::sync::atomic::AtomicBool>) {
    // On Windows, Ctrl+C is handled by the default handler which terminates the process.
}

/// Minimal SIGINT handler using raw libc — avoids adding a ctrlc crate dependency.
#[cfg(unix)]
fn libc_sigint_handler(flag: Arc<std::sync::atomic::AtomicBool>) -> Result<()> {
    use std::sync::atomic::Ordering;
    static mut RUNNING_FLAG: Option<*const std::sync::atomic::AtomicBool> = None;
    extern "C" fn handler(_: libc::c_int) {
        unsafe {
            if let Some(ptr) = RUNNING_FLAG {
                (*ptr).store(false, Ordering::Relaxed);
            }
        }
        let _ = std::io::Write::write_all(&mut std::io::stderr(), b"\n");
    }
    unsafe {
        let ptr = Arc::into_raw(flag);
        RUNNING_FLAG = Some(ptr);
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
    }
    Ok(())
}

fn validate_files(files: &[String]) -> Vec<String> {
    let mut valid = Vec::with_capacity(files.len());
    for file_path in files {
        let path = Path::new(file_path);
        if !path.exists() {
            eprintln!("Warning: skipping '{}' — file does not exist", file_path);
            continue;
        }
        match std::fs::metadata(path) {
            Ok(meta) => {
                if !meta.is_file() {
                    eprintln!("Warning: skipping '{}' — not a regular file", file_path);
                    continue;
                }
                if meta.len() == 0 {
                    eprintln!("Warning: skipping '{}' — file is empty", file_path);
                    continue;
                }
            }
            Err(e) => {
                eprintln!(
                    "Warning: skipping '{}' — cannot read metadata: {}",
                    file_path, e
                );
                continue;
            }
        }
        valid.push(file_path.clone());
    }
    valid
}

fn process_files(
    storage: &mut Storage,
    file_paths: &[String],
    threshold: f64,
    batch_size: usize,
    group_name_override: Option<&str>,
    stdin_paths: &HashSet<String>,
) -> Result<()> {
    let groups = group_files(file_paths, group_name_override, stdin_paths);
    println!(
        "Found {} file group(s) from {} file(s):",
        groups.len(),
        file_paths.len()
    );
    for (group_name, files) in &groups {
        println!("  {} ({} file(s))", group_name, files.len());
        for f in files {
            println!("    - {}", f);
        }
    }
    println!();

    // Pre-create file groups so IDs are available for parallel processing
    let group_ids: Vec<(String, Vec<String>, i64)> = groups
        .iter()
        .map(|(name, files)| {
            let id = storage
                .get_or_create_file_group(name)
                .expect("Failed to create file group");
            (name.clone(), files.clone(), id)
        })
        .collect();

    let db_path = storage.db_path().to_string();

    if group_ids.len() > 1 {
        // Process groups in parallel, each with its own DB connection
        println!("Processing {} groups in parallel...\n", group_ids.len());
        let errors: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = group_ids
                .iter()
                .map(|(group_name, files, file_group_id)| {
                    let db_path = db_path.clone();
                    let group_name = group_name.clone();
                    let files = files.clone();
                    let file_group_id = *file_group_id;
                    s.spawn(move || -> Result<()> {
                        let mut thread_storage =
                            Storage::open(&db_path).context("Failed to open database in thread")?;
                        process_group(
                            &mut thread_storage,
                            &group_name,
                            &files,
                            file_group_id,
                            threshold,
                            batch_size,
                        )
                    })
                })
                .collect();

            handles
                .into_iter()
                .filter_map(|h| match h.join() {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(format!("{:#}", e)),
                    Err(_) => Some("thread panicked".to_string()),
                })
                .collect()
        });

        if !errors.is_empty() {
            for err in &errors {
                eprintln!("Error: {}", err);
            }
            bail!("{} group(s) failed to process", errors.len());
        }
    } else {
        // Single group — process directly (no thread overhead)
        for (group_name, files, file_group_id) in &group_ids {
            process_group(
                storage,
                group_name,
                files,
                *file_group_id,
                threshold,
                batch_size,
            )?;
        }
    }

    // Rebuild FTS index once at the end
    println!("Rebuilding search index...");
    {
        let mut fts_storage = Storage::open(&db_path).context("Failed to reopen database")?;
        let tx = fts_storage.conn.transaction()?;
        tx.execute("DELETE FROM patterns_fts", [])?;
        tx.execute(
            "INSERT INTO patterns_fts (pattern_id, template) SELECT id, template FROM patterns",
            [],
        )?;
        tx.commit()?;
    }

    println!("Done.");
    Ok(())
}

fn process_group(
    storage: &mut Storage,
    group_name: &str,
    files: &[String],
    file_group_id: i64,
    threshold: f64,
    batch_size: usize,
) -> Result<()> {
    println!("═══ Processing group: {} ═══", group_name);

    let mut parser = LogParser::new(15, threshold);
    let mut pattern_id_cache: HashMap<usize, i64> = HashMap::new();
    let mut occurrence_counts: HashMap<usize, u64> = HashMap::new();

    // Seed parser from existing patterns for this group
    {
        let mut stmt = storage
            .conn
            .prepare("SELECT id, template FROM patterns WHERE file_group_id = ?")?;
        let rows = stmt.query_map(rusqlite::params![file_group_id], |row| {
            Ok(LogTemplate {
                id: Some(row.get(0)?),
                template: row.get(1)?,
            })
        })?;
        for row in rows {
            let template = row?;
            let db_id = template.id.unwrap();
            let idx = parser.templates.len();
            parser.add_template(template);
            pattern_id_cache.insert(idx, db_id);
        }
    }

    for file_path in files {
        process_file(
            storage,
            file_path,
            file_group_id,
            &mut parser,
            &mut pattern_id_cache,
            &mut occurrence_counts,
            batch_size,
        )?;
    }

    // Write occurrence counts
    println!("Writing pattern counts for group '{}'...", group_name);
    {
        let tx = storage.conn.transaction()?;
        {
            let mut update_stmt =
                tx.prepare("UPDATE patterns SET occurrence_count = ? WHERE id = ?")?;
            for (template_idx, count) in &occurrence_counts {
                if let Some(&pattern_id) = pattern_id_cache.get(template_idx) {
                    update_stmt.execute(rusqlite::params![*count as i64, pattern_id])?;
                }
            }
        }
        tx.commit()?;
    }

    println!(
        "Group '{}': {} patterns discovered.\n",
        group_name,
        parser.templates.len()
    );
    Ok(())
}

fn process_file(
    storage: &mut Storage,
    file_path: &str,
    file_group_id: i64,
    parser: &mut LogParser,
    pattern_id_cache: &mut HashMap<usize, i64>,
    occurrence_counts: &mut HashMap<usize, u64>,
    batch_size: usize,
) -> Result<()> {
    println!("Processing: {}", file_path);

    let source = storage.get_or_create_log_source(file_path, file_group_id)?;
    let reader = LogReader::new(file_path)?;
    let mut current_pos = source.last_processed_position;

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
                println!(
                    "No dominant line structure detected — treating each line as a separate entry."
                );
            } else {
                let entry_count = lines_with_pos
                    .iter()
                    .filter(|(_, line, _)| is_entry_start(line, &entry_fingerprints))
                    .count();
                let cont = lines_with_pos.len() - entry_count;
                println!(
                    "Auto-detected entry structure ({} pattern(s)): {}/{} lines are entries, {} are continuations ({:.1}%)",
                    entry_fingerprints.len(),
                    entry_count,
                    lines_with_pos.len(),
                    cont,
                    cont as f64 / lines_with_pos.len() as f64 * 100.0
                );
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

        println!(
            "Progress: {:.2}% (Position: {}, Lines: {}, Entries: {})",
            progress,
            start_pos,
            total_lines,
            entries.len()
        );

        // 1. Parallel Transformation Phase (CPU HEAVY)
        // Tokenize primary line + continuation lines into a single token stream.
        let t_parallel = Instant::now();
        let parallel_results: Vec<(Vec<&str>, Option<usize>)> = entries
            .par_iter()
            .map(|entry| {
                let mut tokens: Vec<&str> = tokenize(entry.primary_line);
                for cont_line in &entry.continuation_lines {
                    if tokens.len() >= 1024 {
                        break;
                    }
                    tokens.push("\n");
                    tokens.extend(tokenize(cont_line));
                }
                tokens.truncate(1024);
                let matched = parser.find_match(&tokens);
                (tokens, matched)
            })
            .collect();
        let parallel_ms = t_parallel.elapsed().as_millis();

        // 2. Sequential Discovery Phase — pattern matching only, no DB writes
        let t_sequential = Instant::now();
        let mut new_patterns: Vec<(usize, String)> = Vec::new();
        let mut evolved_patterns: Vec<(i64, String)> = Vec::new();
        let mut occurrence_buffer: Vec<(usize, usize)> = Vec::with_capacity(entries.len());

        for (entry_idx, (tokens, maybe_match)) in parallel_results.iter().enumerate() {
            let template_idx = parser.parse_tokens(tokens, *maybe_match);
            if template_idx == usize::MAX {
                continue;
            }

            // Count occurrence in-memory
            *occurrence_counts.entry(template_idx).or_insert(0) += 1;
            occurrence_buffer.push((entry_idx, template_idx));

            let template = &mut parser.templates[template_idx];

            // Track new/evolved patterns for DB persistence
            let mut p_id = template.id;
            let evolved = p_id.is_some_and(|id| id < 0);
            if evolved {
                p_id = p_id.map(|id| -id);
            }

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
            let changes = tx.execute(
                "INSERT OR IGNORE INTO patterns (file_group_id, template) VALUES (?, ?)",
                rusqlite::params![file_group_id, template_str],
            )?;
            let id: i64 = if changes > 0 {
                tx.last_insert_rowid()
            } else {
                tx.query_row(
                    "SELECT id FROM patterns WHERE file_group_id = ? AND template = ?",
                    rusqlite::params![file_group_id, template_str],
                    |row| row.get(0),
                )?
            };
            pattern_id_cache.insert(*template_idx, id);
            parser.templates[*template_idx].id = Some(id);
        }

        if !evolved_patterns.is_empty() {
            let mut update_stmt =
                tx.prepare_cached("UPDATE patterns SET template = ? WHERE id = ?")?;
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
                "INSERT INTO occurrences (log_source_id, pattern_id, byte_offset, entry_timestamp, variables) VALUES (?, ?, ?, ?, ?)"
            )?;
            for &(entry_idx, template_idx) in &occurrence_buffer {
                if let Some(&pattern_id) = pattern_id_cache.get(&template_idx) {
                    let pos = entries[entry_idx].start_pos as i64;
                    let entry_ts = extract_timestamp(entries[entry_idx].primary_line);
                    let tmpl_tokens = &parser.templates[template_idx].tokens;
                    let entry_tokens = &parallel_results[entry_idx].0;
                    let variables: String = tmpl_tokens
                        .iter()
                        .zip(entry_tokens.iter())
                        .filter_map(|(t, e)| if *t == "<*>" { Some(*e) } else { None })
                        .collect::<Vec<_>>()
                        .join("\t");
                    occ_stmt.execute(rusqlite::params![
                        source_id, pattern_id, pos, entry_ts, variables
                    ])?;
                }
            }
        }

        tx.commit()?;
        let db_ms = t_db.elapsed().as_millis();
        current_pos = last_entry_pos;

        println!(
            "  Batch: parallel={}ms, sequential={}ms, db={}ms, templates={}",
            parallel_ms,
            sequential_ms,
            db_ms,
            parser.templates.len()
        );
    }

    println!("  {} lines processed.", total_lines);
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

fn show_stats(storage: &Storage, db_path: &str) -> Result<()> {
    let pattern_count: i64 = storage
        .conn
        .query_row("SELECT COUNT(*) FROM patterns", [], |r| r.get(0))
        .unwrap_or(0);
    let occurrence_count: i64 = storage
        .conn
        .query_row(
            "SELECT COALESCE(SUM(occurrence_count), 0) FROM patterns",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let source_count: i64 = storage
        .conn
        .query_row("SELECT COUNT(*) FROM log_sources", [], |r| r.get(0))
        .unwrap_or(0);

    println!("═══ Hearken Database Statistics ═══\n");
    println!("Patterns:     {}", pattern_count);
    println!("Occurrences:  {}", occurrence_count);
    println!("Source files:  {}", source_count);

    // File groups with pattern counts
    let mut stmt = storage.conn.prepare(
        "SELECT fg.name, COUNT(p.id), COALESCE(SUM(p.occurrence_count), 0)
         FROM file_groups fg
         LEFT JOIN patterns p ON p.file_group_id = fg.id
         GROUP BY fg.id
         ORDER BY fg.name",
    )?;
    let groups: Vec<(String, i64, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .filter_map(|r| r.ok())
        .collect();

    if !groups.is_empty() {
        println!("\nFile groups:   {}", groups.len());
        for (name, pcount, ocount) in &groups {
            println!(
                "  {:<30} {:>6} patterns, {:>10} occurrences",
                name, pcount, ocount
            );
        }
    }

    // Source files with progress
    let mut stmt = storage.conn.prepare(
        "SELECT ls.file_path, ls.last_processed_position, fg.name
         FROM log_sources ls
         JOIN file_groups fg ON ls.file_group_id = fg.id
         ORDER BY fg.name, ls.file_path",
    )?;
    let sources: Vec<(String, i64, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .filter_map(|r| r.ok())
        .collect();

    if !sources.is_empty() {
        println!("\nSource files:");
        for (path, pos, group) in &sources {
            let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let progress = if file_size > 0 {
                format!("{:.1}%", (*pos as f64 / file_size as f64) * 100.0)
            } else {
                "N/A".to_string()
            };
            println!(
                "  [{}] {} (processed: {} bytes, {})",
                group, path, pos, progress
            );
        }
    }

    // Database file sizes
    let db_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    let wal_path = format!("{}-wal", db_path);
    let wal_size = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    let shm_path = format!("{}-shm", db_path);
    let shm_size = std::fs::metadata(&shm_path).map(|m| m.len()).unwrap_or(0);

    println!("\nDatabase:");
    println!("  DB file:     {} ({})", db_path, format_size(db_size));
    if wal_size > 0 {
        println!("  WAL file:    {} ({})", wal_path, format_size(wal_size));
    }
    if shm_size > 0 {
        println!("  SHM file:    {} ({})", shm_path, format_size(shm_size));
    }
    println!(
        "  Total:       {}",
        format_size(db_size + wal_size + shm_size)
    );

    Ok(())
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

fn load_suppressed_ids(tags_file: Option<&str>) -> HashSet<i64> {
    let mut suppressed = HashSet::new();
    if let Some(tf) = tags_file {
        if let Ok(content) = std::fs::read_to_string(tf) {
            if let Ok(tags) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(obj) = tags.as_object() {
                    for (id_str, tag_list) in obj {
                        if let Some(arr) = tag_list.as_array() {
                            let has_suppress = arr.iter().any(|t| {
                                t.as_str()
                                    .map(|s| s == "suppress" || s == "ignore")
                                    .unwrap_or(false)
                            });
                            if has_suppress {
                                if let Ok(id) = id_str.parse::<i64>() {
                                    suppressed.insert(id);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    suppressed
}

fn generate_report(
    storage: &Storage,
    output_path: &str,
    samples_per_pattern: usize,
    top_n: usize,
    filter: Option<Vec<String>>,
    group_filter: Option<Vec<String>>,
    tags_file: Option<String>,
    include_suppressed: bool,
    bucket: &str,
) -> Result<()> {
    let start = Instant::now();
    println!("Generating report...");

    let (pattern_count, total_occurrences, sources, file_groups) = storage
        .get_report_summary()
        .context("Failed to query report summary")?;

    if pattern_count == 0 {
        anyhow::bail!("No patterns found in database. Process a log file first.");
    }

    let patterns = storage
        .get_all_patterns_ranked(top_n, filter.as_deref(), group_filter.as_deref())
        .context("Failed to query patterns")?;

    if let Some(ref f) = filter {
        println!("  Filter: patterns containing any of {:?}", f);
    }
    if let Some(ref g) = group_filter {
        println!("  Group filter: {:?}", g);
    }

    println!("  Fetching samples for {} patterns...", patterns.len());

    // Fetch trend data (per-source counts) for all patterns
    let pattern_ids: Vec<i64> = patterns.iter().map(|(id, _, _, _)| *id).collect();
    let distribution = storage.get_pattern_trends(&pattern_ids).unwrap_or_default();

    // Check for timestamps and fetch time-series data if available
    let has_timestamps = storage.has_timestamps().unwrap_or(false);
    let time_series = if has_timestamps {
        storage
            .get_pattern_time_series(&pattern_ids, bucket)
            .unwrap_or_default()
    } else {
        HashMap::new()
    };

    let mut pattern_data = Vec::with_capacity(patterns.len());
    for (id, template, count, group_name) in &patterns {
        let raw_samples = storage
            .get_pattern_samples(*id, samples_per_pattern)
            .unwrap_or_default();
        let samples: Vec<serde_json::Value> = raw_samples
            .iter()
            .map(|(vars, source_path)| {
                serde_json::json!({
                    "text": reconstruct_entry(template, vars),
                    "source": Path::new(source_path).file_name()
                        .and_then(|f| f.to_str()).unwrap_or(source_path),
                })
            })
            .collect();
        let dist = distribution
            .get(id)
            .map(|t| {
                t.iter()
                    .map(|(name, cnt)| serde_json::json!({"source": name, "count": cnt}))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let trend = if has_timestamps {
            time_series
                .get(id)
                .map(|t| {
                    t.iter()
                        .map(|(b, cnt)| serde_json::json!({"bucket": b, "count": cnt}))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            dist.clone()
        };

        pattern_data.push(serde_json::json!({
            "id": id,
            "template": template,
            "count": count,
            "group": group_name,
            "samples": samples,
            "trend": trend,
            "distribution": dist,
        }));
    }

    // Mark suppressed patterns and optionally filter them out
    let suppressed_ids = load_suppressed_ids(tags_file.as_deref());
    if !suppressed_ids.is_empty() {
        for p in pattern_data.iter_mut() {
            if let Some(id) = p.get("id").and_then(|v| v.as_i64()) {
                if suppressed_ids.contains(&id) {
                    p.as_object_mut()
                        .unwrap()
                        .insert("suppressed".to_string(), serde_json::json!(true));
                }
            }
        }
        if !include_suppressed {
            pattern_data.retain(|p| {
                !p.get("suppressed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            });
        }
    }

    let now: String = std::process::Command::new("date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let command = std::env::args().collect::<Vec<_>>().join(" ");
    // Load existing tags from sidecar file
    let tags: serde_json::Value = if let Some(ref tf) = tags_file {
        if Path::new(tf).exists() {
            let content = std::fs::read_to_string(tf)
                .with_context(|| format!("Failed to read tags file: {}", tf))?;
            serde_json::from_str(&content)
                .with_context(|| format!("Failed to parse tags file: {}", tf))?
        } else {
            serde_json::json!({})
        }
    } else {
        serde_json::json!({})
    };

    let report_json = serde_json::json!({
        "pattern_count": pattern_count,
        "total_occurrences": total_occurrences,
        "has_timestamps": has_timestamps,
        "sources": sources,
        "file_groups": file_groups.iter().map(|(name, count)| {
            serde_json::json!({"name": name, "pattern_count": count})
        }).collect::<Vec<_>>(),
        "generated_at": now,
        "command": command,
        "params": {
            "top": top_n,
            "samples": samples_per_pattern,
            "filter": filter,
            "group": group_filter,
            "output": output_path,
            "tags_file": tags_file,
        },
        "patterns": pattern_data,
        "tags": tags,
    });

    let json_str =
        serde_json::to_string(&report_json).context("Failed to serialize report data")?;

    // Escape sequences that would break the <script> tag when embedded in HTML.
    // "</script>" or "</Script>" etc. inside a string literal terminates the tag.
    let json_str = json_str.replace("</", "<\\/");

    let template = include_str!("report_template.html");
    let html = template.replace("/*__REPORT_DATA__*/", &json_str);
    let file_size_bytes = html.len();

    // Inject the file size into the already-built HTML via a data attribute on the body
    let html = html.replace(
        "<body>",
        &format!("<body data-file-size=\"{}\">", file_size_bytes),
    );

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

/// Reconstructs a log entry by replacing <*> placeholders in a template with variable values.
fn reconstruct_entry(template: &str, variables: &str) -> String {
    let mut var_iter = variables.split('\t');
    let mut rebuilt = String::with_capacity(template.len() + variables.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' && chars.peek() == Some(&'*') {
            chars.next();
            if chars.peek() == Some(&'>') {
                chars.next();
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
}

fn export_patterns(
    storage: &Storage,
    format: &str,
    output_path: Option<&str>,
    samples_per_pattern: usize,
    top_n: usize,
    filter: Option<Vec<String>>,
    group_filter: Option<Vec<String>>,
    tags_file: Option<&str>,
    include_suppressed: bool,
    _bucket: &str,
) -> Result<()> {
    if format != "json" && format != "csv" {
        bail!("--format must be 'json' or 'csv', got '{}'", format);
    }

    let patterns = storage
        .get_all_patterns_ranked(top_n, filter.as_deref(), group_filter.as_deref())
        .context("Failed to query patterns")?;

    // Filter out suppressed patterns unless include_suppressed is set
    let suppressed_ids = load_suppressed_ids(tags_file);
    let patterns: Vec<_> = if !include_suppressed && !suppressed_ids.is_empty() {
        patterns
            .into_iter()
            .filter(|(id, _, _, _)| !suppressed_ids.contains(id))
            .collect()
    } else {
        patterns
    };

    let content = if format == "json" {
        let mut pattern_data = Vec::with_capacity(patterns.len());
        for (id, template, count, group_name) in &patterns {
            let raw_samples = storage
                .get_pattern_samples(*id, samples_per_pattern)
                .unwrap_or_default();
            let samples: Vec<serde_json::Value> = raw_samples
                .iter()
                .map(|(vars, source_path)| {
                    serde_json::json!({
                        "text": reconstruct_entry(template, vars),
                        "source": Path::new(source_path).file_name()
                            .and_then(|f| f.to_str()).unwrap_or(source_path),
                    })
                })
                .collect();
            pattern_data.push(serde_json::json!({
                "id": id,
                "group": group_name,
                "template": template,
                "occurrence_count": count,
                "samples": samples,
            }));
        }
        serde_json::to_string_pretty(&pattern_data).context("Failed to serialize JSON")?
    } else {
        let mut csv = String::new();
        // Header
        let sample_headers: Vec<String> = (1..=samples_per_pattern)
            .map(|i| format!("sample_{}", i))
            .collect();
        csv.push_str(&format!(
            "id,group,template,occurrence_count,{}\n",
            sample_headers.join(",")
        ));
        for (id, template, count, group_name) in &patterns {
            let raw_samples = storage
                .get_pattern_samples(*id, samples_per_pattern)
                .unwrap_or_default();
            let samples: Vec<String> = raw_samples
                .iter()
                .map(|(vars, _)| reconstruct_entry(template, vars))
                .collect();
            csv.push_str(&format!(
                "{},{},{},{}",
                id,
                csv_escape(group_name),
                csv_escape(template),
                count
            ));
            for i in 0..samples_per_pattern {
                csv.push(',');
                if let Some(s) = samples.get(i) {
                    csv.push_str(&csv_escape(s));
                }
            }
            csv.push('\n');
        }
        csv
    };

    match output_path {
        Some(path) => {
            std::fs::write(path, &content)
                .with_context(|| format!("Failed to write export to {}", path))?;
            eprintln!(
                "Exported {} patterns to {} ({})",
                patterns.len(),
                path,
                format.to_uppercase()
            );
        }
        None => {
            print!("{}", content);
        }
    }

    Ok(())
}

fn save_baseline(storage: &Storage, output_path: &str) -> Result<()> {
    let start = Instant::now();
    println!("Saving baseline to {}...", output_path);

    let mut baseline = Storage::open(output_path).context("Failed to create baseline database")?;

    // Copy file_groups
    let mut stmt = storage.conn.prepare("SELECT id, name FROM file_groups")?;
    let groups: Vec<(i64, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    let tx = baseline.conn.transaction()?;
    for (id, name) in &groups {
        tx.execute(
            "INSERT INTO file_groups (id, name) VALUES (?, ?)",
            rusqlite::params![id, name],
        )?;
    }

    // Copy patterns (with occurrence counts)
    let mut stmt = storage.conn.prepare(
        "SELECT id, file_group_id, template, occurrence_count FROM patterns WHERE occurrence_count > 0"
    )?;
    let patterns: Vec<(i64, i64, String, i64)> = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    {
        let mut insert_stmt = tx.prepare(
            "INSERT INTO patterns (id, file_group_id, template, occurrence_count) VALUES (?, ?, ?, ?)"
        )?;
        for (id, group_id, template, count) in &patterns {
            insert_stmt.execute(rusqlite::params![id, group_id, template, count])?;
        }
    }

    // Also populate the FTS table
    {
        let mut fts_stmt =
            tx.prepare("INSERT INTO patterns_fts (pattern_id, template) VALUES (?, ?)")?;
        for (id, _, template, _) in &patterns {
            fts_stmt.execute(rusqlite::params![id, template])?;
        }
    }

    tx.commit()?;

    let baseline_size = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    let elapsed = start.elapsed();
    println!("Baseline saved in {:.1}s", elapsed.as_secs_f64());
    println!("  Groups: {}", groups.len());
    println!("  Patterns: {}", patterns.len());
    println!("  Size: {}", format_size(baseline_size));

    Ok(())
}

struct DiffResult {
    new_patterns: Vec<(String, String, i64)>,
    resolved_patterns: Vec<(String, String, i64)>,
    changed_patterns: Vec<(String, String, i64, i64)>,
}

fn compute_diff(before_path: &str, after_path: &str) -> Result<DiffResult> {
    let conn = rusqlite::Connection::open(after_path)
        .with_context(|| format!("Failed to open 'after' database: {}", after_path))?;
    conn.execute(
        "ATTACH DATABASE ? AS before_db",
        rusqlite::params![before_path],
    )
    .with_context(|| format!("Failed to attach 'before' database: {}", before_path))?;

    let mut new_patterns: Vec<(String, String, i64)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT fg.name, p.template, p.occurrence_count
             FROM patterns p
             JOIN file_groups fg ON p.file_group_id = fg.id
             WHERE NOT EXISTS (
                 SELECT 1 FROM before_db.patterns bp
                 JOIN before_db.file_groups bfg ON bp.file_group_id = bfg.id
                 WHERE bfg.name = fg.name AND bp.template = p.template
             )
             AND p.occurrence_count > 0
             ORDER BY p.occurrence_count DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            new_patterns.push(row?);
        }
    }

    let mut resolved_patterns: Vec<(String, String, i64)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT bfg.name, bp.template, bp.occurrence_count
             FROM before_db.patterns bp
             JOIN before_db.file_groups bfg ON bp.file_group_id = bfg.id
             WHERE NOT EXISTS (
                 SELECT 1 FROM patterns p
                 JOIN file_groups fg ON p.file_group_id = fg.id
                 WHERE fg.name = bfg.name AND p.template = bp.template
             )
             AND bp.occurrence_count > 0
             ORDER BY bp.occurrence_count DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            resolved_patterns.push(row?);
        }
    }

    let mut changed_patterns: Vec<(String, String, i64, i64)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT fg.name, p.template, bp.occurrence_count, p.occurrence_count
             FROM patterns p
             JOIN file_groups fg ON p.file_group_id = fg.id
             JOIN before_db.patterns bp ON bp.template = p.template
             JOIN before_db.file_groups bfg ON bp.file_group_id = bfg.id AND bfg.name = fg.name
             WHERE p.occurrence_count > 0 AND bp.occurrence_count > 0
               AND (p.occurrence_count > bp.occurrence_count * 2 OR p.occurrence_count * 2 < bp.occurrence_count)
             ORDER BY ABS(p.occurrence_count - bp.occurrence_count) DESC"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            changed_patterns.push(row?);
        }
    }

    conn.execute("DETACH DATABASE before_db", [])?;

    Ok(DiffResult {
        new_patterns,
        resolved_patterns,
        changed_patterns,
    })
}

fn format_diff(diff: &DiffResult, before_path: &str, after_path: &str, format: &str) -> Result<()> {
    if format != "text" && format != "json" {
        bail!("--format must be 'text' or 'json', got '{}'", format);
    }

    let DiffResult {
        new_patterns,
        resolved_patterns,
        changed_patterns,
    } = diff;

    if format == "json" {
        let output = serde_json::json!({
            "before": before_path,
            "after": after_path,
            "new_patterns": new_patterns.iter().map(|(g, t, c)| {
                serde_json::json!({"group": g, "template": t, "count": c})
            }).collect::<Vec<_>>(),
            "resolved_patterns": resolved_patterns.iter().map(|(g, t, c)| {
                serde_json::json!({"group": g, "template": t, "count": c})
            }).collect::<Vec<_>>(),
            "changed_patterns": changed_patterns.iter().map(|(g, t, before, after)| {
                serde_json::json!({
                    "group": g, "template": t,
                    "before_count": before, "after_count": after,
                    "change": format!("{:+.0}%", (*after as f64 / *before as f64 - 1.0) * 100.0),
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("═══ Hearken Diff: {} → {} ═══\n", before_path, after_path);

        fn truncate_template(t: &str, max: usize) -> String {
            let first_line = t.lines().next().unwrap_or(t);
            if first_line.len() > max {
                format!("{}…", &first_line[..max])
            } else {
                first_line.to_string()
            }
        }

        if new_patterns.is_empty() {
            println!("✅ New patterns: none");
        } else {
            println!("🆕 New patterns: {}", new_patterns.len());
            for (group, template, count) in new_patterns {
                println!(
                    "  [{:<20}] {:>8}x  {}",
                    group,
                    count,
                    truncate_template(template, 100)
                );
            }
        }
        println!();

        if resolved_patterns.is_empty() {
            println!("✅ Resolved patterns: none");
        } else {
            println!("🗑️  Resolved patterns: {}", resolved_patterns.len());
            for (group, template, count) in resolved_patterns {
                println!(
                    "  [{:<20}] {:>8}x  {}",
                    group,
                    count,
                    truncate_template(template, 100)
                );
            }
        }
        println!();

        if changed_patterns.is_empty() {
            println!("✅ Changed patterns (>2x): none");
        } else {
            println!("📈 Changed patterns (>2x): {}", changed_patterns.len());
            for (group, template, before, after) in changed_patterns {
                let pct = (*after as f64 / *before as f64 - 1.0) * 100.0;
                let arrow = if pct > 0.0 { "↑" } else { "↓" };
                println!(
                    "  [{:<20}] {} → {} ({}{:.0}%)  {}",
                    group,
                    before,
                    after,
                    arrow,
                    pct.abs(),
                    truncate_template(template, 80)
                );
            }
        }

        println!(
            "\nSummary: {} new, {} resolved, {} changed (>2x)",
            new_patterns.len(),
            resolved_patterns.len(),
            changed_patterns.len()
        );
    }

    Ok(())
}

fn diff_databases(before_path: &str, after_path: &str, format: &str) -> Result<()> {
    let diff = compute_diff(before_path, after_path)?;
    format_diff(&diff, before_path, after_path, format)
}

fn find_duplicates(
    storage: &Storage,
    threshold: f64,
    group_filter: Option<&str>,
    format: &str,
    mode: &str,
) -> Result<()> {
    use hearken_core::tokenize;
    use hearken_ml::semantic_similarity;

    let patterns = storage.get_patterns_for_dedup(group_filter)?;
    if patterns.is_empty() {
        println!("No patterns found in database.");
        return Ok(());
    }

    // Group patterns by group_name for pairwise comparison
    let mut by_group: BTreeMap<String, Vec<(i64, Vec<String>, i64)>> = BTreeMap::new();
    for (id, template, count, group) in &patterns {
        let tokens: Vec<String> = tokenize(template).iter().map(|s| s.to_string()).collect();
        by_group
            .entry(group.clone())
            .or_default()
            .push((*id, tokens, *count));
    }

    // Union-Find for clustering
    let mut parent: HashMap<i64, i64> = HashMap::new();
    fn uf_find(parent: &mut HashMap<i64, i64>, x: i64) -> i64 {
        let p = *parent.get(&x).unwrap_or(&x);
        if p == x {
            return x;
        }
        let root = uf_find(parent, p);
        parent.insert(x, root);
        root
    }

    let mut total_pairs = 0usize;

    if mode == "semantic" {
        // Semantic mode: TF-IDF cosine similarity, works across different lengths
        for group_patterns in by_group.values_mut() {
            if group_patterns.len() < 2 {
                continue;
            }
            // Cap at 1000 for O(n²) tractability
            group_patterns.truncate(1000);

            // Compute IDF across all templates in this group
            let all_tokens: Vec<Vec<String>> =
                group_patterns.iter().map(|(_, t, _)| t.clone()).collect();
            let idf = hearken_ml::compute_idf(&all_tokens);

            for i in 0..group_patterns.len() {
                for j in (i + 1)..group_patterns.len() {
                    let sim = semantic_similarity(&group_patterns[i].1, &group_patterns[j].1, &idf);
                    if sim >= threshold {
                        let ra = uf_find(&mut parent, group_patterns[i].0);
                        let rb = uf_find(&mut parent, group_patterns[j].0);
                        if ra != rb {
                            parent.insert(rb, ra);
                        }
                        total_pairs += 1;
                    }
                }
            }
        }
    } else {
        // Structural mode: same-length bucketing (existing logic)
        for group_patterns in by_group.values() {
            if group_patterns.len() < 2 {
                continue;
            }
            let mut by_len: HashMap<usize, Vec<usize>> = HashMap::new();
            for (i, (_, tokens, _)) in group_patterns.iter().enumerate() {
                by_len.entry(tokens.len()).or_default().push(i);
            }
            for indices in by_len.values() {
                for (ai, &i) in indices.iter().enumerate() {
                    for &j in &indices[ai + 1..] {
                        let sim = template_similarity(&group_patterns[i].1, &group_patterns[j].1);
                        if sim >= threshold {
                            let ra = uf_find(&mut parent, group_patterns[i].0);
                            let rb = uf_find(&mut parent, group_patterns[j].0);
                            if ra != rb {
                                parent.insert(rb, ra);
                            }
                            total_pairs += 1;
                        }
                    }
                }
            }
        }
    }

    // Gather clusters per group
    struct DupCluster {
        group: String,
        members: Vec<(i64, String, i64)>, // (id, template_preview, count)
    }
    let mut all_clusters: Vec<DupCluster> = Vec::new();

    for (group_name, group_patterns) in &by_group {
        let mut clusters: HashMap<i64, Vec<(i64, String, i64)>> = HashMap::new();
        for (id, tokens, count) in group_patterns {
            let root = uf_find(&mut parent, *id);
            let tmpl: String = tokens.join(" ").replace(" \n ", "\n");
            clusters.entry(root).or_default().push((*id, tmpl, *count));
        }
        for members in clusters.into_values() {
            if members.len() > 1 {
                all_clusters.push(DupCluster {
                    group: group_name.clone(),
                    members,
                });
            }
        }
    }

    if all_clusters.is_empty() {
        println!(
            "No near-duplicate patterns found (threshold={:.2}).",
            threshold
        );
        return Ok(());
    }

    if format == "json" {
        let json_items: Vec<String> = all_clusters
            .iter()
            .map(|c| {
                let members: Vec<String> = c
                    .members
                    .iter()
                    .map(|(id, tmpl, count)| {
                        let escaped = tmpl
                            .replace('\\', "\\\\")
                            .replace('"', "\\\"")
                            .replace('\n', "\\n");
                        format!(
                            "{{\"id\":{},\"count\":{},\"template\":\"{}\"}}",
                            id, count, escaped
                        )
                    })
                    .collect();
                format!(
                    "{{\"group\":\"{}\",\"patterns\":[{}]}}",
                    c.group,
                    members.join(",")
                )
            })
            .collect();
        println!("[{}]", json_items.join(",\n"));
    } else {
        let mut current_group = "";
        for cluster in &all_clusters {
            if cluster.group != current_group {
                current_group = &cluster.group;
                let group_count = all_clusters
                    .iter()
                    .filter(|c| c.group == current_group)
                    .count();
                println!(
                    "═══ Group: {} — {} duplicate cluster(s) ═══\n",
                    current_group, group_count
                );
            }
            let combined: i64 = cluster.members.iter().map(|m| m.2).sum();
            println!(
                "  Cluster ({} patterns, combined {} occurrences):",
                cluster.members.len(),
                combined
            );
            for (id, tmpl, count) in &cluster.members {
                let preview = if tmpl.len() > 100 {
                    format!("{}…", &tmpl[..100])
                } else {
                    tmpl.clone()
                };
                println!("    [id={}, count={}] {}", id, count, preview);
            }
            println!();
        }
        println!(
            "Found {} duplicate cluster(s) with {} similar pair(s) total.",
            all_clusters.len(),
            total_pairs
        );
    }

    Ok(())
}

fn find_clusters(
    storage: &Storage,
    min_shared: usize,
    top: usize,
    group_filter: Option<&str>,
    pattern_limit: usize,
    format: &str,
) -> Result<()> {
    println!(
        "Finding root-cause clusters (min_shared={}, pattern_limit={})...",
        min_shared, pattern_limit
    );

    let variable_data = storage.get_variable_index(group_filter, pattern_limit)?;
    if variable_data.is_empty() {
        println!("No variable data found. Process logs first.");
        return Ok(());
    }

    // Build pattern metadata: id -> (group, template)
    let mut pattern_meta: HashMap<i64, (String, String)> = HashMap::new();
    for (pid, group, tmpl, _) in &variable_data {
        pattern_meta
            .entry(*pid)
            .or_insert_with(|| (group.clone(), tmpl.clone()));
    }

    let total_patterns = pattern_meta.len();
    println!(
        "  Analyzing variables from {} patterns ({} occurrences)...",
        total_patterns,
        variable_data.len()
    );

    // Build inverted index: variable_value -> HashSet<pattern_id>
    let mut var_to_patterns: HashMap<String, HashSet<i64>> = HashMap::new();
    for (pid, _, _, vars_str) in &variable_data {
        for var in vars_str.split('\t') {
            let var = var.trim();
            if var.is_empty() {
                continue;
            }
            if var.len() <= 1 {
                continue;
            }
            if var.len() < 5 && var.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(n) = var.parse::<i64>() {
                    if n < 1000 {
                        continue;
                    }
                }
            }
            var_to_patterns
                .entry(var.to_string())
                .or_default()
                .insert(*pid);
        }
    }

    // Filter out high-cardinality values (>50% of patterns)
    let cardinality_threshold = total_patterns / 2;
    var_to_patterns.retain(|_, pids| pids.len() >= 2 && pids.len() <= cardinality_threshold.max(2));

    if var_to_patterns.is_empty() {
        println!("No shared variable values found across patterns.");
        return Ok(());
    }

    // Count shared variable values between pattern pairs
    let mut pair_shared: HashMap<(i64, i64), Vec<String>> = HashMap::new();
    for (val, pids) in &var_to_patterns {
        let pids_vec: Vec<i64> = pids.iter().copied().collect();
        for i in 0..pids_vec.len() {
            for j in (i + 1)..pids_vec.len() {
                let (a, b) = if pids_vec[i] < pids_vec[j] {
                    (pids_vec[i], pids_vec[j])
                } else {
                    (pids_vec[j], pids_vec[i])
                };
                let shared = pair_shared.entry((a, b)).or_default();
                if shared.len() < 20 {
                    shared.push(val.clone());
                }
            }
        }
    }

    // Union-Find clustering for pairs with >= min_shared values
    let mut parent: HashMap<i64, i64> = HashMap::new();

    fn uf_find(parent: &mut HashMap<i64, i64>, x: i64) -> i64 {
        let p = *parent.get(&x).unwrap_or(&x);
        if p == x {
            return x;
        }
        let root = uf_find(parent, p);
        parent.insert(x, root);
        root
    }

    for ((a, b), shared) in &pair_shared {
        if shared.len() >= min_shared {
            let ra = uf_find(&mut parent, *a);
            let rb = uf_find(&mut parent, *b);
            if ra != rb {
                parent.insert(rb, ra);
            }
        }
    }

    // Gather clusters
    let mut clusters_map: HashMap<i64, Vec<i64>> = HashMap::new();
    for pid in pattern_meta.keys() {
        let root = uf_find(&mut parent, *pid);
        clusters_map.entry(root).or_default().push(*pid);
    }

    struct RootCauseCluster {
        patterns: Vec<(i64, String, String)>,
        shared_values: Vec<String>,
    }

    let mut clusters: Vec<RootCauseCluster> = Vec::new();
    for members in clusters_map.values() {
        if members.len() < 2 {
            continue;
        }
        let patterns: Vec<(i64, String, String)> = members
            .iter()
            .map(|pid| {
                let (group, tmpl) = pattern_meta.get(pid).cloned().unwrap_or_default();
                (*pid, group, tmpl)
            })
            .collect();

        let member_set: HashSet<i64> = members.iter().copied().collect();
        let mut shared: Vec<String> = Vec::new();
        for (val, pids) in &var_to_patterns {
            if pids.iter().filter(|p| member_set.contains(p)).count() >= 2 {
                if shared.len() < 10 {
                    shared.push(val.clone());
                }
            }
        }

        clusters.push(RootCauseCluster {
            patterns,
            shared_values: shared,
        });
    }

    clusters.sort_by(|a, b| b.patterns.len().cmp(&a.patterns.len()));
    clusters.truncate(top);

    if clusters.is_empty() {
        println!("No root-cause clusters found (min_shared={}).", min_shared);
        return Ok(());
    }

    if format == "json" {
        let items: Vec<serde_json::Value> = clusters
            .iter()
            .map(|c| {
                serde_json::json!({
                    "patterns": c.patterns.iter().map(|(id, group, tmpl)| {
                        serde_json::json!({"id": id, "group": group, "template": tmpl})
                    }).collect::<Vec<_>>(),
                    "shared_values": c.shared_values,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        println!("═══ Root-Cause Clusters (top {}) ═══\n", clusters.len());
        for (i, c) in clusters.iter().enumerate() {
            println!("  Cluster {} ({} patterns):", i + 1, c.patterns.len());
            if !c.shared_values.is_empty() {
                let preview: Vec<&str> =
                    c.shared_values.iter().take(5).map(|s| s.as_str()).collect();
                println!("    Shared values: {}", preview.join(", "));
            }
            for (id, group, tmpl) in &c.patterns {
                let preview = if tmpl.len() > 80 {
                    format!("{}…", &tmpl[..80])
                } else {
                    tmpl.clone()
                };
                println!("    [id={}, {}] {}", id, group, preview);
            }
            println!();
        }
    }

    Ok(())
}

fn run_check(
    storage: &Storage,
    max_anomaly_score: Option<f64>,
    max_new_patterns: Option<usize>,
    fail_on_pattern: Option<Vec<String>>,
    baseline: Option<String>,
    tags_file: Option<&str>,
    group_filter: Option<&str>,
    format: &str,
) -> Result<bool> {
    let suppressed = load_suppressed_ids(tags_file);
    let mut checks: Vec<serde_json::Value> = Vec::new();
    let mut all_passed = true;

    // Check 1: Anomaly score
    if let Some(max_score) = max_anomaly_score {
        let anomalies = compute_anomalies(storage, group_filter, 1, &suppressed)?;
        let top_score = anomalies.first().map(|a| a.score).unwrap_or(0.0);
        let passed = top_score <= max_score;
        if !passed {
            all_passed = false;
        }
        checks.push(serde_json::json!({
            "name": "max_anomaly_score",
            "threshold": max_score,
            "actual": top_score,
            "passed": passed,
        }));
    }

    // Check 2: New patterns vs baseline
    if let Some(max_new) = max_new_patterns {
        if let Some(ref baseline_path) = baseline {
            let diff = compute_diff(baseline_path, storage.db_path())?;
            let actual = diff.new_patterns.len();
            let passed = actual <= max_new;
            if !passed {
                all_passed = false;
            }
            checks.push(serde_json::json!({
                "name": "max_new_patterns",
                "threshold": max_new,
                "actual": actual,
                "passed": passed,
            }));
        } else {
            checks.push(serde_json::json!({
                "name": "max_new_patterns",
                "threshold": max_new,
                "actual": null,
                "passed": true,
                "note": "no baseline provided, skipped",
            }));
        }
    }

    // Check 3: Fail on pattern substring
    if let Some(ref patterns) = fail_on_pattern {
        let all_patterns = storage.get_patterns_for_dedup(group_filter)?;
        for needle in patterns {
            let found = all_patterns
                .iter()
                .filter(|(id, _, _, _)| !suppressed.contains(id))
                .any(|(_, tmpl, _, _)| tmpl.contains(needle.as_str()));
            if found {
                all_passed = false;
            }
            checks.push(serde_json::json!({
                "name": "fail_on_pattern",
                "pattern": needle,
                "found": found,
                "passed": !found,
            }));
        }
    }

    // Output results
    if format == "json" {
        let output = serde_json::json!({
            "passed": all_passed,
            "checks": checks,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("═══ Hearken Check ═══\n");
        for check in &checks {
            let name = check["name"].as_str().unwrap_or("unknown");
            let passed = check["passed"].as_bool().unwrap_or(false);
            let icon = if passed { "✅" } else { "❌" };
            match name {
                "max_anomaly_score" => {
                    println!(
                        "  {} anomaly_score: {:.1} (threshold: {})",
                        icon,
                        check["actual"].as_f64().unwrap_or(0.0),
                        check["threshold"]
                    );
                }
                "max_new_patterns" => {
                    println!(
                        "  {} new_patterns: {} (threshold: {})",
                        icon, check["actual"], check["threshold"]
                    );
                }
                "fail_on_pattern" => {
                    println!(
                        "  {} pattern '{}': {}",
                        icon,
                        check["pattern"].as_str().unwrap_or(""),
                        if check["found"].as_bool().unwrap_or(false) {
                            "FOUND"
                        } else {
                            "not found"
                        }
                    );
                }
                _ => {}
            }
        }
        println!(
            "\n{}",
            if all_passed {
                "✅ All checks passed"
            } else {
                "❌ Some checks failed"
            }
        );
    }

    Ok(all_passed)
}

struct AnomalyResult {
    id: i64,
    template: String,
    count: i64,
    group: String,
    score: f64,
    reasons: Vec<String>,
}

fn compute_anomalies(
    storage: &Storage,
    group_filter: Option<&str>,
    top: usize,
    suppressed_ids: &HashSet<i64>,
) -> Result<Vec<AnomalyResult>> {
    let patterns = storage.get_patterns_for_dedup(group_filter)?;
    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let patterns: Vec<_> = if !suppressed_ids.is_empty() {
        patterns
            .into_iter()
            .filter(|p| !suppressed_ids.contains(&p.0))
            .collect()
    } else {
        patterns
    };

    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let pattern_ids: Vec<i64> = patterns.iter().map(|p| p.0).collect();
    let trends = storage.get_pattern_trends(&pattern_ids)?;
    let source_counts = storage.get_source_counts_per_group()?;

    // Compute per-group stats for z-score
    let mut group_counts: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
    for (id, _, count, group) in &patterns {
        group_counts
            .entry(group.clone())
            .or_default()
            .push((*id, *count));
    }
    let mut group_stats: HashMap<String, (f64, f64)> = HashMap::new();
    for (group, counts) in &group_counts {
        let n = counts.len() as f64;
        let mean = counts.iter().map(|(_, c)| *c as f64).sum::<f64>() / n;
        let variance = counts
            .iter()
            .map(|(_, c)| (*c as f64 - mean).powi(2))
            .sum::<f64>()
            / n;
        group_stats.insert(group.clone(), (mean, variance.sqrt()));
    }

    let mut anomalies: Vec<AnomalyResult> = Vec::new();

    for (id, template, count, group) in &patterns {
        let mut reasons = Vec::new();
        let mut score = 0.0f64;

        let group_sources = source_counts.get(group).copied().unwrap_or(1);
        let pattern_sources = trends.get(id).map(|t| t.len()).unwrap_or(1);
        if group_sources > 1 && pattern_sources == 1 {
            reasons.push(format!("single-source (1/{} files)", group_sources));
            score += 2.0;
        }

        if let Some(&(mean, stddev)) = group_stats.get(group) {
            if stddev > 0.0 {
                let z = (*count as f64 - mean) / stddev;
                if z > 3.0 {
                    reasons.push(format!("high-count outlier (z={:.1})", z));
                    score += z;
                }
            }
        }

        if !reasons.is_empty() {
            anomalies.push(AnomalyResult {
                id: *id,
                template: template.clone(),
                count: *count,
                group: group.clone(),
                score,
                reasons,
            });
        }
    }

    anomalies.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    anomalies.truncate(top);

    Ok(anomalies)
}

fn detect_anomalies(
    storage: &Storage,
    group_filter: Option<&str>,
    top: usize,
    format: &str,
    tags_file: Option<&str>,
    include_suppressed: bool,
) -> Result<()> {
    let suppressed = if include_suppressed {
        HashSet::new()
    } else {
        load_suppressed_ids(tags_file)
    };
    let anomalies = compute_anomalies(storage, group_filter, top, &suppressed)?;

    if anomalies.is_empty() {
        println!("No anomalous patterns detected.");
        return Ok(());
    }

    if format == "json" {
        let items: Vec<String> = anomalies.iter().map(|a| {
            let tmpl = a.template.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
            let reasons: Vec<String> = a.reasons.iter().map(|r| format!("\"{}\"", r)).collect();
            format!(
                "{{\"id\":{},\"group\":\"{}\",\"count\":{},\"anomaly_score\":{:.2},\"reasons\":[{}],\"template\":\"{}\"}}",
                a.id, a.group, a.count, a.score, reasons.join(","), tmpl
            )
        }).collect();
        println!("[{}]", items.join(",\n"));
    } else {
        println!("═══ Anomalous Patterns (top {}) ═══\n", anomalies.len());
        for (i, a) in anomalies.iter().enumerate() {
            let preview = if a.template.len() > 100 {
                format!("{}…", &a.template[..100])
            } else {
                a.template.replace('\n', "\\n")
            };
            println!(
                "  {}. [score={:.1}] {} (count={}, group={})",
                i + 1,
                a.score,
                a.reasons.join("; "),
                a.count,
                a.group
            );
            println!("     {}\n", preview);
        }
    }

    Ok(())
}

/// Escapes a value for CSV output (wraps in quotes if it contains comma, quote, or newline).
fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// Derives a canonical file group name from a log file path by stripping
/// date patterns (YYYY-MM-DD, YYYYMMDD) and numeric suffixes from the filename.
/// Files that share the same group name are assumed to have the same log format.
fn derive_group_name(file_path: &str) -> String {
    let filename = Path::new(file_path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(file_path);

    let mut name = filename.to_string();

    // Strip YYYY-MM-DD or YYYY_MM_DD patterns
    loop {
        let before = name.clone();
        name = strip_pattern(&name, |s| {
            let bytes = s.as_bytes();
            if bytes.len() >= 10
                && bytes[0..4].iter().all(|b| b.is_ascii_digit())
                && (bytes[4] == b'-' || bytes[4] == b'_')
                && bytes[5..7].iter().all(|b| b.is_ascii_digit())
                && (bytes[7] == b'-' || bytes[7] == b'_')
                && bytes[8..10].iter().all(|b| b.is_ascii_digit())
            {
                return Some(10);
            }
            None
        });
        // Strip YYYYMMDD patterns (8 consecutive digits that look like a date)
        name = strip_pattern(&name, |s| {
            let bytes = s.as_bytes();
            if bytes.len() >= 8 && bytes[0..8].iter().all(|b| b.is_ascii_digit()) {
                let year = &s[0..4];
                let month = &s[4..6];
                let day = &s[6..8];
                if let (Ok(y), Ok(m), Ok(d)) = (
                    year.parse::<u32>(),
                    month.parse::<u32>(),
                    day.parse::<u32>(),
                ) {
                    if (1900..=2100).contains(&y) && (1..=12).contains(&m) && (1..=31).contains(&d)
                    {
                        if bytes.len() == 8 || !bytes[8].is_ascii_digit() {
                            return Some(8);
                        }
                    }
                }
            }
            None
        });
        if name == before {
            break;
        }
    }

    // Strip trailing pure-numeric segments (e.g., .1, .2, .003)
    loop {
        let before = name.clone();
        if let Some(dot_pos) = name.rfind('.') {
            let suffix = &name[dot_pos + 1..];
            if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                name = name[..dot_pos].to_string();
            }
        }
        if name == before {
            break;
        }
    }

    // Clean up: normalize separators to dots, collapse consecutive ones
    let mut cleaned = String::with_capacity(name.len());
    let mut last_sep = false;
    for c in name.chars() {
        if c == '.' || c == '-' || c == '_' {
            if !last_sep && !cleaned.is_empty() {
                cleaned.push('.');
            }
            last_sep = true;
        } else {
            cleaned.push(c);
            last_sep = false;
        }
    }
    while cleaned.ends_with('.') {
        cleaned.pop();
    }

    // Strip trailing digits from segments (e.g., test2.log → test.log, server8080.log → server.log)
    // Only strip if it leaves a non-empty prefix in the segment.
    let segments: Vec<&str> = cleaned.split('.').collect();
    let stripped: Vec<String> = segments
        .iter()
        .map(|seg| {
            let trimmed = seg.trim_end_matches(|c: char| c.is_ascii_digit());
            if trimmed.is_empty() {
                seg.to_string()
            } else {
                trimmed.to_string()
            }
        })
        .collect();
    // Dedup consecutive identical segments that arose from stripping
    let mut deduped = Vec::with_capacity(stripped.len());
    for seg in &stripped {
        if deduped.last().map(|s: &String| s == seg).unwrap_or(false) {
            continue;
        }
        deduped.push(seg.clone());
    }
    let cleaned = deduped.join(".");

    if cleaned.is_empty() {
        filename.to_string()
    } else {
        cleaned
    }
}

/// Helper: finds and removes the first occurrence of a pattern detected by `detector`.
fn strip_pattern(input: &str, detector: impl Fn(&str) -> Option<usize>) -> String {
    for i in 0..input.len() {
        if let Some(match_len) = detector(&input[i..]) {
            let mut result = String::with_capacity(input.len());
            result.push_str(&input[..i]);
            if i + match_len < input.len() {
                result.push_str(&input[i + match_len..]);
            }
            return result;
        }
    }
    input.to_string()
}

/// Groups file paths by their derived group name.
/// Returns a BTreeMap (sorted by group name) of group_name → sorted file paths.
fn group_files(
    file_paths: &[String],
    group_name_override: Option<&str>,
    stdin_paths: &HashSet<String>,
) -> BTreeMap<String, Vec<String>> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in file_paths {
        let group = if stdin_paths.contains(path) {
            group_name_override.unwrap_or("stdin").to_string()
        } else {
            derive_group_name(path)
        };
        groups.entry(group).or_default().push(path.clone());
    }
    for files in groups.values_mut() {
        files.sort();
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_group_name_date_suffix() {
        assert_eq!(derive_group_name("error.log.2026-03-01"), "error.log");
        assert_eq!(derive_group_name("error.log.2026-03-02"), "error.log");
        assert_eq!(
            derive_group_name("/var/log/error.log.2026-10-23"),
            "error.log"
        );
    }

    #[test]
    fn test_derive_group_name_date_infix() {
        assert_eq!(derive_group_name("error.20260301.log"), "error.log");
        assert_eq!(derive_group_name("error.20260302.log"), "error.log");
    }

    #[test]
    fn test_derive_group_name_numeric_suffix() {
        assert_eq!(derive_group_name("error.log.1"), "error.log");
        assert_eq!(derive_group_name("error.log.42"), "error.log");
    }

    #[test]
    fn test_derive_group_name_plain() {
        assert_eq!(derive_group_name("access.log"), "access.log");
        assert_eq!(derive_group_name("request.log"), "request.log");
        assert_eq!(derive_group_name("/home/user/logs/app.log"), "app.log");
    }

    #[test]
    fn test_derive_group_name_underscore_date() {
        // Underscores in original name are preserved
        assert_eq!(derive_group_name("app_2026_03_15.log"), "app.log");
        assert_eq!(derive_group_name("app-2026-03-15.log"), "app.log");
    }

    #[test]
    fn test_derive_group_name_preserves_non_date_numbers() {
        // Trailing digits on segments are stripped as rotation/instance numbers
        assert_eq!(derive_group_name("server8080.log"), "server.log");
    }

    #[test]
    fn test_derive_group_name_rotation_suffix() {
        assert_eq!(derive_group_name("test2.log"), "test.log");
        assert_eq!(derive_group_name("test.log"), "test.log");
        assert_eq!(derive_group_name("app1.log"), "app.log");
        assert_eq!(derive_group_name("app8080.log"), "app.log");
    }

    #[test]
    fn test_group_files() {
        let files = vec![
            "error.log.2026-03-02".to_string(),
            "error.log.2026-03-01".to_string(),
            "access.log".to_string(),
            "access.log.1".to_string(),
        ];
        let groups = group_files(&files, None, &HashSet::new());
        assert_eq!(groups.len(), 2);
        assert_eq!(groups["access.log"], vec!["access.log", "access.log.1"]);
        assert_eq!(
            groups["error.log"],
            vec!["error.log.2026-03-01", "error.log.2026-03-02"]
        );
    }

    #[test]
    fn test_config_parsing() {
        let toml_str = r#"
database = "logs/hearken.db"
threshold = 0.4
batch_size = 1000000

[report]
top = 1000
samples = 10
output = "my-report.html"
filter = ["*ERROR*", "*WARN*"]
tags_file = "my-tags.json"
bucket = "hour"

[check]
max_anomaly_score = 5.0
max_new_patterns = 10
baseline = "baseline.db"
"#;
        let config: HearkenConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.database.unwrap(), "logs/hearken.db");
        assert_eq!(config.threshold.unwrap(), 0.4);
        assert_eq!(config.batch_size.unwrap(), 1000000);
        assert_eq!(config.report.top.unwrap(), 1000);
        assert_eq!(config.report.samples.unwrap(), 10);
        assert_eq!(config.report.output.unwrap(), "my-report.html");
        assert_eq!(config.report.filter.unwrap(), vec!["*ERROR*", "*WARN*"]);
        assert_eq!(config.report.tags_file.unwrap(), "my-tags.json");
        assert_eq!(config.report.bucket.unwrap(), "hour");
        assert_eq!(config.check.max_anomaly_score.unwrap(), 5.0);
        assert_eq!(config.check.max_new_patterns.unwrap(), 10);
        assert_eq!(config.check.baseline.unwrap(), "baseline.db");
    }
}
