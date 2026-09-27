//! 只读 ACP 客户端与长连接池。
//!
//! 只读契约：客户端只发送 `initialize`、`session/list`、`session/load` 三个请求，
//! 初始化时不声明任何能力，agent 发来的权限申请一律拒绝。
//!
//! 连接模型：每个 agent 由一条长连接对应一个后台任务（actor），所有操作在该任务里串行执行，
//! 因此不需要在多个调用方之间做并发控制。连接空闲超时或被判定失效后会被丢弃，下次使用时重新建立。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    InitializeRequest, ListSessionsRequest, LoadSessionRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionNotification,
};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo, on_receive_notification, on_receive_request,
};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::config::AgentSpec;

/// 分页上限，防止翻页失控。
const MAX_LIST_PAGES: usize = 20;
/// 空闲回收任务的检查间隔。
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("agent {agent} 启动或初始化失败：{message}")]
    Start { agent: String, message: String },
    #[error("agent {agent} 不支持该能力：{message}")]
    Unsupported { agent: String, message: String },
    #[error("agent {agent} 操作超时（{seconds:.0} 秒）")]
    Timeout { agent: String, seconds: f64 },
    #[error("agent {agent} 的连接中断：{message}")]
    Transport { agent: String, message: String },
    #[error("agent {agent} 返回错误：{message}")]
    Remote { agent: String, message: String },
    #[error("agent {agent} 的响应不符合预期：{message}")]
    Protocol { agent: String, message: String },
}

impl AcpError {
    /// 连接是否已经不可用（用于决定要不要丢弃连接重试）。
    #[must_use]
    pub fn is_transport(&self) -> bool {
        matches!(self, AcpError::Transport { .. } | AcpError::Timeout { .. })
    }
}

/// 一条会话的元信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub agent: String,
    pub session_id: String,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub updated_at: Option<String>,
}

/// agent 的能力快照。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub protocol_version: i64,
    pub agent_name: Option<String>,
    pub agent_version: Option<String>,
    pub load_session: bool,
    pub can_list: bool,
    pub session_capabilities: Vec<String>,
}

