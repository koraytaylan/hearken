use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use hearken_storage::Storage;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tower_http::cors::CorsLayer;

type SharedStorage = Arc<Mutex<Storage>>;

#[derive(Serialize)]
struct SummaryResponse {
    pattern_count: i64,
    total_occurrences: i64,
    sources: Vec<String>,
    file_groups: Vec<FileGroupInfo>,
    time_range: Option<TimeRange>,
}

#[derive(Serialize)]
struct FileGroupInfo {
    name: String,
    pattern_count: i64,
}

#[derive(Serialize)]
struct TimeRange {
    min_ts: i64,
    max_ts: i64,
}

#[derive(Deserialize)]
pub struct PatternsQuery {
    top: Option<usize>,
    group: Option<String>,
    filter: Option<String>,
}

#[derive(Serialize)]
struct PatternResponse {
    id: i64,
    template: String,
    count: i64,
    group: String,
    samples: Vec<SampleEntry>,
    trend: Vec<serde_json::Value>,
    distribution: Vec<serde_json::Value>,
    tags: Vec<String>,
}

#[derive(Serialize)]
struct SampleEntry {
    text: String,
    source: String,
}

#[derive(Serialize)]
struct AnomalyResponse {
    id: i64,
    template: String,
    count: i64,
    group: String,
    anomaly_score: f64,
    reasons: Vec<String>,
}

#[derive(Deserialize)]
struct TagsRequest {
    pattern_id: i64,
    tags: Vec<String>,
}

#[derive(Deserialize)]
pub struct ExportQuery {
    format: Option<String>,
}

pub async fn run_server(db_path: &str, port: u16) -> Result<()> {
    let storage = Storage::open(db_path)
        .context("Failed to open database for web server")?;
    let shared = Arc::new(Mutex::new(storage));

    let app = Router::new()
        .route("/", get(dashboard_handler))
        .route("/api/summary", get(summary_handler))
        .route("/api/patterns", get(patterns_handler))
        .route("/api/anomalies", get(anomalies_handler))
        .route("/api/tags", post(tags_handler))
        .route("/api/export", get(export_handler))
        .layer(CorsLayer::permissive())
        .with_state(shared);

    let addr = format!("0.0.0.0:{}", port);
    println!("Hearken web dashboard: http://127.0.0.1:{}", port);
    println!("Press Ctrl+C to stop");

    let listener = tokio::net::TcpListener::bind(&addr).await
        .with_context(|| format!("Failed to bind to {}", addr))?;
    axum::serve(listener, app).await
        .context("Server error")?;

    Ok(())
}

async fn dashboard_handler() -> Html<&'static str> {
    Html(include_str!("dashboard_template.html"))
}

async fn summary_handler(State(storage): State<SharedStorage>) -> impl IntoResponse {
    let db = storage.lock().unwrap();

    let summary = match db.get_report_summary() {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    };

    let (pattern_count, total_occurrences, sources, groups) = summary;

    let time_range = db.conn.query_row(
        "SELECT MIN(entry_timestamp), MAX(entry_timestamp) FROM occurrences WHERE entry_timestamp IS NOT NULL",
        [],
        |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
    ).ok().and_then(|(min, max)| {
        match (min, max) {
            (Some(mn), Some(mx)) => Some(TimeRange { min_ts: mn, max_ts: mx }),
            _ => None,
        }
    });

    let resp = SummaryResponse {
        pattern_count,
        total_occurrences,
        sources,
        file_groups: groups.into_iter().map(|(name, pattern_count)| FileGroupInfo { name, pattern_count }).collect(),
        time_range,
    };

    Json(serde_json::to_value(&resp).unwrap()).into_response()
}

