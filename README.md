# acp-session-viewer

用 Agent Client Protocol（ACP）只读查看各家 coding agent 的会话。由常驻服务端与轻量客户端组成。

## 为什么用 ACP

社区里现成的会话浏览器都在解析各家 agent 的原生存储格式——Claude Code 的 JSONL、Codex 的 rollout、OpenCode 的 SQLite。
这些格式由各家实现细节决定，一改就要跟着改，维护成本高。

ACP 把会话做成了标准接口：

- `session/list`：枚举会话（会话 ID、工作目录、标题、更新时间）
- `session/load`：把整段历史通过 `session/update` 通知重放出来

只要 agent 支持 ACP，这个工具就能看它的会话，不需要为每家写一个解析器。
本项目的宿主 omp 也提供 ACP 服务端（`omp acp`），因此可以读自己的会话。

## 只读契约

只读性由代码结构保证，不依赖使用者的自觉：

- 客户端只发送三个方法：`initialize`、`session/list`、`session/load`。
- 初始化时声明空的能力集，agent 无法要求客户端读写文件或执行命令。
- agent 发来的任何反向请求一律拒绝：权限申请返回 `cancelled`，其余方法报"方法不存在"。
- 只有服务端会启动 agent 进程；客户端通过 HTTP 调用服务端，不直接接触 agent。
- 本工具不读写任何 agent 的本地存储文件。

需要注意：启动 agent 进程本身可能触发 agent 自己的记账行为（见"已知限制"），这部分与客户端无关。

## 快速开始

### 启动服务端

```bash
uv run acp-view serve                             # 默认绑定 127.0.0.1:8787
uv run acp-view serve --bind 0.0.0.0:8787         # 对外提供服务
uv run acp-view serve --bind 127.0.0.1 --port 9000
uv run acp-view serve --idle-timeout 60 --max-uses 16   # 调整连接回收与轮换策略
```

令牌按以下顺序确定：`--token` 参数 → `ACP_VIEW_TOKEN` 环境变量 → 令牌文件（默认 `~/.config/acp-session-viewer/token`）。
三者都没有时，服务端会生成一个随机令牌写入该文件（权限 0600）并在启动信息里打印来源。
绑定到非回环地址时会额外打印一条警告。

启动后会为每个请求打印一行访问记录，格式为 `时间 来源 IP 方法 路径 → 状态码 耗时`。

### 客户端

```bash
uv run acp-view health                     # 检查服务端是否可用
uv run acp-view agents                     # 各 agent 的 ACP 能力
uv run acp-view list --limit 20            # 列出会话
uv run acp-view list --query acp --agent omp
uv run acp-view show last --agent omp --no-thoughts
uv run acp-view show "标题子串"             # 选择器也接受标题片段
uv run acp-view show 01a0e148 --json
```

服务端地址默认 `http://127.0.0.1:8787`，可用 `--server` 或 `ACP_VIEW_SERVER` 覆盖；令牌解析顺序与服务端一致。

## HTTP 接口

全部接口都需要 `Authorization: Bearer <token>`。

| 方法 | 路径 | 参数 | 说明 |
| --- | --- | --- | --- |
| GET | `/health` | | 返回版本与连接池状态（每个 agent 的连接、进程号、空闲秒数、使用次数）|
| GET | `/agents` | | 各 agent 的实现名、能力与错误 |
| GET | `/sessions` | `agent`（可重复）、`query`、`limit` | 会话列表，按更新时间倒序；`errors` 里列出被跳过的 agent |
| GET | `/transcript` | `selector`（必填）、`agent`（可重复）、`cwd`、`format=json\|text`、`no_thoughts`、`meta`、`truncate`、`budget` | 重放一条会话；`text` 返回纯文本，`budget` 只对 `text` 生效 |

状态码：`400` 参数错误、`401` 令牌缺失或错误、`404` 没有匹配的会话、`502` agent 操作失败。

## 客户端约定（为 agent 调用设计）

- **字符上界**：`--budget`（默认 12000）限制单条命令的输出长度，被丢弃的字符数在 stderr 说明；`--json` 不受该上界约束。
- **退出码**：`0` 成功、`1` 失败、`2` 没有找到会话。
- **选择器**：`show` 接受会话 ID、ID 前缀、标题子串、工作目录子串，或 `last`；命中多条时取更新时间最新的，并在输出里说明命中数量。
- **显式错误**：连不上服务端、令牌缺失、agent 不支持所需能力都会直接报错，不静默返回空结果。

## 连接复用

服务端按 agent 维护长连接，跨请求复用，避免每次查询都付出冷启动开销：

- 同一 agent 的操作串行执行，不同 agent 互不影响；
- 连接空闲超过 `--idle-timeout`（默认 300 秒）后被回收任务关闭，用完即走的场景不会留下常驻进程；
- 单条连接服务 `--max-uses`（默认 64）次操作后轮换，避免进程长期累积状态；
- agent 进程中途退出时，丢弃旧连接并用新连接重试一次；操作层面的失败（例如 agent 不支持某能力）保留连接、直接上报。

