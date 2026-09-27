//! HTTP 服务端：带令牌鉴权的只读查询接口。
//!
//! 接口一览（全部需要 `Authorization: Bearer <token>`）：
//!
//! * `GET /health`：版本与连接池状态
//! * `GET /agents`：各 agent 的能力
//! * `GET /sessions?agent=&query=&limit=`：会话列表（只含通过策略的会话）
//! * `GET /transcript?selector=&agent=&cwd=&format=json|text`：重放一条会话
//! * `GET /policy/explain?agent=&sessionId=`：解释一条会话的可见性判定

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Extension, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::acl::Principal;
use crate::render::{RenderOptions, apply_budget, render_transcript};
use crate::service::{Service, ServiceError};

/// 应用状态。
#[derive(Clone)]
pub struct AppState {
    pub service: Arc<Service>,
    /// 令牌与对应主体的映射。
    pub principals: Arc<Vec<(String, Principal)>>,
    pub log_requests: bool,
}

/// 请求来源地址，供服务层写访问记录。
#[derive(Clone, Debug)]
pub struct RemoteAddr(pub String);

/// 一次请求里解析出来的查询参数。
pub struct Params(Vec<(String, String)>);

impl Params {
    #[must_use]
    pub fn new(raw: Option<String>) -> Self {
        let pairs = raw
            .map(|text| {
                url::form_urlencoded::parse(text.as_bytes())
                    .into_owned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Self(pairs)
    }

    /// 取一个参数；同名多次出现时取第一次。
    #[must_use]
    pub fn one(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .filter(|value| !value.is_empty())
    }

    /// 取多值参数，同时支持重复出现与逗号分隔。
    #[must_use]
    pub fn many(&self, key: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(name, _)| name == key)
            .flat_map(|(_, value)| value.split(','))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect()
    }

    pub fn usize_value(&self, key: &str, default: usize, min: usize, max: usize) -> Result<usize, ServiceError> {
        match self.one(key) {
            None => Ok(default),
            Some(text) => {
                let value: usize = text
                    .parse()
                    .map_err(|_| ServiceError::BadRequest(format!("参数 {key} 必须是整数: {text:?}")))?;
                if value < min || value > max {
                    return Err(ServiceError::BadRequest(format!(
                        "参数 {key} 超出范围 {min}-{max}: {value}"
                    )));
                }
                Ok(value)
            }
        }
    }

    pub fn bool_value(&self, key: &str, default: bool) -> Result<bool, ServiceError> {
        match self.one(key) {
            None => Ok(default),
            Some(text) => match text.to_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Ok(true),
                "0" | "false" | "no" | "off" => Ok(false),
                other => Err(ServiceError::BadRequest(format!("参数 {key} 必须是布尔值: {other:?}"))),
            },
        }
    }
}

fn error_response(error: &ServiceError) -> Response {
    let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(json!({ "error": error.to_string() }))).into_response()
}

fn timestamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{}", now.as_secs())
}

/// 令牌鉴权中间件：逐个令牌定长比较，命中后把主体放进请求扩展。
async fn require_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string();
    let actual = provided.as_bytes();
    let matched = state.principals.iter().find(|(token, _)| {
        let expected = token.as_bytes();
        actual.len() == expected.len() && bool::from(actual.ct_eq(expected))
    });
    match matched {
        Some((_, principal)) => {
            request.extensions_mut().insert(principal.clone());
            next.run(request).await
        }
        None => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "缺少或错误的令牌：请提供 Authorization: Bearer <token>" })),
        )
            .into_response(),
    }
}

/// 访问日志中间件：每个请求一行。
async fn access_log(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    request
        .extensions_mut()
        .insert(RemoteAddr(addr.ip().to_string()));
    let started = Instant::now();
    let method = request.method().clone();
    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_default();
    let response = next.run(request).await;
    if state.log_requests {
        println!(
            "[{}] {} {method} {path} → {} {}ms",
            timestamp(),
            addr.ip(),
            response.status().as_u16(),
            started.elapsed().as_millis()
        );
    }
    response
}

async fn health(State(state): State<AppState>) -> Response {
    let connections = state.service.pool().status().await;
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "connections": connections,
    }))
    .into_response()
}

async fn agents(State(state): State<AppState>) -> Response {
    let reports = state.service.agents().await;
    Json(json!({ "agents": reports })).into_response()
}

