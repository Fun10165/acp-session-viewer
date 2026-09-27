//! HTTP 客户端：解析服务地址与令牌，调用服务端接口。

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{self, ConfigError};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("无法连接服务端 {url}（{source}）；请先运行 acp-view serve")]
    Connect {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("服务端返回 {status}：{message}")]
    Status { status: u16, message: String },
    #[error("请求失败：{0}")]
    Transport(#[from] reqwest::Error),
    #[error("服务端返回的不是 JSON 对象：{0}")]
    NotJson(String),
    #[error("{0}")]
    Config(#[from] ConfigError),
}

impl ClientError {
    /// 是不是「没有找到」这类错误（命令行用退出码 2 表示）。
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, ClientError::Status { status: 404, .. })
    }
}

/// 客户端配置。
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub base_url: String,
    pub token: String,
}

/// 按 参数 → 环境变量 → 默认值 的顺序确定服务地址与令牌。
pub fn resolve_config(server: Option<&str>, token: Option<&str>, token_file: Option<&Path>) -> Result<ClientConfig, ClientError> {
    let base_url = server
        .map(str::to_string)
        .or_else(|| std::env::var("ACP_VIEW_SERVER").ok())
        .unwrap_or_else(|| config::DEFAULT_SERVER_URL.to_string());
    let base_url = base_url.trim().trim_end_matches('/').to_string();
    let base_url = if base_url.starts_with("http://") || base_url.starts_with("https://") {
        base_url
    } else {
        format!("http://{base_url}")
    };
    let path = match token_file {
        Some(path) => path.to_path_buf(),
        None => default_token_path()?,
    };
    let token = resolve_token(token, &path)?;
    Ok(ClientConfig { base_url, token })
}

/// 默认令牌文件位置。
pub fn default_token_path() -> Result<PathBuf, ConfigError> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| config::home_dir().map(|home| home.join(".config")))
        .ok_or_else(|| ConfigError::NoHome("~/.config".into()))?;
    Ok(base.join("acp-session-viewer").join("token"))
}

/// 令牌解析：参数 → 环境变量 → 令牌文件。
pub fn resolve_token(explicit: Option<&str>, path: &Path) -> Result<String, ClientError> {
    if let Some(token) = explicit.filter(|value| !value.is_empty()) {
        return Ok(token.to_string());
    }
    if let Ok(token) = std::env::var("ACP_VIEW_TOKEN") {
        if !token.is_empty() {
            return Ok(token);
        }
    }
    if path.is_file() {
        let token = std::fs::read_to_string(path)
            .map_err(|source| ClientError::Config(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }))?
            .trim()
            .to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    Err(ClientError::Config(ConfigError::MissingToken { path: path.to_path_buf() }))
}

/// 轻量 API 客户端。
pub struct ApiClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl ApiClient {
    #[must_use]
    pub fn new(config: &ClientConfig) -> Self {
        Self {
            base_url: config.base_url.clone(),
            token: config.token.clone(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .unwrap_or_default(),
        }
    }

    async fn get(&self, path: &str, params: &[(String, String)]) -> Result<(u16, String), ClientError> {
        let mut url = format!("{}{path}", self.base_url);
        if !params.is_empty() {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            for (key, value) in params {
                serializer.append_pair(key, value);
            }
            url.push('?');
            url.push_str(&serializer.finish());
        }
        let response = self
            .http
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", self.token))
            .send()
            .await
            .map_err(|source| {
                if source.is_connect() {
                    ClientError::Connect {
                        url: self.base_url.clone(),
                        source,
                    }
                } else {
                    ClientError::Transport(source)
                }
            })?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        Ok((status, text))
    }

    /// 调用接口并解析 JSON。
    pub async fn get_json(&self, path: &str, params: &[(String, String)]) -> Result<Value, ClientError> {
        let (status, text) = self.get(path, params).await?;
        if status != 200 {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| value.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| text.chars().take(200).collect());
            return Err(ClientError::Status { status, message });
        }
        serde_json::from_str(&text).map_err(|_| ClientError::NotJson(text.chars().take(200).collect()))
    }

    /// 调用接口并取纯文本。
    pub async fn get_text(&self, path: &str, params: &[(String, String)]) -> Result<String, ClientError> {
        let (status, text) = self.get(path, params).await?;
        if status != 200 {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| value.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| text.chars().take(200).collect());
            return Err(ClientError::Status { status, message });
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 令牌优先级() {
        let dir = std::env::temp_dir().join(format!("acp-view-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        // 参数优先
        assert_eq!(resolve_token(Some("显式"), &path).unwrap(), "显式");
        // 文件兜底
        std::fs::write(&path, "文件令牌\n").unwrap();
        // 进程级环境变量在多线程测试里共享，这里显式清掉以便验证文件兜底。
        unsafe { std::env::remove_var("ACP_VIEW_TOKEN") };
        assert_eq!(resolve_token(None, &path).unwrap(), "文件令牌");
        // 缺失时显式报错
        std::fs::remove_file(&path).unwrap();
        assert!(resolve_token(None, &path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn 服务地址补全协议前缀() {
        let config = resolve_config(Some("127.0.0.1:9000"), Some("t"), None).unwrap();
        assert_eq!(config.base_url, "http://127.0.0.1:9000");
        let config = resolve_config(Some("http://example.com/"), Some("t"), None).unwrap();
        assert_eq!(config.base_url, "http://example.com");
    }
}