enum Op {
    List {
        reply: oneshot::Sender<Result<Vec<SessionSummary>, AcpError>>,
    },
    Load {
        session_id: String,
        cwd: PathBuf,
        reply: oneshot::Sender<Result<Vec<Value>, AcpError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

struct AgentSession {
    tx: mpsc::Sender<Op>,
    join: JoinHandle<()>,
    capabilities: Capabilities,
    last_used: Instant,
    uses: u32,
}

/// 连接池里一条连接的对外状态。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolStatus {
    pub agent: String,
    pub uses: u32,
    pub idle_seconds: f64,
}

/// 长连接池。
pub struct AgentPool {
    timeout: Duration,
    idle_timeout: Duration,
    max_uses: u32,
    sessions: tokio::sync::Mutex<BTreeMap<String, AgentSession>>,
    sweeper: Mutex<Option<JoinHandle<()>>>,
}

impl AgentPool {
    #[must_use]
    pub fn new(timeout: Duration, idle_timeout: Duration, max_uses: u32) -> Self {
        Self {
            timeout,
            idle_timeout,
            max_uses,
            sessions: tokio::sync::Mutex::new(BTreeMap::new()),
            sweeper: Mutex::new(None),
        }
    }

    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// 启动空闲回收任务。
    pub fn start(self: &Arc<Self>) {
        let pool = Arc::clone(self);
        let mut slot = self.sweeper.lock();
        if slot.is_some() {
            return;
        }
        *slot = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(SWEEP_INTERVAL).await;
                pool.sweep().await;
            }
        }));
    }

    /// 关闭所有连接并停止回收任务。
    pub async fn close(&self) {
        let handle = self.sweeper.lock().take();
        if let Some(handle) = handle {
            handle.abort();
        }
        let sessions = {
            let mut guard = self.sessions.lock().await;
            std::mem::take(&mut *guard)
        };
        for (_, session) in sessions {
            shutdown(session).await;
        }
    }

    /// 各 agent 的连接状态。
    pub async fn status(&self) -> Vec<PoolStatus> {
        let guard = self.sessions.lock().await;
        let mut items: Vec<PoolStatus> = guard
            .iter()
            .map(|(name, session)| PoolStatus {
                agent: name.clone(),
                uses: session.uses,
                idle_seconds: session.last_used.elapsed().as_secs_f64(),
            })
            .collect();
        items.sort_by(|left, right| left.agent.cmp(&right.agent));
        items
    }

    async fn sweep(&self) {
        let stale: Vec<String> = {
            let guard = self.sessions.lock().await;
            guard
                .iter()
                .filter(|(_, session)| session.last_used.elapsed() >= self.idle_timeout)
                .map(|(name, _)| name.clone())
                .collect()
        };
        for name in stale {
            let session = {
                let mut guard = self.sessions.lock().await;
                guard.remove(&name)
            };
            if let Some(session) = session {
                println!("[pool] 关闭 {name} 连接（空闲超过 {} 秒）", self.idle_timeout.as_secs());
                shutdown(session).await;
            }
        }
    }

    /// 探测能力（必要时建立连接）。
    pub async fn capabilities(self: &Arc<Self>, spec: &AgentSpec) -> Result<Capabilities, AcpError> {
        self.ensure(spec).await.map(|session| session.capabilities.clone())
    }

    /// 枚举会话。
    pub async fn list(self: &Arc<Self>, spec: &AgentSpec) -> Result<Vec<SessionSummary>, AcpError> {
        let agent = spec.name.clone();
        if !self.ensure(spec).await?.capabilities.can_list {
            return Err(AcpError::Unsupported {
                agent,
                message: "未声明 sessionCapabilities.list，无法枚举会话".into(),
            });
        }
        self.execute(spec, |reply| Op::List { reply }).await
    }

    /// 重放一条会话，返回全部 `session/update` 事件。
    pub async fn load(
        self: &Arc<Self>,
        spec: &AgentSpec,
        session_id: &str,
        cwd: &Path,
    ) -> Result<Vec<Value>, AcpError> {
        let agent = spec.name.clone();
        if !self.ensure(spec).await?.capabilities.load_session {
            return Err(AcpError::Unsupported {
                agent,
                message: "未声明 loadSession，无法加载会话".into(),
            });
        }
        let session_id = session_id.to_string();
        let cwd = cwd.to_path_buf();
        self.execute(spec, |reply| Op::Load {
            session_id: session_id.clone(),
            cwd: cwd.clone(),
            reply,
        })
        .await
    }

    /// 取一条可用连接，必要时建立；失效的连接会被替换。
    async fn ensure(self: &Arc<Self>, spec: &AgentSpec) -> Result<AgentSession, AcpError> {
        let mut guard = self.sessions.lock().await;
        if let Some(session) = guard.get(&spec.name) {
            if session.uses < self.max_uses {
                let mut existing = guard.remove(&spec.name).expect("刚刚还在");
                existing.last_used = Instant::now();
                let snapshot = existing.snapshot();
                guard.insert(spec.name.clone(), existing);
                return Ok(snapshot);
            }
            println!(
                "[pool] 关闭 {} 连接（达到 {} 次使用上限，轮换）",
                spec.name, self.max_uses
            );
            let stale = guard.remove(&spec.name).expect("刚刚还在");
            drop(guard);
            shutdown(stale).await;
            guard = self.sessions.lock().await;
        }
        let (session, capabilities) = spawn_session(spec, self.timeout).await?;
        println!(
            "[pool] 建立 {} 连接（{}）",
            spec.name,
            capabilities.agent_name.as_deref().unwrap_or("未知实现")
        );
        guard.insert(spec.name.clone(), session);
        let snapshot = guard.get(&spec.name).expect("刚插入").snapshot();
        Ok(snapshot)
    }

    /// 在连接上执行一次操作；连接失效时丢弃并重试一次。
    async fn execute<T>(
        self: &Arc<Self>,
        spec: &AgentSpec,
        make_op: impl Fn(oneshot::Sender<Result<T, AcpError>>) -> Op,
    ) -> Result<T, AcpError> {
        for attempt in 1..=2 {
            let _ = self.ensure(spec).await?;
            let sender = {
                let guard = self.sessions.lock().await;
                guard.get(&spec.name).map(|session| session.tx.clone())
            };
            let Some(sender) = sender else {
                continue;
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            if sender.send(make_op(reply_tx)).await.is_err() {
                self.discard(&spec.name).await;
                continue;
            }
            let result = match tokio::time::timeout(self.timeout * 2, reply_rx).await {
                Ok(Ok(value)) => value,
                Ok(Err(_)) => Err(AcpError::Transport {
                    agent: spec.name.clone(),
                    message: "连接在响应前关闭".into(),
                }),
                Err(_) => Err(AcpError::Timeout {
                    agent: spec.name.clone(),
                    seconds: (self.timeout * 2).as_secs_f64(),
                }),
            };
            match result {
                Ok(value) => {
                    let mut guard = self.sessions.lock().await;
                    if let Some(session) = guard.get_mut(&spec.name) {
                        session.last_used = Instant::now();
                        session.uses += 1;
                    }
                    return Ok(value);
                }
                Err(error) => {
                    self.discard(&spec.name).await;
                    if attempt == 1 && error.is_transport() {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Err(AcpError::Transport {
            agent: spec.name.clone(),
            message: "连接反复失效".into(),
        })
    }

    async fn discard(&self, name: &str) {
        let session = {
            let mut guard = self.sessions.lock().await;
            guard.remove(name)
        };
        if let Some(session) = session {
            println!("[pool] 关闭 {name} 连接（失效或操作失败）");
            shutdown(session).await;
        }
    }
}

impl AgentSession {
    /// 复制一份用于返回的元信息（连接本身留在池里）。
    fn snapshot(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            join: tokio::spawn(async {}),
            capabilities: self.capabilities.clone(),
            last_used: self.last_used,
            uses: self.uses,
        }
    }
}

/// 关闭一条连接：先请求退出，超时后直接中止任务。
async fn shutdown(session: AgentSession) {
    let AgentSession { tx, join, .. } = session;
    let (reply_tx, reply_rx) = oneshot::channel();
    if tx.send(Op::Shutdown { reply: reply_tx }).await.is_ok() {
        let _ = tokio::time::timeout(Duration::from_secs(3), reply_rx).await;
    }
    join.abort();
}

/// 建立一条长连接并完成初始化。
async fn spawn_session(spec: &AgentSpec, timeout: Duration) -> Result<(AgentSession, Capabilities), AcpError> {
    let (ready_tx, ready_rx) = oneshot::channel::<Capabilities>();
    let (op_tx, mut op_rx) = mpsc::channel::<Op>(8);
    let sink: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));

    let mut command = spec.command.clone();
    if command.is_empty() {
        return Err(AcpError::Start {
            agent: spec.name.clone(),
            message: "命令为空".into(),
        });
    }
    let executable = command.remove(0);
    let agent = AcpAgent::new(AcpAgentConfig::new(executable).args(command));

    let notification_sink = Arc::clone(&sink);
    let name = spec.name.clone();
    let report_name = name.clone();
    let join = tokio::spawn(async move {
        let outcome = Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    let session_id = notification.session_id.to_string();
                    let value = serde_json::to_value(&notification.update).unwrap_or(Value::Null);
                    notification_sink.lock().push((session_id, value));
                    Ok(())
                },
                on_receive_notification!(),
            )
            .on_receive_request(
                async move |_request: RequestPermissionRequest, responder, _cx| {
                    // 只读客户端：不授予任何权限。
                    responder.respond(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled))
                },
                on_receive_request!(),
            )
            .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
                let init = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let capabilities = capabilities_from(&init);
                let can_list = capabilities.can_list;
                let load_session = capabilities.load_session;
                if ready_tx.send(capabilities).is_err() {
                    return Ok(());
                }
                while let Some(op) = op_rx.recv().await {
                    match op {
                        Op::List { reply } => {
                            let result = if can_list {
                                list_sessions(&connection, &name, timeout).await
                            } else {
                                Err(AcpError::Unsupported {
                                    agent: name.clone(),
                                    message: "未声明 sessionCapabilities.list，无法枚举会话".into(),
                                })
                            };
                            let _ = reply.send(result);
                        }
                        Op::Load {
                            session_id,
                            cwd,
                            reply,
                        } => {
                            let result = if load_session {
                                load_transcript(&connection, &sink, &name, &session_id, &cwd, timeout).await
                            } else {
                                Err(AcpError::Unsupported {
                                    agent: name.clone(),
                                    message: "未声明 loadSession，无法加载会话".into(),
                                })
                            };
                            let _ = reply.send(result);
                        }
                        Op::Shutdown { reply } => {
                            let _ = reply.send(());
                            break;
                        }
                    }
                }
                Ok(())
            })
            .await;
        if let Err(error) = outcome {
            eprintln!("[pool] {report_name} 连接结束：{error}");
        }
    });

    let capabilities = match tokio::time::timeout(timeout, ready_rx).await {
        Ok(Ok(capabilities)) => capabilities,
        Ok(Err(_)) => {
            join.abort();
            return Err(AcpError::Start {
                agent: spec.name.clone(),
                message: "连接在初始化完成前关闭".into(),
            });
        }
        Err(_) => {
            join.abort();
            return Err(AcpError::Start {
                agent: spec.name.clone(),
                message: format!("初始化超时（{:.0} 秒）", timeout.as_secs_f64()),
            });
        }
    };

    Ok((
        AgentSession {
            tx: op_tx,
            join,
            capabilities: capabilities.clone(),
            last_used: Instant::now(),
            uses: 0,
        },
        capabilities,
    ))
}

