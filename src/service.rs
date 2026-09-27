//! 服务层：把连接池、台账与可见性策略组装成查询接口。
//!
//! 三条不变量：
//!
//! * 对外只暴露通过策略的会话；未通过的会话在列表里不出现，加载时按「没有找到」处理，
//!   不泄露它是否存在。
//! * 每条会话的判定与访问都会写进台账，便于事后核对。
//! * 单个 agent 失败只记入 errors，不影响其他 agent。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use tokio::task::JoinSet;

use crate::acp::{AcpError, AgentPool, SessionSummary};
use crate::acl::{Access as AclAccess, ObjectAcl, Principal, check as acl_check};
use crate::catalog::{AccessEntry, Catalog, CatalogError, GroupUpdate};
use crate::config::{AgentSpec, Config};
use crate::locate;
use crate::policy::{Decision, Policy, PolicyMode, SessionFacts, TimeWindow, format_time};

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("未知 agent {0}")]
    UnknownAgent(String),
    #[error("没有找到匹配 {selector:?} 的会话{detail}")]
    NotFound { selector: String, detail: String },
    #[error("该会话未通过可见性策略")]
    Denied,
    #[error("参数错误：{0}")]
    BadRequest(String),
    #[error("{0}")]
    Agent(#[from] AcpError),
    #[error("台账操作失败：{0}")]
    Catalog(#[from] CatalogError),
}

impl ServiceError {
    /// 对应的 HTTP 状态码；被策略拒绝的会话按「没有找到」返回，不透露其存在。
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            ServiceError::UnknownAgent(_) | ServiceError::BadRequest(_) => 400,
            ServiceError::NotFound { .. } | ServiceError::Denied => 404,
            ServiceError::Agent(_) => 502,
            ServiceError::Catalog(_) => 500,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentReport {
    pub name: String,
    pub implementation: String,
    pub description: String,
    pub protocol_version: i64,
    pub can_list: bool,
    pub can_load: bool,
    pub session_capabilities: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub agent: String,
    pub session_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
    pub first_seen: String,
    pub decision: String,
    pub rule: Option<String>,
    pub acl_owner: String,
    pub acl_group: Option<String>,
    pub acl_mode: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentFailure {
    pub agent: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionsReport {
    /// 当前的判定模式：rules 或 time-window。
    pub mode: String,
    pub sessions: Vec<SessionView>,
    pub total: usize,
    /// 被可见性策略隐藏的条数。
    pub hidden: usize,
    /// 通过了策略但被权限位挡住的条数。
    pub acl_hidden: usize,
    pub errors: Vec<AgentFailure>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptReport {
    pub agent: String,
    pub session_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub matched: usize,
    pub note: Option<String>,
    pub decision: String,
    pub rule: Option<String>,
    pub updates: Vec<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleExplanation {
    pub effect: String,
    pub name: String,
    pub matched: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainReport {
    pub agent: String,
    pub session_id: String,
    pub decision: String,
    pub rule: Option<String>,
    pub reason: String,
    pub first_seen: String,
    pub last_seen: String,
    pub remembered_decision: Option<String>,
    pub file_path: Option<String>,
    pub file_available: bool,
    pub rules: Vec<RuleExplanation>,
    /// 权限属性与本次请求者的判定结果。
    pub acl_owner: String,
    pub acl_group: Option<String>,
    pub acl_mode: String,
    pub acl_class: String,
    pub acl_allowed: bool,
    pub acl_reason: String,
}

struct Evaluated {
    view: SessionView,
    /// 是否通过可见性策略。
    policy_allowed: bool,
    /// 是否通过权限位（列表可见）。
    acl_allowed: bool,
    decision: Decision,
    rule: Option<String>,
    reason: String,
    acl_reason: String,
}

impl Evaluated {
    fn visible(&self) -> bool {
        self.policy_allowed && self.acl_allowed
    }
}

fn parse_optional_time(text: Option<&str>) -> Option<DateTime<Utc>> {
    text.and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

/// 选择器匹配：精确 ID → ID 前缀 → 标题/ID/目录/agent 子串；`last` 取最新一条。
#[must_use]
pub fn match_summaries(summaries: &[SessionSummary], selector: &str) -> Vec<SessionSummary> {
    let newest_first = |mut items: Vec<SessionSummary>| {
        items.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        items
    };
    if selector == "last" {
        return newest_first(summaries.to_vec()).into_iter().take(1).collect();
    }
    let exact: Vec<SessionSummary> = summaries
        .iter()
        .filter(|item| item.session_id == selector)
        .cloned()
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let lowered = selector.to_lowercase();
    let prefixed: Vec<SessionSummary> = summaries
        .iter()
        .filter(|item| item.session_id.to_lowercase().starts_with(&lowered))
        .cloned()
        .collect();
    if !prefixed.is_empty() {
        return newest_first(prefixed);
    }
    let contains: Vec<SessionSummary> = summaries
        .iter()
        .filter(|item| {
            item.title.as_deref().unwrap_or_default().to_lowercase().contains(&lowered)
                || item.session_id.to_lowercase().contains(&lowered)
                || item.cwd.to_string_lossy().to_lowercase().contains(&lowered)
                || item.agent.to_lowercase().contains(&lowered)
        })
        .cloned()
        .collect();
    newest_first(contains)
}

/// 服务主体。
pub struct Service {
    config: Config,
    policy: Policy,
    mode: PolicyMode,
    window: TimeWindow,
    catalog: Catalog,
    pool: Arc<AgentPool>,
    current_uid: u32,
    /// 新会话的默认权限。
    default_acl: ObjectAcl,
    /// 配置里对具体会话的权限覆盖（键为 `agent/会话 ID`）。
    acl_overrides: std::collections::BTreeMap<String, ObjectAcl>,
    /// 会话文件索引缓存：agent → （建立时间, 文件名与路径）。
    file_indexes: parking_lot::Mutex<std::collections::BTreeMap<String, (std::time::Instant, Arc<Vec<(String, PathBuf)>>)>>,
}

impl Service {
    /// 按配置装配服务（会打开台账）。
    pub fn new(config: Config) -> Result<Self, ServiceError> {
        let policy = Policy::compile(&config.policy).map_err(|error| {
            ServiceError::BadRequest(format!("可见性策略不合法：{error}"))
        })?;
        let mode = config.policy.mode;
        let window = TimeWindow::compile(&config.policy.window).map_err(|error| {
            ServiceError::BadRequest(format!("时间段窗口配置不合法：{error}"))
        })?;
        let catalog = Catalog::open(&config.server.catalog_path().map_err(|error| {
            ServiceError::BadRequest(error.to_string())
        })?)?;
        let pool = Arc::new(AgentPool::new(
            std::time::Duration::from_secs_f64(config.server.timeout_secs),
            std::time::Duration::from_secs_f64(config.server.idle_timeout_secs),
            config.server.max_uses_per_connection,
        ));
        let current_uid = {
            use std::os::unix::fs::MetadataExt as _;
            std::fs::metadata(".").map(|meta| meta.uid()).unwrap_or(0)
        };
        let default_owner = Config::default_principal_name();
        let default_acl = config
            .acl
            .default
            .resolve(&default_owner, 0o750)
            .map_err(|error| ServiceError::BadRequest(error.to_string()))?;
        let mut acl_overrides = std::collections::BTreeMap::new();
        for (key, template) in &config.acl.sessions {
            let acl = template
                .resolve(&default_owner, 0o750)
                .map_err(|error| ServiceError::BadRequest(error.to_string()))?;
            acl_overrides.insert(key.clone(), acl);
        }
        Ok(Self {
            config,
            policy,
            mode,
            window,
            catalog,
            pool,
            current_uid,
            default_acl,
            acl_overrides,
            file_indexes: parking_lot::Mutex::new(std::collections::BTreeMap::new()),
        })
    }

    /// 当前判定模式，供接口与命令展示。
    #[must_use]
    pub fn mode(&self) -> PolicyMode {
        self.mode
    }

    /// 某个 agent 的会话文件索引（文件名 → 路径），60 秒内复用。
    fn file_index(&self, agent: &str) -> Arc<Vec<(String, PathBuf)>> {
        const TTL: std::time::Duration = std::time::Duration::from_secs(60);
        {
            let guard = self.file_indexes.lock();
            if let Some((built, index)) = guard.get(agent) {
                if built.elapsed() < TTL {
                    return Arc::clone(index);
                }
            }
        }
        let patterns = self.config.server.locate.get(agent).cloned().unwrap_or_default();
        let mut entries: Vec<(String, PathBuf)> = Vec::new();
        for pattern in &patterns {
            // 把 {id} 换成通配符，一次遍历建好整个索引，避免每条会话都走一遍目录。
            let expanded = locate::index_pattern(pattern);
            let Ok(expanded) = crate::config::expand_home(Path::new(&expanded)) else {
                continue;
            };
            let Ok(paths) = glob::glob(&expanded.to_string_lossy()) else {
                continue;
            };
            for path in paths.flatten() {
                if !path.is_file() {
                    continue;
                }
                let name = path
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default();
                entries.push((name, path));
            }
        }
        let index = Arc::new(entries);
        self.file_indexes
            .lock()
            .insert(agent.to_string(), (std::time::Instant::now(), Arc::clone(&index)));
        index
    }

    /// 找到会话文件并读出它的属主名字。
    fn owner_from_file(&self, agent: &str, session_id: &str, cached: Option<&Path>) -> Option<(PathBuf, String)> {
        let path = match cached {
            Some(path) if path.is_file() => path.to_path_buf(),
            _ => {
                let index = self.file_index(agent);
                index
                    .iter()
                    .find(|(name, _)| name.contains(session_id))
                    .map(|(_, path)| path.clone())?
            }
        };
        use std::os::unix::fs::MetadataExt as _;
        let metadata = std::fs::metadata(&path).ok()?;
        let owner = crate::system::user_name(metadata.uid())?;
        Some((path, owner))
    }

    /// 取会话权限：配置覆盖优先，其次是台账里记录的值，最后是默认模板。
    fn acl_for(&self, agent: &str, session_id: &str, recorded: Option<&ObjectAcl>) -> ObjectAcl {
        let key = format!("{agent}/{session_id}");
        if let Some(acl) = self.acl_overrides.get(&key) {
            return acl.clone();
        }
        recorded.cloned().unwrap_or_else(|| self.default_acl.clone())
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    #[must_use]
    pub fn pool(&self) -> &Arc<AgentPool> {
        &self.pool
    }

    /// 启动连接池的空闲回收。
    pub fn start(&self) {
        self.pool.start();
    }

    /// 关闭连接池。
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// 按名称筛选 agent。
    pub fn specs(&self, names: &[String]) -> Result<Vec<AgentSpec>, ServiceError> {
        if names.is_empty() {
            return Ok(self.config.agent.clone());
        }
        let mut selected = Vec::new();
        for name in names {
            let spec = self
                .config
                .agent(name)
                .ok_or_else(|| ServiceError::UnknownAgent(name.clone()))?;
            selected.push(spec.clone());
        }
        Ok(selected)
    }

    /// 各 agent 的能力。
    pub async fn agents(self: &Arc<Self>) -> Vec<AgentReport> {
        let mut set = JoinSet::new();
        for spec in self.config.agent.clone() {
            let service = Arc::clone(self);
            set.spawn(async move {
                match service.pool.capabilities(&spec).await {
                    Ok(capabilities) => AgentReport {
                        name: spec.name.clone(),
                        implementation: capabilities.agent_name.clone().unwrap_or_default(),
                        description: spec.description.clone(),
                        protocol_version: capabilities.protocol_version,
                        can_list: capabilities.can_list,
                        can_load: capabilities.load_session,
                        session_capabilities: capabilities.session_capabilities.clone(),
                        error: None,
                    },
                    Err(error) => AgentReport {
                        name: spec.name.clone(),
                        implementation: String::new(),
                        description: spec.description.clone(),
                        protocol_version: 0,
                        can_list: false,
                        can_load: false,
                        session_capabilities: Vec::new(),
                        error: Some(error.to_string()),
                    },
                }
            });
        }
        let mut reports = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(report) = joined {
                reports.push(report);
            }
        }
        reports.sort_by(|left, right| left.name.cmp(&right.name));
        reports
    }

    /// 会话列表（只含通过策略的会话）。
    pub async fn sessions(
        self: &Arc<Self>,
        names: &[String],
        query: Option<&str>,
        limit: usize,
        remote: Option<&str>,
        principal: &Principal,
    ) -> Result<SessionsReport, ServiceError> {
        let specs = self.specs(names)?;
        let now = Utc::now();
        let mut set = JoinSet::new();
        for spec in specs {
            let service = Arc::clone(self);
            let principal = principal.clone();
            set.spawn(async move { service.list_agent(&spec, &principal, now, AclAccess::List).await });
        }

        let mut views: Vec<SessionView> = Vec::new();
        let mut errors: Vec<AgentFailure> = Vec::new();
        let mut hidden = 0usize;
        let mut acl_hidden = 0usize;
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok((agent_views, agent_hidden, agent_acl_hidden))) => {
                    views.extend(agent_views);
                    hidden += agent_hidden;
                    acl_hidden += agent_acl_hidden;
                }
                Ok(Err(failure)) => errors.push(failure),
                Err(join_error) => errors.push(AgentFailure {
                    agent: "未知".into(),
                    message: format!("任务失败：{join_error}"),
                }),
            }
        }

        if let Some(query) = query {
            let needle = query.to_lowercase();
            views.retain(|view| {
                view.title.as_deref().unwrap_or_default().to_lowercase().contains(&needle)
                    || view.session_id.to_lowercase().contains(&needle)
                    || view.cwd.to_lowercase().contains(&needle)
                    || view.agent.to_lowercase().contains(&needle)
            });
        }
        views.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        let total = views.len();
        views.truncate(limit);
        let shown = views.len();

        self.catalog.log_access(
            &AccessEntry {
                remote: remote.map(str::to_string),
                agent: if names.is_empty() {
                    "全部".into()
                } else {
                    names.join(",")
                },
                session_id: None,
                action: "list".into(),
                outcome: format!(
                    "主体 {}：返回 {shown} 条，策略隐藏 {hidden} 条，权限隐藏 {acl_hidden} 条，共匹配 {total} 条",
                    principal.name
                ),
            },
            now,
        )?;

        Ok(SessionsReport {
            mode: self.mode.as_str().to_string(),
            sessions: views,
            total,
            hidden,
            acl_hidden,
            errors,
        })
    }

    /// 重放一条会话。
    pub async fn transcript(
        self: &Arc<Self>,
        selector: &str,
        names: &[String],
        cwd: Option<&str>,
        remote: Option<&str>,
        principal: &Principal,
    ) -> Result<TranscriptReport, ServiceError> {
        let specs = self.specs(names)?;
        let now = Utc::now();

        if let Some(cwd) = cwd {
            if specs.len() != 1 {
                return Err(ServiceError::BadRequest(
                    "使用 cwd 直接加载时必须恰好指定一个 agent".into(),
                ));
            }
            let path = PathBuf::from(cwd);
            if !path.is_absolute() {
                return Err(ServiceError::BadRequest(format!("cwd 必须是绝对路径: {cwd}")));
            }
            let summary = SessionSummary {
                agent: specs[0].name.clone(),
                session_id: selector.to_string(),
                cwd: path,
                title: None,
                updated_at: None,
            };
            let evaluated = self.evaluate(&specs[0], principal, &summary, now, AclAccess::Read)?;
            if !evaluated.policy_allowed || !evaluated.acl_allowed {
                let reason = if evaluated.policy_allowed {
                    evaluated.acl_reason.clone()
                } else {
                    evaluated.reason.clone()
                };
                self.log_denied(remote, &specs[0].name, selector, "transcript", &reason, now);
                return Err(ServiceError::Denied);
            }
            let updates = self.pool.load(&specs[0], selector, &summary.cwd).await?;
            self.catalog.log_access(
                &AccessEntry {
                    remote: remote.map(str::to_string),
                    agent: specs[0].name.clone(),
                    session_id: Some(selector.to_string()),
                    action: "transcript".into(),
                    outcome: "ok".into(),
                },
                now,
            )?;
            return Ok(TranscriptReport {
                agent: specs[0].name.clone(),
                session_id: selector.to_string(),
                cwd: summary.cwd.to_string_lossy().into_owned(),
                title: None,
                matched: 1,
                note: Some("按 cwd 直接加载，未做枚举".into()),
                decision: evaluated.decision.as_str().into(),
                rule: evaluated.rule,
                updates,
            });
        }

        let mut set = JoinSet::new();
        for spec in specs.clone() {
            let service = Arc::clone(self);
            let principal = principal.clone();
            set.spawn(async move {
                let name = spec.name.clone();
                // 读取路径按 r 位筛选：只要能读，即使 x 位为 0（不出现在列表里）也能按 ID 取到。
                match service.list_agent(&spec, &principal, now, AclAccess::Read).await {
                    Ok((views, _hidden, _acl_hidden)) => (name, views, None),
                    Err(failure) => (name, Vec::new(), Some(failure.message)),
                }
            });
        }
        let mut visible: Vec<SessionSummary> = Vec::new();
        let mut problems: Vec<String> = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok((name, views, problem)) = joined {
                if let Some(message) = problem {
                    problems.push(format!("{name}: {message}"));
                }
                for view in views {
                    visible.push(SessionSummary {
                        agent: view.agent.clone(),
                        session_id: view.session_id.clone(),
                        cwd: PathBuf::from(&view.cwd),
                        title: view.title.clone(),
                        updated_at: view.updated_at.clone(),
                    });
                }
            }
        }

        let matches = match_summaries(&visible, selector);
        let Some(chosen) = matches.first().cloned() else {
            let detail = if problems.is_empty() {
                String::new()
            } else {
                format!("；已跳过：{}", problems.join("，"))
            };
            return Err(ServiceError::NotFound {
                selector: selector.to_string(),
                detail,
            });
        };
        let spec = self
            .config
            .agent(&chosen.agent)
            .ok_or_else(|| ServiceError::UnknownAgent(chosen.agent.clone()))?;
        if !chosen.cwd.is_absolute() {
            return Err(ServiceError::BadRequest(format!(
                "会话记录的工作目录不是绝对路径: {}",
                chosen.cwd.to_string_lossy()
            )));
        }
        let note = (matches.len() > 1).then(|| format!("选择器命中 {} 条会话，已取更新时间最新的一条", matches.len()));
        let evaluated = self.evaluate(&spec, principal, &chosen, now, AclAccess::Read)?;
        if !evaluated.policy_allowed || !evaluated.acl_allowed {
            let reason = if evaluated.policy_allowed {
                evaluated.acl_reason.clone()
            } else {
                evaluated.reason.clone()
            };
            self.log_denied(remote, &spec.name, &chosen.session_id, "transcript", &reason, now);
            return Err(ServiceError::Denied);
        }
        let updates = self.pool.load(&spec, &chosen.session_id, &chosen.cwd).await?;
        self.catalog.log_access(
            &AccessEntry {
                remote: remote.map(str::to_string),
                agent: spec.name.clone(),
                session_id: Some(chosen.session_id.clone()),
                action: "transcript".into(),
                outcome: "ok".into(),
            },
            now,
        )?;
        Ok(TranscriptReport {
            agent: spec.name.clone(),
            session_id: chosen.session_id.clone(),
            cwd: chosen.cwd.to_string_lossy().into_owned(),
            title: chosen.title.clone(),
            matched: matches.len(),
            note,
            decision: evaluated.decision.as_str().into(),
            rule: evaluated.rule,
            updates,
        })
    }

    /// 解释一条会话的判定过程（会话必须在台账里出现过）。
    pub fn explain(&self, agent: &str, session_id: &str, principal: &Principal) -> Result<ExplainReport, ServiceError> {
        let spec = self
            .config
            .agent(agent)
            .ok_or_else(|| ServiceError::UnknownAgent(agent.to_string()))?;
        let record = self
            .catalog
            .session(agent, session_id)?
            .ok_or_else(|| ServiceError::NotFound {
                selector: session_id.to_string(),
                detail: format!("（台账里没有 {agent} 的这条会话，先查询一次会话列表即可登记）"),
            })?;
        let now = Utc::now();
        let file = self.file_facts(agent, session_id, record.file_path.as_deref());
        let facts = SessionFacts {
            agent: agent.to_string(),
            session_id: session_id.to_string(),
            cwd: record.cwd.clone(),
            title: record.title.clone(),
            updated_at: parse_optional_time(record.updated_at.as_deref()),
            first_seen: record.first_seen,
            last_seen: record.last_seen,
            file: file.clone(),
        };
        let mut rules = Vec::new();
        for (effect, name, outcome) in self.policy.explain(
            &facts,
            record.remembered(),
            |rule| self.catalog.rule_first_applied(rule, now).ok(),
            now,
            self.current_uid,
        ) {
            let (matched, detail) = match outcome {
                crate::policy::MatchOutcome::Matched => (true, "命中".to_string()),
                crate::policy::MatchOutcome::NotMatched(reason) => (false, reason),
            };
            rules.push(RuleExplanation {
                effect: effect.as_str().into(),
                name,
                matched,
                detail,
            });
        }
        let outcome = self.policy.evaluate(
            &facts,
            record.remembered(),
            |rule| self.catalog.rule_first_applied(rule, now).ok(),
            now,
            self.current_uid,
        );
        let _ = spec;
        let acl = self.acl_for(agent, session_id, Some(&record.acl));
        let acl_decision = acl_check(principal, &acl, AclAccess::List);
        Ok(ExplainReport {
            agent: agent.to_string(),
            session_id: session_id.to_string(),
            decision: outcome.decision.as_str().into(),
            rule: outcome.rule,
            reason: outcome.reason,
            first_seen: format_time(record.first_seen),
            last_seen: format_time(record.last_seen),
            remembered_decision: record.decision.map(|decision| decision.as_str().to_string()),
            file_path: record.file_path.map(|path| path.to_string_lossy().into_owned()),
            file_available: file.is_some(),
            rules,
            acl_owner: acl.owner.clone(),
            acl_group: acl.group.clone(),
            acl_mode: acl.mode_text(),
            acl_class: acl_decision.class.to_string(),
            acl_allowed: acl_decision.allowed,
            acl_reason: acl_decision.reason,
        })
    }

    /// 读取一条会话的权限与来源（配置文件覆盖 或 台账）。
    pub fn acl_status(&self, agent: &str, session_id: &str) -> Result<(ObjectAcl, String), ServiceError> {
        let record = self
            .catalog
            .session(agent, session_id)?
            .ok_or_else(|| ServiceError::NotFound {
                selector: format!("{agent}/{session_id}"),
                detail: "（台账里没有这条会话，先查询一次会话列表即可登记）".into(),
            })?;
        let key = format!("{agent}/{session_id}");
        if let Some(acl) = self.acl_overrides.get(&key) {
            return Ok((acl.clone(), "配置文件覆盖".to_string()));
        }
        let owner_source = match record.acl_owner_source.as_str() {
            "file" => "会话文件属主",
            "manual" => "手工修改",
            _ => "默认模板",
        };
        Ok((record.acl, format!("台账（属主来源：{owner_source}）")))
    }

    /// 修改权限位。
    pub fn chmod(&self, agent: &str, session_id: &str, mode: u16) -> Result<(), ServiceError> {
        let changed = self
            .catalog
            .set_acl(agent, session_id, None, GroupUpdate::Keep, Some(mode))?;
        if !changed {
            return Err(ServiceError::NotFound {
                selector: format!("{agent}/{session_id}"),
                detail: "（台账里没有这条会话）".into(),
            });
        }
        Ok(())
    }

    /// 修改属主与属组。
    pub fn chown(
        &self,
        agent: &str,
        session_id: &str,
        owner: Option<&str>,
        group: GroupUpdate,
    ) -> Result<(), ServiceError> {
        let changed = self.catalog.set_acl(agent, session_id, owner, group, None)?;
        if !changed {
            return Err(ServiceError::NotFound {
                selector: format!("{agent}/{session_id}"),
                detail: "（台账里没有这条会话）".into(),
            });
        }
        Ok(())
    }

    /// 台账统计与最近判定。
    pub fn policy_status(&self, limit: usize) -> Result<(i64, i64, i64, Vec<(String, String, String, Option<String>, String)>), ServiceError> {
        let stats = self.catalog.stats()?;
        let recent = self.catalog.recent_decisions(limit)?;
        Ok((stats.0, stats.1, stats.2, recent))
    }

    /// 解除某条会话的历史拒绝。
    pub fn forget(&self, agent: &str, session_id: &str) -> Result<bool, ServiceError> {
        Ok(self.catalog.forget(agent, session_id)?)
    }


    async fn list_agent(
        &self,
        spec: &AgentSpec,
        principal: &Principal,
        now: DateTime<Utc>,
        access: AclAccess,
    ) -> Result<(Vec<SessionView>, usize, usize), AgentFailure> {
        let summaries = self.pool.list(spec).await.map_err(|error| AgentFailure {
            agent: spec.name.clone(),
            message: error.to_string(),
        })?;
        let mut views = Vec::new();
        let mut hidden = 0usize;
        let mut acl_hidden = 0usize;
        for summary in &summaries {
            match self.evaluate(spec, principal, summary, now, access) {
                Ok(evaluated) => {
                    if evaluated.visible() {
                        views.push(evaluated.view);
                    } else if evaluated.policy_allowed {
                        acl_hidden += 1;
                    } else {
                        hidden += 1;
                    }
                }
                Err(error) => {
                    return Err(AgentFailure {
                        agent: spec.name.clone(),
                        message: error.to_string(),
                    });
                }
            }
        }
        let _ = self.catalog.log_access(
            &AccessEntry {
                remote: None,
                agent: spec.name.clone(),
                session_id: None,
                action: "list-agent".into(),
                outcome: format!(
                    "主体 {}：可见 {} 条，策略隐藏 {hidden} 条，权限隐藏 {acl_hidden} 条",
                    principal.name,
                    views.len()
                ),
            },
            now,
        );
        Ok((views, hidden, acl_hidden))
    }

    fn evaluate(
        &self,
        spec: &AgentSpec,
        principal: &Principal,
        summary: &SessionSummary,
        now: DateTime<Utc>,
        access: AclAccess,
    ) -> Result<Evaluated, ServiceError> {
        let cwd = summary.cwd.to_string_lossy().into_owned();
        // 新会话用模板或配置覆盖登记权限；已有会话沿用台账里的值。
        let initial_acl = self.acl_for(&spec.name, &summary.session_id, None);
        let record = self.catalog.touch_session(
            &spec.name,
            &summary.session_id,
            &cwd,
            summary.title.as_deref(),
            summary.updated_at.as_deref(),
            &initial_acl,
            now,
        )?;
        let acl_enforced = self.mode == PolicyMode::Rules;
        let mut acl = self.acl_for(&spec.name, &summary.session_id, Some(&record.acl));
        // 属主默认来自模板；能定位到会话文件时改用文件属主（配置覆盖与 chown 过的都保持原样）。
        let override_key = format!("{}/{}", spec.name, summary.session_id);
        if record.acl_owner_source == "default" && !self.acl_overrides.contains_key(&override_key) {
            if let Some((path, owner)) =
                self.owner_from_file(&spec.name, &summary.session_id, record.file_path.as_deref())
            {
                // 记录来源，之后就不再重复解析；chown 过的（source=manual）不会被覆盖。
                let _ = self
                    .catalog
                    .set_owner_from_file(&spec.name, &summary.session_id, &owner);
                let _ = self
                    .catalog
                    .remember_file_path(&spec.name, &summary.session_id, &path);
                acl.owner = owner;
            }
        }
        let acl_decision = if acl_enforced {
            acl_check(principal, &acl, access)
        } else {
            crate::acl::AclDecision {
                allowed: true,
                class: "disabled",
                reason: "简单模式：不做权限判定".into(),
            }
        };
        let file = self.file_facts(&spec.name, &summary.session_id, record.file_path.as_deref());
        let facts = SessionFacts {
            agent: spec.name.clone(),
            session_id: summary.session_id.clone(),
            cwd: cwd.clone(),
            title: summary.title.clone(),
            updated_at: parse_optional_time(summary.updated_at.as_deref()),
            first_seen: record.first_seen,
            last_seen: record.last_seen,
            file,
        };
        let outcome = match self.mode {
            PolicyMode::Rules => self.policy.evaluate(
                &facts,
                record.remembered(),
                |rule| self.catalog.rule_first_applied(rule, now).ok(),
                now,
                self.current_uid,
            ),
            // 简单模式：只看时间段，不写判定记录，因此切回完整模式时不会留下「历史拒绝」。
            PolicyMode::TimeWindow => self.window.evaluate(&facts, now),
        };
        if acl_enforced {
            let changed = record.decision != Some(outcome.decision)
                || record.rule != outcome.rule
                || record.reason != outcome.reason;
            if changed {
                self.catalog.record_decision(
                    &spec.name,
                    &summary.session_id,
                    outcome.decision,
                    outcome.rule.as_deref(),
                    &outcome.reason,
                    now,
                )?;
            }
        }
        Ok(Evaluated {
            view: SessionView {
                agent: spec.name.clone(),
                session_id: summary.session_id.clone(),
                cwd,
                title: summary.title.clone(),
                updated_at: summary.updated_at.clone(),
                first_seen: format_time(record.first_seen),
                decision: outcome.decision.as_str().into(),
                rule: outcome.rule.clone(),
                acl_owner: acl.owner.clone(),
                acl_group: acl.group.clone(),
                acl_mode: acl.mode_text(),
            },
            policy_allowed: outcome.decision == Decision::Allow,
            acl_allowed: acl_decision.allowed,
            decision: outcome.decision,
            rule: outcome.rule,
            reason: outcome.reason,
            acl_reason: acl_decision.reason,
        })
    }

    fn file_facts(&self, agent: &str, session_id: &str, cached: Option<&Path>) -> Option<crate::policy::FileFacts> {
        if !self.policy.needs_file_facts() {
            return None;
        }
        if let Some(path) = cached {
            if let Some(facts) = locate::file_facts(path) {
                return Some(facts);
            }
        }
        let patterns = self.config.server.locate.get(agent)?;
        let path = locate::find_file(patterns, session_id)?;
        let _ = self.catalog.remember_file_path(agent, session_id, &path);
        locate::file_facts(&path)
    }

    fn log_denied(
        &self,
        remote: Option<&str>,
        agent: &str,
        session_id: &str,
        action: &str,
        reason: &str,
        now: DateTime<Utc>,
    ) {
        let _ = self.catalog.log_access(
            &AccessEntry {
                remote: remote.map(str::to_string),
                agent: agent.to_string(),
                session_id: Some(session_id.to_string()),
                action: action.to_string(),
                outcome: format!("denied: {reason}"),
            },
            now,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn summary(agent: &str, id: &str, title: &str, updated: &str) -> SessionSummary {
        SessionSummary {
            agent: agent.into(),
            session_id: id.into(),
            cwd: PathBuf::from("/Users/x/tmp"),
            title: Some(title.into()),
            updated_at: Some(updated.into()),
        }
    }

    #[test]
    fn 选择器精确与前缀匹配() {
        let items = vec![
            summary("omp", "01a0e148-aaaa", "实现会话查看器", "2026-09-27T05:00:00Z"),
            summary("omp", "01a0e148-bbbb", "另一个任务", "2026-09-26T05:00:00Z"),
            summary("opencode", "ses_28a0", "Clash 规则", "2026-04-21T08:00:00Z"),
        ];
        assert_eq!(match_summaries(&items, "ses_28a0").len(), 1);
        let prefixed = match_summaries(&items, "01a0e148");
        assert_eq!(prefixed.len(), 2);
        assert_eq!(prefixed[0].session_id, "01a0e148-aaaa", "应按更新时间倒序");
        assert_eq!(match_summaries(&items, "clash").len(), 1);
        assert_eq!(match_summaries(&items, "last")[0].session_id, "01a0e148-aaaa");
        assert!(match_summaries(&items, "不存在").is_empty());
    }

    #[test]
    fn 相对时间表达式解析() {
        let now = Utc.with_ymd_and_hms(2026, 9, 27, 12, 0, 0).unwrap();
        assert_eq!(
            crate::policy::parse_time_expression("-7d", now),
            Some(Utc.with_ymd_and_hms(2026, 9, 20, 12, 0, 0).unwrap())
        );
    }

    #[test]
    fn 可选时间解析() {
        assert!(parse_optional_time(Some("2026-09-27T05:00:00Z")).is_some());
        assert!(parse_optional_time(Some("不是时间")).is_none());
        assert!(parse_optional_time(None).is_none());
    }
}
