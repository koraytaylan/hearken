#![cfg(feature = "web")]

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn cli_bin() -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
    path.push("hearken-cli");
    path
}

fn generate_log_lines(count: usize) -> String {
    let mut lines = String::new();
    for i in 0..count {
        let level = if i % 5 == 0 { "ERROR" } else { "INFO" };
        lines.push_str(&format!(
            "2026-01-15 08:00:{:02}.{:03} {} [thread-{}] com.app.Service - Operation completed in {}ms for request-{}\n",
            i % 60, i % 1000, level, i % 4, (i * 7) % 500, i
        ));
    }
    lines
}

fn setup_db(dir: &TempDir) -> std::path::PathBuf {
    let log_path = dir.path().join("test.log");
    fs::write(&log_path, generate_log_lines(100)).unwrap();

    let db_path = dir.path().join("test.db");
    let output = Command::new(cli_bin())
        .args(["--database", db_path.to_str().unwrap(), "process", log_path.to_str().unwrap()])
        .output()
        .expect("Failed to run process command");
    assert!(output.status.success(), "Process failed: {}", String::from_utf8_lossy(&output.stderr));
    db_path
}

#[tokio::test]
async fn test_web_server_endpoints() {
    let dir = TempDir::new().unwrap();
    let db_path = setup_db(&dir);

    // Start the server on a random-ish port
    let port = 18234u16;
    let mut child = Command::new(cli_bin())
        .args([
            "--database", db_path.to_str().unwrap(),
            "serve", "--port", &port.to_string(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to start serve command");

    // Wait for the server to be ready
    let base = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::new();
    let mut ready = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if client.get(&base).send().await.is_ok() {
            ready = true;
            break;
        }
    }
    assert!(ready, "Server did not become ready in time");

    // Test GET / returns HTML
    let resp = client.get(&base).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Hearken Dashboard"), "Dashboard HTML not returned");

    // Test GET /api/summary returns JSON with expected fields
    let resp = client.get(format!("{}/api/summary", base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert!(json["pattern_count"].as_i64().unwrap() > 0, "Expected patterns in summary");
    assert!(json["total_occurrences"].as_i64().unwrap() > 0);
    assert!(json["file_groups"].as_array().unwrap().len() > 0);

    // Test GET /api/patterns returns patterns
    let resp = client.get(format!("{}/api/patterns?top=10", base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    let patterns = json["patterns"].as_array().unwrap();
    assert!(!patterns.is_empty(), "Expected some patterns");
    let first = &patterns[0];
    assert!(first["id"].as_i64().is_some());
    assert!(first["template"].as_str().is_some());
    assert!(first["count"].as_i64().unwrap() > 0);
    assert!(first["group"].as_str().is_some());

    // Test GET /api/anomalies returns array
    let resp = client.get(format!("{}/api/anomalies", base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert!(json.as_array().is_some(), "Expected anomalies array");

    // Test POST /api/tags
    let pattern_id = patterns[0]["id"].as_i64().unwrap();
    let resp = client.post(format!("{}/api/tags", base))
        .json(&serde_json::json!({"pattern_id": pattern_id, "tags": ["important", "reviewed"]}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert!(json["ok"].as_bool().unwrap());

    // Verify tags appear in pattern response
    let resp = client.get(format!("{}/api/patterns?top=10", base)).send().await.unwrap();
    let json: serde_json::Value = resp.json().await.unwrap();
    let p = json["patterns"].as_array().unwrap().iter()
        .find(|p| p["id"].as_i64().unwrap() == pattern_id).unwrap();
    let tags: Vec<&str> = p["tags"].as_array().unwrap().iter()
        .map(|t| t.as_str().unwrap()).collect();
    assert!(tags.contains(&"important"));
    assert!(tags.contains(&"reviewed"));

    // Test GET /api/export?format=json
    let resp = client.get(format!("{}/api/export?format=json", base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert!(json["patterns"].as_array().unwrap().len() > 0);

    // Clean up
    let _ = child.kill();
    let _ = child.wait();
}
