//! 命令行入口：`serve` 启动服务端，其余子命令作为客户端调用它。
//!
//! 客户端约定（便于 agent 调用）：
//!
//! * 输出受 `--budget` 字符上界约束，被丢弃的部分在 stderr 说明；
//! * 退出码：0 成功、1 失败、2 没找到会话；
//! * `show` 的选择器支持会话 ID、ID 前缀、标题子串与 `last`。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::acl::{ObjectAcl, Principal, parse_mode};
use crate::catalog::GroupUpdate;
use crate::client::{ApiClient, ClientError, resolve_config};
use crate::config::{Config, DEFAULT_PORT, parse_bind};
use crate::render::apply_budget;
use crate::server::{AppState, run_server};
use crate::service::Service;

const EXIT_ERROR: u8 = 1;
const EXIT_NOT_FOUND: u8 = 2;
const DEFAULT_BUDGET: usize = 12_000;

#[derive(Parser)]
#[command(
    name = "acp-view",
    version,
    about = "用 ACP 协议只读查看各家 agent 的会话（服务端 + 客户端）"
)]
pub struct Cli {
    /// 配置文件（TOML）；缺省时读 ~/.config/acp-session-viewer/config.toml
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 启动查询服务
    Serve(Box<ServeArgs>),
    /// 检查服务端是否可用
    Health(ClientArgs),
    /// 查看各 agent 的 ACP 能力
    Agents(ClientArgs),
    /// 列出会话
    List(Box<ListArgs>),
    /// 重放一条会话
    Show(Box<ShowArgs>),
    /// 查看或维护可见性台账
    Policy(PolicyArgs),
    /// 查看或修改会话权限（Linux 风格：属主 / 属组 / 模式位）
    Acl(AclArgs),
}

#[derive(Args)]
struct AclArgs {
    #[command(subcommand)]
    command: AclCommand,
}

#[derive(Subcommand)]
enum AclCommand {
    /// 查看一条会话的权限与来源
    Show {
        /// agent 名称
        agent: String,
        /// 会话 ID
        session: String,
    },
    /// 修改权限位，例如 acl chmod 640 omp/01a0e148
    Chmod {
        /// 权限，支持 640、0640 或 rw-r-----
        mode: String,
        /// 目标，写成 agent/会话 ID
        target: String,
    },
    /// 修改属主或属组，例如 acl chown alice:dev omp/01a0e148（group 留空表示清空属组）
    Chown {
        /// owner 或 owner:group
        owner: String,
        /// 目标，写成 agent/会话 ID
        target: String,
    },
}

#[derive(Args, Clone)]
struct ClientArgs {
    /// 服务端地址（默认 http://127.0.0.1:8787，也可用 ACP_VIEW_SERVER）
    #[arg(long)]
    server: Option<String>,
    /// 访问令牌（也可用 ACP_VIEW_TOKEN 或令牌文件）
    #[arg(long)]
    token: Option<String>,
    /// 令牌文件位置
    #[arg(long)]
    token_file: Option<PathBuf>,
}

#[derive(Args)]
struct ServeArgs {
    /// 绑定地址，HOST 或 HOST:PORT（默认取配置，内置 127.0.0.1:8787）
    #[arg(long)]
    bind: Option<String>,
    /// 覆盖绑定地址里的端口
    #[arg(long)]
    port: Option<u16>,
    /// 访问令牌；未提供时依次查找环境变量与令牌文件，都没有就新生成并写入
    #[arg(long)]
    token: Option<String>,
    /// 令牌文件位置
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// 台账数据库位置
    #[arg(long)]
    catalog: Option<PathBuf>,
    /// 每个 agent 操作的超时秒数
    #[arg(long, default_value_t = 180.0)]
    timeout: f64,
    /// agent 连接空闲多少秒后回收
    #[arg(long, default_value_t = 300.0)]
    idle_timeout: f64,
    /// 单条连接最多服务多少次操作后轮换
    #[arg(long, default_value_t = 64)]
    max_uses: u32,
    /// 不打印访问日志
    #[arg(long)]
    quiet: bool,
}

