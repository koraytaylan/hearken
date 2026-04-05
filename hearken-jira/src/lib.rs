pub mod client;
pub mod filter;
pub mod mapper;

use serde::Deserialize;
use std::fmt;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum JiraInstanceType {
    Cloud,
    Server,
}

impl fmt::Display for JiraInstanceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JiraInstanceType::Cloud => write!(f, "cloud"),
            JiraInstanceType::Server => write!(f, "server"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct JiraTomlConfig {
    pub url: String,
    pub project: String,
    pub label: String,
    #[serde(rename = "type")]
    pub instance_type: JiraInstanceType,
    pub issue_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JiraConfig {
    pub url: String,
    pub project: String,
    pub label: String,
    pub instance_type: JiraInstanceType,
    pub issue_type: String,
    pub user: String,
    pub token: String,
}

#[derive(Debug, thiserror::Error)]
pub enum JiraError {
    #[error("JIRA configuration error: {0}")]
    Config(String),
    #[error("JIRA API error: {status} - {message}")]
    Api { status: u16, message: String },
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Storage error: {0}")]
    Storage(#[from] hearken_storage::StorageError),
}

impl JiraConfig {
    pub fn from_toml_and_env(toml: JiraTomlConfig) -> Result<Self, JiraError> {
        let user = std::env::var("HEARKEN_JIRA_USER").map_err(|_| {
            JiraError::Config("HEARKEN_JIRA_USER environment variable not set".into())
        })?;
        let token = std::env::var("HEARKEN_JIRA_TOKEN").map_err(|_| {
            JiraError::Config("HEARKEN_JIRA_TOKEN environment variable not set".into())
        })?;
        let url = toml.url.trim_end_matches('/').to_string();
        Ok(Self {
            url,
            project: toml.project,
            label: toml.label,
            instance_type: toml.instance_type,
            issue_type: toml.issue_type.unwrap_or_else(|| "Bug".to_string()),
            user,
            token,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jira_toml_config_deserialize() {
        let toml_str = r#"
            url = "https://mycompany.atlassian.net"
            project = "OPS"
            label = "hearken"
            type = "cloud"
        "#;
        let config: JiraTomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.url, "https://mycompany.atlassian.net");
        assert_eq!(config.project, "OPS");
        assert_eq!(config.label, "hearken");
        assert_eq!(config.instance_type, JiraInstanceType::Cloud);
        assert_eq!(config.issue_type, None);
    }

    #[test]
    fn test_jira_toml_config_server() {
        let toml_str = r#"
            url = "https://jira.internal.com"
            project = "INFRA"
            label = "hearken-logs"
            type = "server"
            issue_type = "Task"
        "#;
        let config: JiraTomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.instance_type, JiraInstanceType::Server);
        assert_eq!(config.issue_type, Some("Task".to_string()));
    }

    #[test]
    fn test_jira_config_from_env() {
        unsafe {
            std::env::set_var("HEARKEN_JIRA_USER", "test@example.com");
            std::env::set_var("HEARKEN_JIRA_TOKEN", "secret-token");
        }
        let toml = JiraTomlConfig {
            url: "https://myco.atlassian.net/".to_string(),
            project: "OPS".to_string(),
            label: "hearken".to_string(),
            instance_type: JiraInstanceType::Cloud,
            issue_type: None,
        };
        let config = JiraConfig::from_toml_and_env(toml).unwrap();
        assert_eq!(config.url, "https://myco.atlassian.net");
        assert_eq!(config.issue_type, "Bug");
        assert_eq!(config.user, "test@example.com");
        assert_eq!(config.token, "secret-token");
        unsafe {
            std::env::remove_var("HEARKEN_JIRA_USER");
            std::env::remove_var("HEARKEN_JIRA_TOKEN");
        }
    }

    #[test]
    fn test_jira_config_missing_env() {
        unsafe {
            std::env::remove_var("HEARKEN_JIRA_USER");
            std::env::remove_var("HEARKEN_JIRA_TOKEN");
        }
        let toml = JiraTomlConfig {
            url: "https://myco.atlassian.net".to_string(),
            project: "OPS".to_string(),
            label: "hearken".to_string(),
            instance_type: JiraInstanceType::Cloud,
            issue_type: None,
        };
        let err = JiraConfig::from_toml_and_env(toml).unwrap_err();
        assert!(err.to_string().contains("HEARKEN_JIRA_USER"));
    }
}
