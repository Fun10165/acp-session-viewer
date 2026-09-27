//! 可见性策略：决定一条会话能不能被暴露给调用方。
//!
//! 判定顺序（先命中先算数）：
//!
//! 1. 台账里记录过的历史拒绝：打开 `sticky_deny` 时，被拒绝过的会话永久不再暴露；
//! 2. 按顺序匹配 deny 规则，命中即拒绝；
//! 3. 按顺序匹配 allow 规则，命中即放行；
//! 4. 都没命中时采用 `default` 策略。
//!
//! 规则里的多个条件之间是「与」的关系；某个条件涉及的信息不可用（例如会话文件定位不到）
//! 时该条件判定为不匹配，判定结果里会给出原因，便于用 `policy explain` 排查。

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use globset::{Glob, GlobMatcher};
use serde::Deserialize;

/// 判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
}

impl Decision {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
        }
    }
}

/// 会话文件的事实（能定位到文件时才有）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFacts {
    pub path: String,
    pub size: u64,
    pub btime: Option<DateTime<Utc>>,
    pub mtime: Option<DateTime<Utc>>,
    pub mode: u32,
    pub owner_uid: u32,
    pub group_gid: u32,
}

/// 判定一条会话所需的全部事实。
#[derive(Debug, Clone)]
pub struct SessionFacts {
    pub agent: String,
    pub session_id: String,
    pub cwd: String,
    pub title: Option<String>,
    /// agent 报告的更新时间。
    pub updated_at: Option<DateTime<Utc>>,
    /// 台账里记录的首次见到时间（当作 btime 使用）。
    pub first_seen: DateTime<Utc>,
    /// 台账里记录的最后见到时间。
    pub last_seen: DateTime<Utc>,
    /// 会话文件事实，定位不到时为 None。
    pub file: Option<FileFacts>,
}

/// 台账里已有的判定记录，用于「历史拒绝永久生效」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remembered {
    pub decision: Decision,
}

/// 规则命中的细节，供解释与审计使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchOutcome {
    Matched,
    /// 未命中，附上第一条不满足的条件说明。
    NotMatched(String),
}

/// 最终的可见性判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub decision: Decision,
    /// 命中的规则名；采用默认策略时为 None。
    pub rule: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OwnerSpec {
    /// 文件属主是当前进程的属主。
    SelfOwner,
    /// 文件属主不是当前进程的属主。
    Other,
    /// 指定属主 uid。
    Uid(u32),
}

fn default_decision() -> Decision {
    Decision::Deny
}

fn default_sticky() -> bool {
    true
}

/// 策略配置（来自 TOML）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    #[serde(default = "default_decision")]
    pub default: Decision,
    /// 被拒绝过的会话是否永久不再暴露。
    #[serde(default = "default_sticky")]
    pub sticky_deny: bool,
    #[serde(default)]
    pub deny: Vec<RuleConfig>,
    #[serde(default)]
    pub allow: Vec<RuleConfig>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            default: Decision::Deny,
            sticky_deny: true,
            deny: Vec::new(),
            allow: Vec::new(),
        }
    }
}

/// 一条规则的条件集合。
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub name: String,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub session_ids: Vec<String>,
    pub session_id_glob: Option<String>,
    pub title_contains: Option<String>,
    pub title_glob: Option<String>,
    pub cwd_glob: Option<String>,
    pub file_glob: Option<String>,
    /// 相对 `first_seen` 的下界，可写 RFC3339、相对时长（`-7d`）或 `rule-created`。
    pub since: Option<String>,
    pub first_seen_after: Option<String>,
    pub first_seen_before: Option<String>,
    pub last_seen_after: Option<String>,
    pub last_seen_before: Option<String>,
    pub updated_after: Option<String>,
    pub updated_before: Option<String>,
    pub btime_after: Option<String>,
    pub btime_before: Option<String>,
    pub mtime_after: Option<String>,
    pub mtime_before: Option<String>,
    #[serde(default, deserialize_with = "deserialize_size")]
    pub min_size: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_size")]
    pub max_size: Option<u64>,
    pub owner: Option<OwnerSpec>,
    /// 权限位必须落在该八进制掩码之内，例如 "0600"。
    pub mode_within: Option<String>,
    pub other_readable: Option<bool>,
    pub other_writable: Option<bool>,
    pub group_readable: Option<bool>,
    pub group_writable: Option<bool>,
}

