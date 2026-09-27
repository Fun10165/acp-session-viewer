//! 配置：服务端参数、agent 注册表与可见性策略。

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::policy::PolicyConfig;

/// 默认的服务端地址。
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8787";
/// 默认端口。
pub const DEFAULT_PORT: u16 = 8787;
/// 默认的单次操作超时。
pub const DEFAULT_TIMEOUT_SECS: f64 = 180.0;
/// 默认的连接空闲回收时间。
pub const DEFAULT_IDLE_TIMEOUT_SECS: f64 = 300.0;
/// 默认的单连接最大使用次数。
pub const DEFAULT_MAX_USES: u32 = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub agent: Vec<AgentSpec>,
    #[serde(default)]
    pub policy: PolicyConfig,
    /// 调用者身份；省略时使用零配置的默认主体。
    #[serde(default)]
    pub principal: Vec<PrincipalConfig>,
    #[serde(default)]
    pub acl: AclConfig,
}

/// 一个调用者身份。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrincipalConfig {
    pub name: String,
    /// 访问令牌；与 token_file 二选一。
    pub token: Option<String>,
    pub token_file: Option<PathBuf>,
    #[serde(default)]
    pub groups: Vec<String>,
}

/// 会话权限配置。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AclConfig {
    /// 新会话套用的模板。
    #[serde(default)]
    pub default: AclTemplate,
    /// 针对具体会话的覆盖，键是 `agent/会话 ID`；优先级高于台账。
    #[serde(default)]
    pub sessions: BTreeMap<String, AclTemplate>,
}

/// 权限模板：属主、属组、模式位。
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AclTemplate {
    pub owner: Option<String>,
    pub group: Option<String>,
    pub mode: Option<String>,
}

impl AclTemplate {
    /// 解析成具体权限；缺省项用给定值补齐。
    pub fn resolve(&self, fallback_owner: &str, fallback_mode: u16) -> Result<crate::acl::ObjectAcl, ConfigError> {
        let mode = match &self.mode {
            None => fallback_mode,
            Some(text) => crate::acl::parse_mode(text).map_err(|error| ConfigError::BadAcl(error.to_string()))?,
        };
        Ok(crate::acl::ObjectAcl::new(
            self.owner.as_deref().unwrap_or(fallback_owner),
            self.group.as_deref(),
            mode,
        ))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// 绑定地址：`127.0.0.1`、`127.0.0.1:8787`、`[::1]:8787` 都接受。
    #[serde(default = "default_bind")]
    pub bind: String,
    /// 令牌文件位置，缺省为 `~/.config/acp-session-viewer/token`。
    pub token_file: Option<PathBuf>,
    /// 台账数据库位置，缺省为 `~/.local/share/acp-session-viewer/catalog.db`。
    pub catalog: Option<PathBuf>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: f64,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: f64,
    #[serde(default = "default_max_uses")]
    pub max_uses_per_connection: u32,
    /// 会话文件定位规则：agent 名称 → 文件路径模式（支持 `{id}` 占位符与 `~`）。
    #[serde(default)]
    pub locate: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub description: String,
}

fn default_bind() -> String {
    format!("127.0.0.1:{DEFAULT_PORT}")
}

fn default_timeout() -> f64 {
    DEFAULT_TIMEOUT_SECS
}

fn default_idle_timeout() -> f64 {
    DEFAULT_IDLE_TIMEOUT_SECS
}

fn default_max_uses() -> u32 {
    DEFAULT_MAX_USES
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            token_file: None,
            catalog: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            max_uses_per_connection: DEFAULT_MAX_USES,
            locate: default_locate_rules(),
        }
    }
}

/// 内置的会话文件定位规则（只覆盖已知布局；定位不到时相关规则判为不匹配）。
fn default_locate_rules() -> BTreeMap<String, Vec<String>> {
    let mut rules = BTreeMap::new();
    rules.insert(
        "omp".to_string(),
        vec!["~/.omp/agent/sessions/**/*{id}*.jsonl".to_string()],
    );
    rules.insert(
        "codex".to_string(),
        vec![
            "~/.codex/sessions/**/*{id}*.jsonl".to_string(),
            "~/.codex/archived_sessions/*{id}*.jsonl".to_string(),
        ],
    );
    rules
}