#[derive(Args)]
struct ListArgs {
    #[command(flatten)]
    client: ClientArgs,
    /// 只查指定 agent，可重复
    #[arg(long)]
    agent: Vec<String>,
    /// 按关键词过滤（匹配标题、会话 ID、工作目录、agent 名）
    #[arg(long)]
    query: Option<String>,
    /// 最多显示多少条
    #[arg(long, default_value_t = 30)]
    limit: usize,
    /// 输出 JSON
    #[arg(long)]
    json: bool,
    /// 输出字符上界（0 表示不限）
    #[arg(long, default_value_t = DEFAULT_BUDGET)]
    budget: usize,
}

#[derive(Args)]
struct ShowArgs {
    #[command(flatten)]
    client: ClientArgs,
    /// 会话 ID、ID 前缀、标题子串，或 last
    selector: String,
    /// 限定 agent，可重复
    #[arg(long)]
    agent: Vec<String>,
    /// 按该工作目录直接加载（需配合唯一的 --agent，跳过枚举）
    #[arg(long)]
    cwd: Option<String>,
    /// 输出原始事件 JSON（不受 --budget 限制）
    #[arg(long)]
    json: bool,
    /// 不显示思考片段
    #[arg(long)]
    no_thoughts: bool,
    /// 显示元信息事件
    #[arg(long)]
    meta: bool,
    /// 单段内容截断长度，0 表示不截断
    #[arg(long, default_value_t = 400)]
    truncate: usize,
    /// 输出字符上界（0 表示不限）
    #[arg(long, default_value_t = DEFAULT_BUDGET)]
    budget: usize,
}

#[derive(Args)]
struct PolicyArgs {
    #[command(subcommand)]
    command: PolicyCommand,
}

#[derive(Subcommand)]
enum PolicyCommand {
    /// 台账统计与最近判定
    Status {
        /// 显示最近多少条判定
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 解释一条会话的判定过程
    Explain {
        /// agent 名称
        agent: String,
        /// 会话 ID
        session: String,
    },
    /// 解除某条会话的历史拒绝（删除台账记录，首次见到时间随之重置）
    Forget {
        /// agent 名称
        agent: String,
        /// 会话 ID
        session: String,
    },
}

/// 条目宽度按显示宽度对齐。
fn pad(text: &str, width: usize) -> String {
    let current = UnicodeWidthStr::width(text);
    format!("{text}{}", " ".repeat(width.saturating_sub(current)))
}

fn ellipsize(text: &str, limit: usize) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if UnicodeWidthStr::width(single_line.as_str()) <= limit {
        return single_line;
    }
    let mut kept = String::new();
    let mut width = 0;
    for character in single_line.chars() {
        let character_width = UnicodeWidthStr::width(character.to_string().as_str());
        if width + character_width > limit.saturating_sub(1) {
            break;
        }
        kept.push(character);
        width += character_width;
    }
    kept.push('…');
    kept
}

fn format_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|header| UnicodeWidthStr::width(*header)).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(UnicodeWidthStr::width(cell.as_str()));
            }
        }
    }
    let mut lines = Vec::new();
    let header_line = headers
        .iter()
        .enumerate()
        .map(|(index, header)| pad(header, widths[index]))
        .collect::<Vec<_>>()
        .join("  ");
    lines.push(header_line);
    for row in rows {
        let line = row
            .iter()
            .enumerate()
            .map(|(index, cell)| pad(cell, widths.get(index).copied().unwrap_or(0)))
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(line);
    }
    lines.join("\n")
}

fn print_bounded(text: &str, budget: usize) {
    let (bounded, dropped) = apply_budget(text, budget);
    println!("{bounded}");
    if dropped > 0 {
        eprintln!("（另有 {dropped} 个字符因 --budget 上界未显示）");
    }
}

fn load_config(path: Option<PathBuf>) -> Result<Config, String> {
    match path {
        Some(path) => Config::load(&path).map_err(|error| error.to_string()),
        None => {
            let default_path = default_config_path()?;
            if default_path.is_file() {
                Config::load(&default_path).map_err(|error| error.to_string())
            } else {
                Ok(Config::builtin())
            }
        }
    }
}

fn default_config_path() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| crate::config::home_dir().map(|home| home.join(".config")))
        .ok_or_else(|| "取不到家目录".to_string())?;
    Ok(base.join("acp-session-viewer").join("config.toml"))
}

fn build_client(args: &ClientArgs) -> Result<ApiClient, ClientError> {
    let config = resolve_config(args.server.as_deref(), args.token.as_deref(), args.token_file.as_deref())?;
    Ok(ApiClient::new(&config))
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("从操作系统取随机数失败");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_secret(path: &Path, value: &str) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(format!("{value}\n").as_bytes())
        .map_err(|error| error.to_string())
}