/// 编译之后的规则。
#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    agents: Vec<String>,
    session_ids: Vec<String>,
    session_id_glob: Option<GlobMatcher>,
    title_contains: Option<String>,
    title_glob: Option<GlobMatcher>,
    cwd_glob: Option<GlobMatcher>,
    file_glob: Option<GlobMatcher>,
    since: Option<TimeSpec>,
    first_seen_after: Option<TimeSpec>,
    first_seen_before: Option<TimeSpec>,
    last_seen_after: Option<TimeSpec>,
    last_seen_before: Option<TimeSpec>,
    updated_after: Option<TimeSpec>,
    updated_before: Option<TimeSpec>,
    btime_after: Option<TimeSpec>,
    btime_before: Option<TimeSpec>,
    mtime_after: Option<TimeSpec>,
    mtime_before: Option<TimeSpec>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    owner: Option<OwnerSpec>,
    mode_within: Option<u32>,
    other_readable: Option<bool>,
    other_writable: Option<bool>,
    group_readable: Option<bool>,
    group_writable: Option<bool>,
}

/// 时间条件的形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeSpec {
    /// 固定时刻。
    At(DateTime<Utc>),
    /// 规则首次生效的时刻（由台账提供）。
    RuleCreated,
    /// 相对当前时刻的偏移，例如 `-24h` 表示最近 24 小时。
    Relative(Duration),
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("规则 {rule:?} 的 glob 非法：{message}")]
    BadGlob { rule: String, message: String },
    #[error("规则 {rule:?} 的时间表达式非法：{value:?}")]
    BadTime { rule: String, value: String },
    #[error("规则 {rule:?} 的 mode_within 不是合法的八进制权限：{value:?}")]
    BadMode { rule: String, value: String },
}

fn compile_glob(rule: &str, pattern: &Option<String>) -> Result<Option<GlobMatcher>, PolicyError> {
    match pattern {
        None => Ok(None),
        Some(text) => Glob::new(text)
            .map(|glob| Some(glob.compile_matcher()))
            .map_err(|error| PolicyError::BadGlob {
                rule: rule.to_string(),
                message: error.to_string(),
            }),
    }
}

fn compile_time(rule: &str, value: &Option<String>) -> Result<Option<TimeSpec>, PolicyError> {
    match value.as_deref() {
        None => Ok(None),
        Some("rule-created") => Ok(Some(TimeSpec::RuleCreated)),
        Some(text) => {
            if let Some(absolute) = parse_absolute(text) {
                return Ok(Some(TimeSpec::At(absolute)));
            }
            if let Some(delta) = parse_relative(text) {
                return Ok(Some(TimeSpec::Relative(delta)));
            }
            Err(PolicyError::BadTime {
                rule: rule.to_string(),
                value: text.to_string(),
            })
        }
    }
}

/// 解析文件大小：支持纯数字（字节）与 `8MiB`、`512KiB`、`1GB` 这类写法。
#[must_use]
pub fn parse_size(text: &str) -> Option<u64> {
    let trimmed = text.trim().to_lowercase();
    let (number, multiplier) = if let Some(rest) = trimmed.strip_suffix("kib") {
        (rest, 1024u64)
    } else if let Some(rest) = trimmed.strip_suffix("mib") {
        (rest, 1024 * 1024)
    } else if let Some(rest) = trimmed.strip_suffix("gib") {
        (rest, 1024 * 1024 * 1024)
    } else if let Some(rest) = trimmed.strip_suffix("kb") {
        (rest, 1000)
    } else if let Some(rest) = trimmed.strip_suffix("mb") {
        (rest, 1000 * 1000)
    } else if let Some(rest) = trimmed.strip_suffix("gb") {
        (rest, 1000 * 1000 * 1000)
    } else if let Some(rest) = trimmed.strip_suffix('b') {
        (rest, 1)
    } else {
        (trimmed.as_str(), 1)
    };
    let value: u64 = number.trim().parse().ok()?;
    value.checked_mul(multiplier)
}

fn deserialize_size<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Number(u64),
        Text(String),
    }
    match Option::<Raw>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Raw::Number(value)) => Ok(Some(value)),
        Some(Raw::Text(text)) => parse_size(&text)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("无法解析文件大小: {text:?}"))),
    }
}

