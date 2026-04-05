use crate::{JiraConfig, JiraError, JiraInstanceType};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct SearchResponse {
    issues: Vec<JiraIssue>,
    total: i64,
    #[serde(rename = "startAt")]
    _start_at: i64,
    #[serde(rename = "maxResults")]
    max_results: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JiraIssue {
    pub key: String,
    pub fields: JiraIssueFields,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JiraIssueFields {
    pub summary: Option<String>,
    pub description: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CreateIssueResponse {
    key: String,
}

pub struct JiraClient {
    config: JiraConfig,
    http: reqwest::Client,
    api_base: String,
}

fn base64_encode(input: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut result = String::new();
    let mut i = 0;

    while i < bytes.len() {
        let b0 = bytes[i] as u32;
        let b1 = if i + 1 < bytes.len() {
            bytes[i + 1] as u32
        } else {
            0
        };
        let b2 = if i + 2 < bytes.len() {
            bytes[i + 2] as u32
        } else {
            0
        };

        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        result.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);

        if i + 1 < bytes.len() {
            result.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        if i + 2 < bytes.len() {
            result.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        i += 3;
    }

    result
}

impl JiraClient {
    pub fn new(config: JiraConfig) -> Result<Self, JiraError> {
        let api_base = match config.instance_type {
            JiraInstanceType::Cloud => format!("{}/rest/api/3", config.url),
            JiraInstanceType::Server => format!("{}/rest/api/2", config.url),
        };

        let auth_header = match config.instance_type {
            JiraInstanceType::Cloud => {
                let credentials = format!("{}:{}", config.user, config.token);
                format!("Basic {}", base64_encode(&credentials))
            }
            JiraInstanceType::Server => {
                format!("Bearer {}", config.token)
            }
        };

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&auth_header)
                .map_err(|e| JiraError::Config(format!("Invalid auth header: {}", e)))?,
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(JiraError::Http)?;

        Ok(Self {
            config,
            http,
            api_base,
        })
    }

    pub async fn search_issues(&self, jql: &str) -> Result<Vec<JiraIssue>, JiraError> {
        const MAX_RESULTS: i64 = 50;
        let mut start_at: i64 = 0;
        let mut all_issues: Vec<JiraIssue> = Vec::new();

        loop {
            let body = serde_json::json!({
                "jql": jql,
                "startAt": start_at,
                "maxResults": MAX_RESULTS,
                "fields": ["summary", "description"]
            });

            let url = format!("{}/search", self.api_base);
            let response = self
                .http
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(JiraError::Http)?;

            let status = response.status();

            if status.as_u16() == 429 {
                // Respect Retry-After header
                let retry_after = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(60);
                tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
                continue;
            }

            if !status.is_success() {
                let message = response.text().await.unwrap_or_default();
                return Err(JiraError::Api {
                    status: status.as_u16(),
                    message,
                });
            }

            let search_response: SearchResponse = response.json().await.map_err(JiraError::Http)?;

            let fetched = search_response.issues.len() as i64;
            all_issues.extend(search_response.issues);

            start_at += fetched;

            if start_at >= search_response.total || fetched < search_response.max_results {
                break;
            }
        }

        Ok(all_issues)
    }

    pub async fn fetch_hearken_tickets(&self) -> Result<Vec<JiraIssue>, JiraError> {
        let jql = format!(
            "project = \"{}\" AND labels = \"{}\"",
            self.config.project, self.config.label
        );
        self.search_issues(&jql).await
    }

    pub async fn create_issue(
        &self,
        summary: &str,
        description: serde_json::Value,
        label: &str,
        issue_type: &str,
    ) -> Result<String, JiraError> {
        let body = serde_json::json!({
            "fields": {
                "project": { "key": self.config.project },
                "summary": summary,
                "description": description,
                "labels": [label],
                "issuetype": { "name": issue_type }
            }
        });

        let url = format!("{}/issue", self.api_base);
        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(JiraError::Http)?;

        let status = response.status();
        if !status.is_success() {
            let message = response.text().await.unwrap_or_default();
            return Err(JiraError::Api {
                status: status.as_u16(),
                message,
            });
        }

        let create_response: CreateIssueResponse =
            response.json().await.map_err(JiraError::Http)?;
        Ok(create_response.key)
    }

    pub async fn update_issue_description(
        &self,
        issue_key: &str,
        description: serde_json::Value,
    ) -> Result<(), JiraError> {
        let body = serde_json::json!({
            "fields": {
                "description": description
            }
        });

        let url = format!("{}/issue/{}", self.api_base, issue_key);
        let response = self
            .http
            .put(&url)
            .json(&body)
            .send()
            .await
            .map_err(JiraError::Http)?;

        let status = response.status();
        if !status.is_success() {
            let message = response.text().await.unwrap_or_default();
            return Err(JiraError::Api {
                status: status.as_u16(),
                message,
            });
        }

        Ok(())
    }

    pub async fn add_comment(
        &self,
        issue_key: &str,
        body_content: serde_json::Value,
    ) -> Result<(), JiraError> {
        let body = serde_json::json!({
            "body": body_content
        });

        let url = format!("{}/issue/{}/comment", self.api_base, issue_key);
        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(JiraError::Http)?;

        let status = response.status();
        if !status.is_success() {
            let message = response.text().await.unwrap_or_default();
            return Err(JiraError::Api {
                status: status.as_u16(),
                message,
            });
        }

        Ok(())
    }

    pub fn config(&self) -> &JiraConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode("user:token"), "dXNlcjp0b2tlbg==");
        assert_eq!(
            base64_encode("test@example.com:abc123"),
            "dGVzdEBleGFtcGxlLmNvbTphYmMxMjM="
        );
    }
}