/// 内置 agent 列表。
#[must_use]
pub fn default_agents() -> Vec<AgentSpec> {
    vec![
        AgentSpec {
            name: "omp".into(),
            command: vec!["omp".into(), "acp".into()],
            description: "omp 自带的 ACP 服务端".into(),
        },
        AgentSpec {
            name: "opencode".into(),
            command: vec!["opencode".into(), "acp".into()],
            description: "OpenCode 自带的 ACP 服务端".into(),
        },
        AgentSpec {
            name: "codex".into(),
            command: vec![
                "npx".into(),
                "-y".into(),
                "@agentclientprotocol/codex-acp".into(),
            ],
            description: "Codex 官方 ACP 适配器（首次运行需要联网下载）".into(),
        },
        AgentSpec {
            name: "cursor".into(),
            command: vec!["cursor-agent".into(), "acp".into()],
            description: "Cursor Agent 的 ACP 服务端".into(),
        },
        AgentSpec {
            name: "gemini".into(),
            command: vec!["gemini".into(), "--acp".into()],
            description: "Gemini CLI 的 ACP 模式（不支持会话枚举）".into(),
        },
    ]
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("配置文件不存在: {0}")]
    Missing(PathBuf),
    #[error("读取配置文件失败 {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("解析配置文件失败 {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("agent 名称不能为空")]
    EmptyAgentName,
    #[error("agent {0:?} 的命令不能为空")]
    EmptyCommand(String),
    #[error("agent 名称重复: {0}")]
    DuplicateAgent(String),
    #[error("绑定地址非法: {0:?}（示例：127.0.0.1、127.0.0.1:8787、[::1]:8787）")]
    BadBind(String),
    #[error("取不到家目录，无法展开 {0:?}")]
    NoHome(String),
    #[error("没有可用的令牌：请用 --token 指定，或设置 ACP_VIEW_TOKEN，或先由服务端生成 {path}")]
    MissingToken { path: PathBuf },
    #[error("权限配置非法：{0}")]
    BadAcl(String),
    #[error("主体配置非法：{0}")]
    BadPrincipal(String),
}

impl Config {
    /// 从 TOML 文件加载配置。
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.is_file() {
            return Err(ConfigError::Missing(path.to_path_buf()));
        }
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let mut config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if config.agent.is_empty() {
            config.agent = default_agents();
        }
        if config.server.locate.is_empty() {
            config.server.locate = default_locate_rules();
        }
        config.validate()?;
        Ok(config)
    }

    /// 内置默认配置（不读文件）。
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            server: ServerConfig::default(),
            agent: default_agents(),
            policy: PolicyConfig::default(),
            principal: Vec::new(),
            acl: AclConfig::default(),
        }
    }

    /// 默认主体名（服务端运行用户的用户名）。
    #[must_use]
    pub fn default_principal_name() -> String {
        std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "owner".to_string())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut seen = std::collections::BTreeSet::new();
        for spec in &self.agent {
            if spec.name.trim().is_empty() {
                return Err(ConfigError::EmptyAgentName);
            }
            if spec.command.is_empty() {
                return Err(ConfigError::EmptyCommand(spec.name.clone()));
            }
            if !seen.insert(spec.name.clone()) {
                return Err(ConfigError::DuplicateAgent(spec.name.clone()));
            }
        }
        let mut principals = std::collections::BTreeSet::new();
        for principal in &self.principal {
            if principal.name.trim().is_empty() {
                return Err(ConfigError::BadPrincipal("主体名称不能为空".into()));
            }
            if !principals.insert(principal.name.clone()) {
                return Err(ConfigError::BadPrincipal(format!("主体名称重复: {}", principal.name)));
            }
            if principal.token.is_none() && principal.token_file.is_none() {
                return Err(ConfigError::BadPrincipal(format!(
                    "主体 {:?} 必须配置 token 或 token_file",
                    principal.name
                )));
            }
        }
        // 提前校验权限写法，避免运行到一半才发现配置有误。
        self.acl.default.resolve("校验", 0o750)?;
        for (key, template) in &self.acl.sessions {
            if !key.contains('/') {
                return Err(ConfigError::BadAcl(format!(
                    "会话权限的键必须写成 agent/会话 ID 的形式: {key:?}"
                )));
            }
            template.resolve("校验", 0o750)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn agent(&self, name: &str) -> Option<&AgentSpec> {
        self.agent.iter().find(|spec| spec.name == name)
    }
}

impl ServerConfig {
    /// 解析绑定地址。
    pub fn socket_addr(&self) -> Result<SocketAddr, ConfigError> {
        parse_bind(&self.bind, DEFAULT_PORT)
    }

    /// 令牌文件位置。
    pub fn token_path(&self) -> Result<PathBuf, ConfigError> {
        match &self.token_file {
            Some(path) => expand_home(path),
            None => Ok(config_dir()?.join("token")),
        }
    }