/// 解析 RFC3339 时间。
fn parse_absolute(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

/// 解析相对时长：`-7d`、`24h`、`30m`、`45s`、`2w`（前导负号可有可无）。
#[must_use]
pub fn parse_relative(text: &str) -> Option<Duration> {
    let trimmed = text.strip_prefix('-').unwrap_or(text);
    let mut chars = trimmed.chars();
    let unit = chars.next_back()?;
    let number = chars.as_str();
    if number.is_empty() {
        return None;
    }
    let amount: i64 = number.parse().ok()?;
    Some(match unit {
        's' => Duration::seconds(amount),
        'm' => Duration::minutes(amount),
        'h' => Duration::hours(amount),
        'd' => Duration::days(amount),
        'w' => Duration::weeks(amount),
        _ => return None,
    })
}

/// 解析时间表达式：RFC3339 绝对时刻或相对时长。
#[must_use]
pub fn parse_time_expression(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if let Some(absolute) = parse_absolute(text) {
        return Some(absolute);
    }
    parse_relative(text).map(|delta| now - delta)
}

impl Rule {
    /// 编译一条规则配置。
    pub fn compile(config: &RuleConfig) -> Result<Self, PolicyError> {
        let name = config.name.clone();
        let mode_within = match &config.mode_within {
            None => None,
            Some(text) => Some(u32::from_str_radix(text.trim_start_matches("0o"), 8).map_err(|_| {
                PolicyError::BadMode {
                    rule: name.clone(),
                    value: text.clone(),
                }
            })?),
        };
        Ok(Self {
            name: name.clone(),
            agents: config.agents.clone(),
            session_ids: config.session_ids.clone(),
            session_id_glob: compile_glob(&name, &config.session_id_glob)?,
            title_contains: config.title_contains.as_ref().map(|text| text.to_lowercase()),
            title_glob: compile_glob(&name, &config.title_glob)?,
            cwd_glob: compile_glob(&name, &config.cwd_glob)?,
            file_glob: compile_glob(&name, &config.file_glob)?,
            since: compile_time(&name, &config.since)?,
            first_seen_after: compile_time(&name, &config.first_seen_after)?,
            first_seen_before: compile_time(&name, &config.first_seen_before)?,
            last_seen_after: compile_time(&name, &config.last_seen_after)?,
            last_seen_before: compile_time(&name, &config.last_seen_before)?,
            updated_after: compile_time(&name, &config.updated_after)?,
            updated_before: compile_time(&name, &config.updated_before)?,
            btime_after: compile_time(&name, &config.btime_after)?,
            btime_before: compile_time(&name, &config.btime_before)?,
            mtime_after: compile_time(&name, &config.mtime_after)?,
            mtime_before: compile_time(&name, &config.mtime_before)?,
            min_size: config.min_size,
            max_size: config.max_size,
            owner: config.owner.clone(),
            mode_within,
            other_readable: config.other_readable,
            other_writable: config.other_writable,
            group_readable: config.group_readable,
            group_writable: config.group_writable,
        })
    }

    /// 判断规则是否命中；未命中时给出第一条不满足的条件。
    #[must_use]
    pub fn matches(
        &self,
        facts: &SessionFacts,
        rule_created_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
        current_uid: u32,
    ) -> MatchOutcome {
        if !self.agents.is_empty() && !self.agents.iter().any(|name| name == &facts.agent) {
            return MatchOutcome::NotMatched("agent 不在列表里".into());
        }
        if !self.session_ids.is_empty() && !self.session_ids.iter().any(|id| id == &facts.session_id) {
            return MatchOutcome::NotMatched("会话 ID 不在列表里".into());
        }
        if let Some(matcher) = &self.session_id_glob
            && !matcher.is_match(&facts.session_id)
        {
            return MatchOutcome::NotMatched("会话 ID 不匹配 glob".into());
        }
        if let Some(needle) = &self.title_contains {
            let title = facts.title.as_deref().unwrap_or_default().to_lowercase();
            if !title.contains(needle) {
                return MatchOutcome::NotMatched("标题不包含关键词".into());
            }
        }
        if let Some(matcher) = &self.title_glob
            && !matcher.is_match(facts.title.as_deref().unwrap_or_default())
        {
            return MatchOutcome::NotMatched("标题不匹配 glob".into());
        }
        if let Some(matcher) = &self.cwd_glob
            && !matcher.is_match(&facts.cwd)
        {
            return MatchOutcome::NotMatched("工作目录不匹配 glob".into());
        }
        if let Some(matcher) = &self.file_glob {
            let Some(file) = &facts.file else {
                return MatchOutcome::NotMatched("会话文件未定位到".into());
            };
            if !matcher.is_match(&file.path) {
                return MatchOutcome::NotMatched("会话文件路径不匹配 glob".into());
            }
        }

        match self.relative_bound(&self.since, facts.first_seen, rule_created_at, now) {
            BoundResult::Pass => {}
            BoundResult::Fail => return MatchOutcome::NotMatched("首次见到时间早于 since".into()),
            BoundResult::Unresolved => return MatchOutcome::NotMatched("规则首次生效时间未知".into()),
        }
        for (bound, value, label) in [
            (&self.first_seen_after, facts.first_seen, "首次见到时间晚于下界"),
            (&self.first_seen_before, facts.first_seen, "首次见到时间早于上界"),
            (&self.last_seen_after, facts.last_seen, "最后见到时间晚于下界"),
            (&self.last_seen_before, facts.last_seen, "最后见到时间早于上界"),
        ] {
            match self.absolute_bound(bound, value, true, now, rule_created_at) {
                BoundResult::Pass => {}
                BoundResult::Fail => return MatchOutcome::NotMatched(label.into()),
                BoundResult::Unresolved => return MatchOutcome::NotMatched("规则首次生效时间未知".into()),
            }
        }
        if let Some(updated) = facts.updated_at {
            match self.absolute_bound(&self.updated_after, updated, true, now, rule_created_at) {
                BoundResult::Pass => {}
                BoundResult::Fail => return MatchOutcome::NotMatched("更新时间晚于下界".into()),
                BoundResult::Unresolved => return MatchOutcome::NotMatched("规则首次生效时间未知".into()),
            }
            match self.absolute_bound(&self.updated_before, updated, false, now, rule_created_at) {
                BoundResult::Pass => {}
                BoundResult::Fail => return MatchOutcome::NotMatched("更新时间早于上界".into()),
                BoundResult::Unresolved => return MatchOutcome::NotMatched("规则首次生效时间未知".into()),
            }
        } else if self.updated_after.is_some() || self.updated_before.is_some() {
            return MatchOutcome::NotMatched("agent 没有报告更新时间".into());
        }

        if self.btime_after.is_some()
            || self.btime_before.is_some()
            || self.mtime_after.is_some()
            || self.mtime_before.is_some()
            || self.min_size.is_some()
            || self.max_size.is_some()
            || self.owner.is_some()
            || self.mode_within.is_some()
            || self.other_readable.is_some()
            || self.other_writable.is_some()
            || self.group_readable.is_some()
            || self.group_writable.is_some()
        {
            let Some(file) = &facts.file else {
                return MatchOutcome::NotMatched("会话文件未定位到，文件条件无法判定".into());
            };
            if let Some(min) = self.min_size
                && file.size < min
            {
                return MatchOutcome::NotMatched("文件小于 min_size".into());
            }
            if let Some(max) = self.max_size
                && file.size > max
            {
                return MatchOutcome::NotMatched("文件大于 max_size".into());
            }
            if let Some(spec) = &self.owner {
                let matched = match spec {
                    OwnerSpec::SelfOwner => file.owner_uid == current_uid,
                    OwnerSpec::Other => file.owner_uid != current_uid,
                    OwnerSpec::Uid(uid) => file.owner_uid == *uid,
                };
                if !matched {
                    return MatchOutcome::NotMatched("文件属主不符合条件".into());
                }
            }
            if let Some(mask) = self.mode_within
                && file.mode & !mask != 0
            {
                return MatchOutcome::NotMatched("权限位超出 mode_within 掩码".into());
            }
            for (expected, bit, label) in [
                (self.other_readable, 0o004, "其他用户可读"),
                (self.other_writable, 0o002, "其他用户可写"),
                (self.group_readable, 0o040, "同组用户可读"),
                (self.group_writable, 0o020, "同组用户可写"),
            ] {
                if let Some(want) = expected
                    && (file.mode & bit != 0) != want
                {
                    return MatchOutcome::NotMatched(format!("{label} 条件不满足"));
                }
            }
            for (bound, value, label) in [
                (&self.btime_after, file.btime, "文件 btime 晚于下界"),
                (&self.btime_before, file.btime, "文件 btime 早于上界"),
                (&self.mtime_after, file.mtime, "文件 mtime 晚于下界"),
                (&self.mtime_before, file.mtime, "文件 mtime 早于上界"),
            ] {
                if bound.is_none() {
                    continue;
                }
                let Some(value) = value else {
                    return MatchOutcome::NotMatched("文件时间戳不可用".into());
                };
                match self.absolute_bound(bound, value, true, now, rule_created_at) {
                    BoundResult::Pass => {}
                    BoundResult::Fail => return MatchOutcome::NotMatched(label.into()),
                    BoundResult::Unresolved => return MatchOutcome::NotMatched("规则首次生效时间未知".into()),
                }
            }
        }
        MatchOutcome::Matched
    }

    fn resolve(
        &self,
        spec: TimeSpec,
        rule_created_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        match spec {
            TimeSpec::At(at) => Some(at),
            TimeSpec::RuleCreated => rule_created_at,
            TimeSpec::Relative(delta) => Some(now - delta),
        }
    }

    fn relative_bound(
        &self,
        bound: &Option<TimeSpec>,
        value: DateTime<Utc>,
        rule_created_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> BoundResult {
        let Some(spec) = bound else {
            return BoundResult::Pass;
        };
        match self.resolve(*spec, rule_created_at, now) {
            Some(limit) if value >= limit => BoundResult::Pass,
            Some(_) => BoundResult::Fail,
            None => BoundResult::Unresolved,
        }
    }

    fn absolute_bound(
        &self,
        bound: &Option<TimeSpec>,
        value: DateTime<Utc>,
        lower: bool,
        now: DateTime<Utc>,
        rule_created_at: Option<DateTime<Utc>>,
    ) -> BoundResult {
        let Some(spec) = bound else {
            return BoundResult::Pass;
        };
        match self.resolve(*spec, rule_created_at, now) {
            Some(limit) => {
                let ok = if lower { value >= limit } else { value <= limit };
                if ok {
                    BoundResult::Pass
                } else {
                    BoundResult::Fail
                }
            }
            None => BoundResult::Unresolved,
        }
    }
}

enum BoundResult {
    Pass,
    Fail,
    Unresolved,
}

/// 编译好的策略引擎。
#[derive(Debug, Clone)]
pub struct Policy {
    pub default: Decision,
    pub sticky_deny: bool,
    deny: Vec<Rule>,
    allow: Vec<Rule>,
}

impl Policy {
    pub fn compile(config: &PolicyConfig) -> Result<Self, PolicyError> {
        let deny = config
            .deny
            .iter()
            .map(Rule::compile)
            .collect::<Result<Vec<_>, _>>()?;
        let allow = config
            .allow
            .iter()
            .map(Rule::compile)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            default: config.default,
            sticky_deny: config.sticky_deny,
            deny,
            allow,
        })
    }

    /// 全部规则名，用于向台账登记规则首次生效时间。
    #[must_use]
    pub fn rule_names(&self) -> Vec<&str> {
        self.deny
            .iter()
            .chain(self.allow.iter())
            .map(|rule| rule.name.as_str())
            .collect()
    }

    /// 是否存在依赖会话文件事实的规则；没有时服务层可以跳过文件定位。
    #[must_use]
    pub fn needs_file_facts(&self) -> bool {
        self.deny.iter().chain(self.allow.iter()).any(|rule| {
            rule.file_glob.is_some()
                || rule.min_size.is_some()
                || rule.max_size.is_some()
                || rule.owner.is_some()
                || rule.mode_within.is_some()
                || rule.other_readable.is_some()
                || rule.other_writable.is_some()
                || rule.group_readable.is_some()
                || rule.group_writable.is_some()
                || rule.btime_after.is_some()
                || rule.btime_before.is_some()
                || rule.mtime_after.is_some()
                || rule.mtime_before.is_some()
        })
    }

    /// 给出最终判定。
    #[must_use]
    pub fn evaluate(
        &self,
        facts: &SessionFacts,
        remembered: Option<Remembered>,
        rule_created_at: impl Fn(&str) -> Option<DateTime<Utc>>,
        now: DateTime<Utc>,
        current_uid: u32,
    ) -> Outcome {
        if self.sticky_deny
            && let Some(previous) = remembered
            && previous.decision == Decision::Deny
        {
            return Outcome {
                decision: Decision::Deny,
                rule: None,
                reason: "该会话此前已被拒绝，历史拒绝永久生效".into(),
            };
        }
        for rule in &self.deny {
            let created = rule_created_at(&rule.name);
            if let MatchOutcome::Matched = rule.matches(facts, created, now, current_uid) {
                return Outcome {
                    decision: Decision::Deny,
                    rule: Some(rule.name.clone()),
                    reason: format!("命中拒绝规则 {:?}", rule.name),
                };
            }
        }
        for rule in &self.allow {
            let created = rule_created_at(&rule.name);
            if let MatchOutcome::Matched = rule.matches(facts, created, now, current_uid) {
                return Outcome {
                    decision: Decision::Allow,
                    rule: Some(rule.name.clone()),
                    reason: format!("命中放行规则 {:?}", rule.name),
                };
            }
        }
        Outcome {
            decision: self.default,
            rule: None,
            reason: format!("未命中任何规则，采用默认策略 {}", self.default.as_str()),
        }
    }

    /// 逐条规则给出匹配情况，用于 `policy explain`。
    #[must_use]
    pub fn explain(
        &self,
        facts: &SessionFacts,
        remembered: Option<Remembered>,
        rule_created_at: impl Fn(&str) -> Option<DateTime<Utc>>,
        now: DateTime<Utc>,
        current_uid: u32,
    ) -> Vec<(Decision, String, MatchOutcome)> {
        let _ = remembered;
        let mut rows = Vec::new();
        for rule in &self.deny {
            let created = rule_created_at(&rule.name);
            rows.push((
                Decision::Deny,
                rule.name.clone(),
                rule.matches(facts, created, now, current_uid),
            ));
        }
        for rule in &self.allow {
            let created = rule_created_at(&rule.name);
            rows.push((
                Decision::Allow,
                rule.name.clone(),
                rule.matches(facts, created, now, current_uid),
            ));
        }
        rows
    }
}

