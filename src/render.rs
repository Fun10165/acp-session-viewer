//! 把 `session/update` 事件渲染成人类可读文本。
//!
//! 全部为纯函数：输入事件（ACP 线格式的 JSON）、输出文本，不产生副作用。

use serde_json::Value;

const LABEL_USER: &str = "用户";
const LABEL_AGENT: &str = "助手";
const LABEL_THOUGHT: &str = "思考";

/// 渲染选项。
#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub show_thoughts: bool,
    pub show_meta: bool,
    pub truncate: usize,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            show_thoughts: true,
            show_meta: false,
            truncate: 400,
        }
    }
}

/// 按字符截断，并标注原始长度。
#[must_use]
pub fn shorten(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if limit == 0 || count <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}…（共 {count} 字符）")
}

/// 把文本压到 budget 个字符以内，返回（结果, 丢弃字符数）。budget 为 0 表示不限制。
///
/// 截断提示行本身也计入上界，因此结果长度按构造不超过 budget。
#[must_use]
pub fn apply_budget(text: &str, budget: usize) -> (String, usize) {
    let total = text.chars().count();
    if budget == 0 || total <= budget {
        return (text.to_string(), 0);
    }
    let marker = |dropped: usize| format!("\n…（已截断，丢弃 {dropped} 字符；调大 --budget 或改用 --json 查看完整内容）");
    let reserved = marker(total).chars().count();
    if budget <= reserved {
        let fallback = "…（预算过小，无法显示内容）";
        return (fallback.chars().take(budget).collect(), total);
    }
    let keep = budget - reserved;
    let dropped = total - keep;
    let kept: String = text.chars().take(keep).collect();
    (format!("{kept}{}", marker(dropped)), dropped)
}

fn compact_json(value: &Value, limit: usize) -> String {
    shorten(&value.to_string(), limit)
}

/// 把单个 ContentBlock 转成一行文本。
#[must_use]
pub fn content_block_text(block: &Value, limit: usize) -> String {
    match block {
        Value::String(text) => text.clone(),
        Value::Object(map) => match map.get("type").and_then(Value::as_str) {
            Some("text") => map
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            Some("image") => format!(
                "[图片 {}]",
                map.get("mimeType").and_then(Value::as_str).unwrap_or("未知类型")
            ),
            Some("audio") => format!(
                "[音频 {}]",
                map.get("mimeType").and_then(Value::as_str).unwrap_or("未知类型")
            ),
            Some("resource_link") => format!(
                "[资源链接] {}",
                map.get("name")
                    .and_then(Value::as_str)
                    .or_else(|| map.get("uri").and_then(Value::as_str))
                    .unwrap_or("未知")
            ),
            Some("resource") => format!(
                "[内嵌资源] {}",
                map.get("resource")
                    .and_then(|inner| inner.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("未知位置")
            ),
            Some(other) => format!("[{other} 内容]"),
            None => format!("[缺少 type 的内容块] {}", compact_json(block, limit)),
        },
        other => format!("[无法识别的内容块] {other}"),
    }
}

fn block_line(label: &str, body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut lines = trimmed.lines();
    let first = lines.next().unwrap_or_default();
    let mut rendered = format!("[{label}] {first}");
    for line in lines {
        rendered.push_str("\n    ");
        rendered.push_str(line);
    }
    Some(rendered)
}

fn tool_lines(titles: &mut std::collections::BTreeMap<String, String>, update: &Value, limit: usize) -> Vec<String> {
    let kind = update.get("sessionUpdate").and_then(Value::as_str).unwrap_or_default();
    let id = update
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or("未知")
        .to_string();
    let title = update
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| titles.get(&id).cloned())
        .unwrap_or_else(|| id.clone());
    titles.insert(id, title.clone());
    let status = update.get("status").and_then(Value::as_str);

    let mut lines = Vec::new();
    if kind == "tool_call" {
        let suffix = status.map(|value| format!("  ({value})")).unwrap_or_default();
        lines.push(format!("[工具] {title}{suffix}"));
    } else {
        let suffix = status.map(|value| format!(" → {value}")).unwrap_or_default();
        lines.push(format!("[工具] {title}{suffix}"));
    }
    if let Some(input) = update.get("rawInput") {
        if !input.is_null() {
            lines.push(format!("    输入: {}", compact_json(input, limit)));
        }
    }
    if let Some(output) = update.get("rawOutput") {
        if !output.is_null() {
            lines.push(format!("    输出: {}", compact_json(output, limit)));
        }
    }
    if let Some(content) = update.get("content").and_then(Value::as_array) {
        for block in content {
            if let Some(inner) = block.get("content") {
                lines.push(format!(
                    "    内容: {}",
                    shorten(&content_block_text(inner, limit), limit)
                ));
            } else {
                lines.push(format!("    内容: {}", compact_json(block, limit)));
            }
        }
    }
    lines
}

