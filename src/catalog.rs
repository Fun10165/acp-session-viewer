//! 会话台账：记录首次见到时间、可见性判定与规则生效时间（SQLite）。
//!
//! 台账解决两件事：
//!
//! * **首次见到时间**（当作 btime 使用）：ACP 只报告会话的更新时间，没有创建时间；
//!   台账在第一次见到某条会话时记下时间，用于 `since` 这类「只放行以后新增的」规则。
//! * **历史拒绝**：被拒绝过的会话永久留档，`sticky_deny` 打开时不会再被放行。

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

use crate::acl::ObjectAcl;
use crate::policy::{Decision, Remembered, format_time};

/// 属组字段的更新方式。
#[derive(Debug, Clone)]
pub enum GroupUpdate {
    /// 保持不变。
    Keep,
    /// 清空属组。
    Clear,
    /// 设为指定组。
    Set(String),
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("打开台账失败 {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("台账操作失败：{0}")]
    Sql(#[from] rusqlite::Error),
    #[error("台账里的时间字段无法解析：{0}")]
    BadTime(String),
    #[error("台账里的判定字段无法识别：{0}")]
    BadDecision(String),
}

/// 台账里一条会话的记录。
///
/// `decision` 为 `None` 表示这条会话还没有被判定过（刚登记进来），
/// 这时历史拒绝不生效，由规则正常判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub cwd: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub decision: Option<Decision>,
    pub decided_at: Option<DateTime<Utc>>,
    pub rule: Option<String>,
    pub reason: String,
    pub file_path: Option<PathBuf>,
    /// 会话权限（属主、属组、模式位）。
    pub acl: ObjectAcl,
}

impl SessionRecord {
    /// 历史判定（只包含真正判定过的结果，未判定时为 None）。
    #[must_use]
    pub fn remembered(&self) -> Option<Remembered> {
        self.decision.map(|decision| Remembered { decision })
    }
}

/// 一条访问记录。
#[derive(Debug, Clone)]
pub struct AccessEntry {
    pub remote: Option<String>,
    pub agent: String,
    pub session_id: Option<String>,
    pub action: String,
    pub outcome: String,
}

/// 台账。
pub struct Catalog {
    connection: Mutex<Connection>,
}

fn parse_time(text: &str) -> Result<DateTime<Utc>, CatalogError> {
    DateTime::parse_from_rfc3339(text)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| CatalogError::BadTime(text.to_string()))
}

fn parse_decision(text: &str) -> Result<Option<Decision>, CatalogError> {
    match text {
        "pending" => Ok(None),
        "allow" => Ok(Some(Decision::Allow)),
        "deny" => Ok(Some(Decision::Deny)),
        other => Err(CatalogError::BadDecision(other.to_string())),
    }
}