/// 把时间格式化成统一的 RFC3339 字符串。
#[must_use]
pub fn format_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    fn facts() -> SessionFacts {
        SessionFacts {
            agent: "omp".into(),
            session_id: "01a0e148".into(),
            cwd: "/Users/x/tmp".into(),
            title: Some("Implement ACP viewer".into()),
            updated_at: Some(at("2026-09-27T05:38:05Z")),
            first_seen: at("2026-09-27T05:00:00Z"),
            last_seen: at("2026-09-27T06:00:00Z"),
            file: Some(FileFacts {
                path: "/Users/x/.omp/agent/sessions/-tmp/2026-09-27T05-00-00_01a0e148.jsonl".into(),
                size: 4096,
                btime: Some(at("2026-09-27T05:00:00Z")),
                mtime: Some(at("2026-09-27T05:38:05Z")),
                mode: 0o600,
                owner_uid: 501,
                group_gid: 20,
            }),
        }
    }

    fn policy(config: &PolicyConfig) -> Policy {
        Policy::compile(config).unwrap()
    }

    fn nothing(_: &str) -> Option<DateTime<Utc>> {
        None
    }

    fn rule(name: &str) -> RuleConfig {
        RuleConfig {
            name: name.into(),
            ..RuleConfig::default()
        }
    }

    #[test]
    fn 默认策略不暴露() {
        let engine = policy(&PolicyConfig::default());
        let outcome = engine.evaluate(&facts(), None, nothing, Utc::now(), 501);
        assert_eq!(outcome.decision, Decision::Deny);
        assert!(outcome.rule.is_none());
        assert!(outcome.reason.contains("默认策略"));
    }

    #[test]
    fn 命中放行规则() {
        let mut allow = rule("点名");
        allow.session_ids = vec!["01a0e148".into()];
        let engine = policy(&PolicyConfig {
            allow: vec![allow],
            ..PolicyConfig::default()
        });
        let outcome = engine.evaluate(&facts(), None, nothing, Utc::now(), 501);
        assert_eq!(outcome.decision, Decision::Allow);
        assert_eq!(outcome.rule.as_deref(), Some("点名"));
    }

    #[test]
    fn 拒绝规则优先于放行规则() {
        let mut allow = rule("全放行");
        allow.agents = vec!["omp".into()];
        let mut deny = rule("涉密目录");
        deny.cwd_glob = Some("**/secret/**".into());
        let engine = policy(&PolicyConfig {
            allow: vec![allow],
            deny: vec![deny],
            ..PolicyConfig::default()
        });
        assert_eq!(
            engine.evaluate(&facts(), None, nothing, Utc::now(), 501).decision,
            Decision::Allow
        );

        let mut facts = facts();
        facts.cwd = "/Users/x/secret/project".into();
        assert_eq!(
            engine.evaluate(&facts, None, nothing, Utc::now(), 501).decision,
            Decision::Deny
        );
    }

    #[test]
    fn 历史拒绝永久生效() {
        let mut allow = rule("全放行");
        allow.agents = vec!["omp".into()];
        let engine = policy(&PolicyConfig {
            allow: vec![allow],
            ..PolicyConfig::default()
        });
        let remembered = Some(Remembered {
            decision: Decision::Deny,
        });
        let outcome = engine.evaluate(&facts(), remembered, nothing, Utc::now(), 501);
        assert_eq!(outcome.decision, Decision::Deny);
        assert!(outcome.reason.contains("历史拒绝"));
    }

    #[test]
    fn 关闭_sticky_deny_后历史拒绝不再生效() {
        let mut allow = rule("全放行");
        allow.agents = vec!["omp".into()];
        let engine = policy(&PolicyConfig {
            allow: vec![allow],
            sticky_deny: false,
            ..PolicyConfig::default()
        });
        let remembered = Some(Remembered {
            decision: Decision::Deny,
        });
        assert_eq!(
            engine.evaluate(&facts(), remembered, nothing, Utc::now(), 501).decision,
            Decision::Allow
        );
    }

    #[test]
    fn 只放行规则生效之后新增的会话() {
        let mut allow = rule("以后新增的");
        allow.since = Some("rule-created".into());
        let engine = policy(&PolicyConfig {
            allow: vec![allow],
            ..PolicyConfig::default()
        });
        let created = |name: &str| (name == "以后新增的").then(|| at("2026-09-27T05:30:00Z"));

        // first_seen 05:00 早于规则生效时间 05:30 → 不放行
        let old = engine.evaluate(&facts(), None, created, Utc::now(), 501);
        assert_eq!(old.decision, Decision::Deny);

        // 规则生效之后首见的会话 → 放行
        let mut fresh = facts();
        fresh.first_seen = at("2026-09-27T06:00:00Z");
        let outcome = engine.evaluate(&fresh, None, created, Utc::now(), 501);
        assert_eq!(outcome.decision, Decision::Allow);
    }

    #[test]
    fn 规则生效时间未知时不放行() {
        let mut allow = rule("以后新增的");
        allow.since = Some("rule-created".into());
        let engine = policy(&PolicyConfig {
            allow: vec![allow],
            ..PolicyConfig::default()
        });
        assert_eq!(
            engine.evaluate(&facts(), None, nothing, Utc::now(), 501).decision,
            Decision::Deny
        );
    }

    #[test]
    fn 按文件权限判定() {
        let mut deny = rule("他人可读");
        deny.other_readable = Some(true);
        let engine = policy(&PolicyConfig {
            deny: vec![deny],
            ..PolicyConfig::default()
        });
        // 0600：其他用户不可读 → 条件不满足
        let rows = engine.explain(&facts(), None, nothing, Utc::now(), 501);
        assert!(
            matches!(rows[0].2, MatchOutcome::NotMatched(_)),
            "0600 的文件不该命中 other_readable=true"
        );
        // 0644：其他用户可读 → 命中拒绝规则
        let mut open = facts();
        open.file.as_mut().unwrap().mode = 0o644;
        let rows = engine.explain(&open, None, nothing, Utc::now(), 501);
        assert_eq!(rows[0].2, MatchOutcome::Matched);
        assert_eq!(
            engine.evaluate(&open, None, nothing, Utc::now(), 501).decision,
            Decision::Deny
        );
    }

    #[test]
    fn 按属主判定() {
        let mut deny = rule("非本人文件");
        deny.owner = Some(OwnerSpec::Other);
        let engine = policy(&PolicyConfig {
            deny: vec![deny],
            ..PolicyConfig::default()
        });
        // 当前 uid 与文件属主相同 → OwnerSpec::Other 不命中
        let rows = engine.explain(&facts(), None, nothing, Utc::now(), 501);
        assert!(matches!(rows[0].2, MatchOutcome::NotMatched(_)));
        // 当前 uid 不同 → 命中
        let rows = engine.explain(&facts(), None, nothing, Utc::now(), 999);
        assert_eq!(rows[0].2, MatchOutcome::Matched);
    }

    #[test]
    fn 权限掩码条件() {
        let mut allow = rule("仅本人可读");
        allow.mode_within = Some("0600".into());
        let engine = policy(&PolicyConfig {
            allow: vec![allow],
            ..PolicyConfig::default()
        });
        assert_eq!(
            engine.evaluate(&facts(), None, nothing, Utc::now(), 501).decision,
            Decision::Allow
        );

        let mut loose = facts();
        loose.file.as_mut().unwrap().mode = 0o640;
        assert_eq!(
            engine.evaluate(&loose, None, nothing, Utc::now(), 501).decision,
            Decision::Deny
        );
    }

    #[test]
    fn 缺少文件信息时文件条件判为不匹配() {
        let mut allow = rule("需要文件信息");
        allow.max_size = Some(1024);
        let engine = policy(&PolicyConfig {
            allow: vec![allow],
            ..PolicyConfig::default()
        });
        let mut no_file = facts();
        no_file.file = None;
        let outcome = engine.evaluate(&no_file, None, nothing, Utc::now(), 501);
        assert_eq!(outcome.decision, Decision::Deny);

        let rows = engine.explain(&no_file, None, nothing, Utc::now(), 501);
        assert_eq!(
            rows[0].2,
            MatchOutcome::NotMatched("会话文件未定位到，文件条件无法判定".into())
        );
    }

    #[test]
    fn 文件大小表达式解析() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("8MiB"), Some(8 * 1024 * 1024));
        assert_eq!(parse_size("512kib"), Some(512 * 1024));
        assert_eq!(parse_size("2GB"), Some(2_000_000_000));
        assert_eq!(parse_size("8 兆"), None);
    }

    #[test]
    fn 时间表达式解析() {
        let now = at("2026-09-27T12:00:00Z");
        assert_eq!(parse_time_expression("-7d", now), Some(at("2026-09-20T12:00:00Z")));
        assert_eq!(parse_time_expression("-24h", now), Some(at("2026-09-26T12:00:00Z")));
        assert_eq!(
            parse_time_expression("2026-01-01T00:00:00Z", now),
            Some(at("2026-01-01T00:00:00Z"))
        );
        assert_eq!(parse_time_expression("昨天", now), None);
    }

    #[test]
    fn 非法配置显式报错() {
        let mut allow = rule("坏 glob");
        allow.cwd_glob = Some("[".into());
        assert!(matches!(
            Policy::compile(&PolicyConfig {
                allow: vec![allow],
                ..PolicyConfig::default()
            }),
            Err(PolicyError::BadGlob { .. })
        ));

        let mut allow = rule("坏时间");
        allow.first_seen_after = Some("前天".into());
        assert!(matches!(
            Policy::compile(&PolicyConfig {
                allow: vec![allow],
                ..PolicyConfig::default()
            }),
            Err(PolicyError::BadTime { .. })
        ));

        let mut allow = rule("坏掩码");
        allow.mode_within = Some("9999".into());
        assert!(matches!(
            Policy::compile(&PolicyConfig {
                allow: vec![allow],
                ..PolicyConfig::default()
            }),
            Err(PolicyError::BadMode { .. })
        ));
    }

    #[test]
    fn 标题与目录条件() {
        let mut allow = rule("标题命中");
        allow.title_contains = Some("acp".into());
        let engine = policy(&PolicyConfig {
            allow: vec![allow],
            ..PolicyConfig::default()
        });
        assert_eq!(
            engine.evaluate(&facts(), None, nothing, Utc::now(), 501).decision,
            Decision::Allow
        );

        let mut other = rule("目录命中");
        other.cwd_glob = Some("/Users/*/tmp".into());
        let engine = policy(&PolicyConfig {
            allow: vec![other],
            ..PolicyConfig::default()
        });
        assert_eq!(
            engine.evaluate(&facts(), None, nothing, Utc::now(), 501).decision,
            Decision::Allow
        );
        let mut far = facts();
        far.cwd = "/srv/data".into();
        assert_eq!(
            engine.evaluate(&far, None, nothing, Utc::now(), 501).decision,
            Decision::Deny
        );
    }

    #[test]
    fn 时间窗条件() {
        let mut allow = rule("最近一天");
        allow.last_seen_after = Some("-24h".into());
        let engine = policy(&PolicyConfig {
            allow: vec![allow],
            ..PolicyConfig::default()
        });
        let now = at("2026-09-27T06:30:00Z");
        assert_eq!(
            engine.evaluate(&facts(), None, nothing, now, 501).decision,
            Decision::Allow
        );
        let later = at("2026-10-05T06:30:00Z");
        assert_eq!(
            engine.evaluate(&facts(), None, nothing, later, 501).decision,
            Decision::Deny
        );
    }
}