/// 渲染整段会话。
#[must_use]
pub fn render_transcript(updates: &[Value], options: &RenderOptions) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut titles: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut pending_label: Option<&'static str> = None;
    let mut pending_message: Option<String> = None;
    let mut pending_parts: Vec<String> = Vec::new();

    fn flush(
        lines: &mut Vec<String>,
        label: &mut Option<&'static str>,
        message: &mut Option<String>,
        parts: &mut Vec<String>,
        truncate: usize,
    ) {
        if let Some(current) = *label {
            let body = parts.join("");
            if let Some(line) = block_line(current, &shorten(&body, truncate)) {
                lines.push(line);
            }
        }
        *label = None;
        *message = None;
        parts.clear();
    }

    for update in updates {
        let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
            flush(
                &mut lines,
                &mut pending_label,
                &mut pending_message,
                &mut pending_parts,
                options.truncate,
            );
            lines.push(format!(
                "[缺少 sessionUpdate 字段的事件] {}",
                compact_json(update, options.truncate)
            ));
            continue;
        };

        let label = match kind {
            "user_message_chunk" => Some(LABEL_USER),
            "agent_message_chunk" => Some(LABEL_AGENT),
            "agent_thought_chunk" => Some(LABEL_THOUGHT),
            _ => None,
        };
        if let Some(label) = label {
            if kind == "agent_thought_chunk" && !options.show_thoughts {
                continue;
            }
            let message_id = update
                .get("messageId")
                .and_then(Value::as_str)
                .map(str::to_string);
            if pending_label != Some(label) || pending_message != message_id {
                flush(
                    &mut lines,
                    &mut pending_label,
                    &mut pending_message,
                    &mut pending_parts,
                    options.truncate,
                );
                pending_label = Some(label);
                pending_message = message_id;
            }
            let content = update.get("content").cloned().unwrap_or(Value::Null);
            pending_parts.push(content_block_text(&content, options.truncate));
            continue;
        }

        flush(
            &mut lines,
            &mut pending_label,
            &mut pending_message,
            &mut pending_parts,
            options.truncate,
        );

        match kind {
            "tool_call" | "tool_call_update" => {
                lines.extend(tool_lines(&mut titles, update, options.truncate));
            }
            "plan" => {
                lines.push("[计划]".to_string());
                if let Some(entries) = update.get("entries").and_then(Value::as_array) {
                    for (index, entry) in entries.iter().enumerate() {
                        let status = entry.get("status").and_then(Value::as_str).unwrap_or("未知状态");
                        let content = entry.get("content").and_then(Value::as_str).unwrap_or_default();
                        lines.push(format!("    {}. ({status}) {content}", index + 1));
                    }
                }
            }
            "available_commands_update" if options.show_meta => {
                let names: Vec<&str> = update
                    .get("availableCommands")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.get("name").and_then(Value::as_str))
                            .collect()
                    })
                    .unwrap_or_default();
                lines.push(format!("[元信息] 可用命令: {}", names.join(", ")));
            }
            "current_mode_update" if options.show_meta => {
                lines.push(format!(
                    "[元信息] 当前模式: {}",
                    update.get("currentModeId").and_then(Value::as_str).unwrap_or("未知")
                ));
            }
            "config_option_update" if options.show_meta => {
                lines.push(format!(
                    "[元信息] 配置项: {}",
                    compact_json(update.get("configOptions").unwrap_or(&Value::Null), options.truncate)
                ));
            }
            "session_info_update" if options.show_meta => {
                lines.push(format!("[元信息] 会话信息: {}", compact_json(update, options.truncate)));
            }
            "usage_update" if options.show_meta => {
                lines.push(format!("[元信息] 用量: {}", compact_json(update, options.truncate)));
            }
            "available_commands_update" | "current_mode_update" | "config_option_update"
            | "session_info_update" | "usage_update" => {}
            other => {
                lines.push(format!("[未知事件 {other}] {}", compact_json(update, options.truncate)));
            }
        }
    }

    flush(
        &mut lines,
        &mut pending_label,
        &mut pending_message,
        &mut pending_parts,
        options.truncate,
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent(text: &str, id: &str) -> Value {
        json!({"sessionUpdate": "agent_message_chunk", "messageId": id, "content": {"type": "text", "text": text}})
    }

    fn thought(text: &str) -> Value {
        json!({"sessionUpdate": "agent_thought_chunk", "messageId": "t", "content": {"type": "text", "text": text}})
    }

    #[test]
    fn 同一消息的片段先合并再渲染() {
        let updates = vec![agent("你", "m1"), agent("好", "m1"), agent("呀", "m1")];
        assert_eq!(render_transcript(&updates, &RenderOptions::default()), "[助手] 你好呀");
    }

    #[test]
    fn 不同消息之间断开() {
        let updates = vec![agent("第一句", "a"), agent("第二句", "b")];
        assert_eq!(
            render_transcript(&updates, &RenderOptions::default()),
            "[助手] 第一句\n[助手] 第二句"
        );
    }

    #[test]
    fn 思考片段可以隐藏() {
        let updates = vec![thought("内部推理"), agent("最终回答", "m")];
        let shown = render_transcript(&updates, &RenderOptions::default());
        assert!(shown.starts_with("[思考] 内部推理"));
        let hidden = render_transcript(
            &updates,
            &RenderOptions {
                show_thoughts: false,
                ..RenderOptions::default()
            },
        );
        assert_eq!(hidden, "[助手] 最终回答");
    }

    #[test]
    fn 多行内容缩进对齐() {
        let updates = vec![agent("第一行\n第二行", "m")];
        assert_eq!(
            render_transcript(&updates, &RenderOptions::default()),
            "[助手] 第一行\n    第二行"
        );
    }

    #[test]
    fn 工具调用与更新都渲染状态() {
        let updates = vec![
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "读取文件", "status": "in_progress", "rawInput": {"path": "a.py"}}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "rawOutput": "ok"}),
        ];
        let text = render_transcript(&updates, &RenderOptions::default());
        assert!(text.contains("[工具] 读取文件  (in_progress)"));
        assert!(text.contains(r#"输入: {"path":"a.py"}"#));
        assert!(text.contains("[工具] 读取文件 → completed"));
        assert!(text.contains(r#"输出: "ok""#));
    }

    #[test]
    fn 工具更新沿用首次标题() {
        let updates = vec![
            json!({"sessionUpdate": "tool_call", "toolCallId": "t9", "title": "编译", "status": "pending"}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t9", "status": "failed"}),
        ];
        let text = render_transcript(&updates, &RenderOptions::default());
        assert!(text.contains("[工具] 编译 → failed"));
    }

    #[test]
    fn 计划逐条编号() {
        let updates = vec![json!({
            "sessionUpdate": "plan",
            "entries": [
                {"content": "读取配置", "status": "completed"},
                {"content": "修改代码", "status": "pending"}
            ]
        })];
        assert_eq!(
            render_transcript(&updates, &RenderOptions::default()),
            "[计划]\n    1. (completed) 读取配置\n    2. (pending) 修改代码"
        );
    }

    #[test]
    fn 元信息默认隐藏() {
        let updates = vec![
            json!({"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "init"}, {"name": "review"}]}),
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}),
        ];
        assert_eq!(render_transcript(&updates, &RenderOptions::default()), "");
        let shown = render_transcript(
            &updates,
            &RenderOptions {
                show_meta: true,
                ..RenderOptions::default()
            },
        );
        assert!(shown.contains("[元信息] 可用命令: init, review"));
        assert!(shown.contains("[元信息] 当前模式: plan"));
    }

    #[test]
    fn 未知事件显式标出() {
        let updates = vec![json!({"sessionUpdate": "品牌新事件", "value": 1})];
        assert!(render_transcript(&updates, &RenderOptions::default()).starts_with("[未知事件 品牌新事件]"));
    }

    #[test]
    fn 缺少字段的事件被指出() {
        let updates = vec![json!({"value": 1})];
        assert!(render_transcript(&updates, &RenderOptions::default()).contains("缺少 sessionUpdate 字段"));
    }

    #[test]
    fn 截断标注原始长度() {
        let long = "甲".repeat(50);
        let updates = vec![agent(&long, "m")];
        let text = render_transcript(
            &updates,
            &RenderOptions {
                truncate: 10,
                ..RenderOptions::default()
            },
        );
        assert_eq!(text, format!("[助手] {}…（共 50 字符）", "甲".repeat(10)));
    }

    #[test]
    fn 内容块的文字表示() {
        assert_eq!(content_block_text(&json!({"type": "text", "text": "hi"}), 100), "hi");
        assert_eq!(
            content_block_text(&json!({"type": "image", "mimeType": "image/png"}), 100),
            "[图片 image/png]"
        );
        assert_eq!(
            content_block_text(&json!({"type": "resource_link", "uri": "file:///a", "name": "a"}), 100),
            "[资源链接] a"
        );
    }

    #[test]
    fn 预算内与不限预算时原样返回() {
        assert_eq!(apply_budget("abc", 10), ("abc".to_string(), 0));
        assert_eq!(apply_budget("abc", 0), ("abc".to_string(), 0));
    }

    #[test]
    fn 恰好等于上界时不截断() {
        let text = "甲".repeat(100);
        assert_eq!(apply_budget(&text, 100), (text, 0));
    }

    #[test]
    fn 截断后不超过上界且说明丢弃量() {
        let text = "甲".repeat(5000);
        for budget in [1usize, 10, 45, 60, 100, 1000, 4999] {
            let (bounded, dropped) = apply_budget(&text, budget);
            assert!(bounded.chars().count() <= budget, "budget={budget}");
            assert!(dropped > 0 && dropped <= 5000);
            if budget > 60 {
                assert!(bounded.contains(&format!("丢弃 {dropped} 字符")));
            }
        }
    }
}
