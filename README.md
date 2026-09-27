# acp-view

用 Agent Client Protocol（ACP）只读查看各家 coding agent 的会话。Rust 实现，由常驻服务端与轻量客户端组成，
带一套可配置的可见性策略。

## 为什么用 ACP

现成的会话浏览器大多在解析各家 agent 的原生存储格式——Claude Code 的 JSONL、Codex 的 rollout、OpenCode 的 SQLite。
这些格式由各家实现细节决定，一改就要跟着改。ACP 把会话做成了标准接口：

- `session/list`：枚举会话（会话 ID、工作目录、标题、更新时间）
- `session/load`：把整段历史通过 `session/update` 通知重放出来

只要 agent 支持 ACP，这个工具就能看它的会话，不需要为每家写解析器。omp 自己也提供 ACP 服务端（`omp acp`），
因此可以读自己的会话。

## 只读契约

只读性由代码结构保证：

- 客户端只发送三个方法：`initialize`、`session/list`、`session/load`；
- 初始化时不声明任何能力，agent 无法要求客户端读写文件或执行命令；
- agent 发来的权限申请一律回答 `cancelled`；
- 只有服务端会启动 agent 进程，命令行客户端只通过 HTTP 调用服务端；
- 不读写任何 agent 的存储文件。

## 快速开始

```bash
cargo build --release

# 启动服务端（默认 127.0.0.1:8787，令牌自动生成并写入 ~/.config/acp-session-viewer/token）
./target/release/acp-view serve

# 另开一个终端
./target/release/acp-view health
./target/release/acp-view agents
./target/release/acp-view list --limit 20
./target/release/acp-view show last --agent omp --no-thoughts
```

安装到 PATH：`cargo install --path .`，之后直接用 `acp-view`。

## 可见性策略

默认**不暴露任何会话**，由规则决定放行谁。判定顺序：

1. **历史拒绝**：`sticky_deny = true`（默认）时，被拒绝过的会话永久不再暴露；
2. 按顺序匹配 `deny` 规则，命中即拒绝；
3. 按顺序匹配 `allow` 规则，命中即放行；
4. 都没命中时采用 `default` 策略。

### 简单模式（可以关掉整套权限）

不需要规则与权限位时，可以把判定切成只看时间段：

```toml
[policy]
mode = "time-window"

[policy.window]
btime_after = "-30d"   # btime：台账里的首次见到时间
mtime_after = "-7d"    # mtime：agent 报告的更新时间（取不到时用最后见到时间）
```

- 给出的条件都要满足，缺省的条件表示不限制；时间写法与规则一致（RFC3339 或 `-7d`、`-24h` 这类相对时长）。
- 这个模式下不做规则判定、不做权限判定，也**不写判定记录**。因此临时切到简单模式再切回完整模式，
  不会因为这段时间的放行与隐藏产生「历史拒绝」。
- 命令行上可以临时覆盖：`acp-view serve --policy-mode time-window`。
- 启动信息、`policy status` 与 `/sessions` 的返回里都会标明当前模式；`acl show` 在简单模式下会提示权限位不参与判定。

规则的条件之间是「与」的关系，条件缺省表示不限制。可用条件：

| 类别 | 条件 |
| --- | --- |
| 身份 | `agents`、`session_ids`、`session_id_glob` |
| 内容 | `title_contains`、`title_glob`、`cwd_glob` |
| 时间 | `since`、`first_seen_after/before`、`last_seen_after/before`、`updated_after/before`、`btime_after/before`、`mtime_after/before` |
| 文件 | `file_glob`、`min_size`、`max_size`、`owner`、`mode_within`、`other_readable`、`other_writable`、`group_readable`、`group_writable` |

时间写法：RFC3339（`2026-09-27T00:00:00Z`）、相对时长（`-7d`、`-24h`）、或 `rule-created`。
文件大小写法：纯数字（字节）或 `8MiB`、`512KiB`、`1GB`。

**「以后新增的」怎么写**：把规则的 `since` 设为 `rule-created`，规则只放行它首次生效之后第一次见到的会话；
在此之前已经存在的会话不会被放行，而且如果它们在此之前被判为拒绝，之后也不会因为新规则而解禁。
需要解禁某条具体会话时用 `acp-view policy forget <agent> <会话 ID>` 显式操作（会在台账里删掉该记录）。

