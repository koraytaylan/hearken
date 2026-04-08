use std::fmt::Write;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

/// Returns the path to the hearken-cli binary (built via cargo test).
fn cli_bin() -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    path.pop(); // remove 'deps'
    path.push("hearken-cli");
    path
}

/// Generates synthetic log lines with a predictable structure.
fn generate_log_lines(count: usize, prefix: &str) -> String {
    let mut lines = String::new();
    for i in 0..count {
        let level = if i % 5 == 0 {
            "*ERROR*"
        } else if i % 3 == 0 {
            "*WARN*"
        } else {
            "*INFO*"
        };
        writeln!(
            lines,
            "2026-01-15 08:00:{:02}.{:03} {} [pool-thread-{}] com.app.Service - Operation completed in {}ms for request-{}",
            i % 60, i % 1000, level, i % 8, (i * 7) % 500, i
        ).unwrap();
        // Add a multi-line entry every 10 lines
        if i % 10 == 0 {
            writeln!(
                lines,
                "2026-01-15 08:00:{:02}.{:03} *ERROR* [pool-thread-{}] com.app.Handler - Processing failed for item-{}",
                i % 60, i % 1000, i % 8, i
            ).unwrap();
            lines.push_str("java.lang.RuntimeException: Something went wrong\n");
            writeln!(
                lines,
                "\tat com.app.Handler.process(Handler.java:{})",
                100 + i % 50
            )
            .unwrap();
            writeln!(
                lines,
                "\tat com.app.Service.run(Service.java:{})",
                200 + i % 30
            )
            .unwrap();
            lines.push_str("\tat com.app.Main.main(Main.java:42)\n");
        }
        // Add a different pattern for variety
        if i % 20 == 0 {
            writeln!(
                lines,
                "2026-01-15 08:00:{:02}.{:03} *WARN* [scheduler-{}] {} - Retry attempt {} for task-{}",
                i % 60, i % 1000, i % 4, prefix, (i / 20) + 1, i
            ).unwrap();
        }
    }
    lines
}

#[test]
fn test_process_single_file() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("app.log");
    let db_file = dir.path().join("test.db");

    fs::write(&log_file, generate_log_lines(100, "SingleApp")).unwrap();

    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "Process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("file group(s)"), "Should print group info");
    assert!(db_file.exists(), "Database should be created");
}

#[test]
fn test_process_multi_file_grouping() {
    let dir = TempDir::new().unwrap();
    let db_file = dir.path().join("test.db");

    // Create files that should group together
    let log1 = dir.path().join("error.log.2026-01-01");
    let log2 = dir.path().join("error.log.2026-01-02");
    // Create a file that should be a different group
    let log3 = dir.path().join("access.log");

    fs::write(&log1, generate_log_lines(50, "ErrorApp")).unwrap();
    fs::write(&log2, generate_log_lines(50, "ErrorApp")).unwrap();
    fs::write(&log3, generate_log_lines(30, "AccessApp")).unwrap();

    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log1.to_str().unwrap(),
            log2.to_str().unwrap(),
            log3.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "Process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("2 file group(s) from 3 file(s)"),
        "Should detect 2 groups from 3 files: {stdout}"
    );
    assert!(stdout.contains("error.log"), "Should have error.log group");
    assert!(
        stdout.contains("access.log"),
        "Should have access.log group"
    );
}

#[test]
fn test_process_and_search() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("server.log");
    let db_file = dir.path().join("test.db");

    fs::write(&log_file, generate_log_lines(100, "ServerApp")).unwrap();

    // Process
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "Process failed");

    // Search
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "search",
            "Operation completed",
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "Search failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Operation") || stdout.contains("completed"),
        "Search should find matching patterns: {stdout}"
    );
}