    /// 台账数据库位置。
    pub fn catalog_path(&self) -> Result<PathBuf, ConfigError> {
        match &self.catalog {
            Some(path) => expand_home(path),
            None => Ok(data_dir()?.join("catalog.db")),
        }
    }
}

/// 解析绑定地址：接受 IP 字面量（可选端口）与 `localhost`。
///
/// 这里不做域名解析：域名会引入 DNS 层面的不确定性（例如某些解析器对不存在的名字返回通配地址），
/// 而绑定地址本来就该是明确的本地地址。
pub fn parse_bind(value: &str, default_port: u16) -> Result<SocketAddr, ConfigError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::BadBind(value.to_string()));
    }
    if let Ok(addr) = trimmed.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, default_port));
    }
    let (host, port_text) = match trimmed.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (trimmed, None),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        let port = match port_text {
            Some(text) => text
                .parse::<u16>()
                .map_err(|_| ConfigError::BadBind(value.to_string()))?,
            None => default_port,
        };
        return Ok(SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port));
    }
    Err(ConfigError::BadBind(value.to_string()))
}

/// 展开路径开头的 `~`。
pub fn expand_home(path: &Path) -> Result<PathBuf, ConfigError> {
    let text = path.to_string_lossy();
    let Some(rest) = text.strip_prefix('~') else {
        return Ok(path.to_path_buf());
    };
    let home = home_dir().ok_or_else(|| ConfigError::NoHome(text.to_string()))?;
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        Ok(home)
    } else {
        Ok(home.join(rest))
    }
}

#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn config_dir() -> Result<PathBuf, ConfigError> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".config")))
        .ok_or_else(|| ConfigError::NoHome("~/.config".into()))?;
    Ok(base.join("acp-session-viewer"))
}

fn data_dir() -> Result<PathBuf, ConfigError> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".local/share")))
        .ok_or_else(|| ConfigError::NoHome("~/.local/share".into()))?;
    Ok(base.join("acp-session-viewer"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn 解析绑定地址的几种写法() {
        assert_eq!(
            parse_bind("127.0.0.1", 8787).unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787)
        );
        assert_eq!(
            parse_bind("0.0.0.0:9000", 8787).unwrap().port(),
            9000
        );
        assert_eq!(
            parse_bind("[::1]:8080", 8787).unwrap(),
            "[::1]:8080".parse().unwrap()
        );
        assert_eq!(parse_bind("::1", 8787).unwrap().port(), 8787);
        assert_eq!(
            parse_bind("localhost:9000", 8787).unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9000)
        );
        assert!(parse_bind("", 8787).is_err());
        // 域名不做解析，避免 DNS 层面的不确定性
        assert!(parse_bind("example.com", 8787).is_err());
        assert!(parse_bind("不是地址", 8787).is_err());
        assert!(parse_bind("localhost:不是端口", 8787).is_err());
    }

    #[test]
    fn 展开家目录() {
        let home = home_dir().unwrap();
        assert_eq!(expand_home(Path::new("~/x")).unwrap(), home.join("x"));
        assert_eq!(expand_home(Path::new("/abs")).unwrap(), PathBuf::from("/abs"));
    }

    #[test]
    fn 内置配置可用() {
        let config = Config::builtin();
        assert_eq!(config.agent.len(), 5);
        config.validate().unwrap();
        assert!(config.agent("omp").is_some());
        assert!(config.agent("不存在").is_none());
    }

    #[test]
    fn 空配置使用内置_agent() {
        let dir = std::env::temp_dir().join(format!("acp-view-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[server]\nbind = \"127.0.0.1:9001\"\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.agent.len(), 5);
        assert_eq!(config.server.bind, "127.0.0.1:9001");
        assert!(!config.server.locate.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn 缺少文件时显式报错() {
        assert!(matches!(
            Config::load(Path::new("/不存在的配置.toml")),
            Err(ConfigError::Missing(_))
        ));
    }

    #[test]
    fn 重复_agent_报错() {
        let config = Config {
            agent: vec![
                AgentSpec {
                    name: "a".into(),
                    command: vec!["echo".into()],
                    description: String::new(),
                },
                AgentSpec {
                    name: "a".into(),
                    command: vec!["echo".into()],
                    description: String::new(),
                },
            ],
            ..Config::builtin()
        };
        assert!(matches!(config.validate(), Err(ConfigError::DuplicateAgent(_))));
    }
}