async fn patterns_handler(
    State(storage): State<SharedStorage>,
    Query(params): Query<PatternsQuery>,
) -> impl IntoResponse {
    let db = storage.lock().unwrap();
    let top = params.top.unwrap_or(100);

    let filter_vec: Option<Vec<String>> = params.filter.map(|f| {
        f.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    });
    let group_vec: Option<Vec<String>> = params.group.map(|g| {
        g.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    });

    let patterns = match db.get_all_patterns_ranked(top, filter_vec.as_deref(), group_vec.as_deref()) {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    };

    let pattern_ids: Vec<i64> = patterns.iter().map(|(id, _, _, _)| *id).collect();
    let distribution = db.get_pattern_trends(&pattern_ids).unwrap_or_default();
    let has_timestamps = db.has_timestamps().unwrap_or(false);
    let time_series = if has_timestamps {
        db.get_pattern_time_series(&pattern_ids, "auto").unwrap_or_default()
    } else {
        HashMap::new()
    };
    let all_tags = db.get_all_tags().unwrap_or_default();

    let mut results: Vec<PatternResponse> = Vec::with_capacity(patterns.len());
    for (id, template, count, group_name) in &patterns {
        let raw_samples = db.get_pattern_samples(*id, 5).unwrap_or_default();
        let samples: Vec<SampleEntry> = raw_samples.iter().map(|(vars, source_path)| {
            SampleEntry {
                text: reconstruct_entry(template, vars),
                source: Path::new(source_path)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or(source_path)
                    .to_string(),
            }
        }).collect();

        let dist: Vec<serde_json::Value> = distribution.get(id).map(|t| {
            t.iter().map(|(name, cnt)| serde_json::json!({"source": name, "count": cnt})).collect()
        }).unwrap_or_default();

        let trend: Vec<serde_json::Value> = if has_timestamps {
            time_series.get(id).map(|t| {
                t.iter().map(|(b, cnt)| serde_json::json!({"bucket": b, "count": cnt})).collect()
            }).unwrap_or_default()
        } else {
            dist.clone()
        };

        let tags = all_tags.get(id).cloned().unwrap_or_default();

        results.push(PatternResponse {
            id: *id,
            template: template.clone(),
            count: *count,
            group: group_name.clone(),
            samples,
            trend,
            distribution: dist,
            tags,
        });
    }

    Json(serde_json::json!({
        "has_timestamps": has_timestamps,
        "patterns": results,
    })).into_response()
}

async fn anomalies_handler(State(storage): State<SharedStorage>) -> impl IntoResponse {
    let db = storage.lock().unwrap();

    let patterns = match db.get_patterns_for_dedup(None) {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    };

    if patterns.is_empty() {
        return Json(serde_json::json!([])).into_response();
    }

    let pattern_ids: Vec<i64> = patterns.iter().map(|p| p.0).collect();
    let trends = db.get_pattern_trends(&pattern_ids).unwrap_or_default();
    let source_counts = db.get_source_counts_per_group().unwrap_or_default();

    let mut group_counts: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
    for (id, _, count, group) in &patterns {
        group_counts.entry(group.clone()).or_default().push((*id, *count));
    }
    let mut group_stats: HashMap<String, (f64, f64)> = HashMap::new();
    for (group, counts) in &group_counts {
        let n = counts.len() as f64;
        let mean = counts.iter().map(|(_, c)| *c as f64).sum::<f64>() / n;
        let variance = counts.iter().map(|(_, c)| (*c as f64 - mean).powi(2)).sum::<f64>() / n;
        group_stats.insert(group.clone(), (mean, variance.sqrt()));
    }

    let mut anomalies: Vec<AnomalyResponse> = Vec::new();
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
            anomalies.push(AnomalyResponse {
                id: *id,
                template: template.clone(),
                count: *count,
                group: group.clone(),
                anomaly_score: score,
                reasons,
            });
        }
    }

    anomalies.sort_by(|a, b| b.anomaly_score.partial_cmp(&a.anomaly_score).unwrap());
    anomalies.truncate(50);

    Json(serde_json::to_value(&anomalies).unwrap()).into_response()
}

async fn tags_handler(
    State(storage): State<SharedStorage>,
    Json(req): Json<TagsRequest>,
) -> impl IntoResponse {
    let db = storage.lock().unwrap();
    match db.set_tags(req.pattern_id, &req.tags) {
        Ok(()) => Json(serde_json::json!({"ok": true, "pattern_id": req.pattern_id, "tags": req.tags})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

async fn export_handler(
    State(storage): State<SharedStorage>,
    Query(params): Query<ExportQuery>,
) -> impl IntoResponse {
    let format = params.format.as_deref().unwrap_or("json");
    if format != "json" {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "only json export is supported"}))).into_response();
    }

    let db = storage.lock().unwrap();
    let patterns = match db.get_all_patterns_ranked(1000, None, None) {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    };

    let all_tags = db.get_all_tags().unwrap_or_default();
    let mut data = Vec::with_capacity(patterns.len());
    for (id, template, count, group) in &patterns {
        let samples = db.get_pattern_samples(*id, 5).unwrap_or_default();
        let tags = all_tags.get(id).cloned().unwrap_or_default();
        data.push(serde_json::json!({
            "id": id,
            "template": template,
            "count": count,
            "group": group,
            "tags": tags,
            "samples": samples.iter().map(|(vars, src)| {
                serde_json::json!({"text": reconstruct_entry(template, vars), "source": src})
            }).collect::<Vec<_>>(),
        }));
    }

    Json(serde_json::json!({"patterns": data})).into_response()
}

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
