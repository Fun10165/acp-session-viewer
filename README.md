# acp-session-viewer

用 Agent Client Protocol（ACP）只读查看本机各家 coding agent 的会话。

## 为什么用 ACP

社区里现成的会话浏览器都在解析各家 agent 的原生存储格式——Claude Code 的 JSONL、Codex 的 rollout、OpenCode 的 SQLite。
这些格式由各家的实现细节决定，一改就要跟着改，维护成本高。

ACP 把会话做成了标准接口：

- `session/list`：枚举会话（会话 ID、工作目录、标题、更新时间）
- `session/load`：把整段历史通过 `session/update` 通知重放出来

只要 agent 支持 ACP，这个工具就能看它的会话，不需要为每家写一个解析器。
本项目自身的宿主 omp 也提供 ACP 服务端（`omp acp`），因此可以读自己的会话。

## 只读契约

只读性由代码结构保证，而不是靠约定：

- 客户端只发送三个方法：`initialize`、`session/list`、`session/load`。
- 初始化时声明空的能力集，agent 无法要求客户端读写文件或执行命令。
- 服务端发来的任何反向请求一律拒绝：权限申请返回 `cancelled`，其余方法报"方法不存在"。
- 本工具不读写任何 agent 的本地存储文件。

需要注意：启动 agent 进程本身可能触发 agent 自己的记账行为（见"已知限制"），这部分与客户端无关。

## 安装与使用

```bash
uv sync

# 探测每个 agent 的 ACP 能力
uv run acp-view agents

# 枚举会话（可加 --json）
uv run acp-view list --limit 20
uv run acp-view list --agent opencode --json

# 重放一条会话
uv run acp-view show ses_2cfe44e6dffemKoovezOHMntCP
uv run acp-view show <会话 ID> --agent codex --no-thoughts --truncate 200
```

`show` 会先在所选 agent 的会话列表里查找该会话，取出它记录的工作目录，再调用 `session/load`。
如果该 agent 不支持会话枚举（例如 gemini），用 `--cwd` 直接指定工作目录。

`--agents FILE` 可以换用自定义的 agent 配置：

```toml
[[agent]]
name = "opencode"
command = ["opencode", "acp"]
description = "OpenCode 自带的 ACP 服务端"
```

## 实测支持矩阵

2026-09-27 在本机实测（macOS，agent 版本见下）：

| agent | 启动命令 | 版本 | `session/list` | `session/load` | 备注 |
| --- | --- | --- | --- | --- | --- |
| omp | `omp acp` | 18.3.4 | 支持 | 支持 | 实测能读到自己正在进行的会话 |
| opencode | `opencode acp` | 2.0.18 | 支持，每页 100 条带游标 | 支持 | 只列出已迁移到 v2 的会话 |
| codex | `npx -y @agentclientprotocol/codex-acp` | 1.13.1 | 支持 | 支持 | 需要 npx，首次运行要联网 |
| cursor | `cursor-agent acp` | 2026.09.23 | 支持 | 支持 | 需要先 `cursor-agent login` |
| gemini | `gemini --acp` | 0.41.2 | 不支持 | 支持 | 无法枚举，只能按 ID 加载 |

## 已知限制

- **gemini 不支持会话枚举**：ACP 规范里 `session/list` 是可选能力，gemini 只实现了 `session/load`。
- **opencode 的覆盖范围与迁移进度有关**：它正在把 v1 数据迁移到 v2，`session/list` 返回的是已迁移的部分，迁移游标会随着 agent 启动而推进。
- **启动 agent 会触发 agent 自身的写操作**：实测 opencode 每次启动都会推进它自己的迁移记录（数据库内容变化，大小不变）。客户端没有发送任何写请求。
- **`--json` 输出经过 SDK 规范化**：协议模型之外的字段会被丢弃，输出不保证与线路上传输的字节完全一致。
- **会话重放的完整度取决于 agent**：`session/load` 重放哪些内容由各 agent 自己决定（例如 opencode 会重放思考片段与工具调用，gemini 的重放内容尚未实测）。

## 开发

```bash
uv run pytest
```

代码结构：

- `src/acp_session_viewer/config.py`：agent 注册表与配置加载
- `src/acp_session_viewer/viewer.py`：只读 ACP 客户端（探测、枚举、重放）
- `src/acp_session_viewer/render.py`：事件到文本的纯函数渲染
- `src/acp_session_viewer/cli.py`：命令行入口

`scratch/` 存放中间产物，已被 git 忽略。