/// 组装令牌到主体的映射：配置了 [[principal]] 时只用配置里的令牌，否则用单令牌 + 默认主体。
fn resolve_principals(config: &Config, fallback_token: &str) -> Result<Vec<(String, Principal)>, String> {
    if config.principal.is_empty() {
        return Ok(vec![(
            fallback_token.to_string(),
            Principal::new(&Config::default_principal_name(), Vec::new()),
        )]);
    }
    let mut list = Vec::new();
    for entry in &config.principal {
        let token = match (&entry.token, &entry.token_file) {
            (Some(token), _) if !token.is_empty() => token.clone(),
            (_, Some(path)) => std::fs::read_to_string(path)
                .map_err(|error| format!("读取令牌文件 {} 失败：{error}", path.display()))?
                .trim()
                .to_string(),
            _ => return Err(format!("主体 {:?} 既没有 token 也没有 token_file", entry.name)),
        };
        if token.is_empty() {
            return Err(format!("主体 {:?} 的令牌为空", entry.name));
        }
        list.push((token, Principal::new(&entry.name, entry.groups.clone())));
    }
    Ok(list)
}

fn split_target(target: &str) -> Result<(String, String), String> {
    match target.split_once('/') {
        Some((agent, session)) if !agent.is_empty() && !session.is_empty() => {
            Ok((agent.to_string(), session.to_string()))
        }
        _ => Err(format!("目标要写成 agent/会话 ID 的形式: {target:?}")),
    }
}

fn resolve_server_token(explicit: Option<&str>, path: &Path) -> Result<(String, String), String> {
    if let Some(token) = explicit.filter(|value| !value.is_empty()) {
        return Ok((token.to_string(), "--token 参数".into()));
    }
    if let Ok(token) = std::env::var("ACP_VIEW_TOKEN") {
        if !token.is_empty() {
            return Ok((token, "环境变量 ACP_VIEW_TOKEN".into()));
        }
    }
    if path.is_file() {
        if let Ok(text) = std::fs::read_to_string(path) {
            let token = text.trim().to_string();
            if !token.is_empty() {
                return Ok((token, format!("令牌文件 {}", path.display())));
            }
        }
    }
    let token = generate_token();
    write_secret(path, &token)?;
    Ok((token, format!("新生成并写入 {}", path.display())))
}