#[test]
fn test_process_and_report() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("app.log");
    let db_file = dir.path().join("test.db");
    let report_file = dir.path().join("report.html");

    fs::write(&log_file, generate_log_lines(100, "ReportApp")).unwrap();

    // Process
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "Process failed");

    // Generate report
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "report",
            "--output",
            report_file.to_str().unwrap(),
            "--top",
            "50",
            "--samples",
            "3",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Report failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(report_file.exists(), "Report HTML should be created");

    let html = fs::read_to_string(&report_file).unwrap();
    assert!(
        html.contains("<!DOCTYPE html>"),
        "Report should be valid HTML"
    );
    assert!(html.contains("const DATA"), "Report should embed data JSON");
    assert!(html.contains("Hearken Report"), "Report should have title");
}

#[test]
fn test_report_with_filters() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("app.log");
    let db_file = dir.path().join("test.db");
    let report_file = dir.path().join("filtered.html");

    fs::write(&log_file, generate_log_lines(100, "FilterApp")).unwrap();

    // Process
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "Process failed");

    // Generate filtered report
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "report",
            "--output",
            report_file.to_str().unwrap(),
            "--filter",
            "*ERROR*",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Filtered report failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let html = fs::read_to_string(&report_file).unwrap();
    assert!(html.contains("const DATA"), "Report should embed data");
}

#[test]
fn test_validation_bad_threshold() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("app.log");
    let db_file = dir.path().join("test.db");

    fs::write(&log_file, "some log line\n").unwrap();

    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            "--threshold",
            "2.0",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success(), "Should fail with bad threshold");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("threshold") && stderr.contains("0.0"),
        "Should mention threshold range: {stderr}"
    );
}

#[test]
fn test_validation_nonexistent_file() {
    let dir = TempDir::new().unwrap();
    let db_file = dir.path().join("test.db");

    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            "/nonexistent/file.log",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "Should fail with nonexistent file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not exist") || stderr.contains("No valid files"),
        "Should warn about missing file: {stderr}"
    );
}

#[test]
fn test_validation_empty_file() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("empty.log");
    let db_file = dir.path().join("test.db");

    fs::write(&log_file, "").unwrap();

    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success(), "Should fail with empty file only");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("empty") || stderr.contains("No valid files"),
        "Should warn about empty file: {stderr}"
    );
}

#[test]
fn test_multiline_entries_in_patterns() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("app.log");
    let db_file = dir.path().join("test.db");

    // Create log with consistent multi-line entries
    let mut log_content = String::new();
    for i in 0..50 {
        writeln!(
            log_content,
            "2026-01-15 10:00:{:02}.000 *ERROR* [thread-{}] com.app.Handler - Request failed",
            i % 60,
            i % 4
        )
        .unwrap();
        log_content.push_str("java.lang.NullPointerException: Value was null\n");
        writeln!(
            log_content,
            "\tat com.app.Handler.handle(Handler.java:{})",
            50 + i
        )
        .unwrap();
        log_content.push_str("\tat com.app.Server.dispatch(Server.java:120)\n");
    }

    fs::write(&log_file, &log_content).unwrap();

    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify via search that multi-line patterns were captured
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "search",
            "NullPointerException",
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "Search failed");
    assert!(
        stdout.contains("NullPointerException"),
        "Should find multi-line pattern with exception: {stdout}"
    );
}

#[test]
fn test_full_pipeline_multi_group() {
    let dir = TempDir::new().unwrap();
    let db_file = dir.path().join("test.db");
    let report_file = dir.path().join("report.html");

    // Create two groups of files
    let error1 = dir.path().join("error.log.2026-01-01");
    let error2 = dir.path().join("error.log.2026-01-02");
    let access = dir.path().join("access.log");

    fs::write(&error1, generate_log_lines(80, "ErrorSvc")).unwrap();
    fs::write(&error2, generate_log_lines(80, "ErrorSvc")).unwrap();
    fs::write(&access, generate_log_lines(40, "AccessSvc")).unwrap();

    // Process all files
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            error1.to_str().unwrap(),
            error2.to_str().unwrap(),
            access.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Generate report filtered to error.log group
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "report",
            "--output",
            report_file.to_str().unwrap(),
            "--group",
            "error.log",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Report failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let html = fs::read_to_string(&report_file).unwrap();
    assert!(
        html.contains("error.log"),
        "Report should contain error.log group"
    );
    assert!(
        html.contains("const DATA"),
        "Report should have embedded data"
    );
}