实测（本机，五个 agent）：

| 请求 | 连接尚未建立 | 复用连接后 |
| --- | --- | --- |
| `GET /agents` 探测五个 agent | 1.50–1.84s | 0.32–0.35s |
| `GET /sessions` 全量（1651 条会话，含分页）| 5.02s | 0.49s |
| 重放同一条会话（omp，600 条事件）| 0.12s | 0.02s |

表里的 `GET` 耗时包含命令行进程自身的启动与导入（约 0.3 秒），服务端部分在复用后接近零成本。
重放时间是协议层测量值，不含命令行开销。

冷启动的构成：omp 与 opencode 各约 0.15 秒，cursor 约 2.3 秒，codex 的 `npx` 解析约 2.9 秒；
codex 与 cursor 在关闭标准输入后不会主动退出，收尾还会坐满 SDK 的 2 秒等待。复用连接后这些成本只在第一次出现。

## 实测支持矩阵

2026-09-27 在本机实测（macOS）：

| agent | 启动命令 | 版本 | `session/list` | `session/load` |
| --- | --- | --- | --- | --- |
| omp | `omp acp` | 18.3.4 | 支持 | 支持 |
| opencode | `opencode acp` | 2.0.18 | 支持，每页 100 条带游标 | 支持 |
| codex | `npx -y @agentclientprotocol/codex-acp` | 1.13.1 | 支持 | 支持 |
| cursor | `cursor-agent acp` | 2026.09.23 | 支持 | 支持 |
| gemini | `gemini --acp` | 0.41.2 | 不支持 | 支持（只能按 ID 或 `--cwd` 加载）|

## 性能

单个 agent 的实测耗时（第二次运行，缓存已热）：

| agent | 启动加初始化 | 枚举会话 | 会话条数 | 重放会话 | 事件条数 | 进程收尾 |
| --- | --- | --- | --- | --- | --- | --- |
| omp | 0.13s | 0.32s | 853 | 0.89s | 413 | 0.14s |
| opencode | 0.15s | 0.20s | 468–607 | 0.95s | 136 | 0.18s |
| codex | 2.8–3.1s | 2.9s | 3 | 3.3s | 311 | 2.0s |
| cursor | 2.3s | 2.3s | 0 | — | — | 2.0s |
| gemini | 4.6s 冷 / 0.8s 热 | 不支持 | — | — | — | 0.9s |

时间主要花在各家 agent 的进程启动与收尾上：codex 的 `npx` 解析要 2.9 秒，codex 与 cursor 在关闭标准输入后不会主动退出，收尾会坐满 SDK 的 2 秒等待。
服务端对这些操作按 agent 并行执行。
另有一次 codex 初始化耗时 83 秒的离群值，重测 4 轮都在 2.8–3.1 秒，未能复现。

## 已知限制

- **gemini 不支持会话枚举**：ACP 规范里 `session/list` 是可选能力，gemini 只实现了 `session/load`，因此只能用 `--cwd` 配 `--agent gemini` 直接加载。
- **opencode 的覆盖范围与迁移进度有关**：它正在把 v1 数据迁移到 v2，`session/list` 返回已迁移的部分，迁移游标会随 agent 启动而推进。
- **启动 agent 会触发 agent 自身的写操作**：实测 opencode 每次启动都会推进它自己的迁移记录（数据库内容变化、大小不变）。客户端没有发送任何写请求。
- **`--json` 输出经过 SDK 规范化**：协议模型之外的字段会被丢弃，输出不保证与线路上传输的字节完全一致。
- **长时间空闲后仍需冷启动**：连接超过 `--idle-timeout` 会被回收，此后第一个请求要重新付出 agent 的启动成本。
- **gemini 的连接只能用于按 ID 加载**：它不支持会话枚举，因此枚举类请求不会用到它的连接。

## 开发

```bash
uv run pytest
```

代码结构：

- `src/acp_session_viewer/config.py`：agent 注册表、绑定地址解析、令牌处理
- `src/acp_session_viewer/viewer.py`：只读 ACP 客户端与可复用的长连接
- `src/acp_session_viewer/agent_pool.py`：按 agent 维护长连接（复用、空闲回收、轮换、失效重试）
- `src/acp_session_viewer/render.py`：事件到文本的纯函数渲染，以及字符上界处理
- `src/acp_session_viewer/selection.py`：会话选择器匹配规则（纯函数）
- `src/acp_session_viewer/api.py`：服务层，把只读能力包装成报告结构
- `src/acp_session_viewer/server.py`：HTTP 服务端（aiohttp）、令牌鉴权与连接池装配
- `src/acp_session_viewer/client.py`：HTTP 客户端
- `src/acp_session_viewer/cli.py`：命令行入口

测试：`tests/test_pool.py` 用 `tests/fixtures/stub_agent.py`（一个最小 ACP 服务端）核对连接的复用、
轮换、空闲回收与进程退出后的恢复；`tests/test_server.py` 覆盖鉴权与错误映射。

`scratch/` 存放中间产物，已被 git 忽略。