首次见到时间是本工具自己记的：ACP 只报告会话的更新时间，没有创建时间，因此台账在第一次枚举到某条会话时
记下时间，当作 btime 使用。

### 例子

```toml
[policy]
default = "deny"
sticky_deny = true

[[policy.deny]]
name = "涉密目录"
cwd_glob = "**/secrets/**"

[[policy.deny]]
name = "他人可读的会话文件"
other_readable = true

[[policy.allow]]
name = "点名放行"
session_ids = ["01a0e148-3751-719a-997f-b253d58b1eae"]

[[policy.allow]]
name = "本机 omp 新会话"
agents = ["omp"]
since = "rule-created"
max_size = "8MiB"
```

完整示例见 `config.example.toml`。

## 权限模型（Linux 风格）

两层叠加：**可见性策略**决定「这条会话能不能被暴露」，**权限位**决定「谁可以看」。两层都通过才会出现在结果里。

**主体**：用 `[[principal]]` 配置调用者，各自带令牌与所属组。

```toml
[[principal]]
name = "fun10165"
token_file = "~/.config/acp-session-viewer/token"
groups = ["dev"]

[[principal]]
name = "teammate"
token = "另外发放的令牌"
groups = ["dev"]
```

配了 `[[principal]]` 时服务端只认这些令牌；不配时沿用单令牌（参数 → 环境变量 → 令牌文件）加一个默认主体
（名称取服务端运行用户）。

**对象**：每条会话带属主、属组与模式位，新会话套用模板。

```toml
[acl.default]
owner = "fun10165"
group = "dev"
mode  = "750"
```

**属主的来源**（优先级从高到低）：

1. `[acl.sessions."agent/会话 ID"]` 里的显式覆盖；
2. `acl chown` 改过的值（台账里标成手工修改，之后不会被覆盖）；
3. **会话文件的属主**：按 `[server.locate]` 的规则找到会话文件后，取文件 uid 对应的用户名。
   omp 与 codex 这类一条会话一个文件的 agent 适用；opencode 的会话存在 SQLite 里取不到，回落到下一条；
4. 模板 `[acl.default].owner`。

`acl show` 会显示当前属主的来源（默认模板 / 会话文件属主 / 手工修改），以及是否被配置文件覆盖。

**主体的组**：配置里写了 `groups` 就用配置；没写就去系统账号里解析（`getgrouplist`），
成员变动不必改配置。解析不到时用空集合，此时组位不会匹配任何人。

**位语义**（本工具只读，没有「写内容」这回事，三个位按这里的语义定义）：

| 位 | 值 | 含义 |
| --- | --- | --- |
| `r` | 4 | 读取会话内容（重放事件） |
| `w` | 2 | 修改该会话的权限（chmod / chown） |
| `x` | 1 | 在会话列表里可见 |

`r` 与 `x` 互相独立：`chmod 400` 的会话不出现在列表里，但知道会话 ID 就能读取；`chmod 100` 的会话能在列表里看到，却读不了内容。

**判定**：请求者是属主就用属主位；否则看请求者的组与属组是否有交集，有就用属组位；都不是用其他人位。

**运维命令**：

```bash
acp-view acl show omp/01a0e148             # 查看属主、属组、模式位与来源
acp-view acl chmod 640 omp/01a0e148        # 也接受 rw-r----- 写法
acp-view acl chown alice:dev omp/01a0e148  # 省略 group 表示不变，写成 alice: 表示清空属组
```

`[acl.sessions."omp/01a0e148"]` 可以按会话覆盖权限，优先级高于台账；`acl show` 会标明来源是配置覆盖还是台账。

### 排查判定

```bash
acp-view policy status                 # 台账统计与最近判定
acp-view policy explain omp 01a0e148    # 逐条规则说明为什么放行或拒绝
acp-view policy forget omp 01a0e148     # 解除某条会话的历史拒绝
```

## HTTP 接口

全部接口都需要 `Authorization: Bearer <token>`。