fn capabilities_from(init: &agent_client_protocol::schema::v1::InitializeResponse) -> Capabilities {
    let caps = &init.agent_capabilities;
    let mut session_capabilities: Vec<String> = Vec::new();
    if let Some(list) = &caps.session_capabilities.list {
        let _ = list;
        session_capabilities.push("list".into());
    }
    for (present, name) in [
        (caps.session_capabilities.delete.is_some(), "delete"),
        (caps.session_capabilities.resume.is_some(), "resume"),
        (caps.session_capabilities.close.is_some(), "close"),
        (
            caps.session_capabilities.additional_directories.is_some(),
            "additionalDirectories",
        ),
    ] {
        if present {
            session_capabilities.push(name.into());
        }
    }
    session_capabilities.sort();
    Capabilities {
        protocol_version: i64::from(init.protocol_version.as_u16()),
        agent_name: init.agent_info.as_ref().map(|info| info.name.clone()),
        agent_version: init.agent_info.as_ref().map(|info| info.version.clone()),
        load_session: caps.load_session,
        can_list: caps.session_capabilities.list.is_some(),
        session_capabilities,
    }
}

/// 分页枚举会话。
async fn list_sessions(
    connection: &ConnectionTo<Agent>,
    agent: &str,
    timeout: Duration,
) -> Result<Vec<SessionSummary>, AcpError> {
    let mut summaries = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for _ in 0..MAX_LIST_PAGES {
        let request = match &cursor {
            Some(value) => ListSessionsRequest::new().cursor(value.clone()),
            None => ListSessionsRequest::new(),
        };
        let response = tokio::time::timeout(timeout, connection.send_request(request).block_task())
            .await
            .map_err(|_| AcpError::Timeout {
                agent: agent.to_string(),
                seconds: timeout.as_secs_f64(),
            })?
            .map_err(|error| AcpError::Remote {
                agent: agent.to_string(),
                message: error.to_string(),
            })?;
        for item in &response.sessions {
            summaries.push(SessionSummary {
                agent: agent.to_string(),
                session_id: item.session_id.to_string(),
                cwd: item.cwd.clone(),
                title: item.title.clone(),
                updated_at: item.updated_at.clone(),
            });
        }
        match response.next_cursor.clone() {
            None => return Ok(summaries),
            Some(next) if seen.insert(next.clone()) => cursor = Some(next),
            Some(_) => {
                return Err(AcpError::Protocol {
                    agent: agent.to_string(),
                    message: "返回了重复的翻页游标".into(),
                });
            }
        }
    }
    Err(AcpError::Protocol {
        agent: agent.to_string(),
        message: format!("会话翻页超过 {MAX_LIST_PAGES} 页"),
    })
}

/// 加载会话并收集重放事件。
async fn load_transcript(
    connection: &ConnectionTo<Agent>,
    sink: &Arc<Mutex<Vec<(String, Value)>>>,
    agent: &str,
    session_id: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<Vec<Value>, AcpError> {
    sink.lock().clear();
    tokio::time::timeout(
        timeout,
        connection
            .send_request(LoadSessionRequest::new(session_id.to_string(), cwd.to_path_buf()))
            .block_task(),
    )
    .await
    .map_err(|_| AcpError::Timeout {
        agent: agent.to_string(),
        seconds: timeout.as_secs_f64(),
    })?
    .map_err(|error| AcpError::Remote {
        agent: agent.to_string(),
        message: error.to_string(),
    })?;
    // 协议要求 agent 在响应 session/load 之前完成全部重放，实测各家都满足；
    // 这里只取属于该会话的事件，避免把别的会话的通知混进来。
    let collected = std::mem::take(&mut *sink.lock());
    Ok(collected
        .into_iter()
        .filter(|(id, _)| id == session_id)
        .map(|(_, value)| value)
        .collect())
}