async fn run_serve(args: ServeArgs, config_path: Option<PathBuf>) -> ExitCode {
    let mut config = match load_config(config_path) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("配置错误: {message}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    if let Some(bind) = &args.bind {
        config.server.bind = bind.clone();
    }
    if let Some(port) = args.port {
        // 用解析后的地址重写端口，避免手工拼接在 IPv6 上出错。
        match parse_bind(&config.server.bind, DEFAULT_PORT) {
            Ok(addr) => config.server.bind = std::net::SocketAddr::new(addr.ip(), port).to_string(),
            Err(error) => {
                eprintln!("配置错误: {error}");
                return ExitCode::from(EXIT_ERROR);
            }
        }
    }
    if let Some(catalog) = &args.catalog {
        config.server.catalog = Some(catalog.clone());
    }
    config.server.timeout_secs = args.timeout;
    config.server.idle_timeout_secs = args.idle_timeout;
    config.server.max_uses_per_connection = args.max_uses;

    let addr = match parse_bind(&config.server.bind, DEFAULT_PORT) {
        Ok(addr) => addr,
        Err(error) => {
            eprintln!("配置错误: {error}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    let token_path = match config.server.token_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("配置错误: {error}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    let (token, source) = match resolve_server_token(args.token.as_deref(), &token_path) {
        Ok(value) => value,
        Err(message) => {
            eprintln!("配置错误: {message}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    let service = match Service::new(config) {
        Ok(service) => service,
        Err(error) => {
            eprintln!("启动失败: {error}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    service.start();

    println!("acp-view 服务端启动: http://{addr}");
    println!("令牌来源: {source}");
    println!(
        "可用 agent: {}",
        service
            .config()
            .agent
            .iter()
            .map(|spec| spec.name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "连接池: 空闲 {:.0} 秒回收，单条连接最多 {} 次操作后轮换",
        args.idle_timeout, args.max_uses
    );
    println!(
        "可见性策略: 默认 {}，拒绝规则 {} 条，放行规则 {} 条",
        match service.config().policy.default {
            crate::policy::Decision::Allow => "放行",
            crate::policy::Decision::Deny => "拒绝",
        },
        service.config().policy.deny.len(),
        service.config().policy.allow.len()
    );
    if !addr.ip().is_loopback() {
        println!("警告: 绑定在非回环地址，凡是能访问该端口并持有令牌的人都可以读取被放行的会话内容");
    }

    let principals = match resolve_principals(service.config(), &token) {
        Ok(principals) => principals,
        Err(message) => {
            eprintln!("配置错误: {message}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    println!(
        "主体: {}",
        principals
            .iter()
            .map(|(_, principal)| if principal.groups.is_empty() {
                principal.name.clone()
            } else {
                format!(
                    "{}（组：{}）",
                    principal.name,
                    principal.groups.iter().cloned().collect::<Vec<_>>().join(",")
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    );
    if let Some(acl) = service.config().acl.sessions.keys().next() {
        let _ = acl;
        println!(
            "权限覆盖: {} 条会话级配置",
            service.config().acl.sessions.len()
        );
    }

    let state = AppState {
        service: Arc::new(service),
        principals: Arc::new(principals),
        log_requests: !args.quiet,
    };
    match run_server(state, addr).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("服务端退出: {error}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

async fn run_health(args: ClientArgs) -> ExitCode {
    let client = match build_client(&args) {
        Ok(client) => client,
        Err(error) => return report_client_error(error),
    };
    match client.get_json("/health", &[]).await {
        Ok(value) => {
            println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
            ExitCode::SUCCESS
        }
        Err(error) => report_client_error(error),
    }
}

async fn run_agents(args: ClientArgs) -> ExitCode {
    let client = match build_client(&args) {
        Ok(client) => client,
        Err(error) => return report_client_error(error),
    };
    let value = match client.get_json("/agents", &[]).await {
        Ok(value) => value,
        Err(error) => return report_client_error(error),
    };
    let reports = value.get("agents").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut rows = Vec::new();
    for report in &reports {
        let capabilities = format!(
            "{} {}",
            if report["canList"].as_bool().unwrap_or(false) {
                "list"
            } else {
                "no-list"
            },
            if report["canLoad"].as_bool().unwrap_or(false) {
                "load"
            } else {
                "no-load"
            }
        );
        let detail = report["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| report["description"].as_str().unwrap_or_default().to_string());
        rows.push(vec![
            report["name"].as_str().unwrap_or_default().to_string(),
            {
                let implementation = report["implementation"].as_str().unwrap_or_default();
                if implementation.is_empty() {
                    "未知".to_string()
                } else {
                    implementation.to_string()
                }
            },
            capabilities,
            ellipsize(&detail, 60),
        ]);
    }
    print_bounded(&format_table(&["agent", "实现", "能力", "说明"], &rows), DEFAULT_BUDGET);
    ExitCode::SUCCESS
}

async fn run_list(args: ListArgs) -> ExitCode {
    let client = match build_client(&args.client) {
        Ok(client) => client,
        Err(error) => return report_client_error(error),
    };
    let mut params: Vec<(String, String)> = vec![("limit".into(), args.limit.to_string())];
    if let Some(query) = &args.query {
        params.push(("query".into(), query.clone()));
    }
    for name in &args.agent {
        params.push(("agent".into(), name.clone()));
    }
    let value = match client.get_json("/sessions", &params).await {
        Ok(value) => value,
        Err(error) => return report_client_error(error),
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
        return ExitCode::SUCCESS;
    }
    let sessions = value.get("sessions").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut rows = Vec::new();
    for session in &sessions {
        rows.push(vec![
            session["agent"].as_str().unwrap_or_default().to_string(),
            session["sessionId"].as_str().unwrap_or_default().to_string(),
            session["updatedAt"].as_str().unwrap_or_default().to_string(),
            ellipsize(session["title"].as_str().unwrap_or("(无标题)"), 60),
            ellipsize(session["cwd"].as_str().unwrap_or_default(), 40),
        ]);
    }
    let mut lines = vec![format!(
        "# 共 {} 条匹配，隐藏 {} 条，显示 {} 条",
        value["total"].as_u64().unwrap_or(0),
        value["hidden"].as_u64().unwrap_or(0),
        rows.len()
    )];
    if rows.is_empty() {
        lines.push("（没有匹配的会话）".into());
    } else {
        lines.push(format_table(&["agent", "会话 ID", "更新时间", "标题", "工作目录"], &rows));
    }
    for error in value.get("errors").and_then(Value::as_array).cloned().unwrap_or_default() {
        lines.push(format!(
            "# 跳过 {}: {}",
            error["agent"].as_str().unwrap_or_default(),
            error["message"].as_str().unwrap_or_default()
        ));
    }
    print_bounded(&lines.join("\n"), args.budget);
    ExitCode::SUCCESS
}

async fn run_show(args: ShowArgs) -> ExitCode {
    let client = match build_client(&args.client) {
        Ok(client) => client,
        Err(error) => return report_client_error(error),
    };
    let mut params: Vec<(String, String)> = vec![("selector".into(), args.selector.clone())];
    for name in &args.agent {
        params.push(("agent".into(), name.clone()));
    }
    if let Some(cwd) = &args.cwd {
        params.push(("cwd".into(), cwd.clone()));
    }
    if args.json {
        let mut with_format = params.clone();
        with_format.push(("format".into(), "json".into()));
        return match client.get_json("/transcript", &with_format).await {
            Ok(value) => {
                println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
                ExitCode::SUCCESS
            }
            Err(error) => report_client_error(error),
        };
    }
    let mut with_format = params;
    with_format.push(("format".into(), "text".into()));
    with_format.push(("no_thoughts".into(), if args.no_thoughts { "1" } else { "0" }.into()));
    with_format.push(("meta".into(), if args.meta { "1" } else { "0" }.into()));
    with_format.push(("truncate".into(), args.truncate.to_string()));
    with_format.push(("budget".into(), args.budget.to_string()));
    match client.get_text("/transcript", &with_format).await {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(error) => report_client_error(error),
    }
}

fn open_service(config_path: Option<PathBuf>, catalog: Option<PathBuf>) -> Result<Service, String> {
    let mut config = load_config(config_path)?;
    if let Some(catalog) = catalog {
        config.server.catalog = Some(catalog);
    }
    Service::new(config).map_err(|error| error.to_string())
}

async fn run_policy(args: PolicyArgs, config_path: Option<PathBuf>) -> ExitCode {
    let service = match open_service(config_path, None) {
        Ok(service) => service,
        Err(message) => {
            eprintln!("配置错误: {message}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    match args.command {
        PolicyCommand::Status { limit } => match service.policy_status(limit) {
            Ok((total, allowed, logged, recent)) => {
                println!("# 台账：{total} 条会话，其中放行 {allowed} 条；访问记录 {logged} 条");
                let mut rows = Vec::new();
                for (agent, session, decision, rule, reason) in recent {
                    rows.push(vec![
                        agent,
                        session,
                        decision,
                        rule.unwrap_or_default(),
                        ellipsize(&reason, 50),
                    ]);
                }
                if rows.is_empty() {
                    println!("（还没有判定记录）");
                } else {
                    println!(
                        "{}",
                        format_table(&["agent", "会话 ID", "判定", "规则", "原因"], &rows)
                    );
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("读取台账失败: {error}");
                ExitCode::from(EXIT_ERROR)
            }
        },
        PolicyCommand::Explain { agent, session } => {
            let principal = Principal::new(&Config::default_principal_name(), Vec::new());
            match service.explain(&agent, &session, &principal) {
                Ok(report) => {
                    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("解释失败: {error}");
                    ExitCode::from(EXIT_ERROR)
                }
            }
        }
        PolicyCommand::Forget { agent, session } => match service.forget(&agent, &session) {
            Ok(true) => {
                println!("已删除 {agent} / {session} 的台账记录");
                ExitCode::SUCCESS
            }
            Ok(false) => {
                eprintln!("台账里没有 {agent} / {session}");
                ExitCode::from(EXIT_NOT_FOUND)
            }
            Err(error) => {
                eprintln!("操作失败: {error}");
                ExitCode::from(EXIT_ERROR)
            }
        },
    }
}

fn report_client_error(error: ClientError) -> ExitCode {
    eprintln!("调用失败: {error}");
    if error.is_not_found() {
        ExitCode::from(EXIT_NOT_FOUND)
    } else {
        ExitCode::from(EXIT_ERROR)
    }
}

/// 入口。
pub async fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve(args) => run_serve(*args, cli.config).await,
        Command::Health(args) => run_health(args).await,
        Command::Agents(args) => run_agents(args).await,
        Command::List(args) => run_list(*args).await,
        Command::Show(args) => run_show(*args).await,
        Command::Policy(args) => run_policy(args, cli.config).await,
        Command::Acl(args) => run_acl(args, cli.config).await,
    }
}

async fn run_acl(args: AclArgs, config_path: Option<PathBuf>) -> ExitCode {
    let service = match open_service(config_path, None) {
        Ok(service) => service,
        Err(message) => {
            eprintln!("配置错误: {message}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    match args.command {
        AclCommand::Show { agent, session } => match service.acl_status(&agent, &session) {
            Ok((acl, source)) => {
                print_acl(&acl, &source);
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("读取失败: {error}");
                ExitCode::from(EXIT_ERROR)
            }
        },
        AclCommand::Chmod { mode, target } => {
            let (agent, session) = match split_target(&target) {
                Ok(value) => value,
                Err(message) => {
                    eprintln!("{message}");
                    return ExitCode::from(EXIT_ERROR);
                }
            };
            let parsed = match parse_mode(&mode) {
                Ok(value) => value,
                Err(error) => {
                    eprintln!("{error}");
                    return ExitCode::from(EXIT_ERROR);
                }
            };
            match service.chmod(&agent, &session, parsed) {
                Ok(()) => {
                    println!("已把 {agent}/{session} 的权限改为 {}", acl_mode_text(parsed));
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("修改失败: {error}");
                    ExitCode::from(EXIT_ERROR)
                }
            }
        }
        AclCommand::Chown { owner, target } => {
            let (agent, session) = match split_target(&target) {
                Ok(value) => value,
                Err(message) => {
                    eprintln!("{message}");
                    return ExitCode::from(EXIT_ERROR);
                }
            };
            let (owner_name, group_update) = match owner.split_once(':') {
                Some((name, "")) => (name.to_string(), GroupUpdate::Clear),
                Some((name, group)) => (name.to_string(), GroupUpdate::Set(group.to_string())),
                None => (owner.clone(), GroupUpdate::Keep),
            };
            if owner_name.is_empty() {
                eprintln!("属主不能为空");
                return ExitCode::from(EXIT_ERROR);
            }
            match service.chown(&agent, &session, Some(&owner_name), group_update) {
                Ok(()) => {
                    println!("已更新 {agent}/{session} 的属主/属组");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("修改失败: {error}");
                    ExitCode::from(EXIT_ERROR)
                }
            }
        }
    }
}

fn acl_mode_text(mode: u16) -> String {
    format!("{}（{:03o}）", crate::acl::render_mode(mode), mode)
}

fn print_acl(acl: &ObjectAcl, source: &str) {
    println!("属主: {}", acl.owner);
    println!("属组: {}", acl.group.as_deref().unwrap_or("（无）"));
    println!("权限: {}", acl_mode_text(acl.mode));
    println!("来源: {source}");
    println!("位含义: r=读取内容 w=修改权限 x=列表可见");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 表格按显示宽度对齐() {
        let table = format_table(&["a", "标题"], &[vec!["1".into(), "中文".into()]]);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with('a'));
        assert!(lines[1].contains("中文"));
    }

    #[test]
    fn 单元格压缩成单行并截断() {
        assert_eq!(ellipsize("多行\n内容", 40), "多行 内容");
        let long = "甲".repeat(50);
        let clipped = ellipsize(&long, 10);
        assert!(UnicodeWidthStr::width(clipped.as_str()) <= 10);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn 令牌生成有足够长度且不重复() {
        let first = generate_token();
        let second = generate_token();
        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
    }

    #[test]
    fn 服务端令牌会写入文件且权限为_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("acp-view-serve-token-{}", std::process::id()));
        let path = dir.join("token");
        // 该测试要验证「环境变量缺失时走文件」，因此先清掉可能存在的环境变量；
        // 进程级环境变量在多线程测试里是共享状态，这里只在单个测试里改动。
        unsafe { std::env::remove_var("ACP_VIEW_TOKEN") };
        let (token, source) = resolve_server_token(None, &path).expect("应当能生成令牌");
        assert!(source.contains("新生成"));
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), token);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let (again, source) = resolve_server_token(None, &path).unwrap();
        assert_eq!(again, token);
        assert!(source.contains("令牌文件"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