| 方法 | 路径 | 参数 | 说明 |
| --- | --- | --- | --- |
| GET | `/health` | | 版本与连接池状态 |
| GET | `/agents` | | 各 agent 的实现与能力 |
| GET | `/sessions` | `agent`（可重复或逗号分隔）、`query`、`limit` | 会话列表，只含通过策略的会话 |
| GET | `/transcript` | `selector`（必填）、`agent`、`cwd`、`format=json\|text`、`no_thoughts`、`meta`、`truncate`、`budget` | 重放一条会话 |
| GET | `/policy/explain` | `agent`、`sessionId` | 解释一条会话的判定 |

状态码：`400` 参数错误、`401` 令牌缺失或错误、`404` 没有找到（被策略拒绝的会话也按此返回，不透露其存在）、
`502` agent 操作失败。

## 客户端约定（为 agent 调用设计）

- **字符上界**：`--budget`（默认 12000）限制单条命令的输出长度，被丢弃的字符数在 stderr 说明；`--json` 不受该上界约束。
- **退出码**：`0` 成功、`1` 失败、`2` 没有找到会话。
- **选择器**：`show` 接受会话 ID、ID 前缀、标题子串、工作目录子串，或 `last`；命中多条时取更新时间最新的，并在输出里说明命中数量。
- **显式错误**：连不上服务端、令牌缺失、策略拒绝都会直接报错，不静默返回空结果。

## 安全说明

- 令牌按 参数 → `ACP_VIEW_TOKEN` → 令牌文件 解析；服务端在都没有时生成 32 字节随机令牌并写入权限 `0600` 的文件。
- 令牌比较使用定长比较，避免时序侧信道。
- 默认只绑回环地址；绑到非回环地址会打印警告，此时令牌是唯一防线，建议在前面放反向代理并终止 TLS。
- 绑定地址只接受 IP 字面量与 `localhost`，不做域名解析（避免解析器返回通配地址这类意外）。
- 客户端只支持 `http://`；对外部署请在反向代理上终止 TLS。
- 每次判定与访问都写进台账（`access_log` 表），可以事后核对谁在什么时候读了哪条会话。

## 性能

连接复用是主要的优化点：服务端为每个 agent 保有一条长连接，操作串行执行，空闲超时后回收。
实测（本机，五个 agent，release 构建）：

| 命令 | 连接尚未建立 | 复用连接后 |
| --- | --- | --- |
| `agents` 探测五个 agent | 2.56 秒 | 0.017 秒 |
| `list`（1995 条会话，含策略与权限判定） | 0.24 秒 | 0.15 秒 |
| `show`（codex，311 条事件） | 0.25 秒 | 0.09 秒 |

命令行自身的启动与导入开销约 10 毫秒，冷启动时间几乎全部来自各家 agent 进程的启动与收尾：
codex 的 `npx` 解析约 2.9 秒，codex 与 cursor 在关闭标准输入后不会主动退出，收尾要坐满 SDK 的等待。

## 已知限制

- **gemini 不支持会话枚举**：只能按 ID 配合 `--cwd` 加载，列表里看不到它。
- **opencode 的覆盖范围与迁移进度有关**：它把会话从 v1 迁移到 v2，`session/list` 只返回已迁移的部分。
- **启动 agent 会触发 agent 自身的写操作**：例如 opencode 每次启动都会推进它的迁移记录；客户端没有发送任何写请求。
- **文件条件依赖定位规则**：只有 `[server.locate]` 里配置了模式的 agent 才有文件事实；定位不到时相关规则判为不匹配。
- **首次见到时间来自台账**：清空台账后，`since` 这类规则会以重建台账后的第一次枚举为准。

## 代码结构

- `src/config.rs`：配置模型（服务端参数、agent 注册表、策略）、绑定地址解析、令牌处理
- `src/policy.rs`：可见性规则与判定（纯逻辑）
- `src/catalog.rs`：SQLite 台账（首次见到时间、判定记录、规则生效时间、访问记录）
- `src/locate.rs`：会话文件定位与文件事实读取
- `src/acp.rs`：只读 ACP 客户端与长连接池
- `src/service.rs`：服务层（并行查询、策略过滤、报告结构）
- `src/server.rs`：HTTP 服务端（axum）、令牌鉴权、访问日志
- `src/client.rs`：HTTP 客户端
- `src/render.rs`：事件渲染与字符预算
- `src/cli.rs`：命令行入口

测试：`cargo test`。