async fn sessions(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
    Extension(RemoteAddr(remote)): Extension<RemoteAddr>,
    Extension(principal): Extension<Principal>,
) -> Response {
    let params = Params::new(raw);
    let result = async {
        let names = params.many("agent");
        let limit = params.usize_value("limit", 30, 1, 10_000)?;
        let query = params.one("query");
        state
            .service
            .sessions(&names, query.as_deref(), limit, Some(&remote), &principal)
            .await
    }
    .await;
    match result {
        Ok(report) => Json(report).into_response(),
        Err(error) => error_response(&error),
    }
}

async fn transcript(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
    Extension(RemoteAddr(remote)): Extension<RemoteAddr>,
    Extension(principal): Extension<Principal>,
) -> Response {
    let params = Params::new(raw);
    let result = async {
        let selector = params
            .one("selector")
            .ok_or_else(|| ServiceError::BadRequest("缺少 selector 参数".into()))?;
        let output_format = params.one("format").unwrap_or_else(|| "json".into());
        if output_format != "json" && output_format != "text" {
            return Err(ServiceError::BadRequest(format!(
                "未知 format: {output_format:?}，可用 json 或 text"
            )));
        }
        let names = params.many("agent");
        let cwd = params.one("cwd");
        let report = state
            .service
            .transcript(&selector, &names, cwd.as_deref(), Some(&remote), &principal)
            .await?;
        if output_format == "json" {
            return Ok(Json(serde_json::to_value(&report).unwrap_or(Value::Null)).into_response());
        }
        let options = RenderOptions {
            show_thoughts: !params.bool_value("no_thoughts", false)?,
            show_meta: params.bool_value("meta", false)?,
            truncate: params.usize_value("truncate", 400, 0, 1_000_000)?,
        };
        let text = render_transcript(&report.updates, &options);
        let mut header = format!(
            "# {} / {} / {}（{} 条事件）\n# 可见性：{}",
            report.agent,
            report.session_id,
            report.cwd,
            report.updates.len(),
            report.decision
        );
        if let Some(note) = &report.note {
            header.push_str(&format!("\n# {note}"));
        }
        let body = if text.is_empty() {
            format!("{header}\n（该会话没有可渲染的事件）")
        } else {
            format!("{header}\n{text}")
        };
        let budget = params.usize_value("budget", 12_000, 0, 10_000_000)?;
        let (bounded, _dropped) = apply_budget(&body, budget);
        Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], bounded).into_response())
    }
    .await;
    match result {
        Ok(response) => response,
        Err(error) => error_response(&error),
    }
}

async fn explain(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
    Extension(principal): Extension<Principal>,
) -> Response {
    let params = Params::new(raw);
    let result = async {
        let agent = params
            .one("agent")
            .ok_or_else(|| ServiceError::BadRequest("缺少 agent 参数".into()))?;
        let session_id = params
            .one("sessionId")
            .ok_or_else(|| ServiceError::BadRequest("缺少 sessionId 参数".into()))?;
        state.service.explain(&agent, &session_id, &principal)
    }
    .await;
    match result {
        Ok(report) => Json(report).into_response(),
        Err(error) => error_response(&error),
    }
}

/// 组装路由。
pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/agents", get(agents))
        .route("/sessions", get(sessions))
        .route("/transcript", get(transcript))
        .route("/policy/explain", get(explain))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .layer(middleware::from_fn_with_state(state.clone(), access_log))
        .with_state(state)
}

/// 启动服务。
pub async fn run_server(state: AppState, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        build_app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 查询参数解析() {
        let params = Params::new(Some("agent=omp,opencode&agent=codex&limit=5&query=acp".into()));
        assert_eq!(params.many("agent"), vec!["omp", "opencode", "codex"]);
        assert_eq!(params.one("limit").as_deref(), Some("5"));
        assert_eq!(params.one("query").as_deref(), Some("acp"));
        assert_eq!(params.usize_value("limit", 30, 1, 100).unwrap(), 5);
        assert_eq!(params.usize_value("缺失", 30, 1, 100).unwrap(), 30);
        assert!(params.usize_value("query", 30, 1, 100).is_err());
        assert!(params.bool_value("query", false).is_err());
        assert!(params.bool_value("缺失", true).unwrap());
    }

    #[test]
    fn 空参数视为未提供() {
        let params = Params::new(Some("query=&agent=".into()));
        assert!(params.one("query").is_none());
        assert!(params.many("agent").is_empty());
    }

    #[test]
    fn 来源地址扩展可取出() {
        let addr = RemoteAddr("127.0.0.1".into());
        assert_eq!(addr.0, "127.0.0.1");
    }
}