impl Catalog {
    /// 打开（必要时创建）台账。
    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| CatalogError::Open {
                path: path.to_path_buf(),
                source: rusqlite::Error::ToSqlConversionFailure(Box::new(source)),
            })?;
        }
        let connection = Connection::open(path).map_err(|source| CatalogError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        Self::prepare(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// 内存台账（测试用）。
    pub fn in_memory() -> Result<Self, CatalogError> {
        let connection = Connection::open_in_memory()?;
        Self::prepare(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn prepare(connection: &Connection) -> Result<(), CatalogError> {
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 agent TEXT NOT NULL,
                 session_id TEXT NOT NULL,
                 cwd TEXT NOT NULL,
                 title TEXT,
                 updated_at TEXT,
                 first_seen TEXT NOT NULL,
                 last_seen TEXT NOT NULL,
                 -- pending 表示尚未判定；判定过之后是 allow 或 deny
                 decision TEXT NOT NULL,
                 decided_at TEXT,
                 rule TEXT,
                 reason TEXT NOT NULL,
                 file_path TEXT,
                 acl_owner TEXT NOT NULL,
                 acl_group TEXT,
                 acl_mode INTEGER NOT NULL,
                 PRIMARY KEY (agent, session_id)
             );
             CREATE TABLE IF NOT EXISTS rules (
                 name TEXT PRIMARY KEY,
                 first_applied_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS access_log (
                 at TEXT NOT NULL,
                 remote TEXT,
                 agent TEXT NOT NULL,
                 session_id TEXT,
                 action TEXT NOT NULL,
                 outcome TEXT NOT NULL
             );",
        )?;
        Ok(())
    }

    /// 在一个事务里执行一段台账操作，避免批量写入时逐条提交。
    pub fn with_transaction<T>(
        &self,
        action: impl FnOnce(&Connection) -> Result<T, CatalogError>,
    ) -> Result<T, CatalogError> {
        let mut guard = self.connection.lock();
        let transaction = guard.transaction()?;
        let value = action(&transaction)?;
        transaction.commit()?;
        Ok(value)
    }

    /// 记录「见过这条会话」：不存在则插入（首次见到时间取 now），存在则刷新时间与元信息。
    pub fn touch_session(
        &self,
        agent: &str,
        session_id: &str,
        cwd: &str,
        title: Option<&str>,
        updated_at: Option<&str>,
        acl: &ObjectAcl,
        now: DateTime<Utc>,
    ) -> Result<SessionRecord, CatalogError> {
        let connection = self.connection.lock();
        self.touch_session_on(&connection, agent, session_id, cwd, title, updated_at, acl, now)
    }

    /// 同上，但在调用方给定的连接或事务上执行。
    pub fn touch_session_on(
        &self,
        connection: &Connection,
        agent: &str,
        session_id: &str,
        cwd: &str,
        title: Option<&str>,
        updated_at: Option<&str>,
        acl: &ObjectAcl,
        now: DateTime<Utc>,
    ) -> Result<SessionRecord, CatalogError> {
        let stamp = format_time(now);
        // 权限只在首次登记时写入；之后的改动通过 chmod / chown 落库。
        connection.execute(
            "INSERT INTO sessions (agent, session_id, cwd, title, updated_at, first_seen, last_seen,
                                   decision, decided_at, rule, reason, acl_owner, acl_group, acl_mode)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 'pending', NULL, NULL, '尚未判定', ?7, ?8, ?9)
             ON CONFLICT(agent, session_id) DO UPDATE SET
                 cwd = excluded.cwd,
                 title = COALESCE(excluded.title, sessions.title),
                 updated_at = COALESCE(excluded.updated_at, sessions.updated_at),
                 last_seen = excluded.last_seen",
            params![
                agent,
                session_id,
                cwd,
                title,
                updated_at,
                stamp,
                &acl.owner,
                acl.group.as_deref(),
                acl.mode as i64
            ],
        )?;
        self.load_locked(connection, agent, session_id)
    }

    fn load_locked(
        &self,
        connection: &Connection,
        agent: &str,
        session_id: &str,
    ) -> Result<SessionRecord, CatalogError> {
        let row = connection.query_row(
            "SELECT cwd, title, updated_at, first_seen, last_seen, decision, decided_at, rule, reason, file_path,
                    acl_owner, acl_group, acl_mode
             FROM sessions WHERE agent = ?1 AND session_id = ?2",
            params![agent, session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, i64>(12)?,
                ))
            },
        )?;
        Ok(SessionRecord {
            cwd: row.0,
            title: row.1,
            updated_at: row.2,
            first_seen: parse_time(&row.3)?,
            last_seen: parse_time(&row.4)?,
            decision: parse_decision(&row.5)?,
            decided_at: row.6.as_deref().map(parse_time).transpose()?,
            rule: row.7,
            reason: row.8,
            file_path: row.9.map(PathBuf::from),
            acl: ObjectAcl::new(&row.10, row.11.as_deref(), row.12 as u16),
        })
    }

    /// 读取一条会话的记录。
    pub fn session(&self, agent: &str, session_id: &str) -> Result<Option<SessionRecord>, CatalogError> {
        let connection = self.connection.lock();
        let exists: Option<String> = connection
            .query_row(
                "SELECT session_id FROM sessions WHERE agent = ?1 AND session_id = ?2",
                params![agent, session_id],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Ok(None);
        }
        self.load_locked(&connection, agent, session_id).map(Some)
    }

    /// 写入判定结果（只有变化时才写）。
    pub fn record_decision(
        &self,
        agent: &str,
        session_id: &str,
        decision: Decision,
        rule: Option<&str>,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(), CatalogError> {
        let connection = self.connection.lock();
        self.record_decision_on(&connection, agent, session_id, decision, rule, reason, now)
    }

    /// 同上，但在调用方给定的连接或事务上执行。
    pub fn record_decision_on(
        &self,
        connection: &Connection,
        agent: &str,
        session_id: &str,
        decision: Decision,
        rule: Option<&str>,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(), CatalogError> {
        connection.execute(
            "UPDATE sessions SET decision = ?3, rule = ?4, reason = ?5, decided_at = ?6
             WHERE agent = ?1 AND session_id = ?2
               AND (decision != ?3 OR rule IS NOT ?4 OR reason != ?5)",
            params![
                agent,
                session_id,
                decision.as_str(),
                rule,
                reason,
                format_time(now)
            ],
        )?;
        Ok(())
    }

    /// 修改会话权限；返回是否确有记录。
    pub fn set_acl(
        &self,
        agent: &str,
        session_id: &str,
        owner: Option<&str>,
        group: GroupUpdate,
        mode: Option<u16>,
    ) -> Result<bool, CatalogError> {
        let connection = self.connection.lock();
        let mut affected = 0;
        if let Some(owner) = owner {
            affected += connection.execute(
                "UPDATE sessions SET acl_owner = ?3 WHERE agent = ?1 AND session_id = ?2",
                params![agent, session_id, owner],
            )?;
        }
        match &group {
            GroupUpdate::Keep => {}
            GroupUpdate::Clear => {
                affected += connection.execute(
                    "UPDATE sessions SET acl_group = NULL WHERE agent = ?1 AND session_id = ?2",
                    params![agent, session_id],
                )?;
            }
            GroupUpdate::Set(value) => {
                affected += connection.execute(
                    "UPDATE sessions SET acl_group = ?3 WHERE agent = ?1 AND session_id = ?2",
                    params![agent, session_id, value],
                )?;
            }
        }
        if let Some(mode) = mode {
            affected += connection.execute(
                "UPDATE sessions SET acl_mode = ?3 WHERE agent = ?1 AND session_id = ?2",
                params![agent, session_id, mode as i64],
            )?;
        }
        if affected > 0 {
            return Ok(true);
        }
        let exists: Option<String> = connection
            .query_row(
                "SELECT session_id FROM sessions WHERE agent = ?1 AND session_id = ?2",
                params![agent, session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    /// 记住会话文件的路径。
    pub fn remember_file_path(&self, agent: &str, session_id: &str, path: &Path) -> Result<(), CatalogError> {
        let connection = self.connection.lock();
        connection.execute(
            "UPDATE sessions SET file_path = ?3 WHERE agent = ?1 AND session_id = ?2",
            params![agent, session_id, path.to_string_lossy()],
        )?;
        Ok(())
    }

    /// 忘记一条会话的台账（用于解除历史拒绝）；返回是否确有记录。
    pub fn forget(&self, agent: &str, session_id: &str) -> Result<bool, CatalogError> {
        let connection = self.connection.lock();
        let affected = connection.execute(
            "DELETE FROM sessions WHERE agent = ?1 AND session_id = ?2",
            params![agent, session_id],
        )?;
        Ok(affected > 0)
    }

    /// 取规则的首次生效时间；规则第一次出现时以 now 登记。
    pub fn rule_first_applied(&self, name: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, CatalogError> {
        let connection = self.connection.lock();
        self.rule_first_applied_on(&connection, name, now)
    }

    /// 同上，但在调用方给定的连接或事务上执行。
    pub fn rule_first_applied_on(
        &self,
        connection: &Connection,
        name: &str,
        now: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, CatalogError> {
        let existing: Option<String> = connection
            .query_row("SELECT first_applied_at FROM rules WHERE name = ?1", params![name], |row| {
                row.get(0)
            })
            .optional()?;
        if let Some(stamp) = existing {
            return parse_time(&stamp);
        }
        let stamp = format_time(now);
        connection.execute(
            "INSERT OR IGNORE INTO rules (name, first_applied_at) VALUES (?1, ?2)",
            params![name, stamp],
        )?;
        Ok(now)
    }

    /// 记录一次访问。
    pub fn log_access(&self, entry: &AccessEntry, now: DateTime<Utc>) -> Result<(), CatalogError> {
        let connection = self.connection.lock();
        connection.execute(
            "INSERT INTO access_log (at, remote, agent, session_id, action, outcome)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                format_time(now),
                entry.remote,
                entry.agent,
                entry.session_id,
                entry.action,
                entry.outcome
            ],
        )?;
        Ok(())
    }

    /// 列出最近的判定记录。
    pub fn recent_decisions(&self, limit: usize) -> Result<Vec<(String, String, String, Option<String>, String)>, CatalogError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT agent, session_id, decision, rule, reason FROM sessions
             ORDER BY decided_at DESC LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// 台账统计。
    pub fn stats(&self) -> Result<(i64, i64, i64), CatalogError> {
        let connection = self.connection.lock();
        let total: i64 = connection.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        let allowed: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE decision = 'allow'",
            [],
            |row| row.get(0),
        )?;
        let logged: i64 = connection.query_row("SELECT COUNT(*) FROM access_log", [], |row| row.get(0))?;
        Ok((total, allowed, logged))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 27, 12, 0, 0).unwrap()
    }

    fn test_acl() -> ObjectAcl {
        ObjectAcl::new("tester", Some("dev"), 0o750)
    }

    #[test]
    fn 新登记的会话尚未判定() {
        let catalog = Catalog::in_memory().unwrap();
        let record = catalog
            .touch_session("omp", "s1", "/tmp", None, None, &test_acl(), now())
            .unwrap();
        assert_eq!(record.decision, None, "刚登记的会话不应被当成历史拒绝");
        assert!(record.remembered().is_none());
        assert!(record.decided_at.is_none());
    }

    #[test]
    fn 首次见到时间只在第一次写入() {
        let catalog = Catalog::in_memory().unwrap();
        let first = catalog
            .touch_session("omp", "s1", "/tmp", Some("标题"), None, &test_acl(), now())
            .unwrap();
        let later = now() + chrono::Duration::hours(3);
        let second = catalog
            .touch_session("omp", "s1", "/tmp", Some("新标题"), None, &test_acl(), later)
            .unwrap();
        assert_eq!(first.first_seen, second.first_seen);
        assert_eq!(second.last_seen, later);
    }

    #[test]
    fn 记录与读取判定() {
        let catalog = Catalog::in_memory().unwrap();
        catalog
            .touch_session("omp", "s1", "/tmp", None, None, &test_acl(), now())
            .unwrap();
        catalog
            .record_decision("omp", "s1", Decision::Deny, Some("涉密"), "命中拒绝规则", now())
            .unwrap();
        let record = catalog.session("omp", "s1").unwrap().unwrap();
        assert_eq!(record.decision, Some(Decision::Deny));
        assert_eq!(record.rule.as_deref(), Some("涉密"));
        assert_eq!(record.remembered().unwrap().decision, Decision::Deny);
    }

    #[test]
    fn 规则首次生效时间只登记一次() {
        let catalog = Catalog::in_memory().unwrap();
        let first = catalog.rule_first_applied("新规则", now()).unwrap();
        let later = now() + chrono::Duration::days(1);
        let second = catalog.rule_first_applied("新规则", later).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn 忘记会话后记录消失() {
        let catalog = Catalog::in_memory().unwrap();
        catalog
            .touch_session("omp", "s1", "/tmp", None, None, &test_acl(), now())
            .unwrap();
        assert!(catalog.forget("omp", "s1").unwrap());
        assert!(catalog.session("omp", "s1").unwrap().is_none());
        assert!(!catalog.forget("omp", "s1").unwrap());
    }

    #[test]
    fn 访问记录与统计() {
        let catalog = Catalog::in_memory().unwrap();
        catalog
            .touch_session("omp", "s1", "/tmp", None, None, &test_acl(), now())
            .unwrap();
        catalog
            .record_decision("omp", "s1", Decision::Allow, Some("放行"), "命中放行规则", now())
            .unwrap();
        catalog
            .log_access(
                &AccessEntry {
                    remote: Some("127.0.0.1".into()),
                    agent: "omp".into(),
                    session_id: Some("s1".into()),
                    action: "transcript".into(),
                    outcome: "ok".into(),
                },
                now(),
            )
            .unwrap();
        let (total, allowed, logged) = catalog.stats().unwrap();
        assert_eq!((total, allowed, logged), (1, 1, 1));
        assert_eq!(catalog.recent_decisions(10).unwrap().len(), 1);
    }

    #[test]
    fn 记住文件路径() {
        let catalog = Catalog::in_memory().unwrap();
        catalog
            .touch_session("omp", "s1", "/tmp", None, None, &test_acl(), now())
            .unwrap();
        catalog
            .remember_file_path("omp", "s1", Path::new("/x/s1.jsonl"))
            .unwrap();
        let record = catalog.session("omp", "s1").unwrap().unwrap();
        assert_eq!(record.file_path, Some(PathBuf::from("/x/s1.jsonl")));
    }
}