#[test]
fn test_report_with_bucket_hour() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("app.log");
    let db_file = dir.path().join("test.db");
    let report_file = dir.path().join("bucket_report.html");

    fs::write(&log_file, generate_log_lines(100, "BucketApp")).unwrap();

    // Process
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Generate report with --bucket hour
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "report",
            "--output",
            report_file.to_str().unwrap(),
            "--top",
            "50",
            "--samples",
            "3",
            "--bucket",
            "hour",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Report with --bucket hour failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(report_file.exists(), "Report HTML should be created");

    let html = fs::read_to_string(&report_file).unwrap();
    assert!(
        html.contains("<!DOCTYPE html>"),
        "Report should be valid HTML"
    );
    assert!(html.contains("const DATA"), "Report should embed data JSON");
    assert!(
        html.contains("has_timestamps"),
        "Report should include has_timestamps field"
    );
}

#[test]
fn test_pattern_suppression() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("app.log");
    let db_file = dir.path().join("test.db");
    let tags_file = dir.path().join("tags.json");

    fs::write(&log_file, generate_log_lines(100, "SuppressApp")).unwrap();

    // Process
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Create tags file suppressing pattern ID 1
    fs::write(&tags_file, r#"{"1": ["suppress"]}"#).unwrap();

    // Export without include-suppressed — should exclude pattern 1
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "export",
            "--format",
            "json",
            "--tags-file",
            tags_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Export failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Pattern 1 should NOT appear
    assert!(
        !stdout.contains("\"id\":1,") && !stdout.contains("\"id\": 1,"),
        "Suppressed pattern should be excluded: {}",
        &stdout[..stdout.len().min(500)]
    );

    // Export WITH include-suppressed — should include pattern 1
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "export",
            "--format",
            "json",
            "--tags-file",
            tags_file.to_str().unwrap(),
            "--include-suppressed",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Export with --include-suppressed failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_check_command_pass() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("app.log");
    let db_file = dir.path().join("test.db");

    fs::write(&log_file, generate_log_lines(50, "CheckApp")).unwrap();

    // Process
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Check with generous threshold — should pass
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "check",
            "--max-anomaly-score",
            "999",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Check should pass with high threshold"
    );

    // Check with fail-on-pattern for something that doesn't exist
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "check",
            "--fail-on-pattern",
            "NONEXISTENT_PATTERN_XYZ",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Check should pass when pattern not found"
    );
}

#[test]
fn test_check_command_fail() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("app.log");
    let db_file = dir.path().join("test.db");

    fs::write(&log_file, generate_log_lines(50, "CheckFail")).unwrap();

    // Process
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Check with fail-on-pattern for "Operation" which should exist
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "check",
            "--fail-on-pattern",
            "Operation",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "Check should fail when pattern found"
    );
}

#[test]
fn test_dedup_semantic_mode() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("app.log");
    let db_file = dir.path().join("test.db");

    fs::write(&log_file, generate_log_lines(200, "SemanticApp")).unwrap();

    // Process
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Run dedup in semantic mode
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "dedup",
            "--mode",
            "semantic",
            "--threshold",
            "0.3",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Semantic dedup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_baseline_save_and_compare() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("app.log");
    let db_file = dir.path().join("test.db");
    let baseline_file = dir.path().join("baseline.db");

    fs::write(&log_file, generate_log_lines(50, "BaselineApp")).unwrap();

    // Process
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Save baseline
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "baseline",
            "save",
            "--output",
            baseline_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "Baseline save failed: {stdout}");
    assert!(baseline_file.exists(), "Baseline file should exist");
    assert!(
        stdout.contains("Baseline saved"),
        "Should confirm save: {stdout}"
    );

    // Compare (should show no changes since same data)
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "baseline",
            "compare",
            baseline_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "Baseline compare failed: {stdout}");
    assert!(
        stdout.contains("0 new"),
        "Should show no new patterns: {stdout}"
    );
}

#[test]
fn test_cluster_command() {
    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("app.log");
    let db_file = dir.path().join("test.db");

    fs::write(&log_file, generate_log_lines(200, "ClusterApp")).unwrap();

    // Process
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Run cluster
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "cluster",
            "--min-shared",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Cluster failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_correlate_command() {
    let dir = TempDir::new().unwrap();
    let db_file = dir.path().join("test.db");
    let log1 = dir.path().join("error.log");
    let log2 = dir.path().join("access.log");

    // Create two log files with correlated timestamps
    fs::write(&log1, generate_log_lines(100, "ErrorApp")).unwrap();
    fs::write(&log2, generate_log_lines(100, "AccessApp")).unwrap();

    // Process both
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            log1.to_str().unwrap(),
            log2.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Run correlate
    let output = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "correlate",
            "--window",
            "300",
            "--min-count",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Correlate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Finding correlations"),
        "Should show progress: {stdout}"
    );
}

#[test]
fn test_process_stdin() {
    let dir = TempDir::new().unwrap();
    let db_file = dir.path().join("test.db");

    let log_data = generate_log_lines(50, "StdinApp");

    let mut child = Command::new(cli_bin())
        .args(["-d", db_file.to_str().unwrap(), "process", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(log_data.as_bytes()).unwrap();
    }

    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "Process stdin failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("file group(s)"),
        "Should print group info: {stdout}"
    );
    assert!(
        stdout.contains("stdin"),
        "Group name should be 'stdin': {stdout}"
    );
    assert!(db_file.exists(), "Database should be created");
}

#[test]
fn test_process_stdin_with_group_name() {
    let dir = TempDir::new().unwrap();
    let db_file = dir.path().join("test.db");

    let log_data = generate_log_lines(30, "CustomApp");

    let mut child = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "process",
            "--group-name",
            "my-logs",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(log_data.as_bytes()).unwrap();
    }

    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "Process stdin with group-name failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("my-logs"),
        "Group name should be 'my-logs': {stdout}"
    );
}

#[test]
fn test_watch_detects_appended_content() {
    use std::io::Write;

    let dir = TempDir::new().unwrap();
    let log_file = dir.path().join("watch-test.log");
    let db_file = dir.path().join("watch.db");

    // Create initial log file
    fs::write(&log_file, generate_log_lines(20, "WatchApp")).unwrap();

    // Start the watch command in a child process
    #[allow(unused_mut)]
    let mut child = Command::new(cli_bin())
        .args([
            "-d",
            db_file.to_str().unwrap(),
            "watch",
            log_file.to_str().unwrap(),
            "--threshold",
            "0.5",
            "--batch-size",
            "100000",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Wait for initial processing to complete
    std::thread::sleep(std::time::Duration::from_secs(3));

    // Append new lines to the file
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_file)
            .unwrap();
        let new_lines = generate_log_lines(10, "WatchApp");
        f.write_all(new_lines.as_bytes()).unwrap();
        f.flush().unwrap();
    }

    // Give the watcher time to detect and process
    std::thread::sleep(std::time::Duration::from_secs(3));

    // Kill the process
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    assert!(
        combined.contains("[watch] Initial processing complete"),
        "Should complete initial processing. Output:\n{combined}"
    );
    assert!(
        combined.contains("[watch] File modified:"),
        "Should detect file modification. Output:\n{combined}"
    );
    assert!(
        combined.contains("new entries"),
        "Should report new entries. Output:\n{combined}"
    );
}
