use serde_json::json;

use super::card_shell;

/// The `/help` reference card: a pure command manual grouped by 会话 / 操作 /
/// 运维, one line per command with a short description. No buttons — reading is
/// its only job (previously the "试试"/"看卡" buttons made it an execution
/// launcher, which mixed concerns and was hard to keep consistent). Detail for a
/// single command stays text via `/help <command>`.
pub fn build_help_card() -> serde_json::Value {
    let groups: &[(&str, &[(&str, &str)])] = &[
        (
            "📂 会话",
            &[
                ("/new [名字]", "在当前项目新建会话"),
                (
                    "/dir [路径] [名字]",
                    "切换项目，在新目录开会话（无参弹最近目录卡片）",
                ),
                (
                    "/switch [关键字]",
                    "会话卡片，或按名称/目录/ID 切换（含 list / forget）",
                ),
                ("/topic [目录] [名字]", "新话题 + 新会话（无参用当前项目目录）"),
                ("/topic --adopt <关键字> [--force]", "围绕已有会话开话题"),
                ("/name <名字>", "重命名当前会话"),
            ],
        ),
        (
            "⚙️ 操作",
            &[
                ("/agent <名字>", "切换 agent（下条消息生效）"),
                ("/model <提供方/模型>", "切换模型（下条消息生效）"),
                ("/think [等级]", "设置/清除思考等级（下条消息生效）"),
                ("/autoaccept [on|off]", "查看或切换自动授权"),
                ("/stop", "中断当前执行"),
                ("/compact", "压缩上下文"),
            ],
        ),
        (
            "🛠 运维",
            &[
                ("/help [命令]", "全部命令，或单命令详情"),
                ("/restart", "重启 cola"),
                ("/restart-opencode", "重启 OpenCode 服务器（仅 cola 启动的）"),
                ("/update", "检查并应用自更新"),
            ],
        ),
    ];
    let mut elements: Vec<serde_json::Value> = Vec::new();
    for (title, rows) in groups {
        elements.push(json!({ "tag": "markdown", "content": format!("**{title}**") }));
        for (cmd, desc) in *rows {
            elements.push(json!({
                "tag": "markdown",
                "content": format!("`{cmd}` · {desc}"),
            }));
        }
    }
    elements.push(json!({
        "tag": "markdown",
        "content": "详细用法发 `/help <命令>`，如 `/help switch`。群话题规则见首次引导。",
    }));
    card_shell("📖 cola 命令", "blue", elements)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `/help` card is a pure command manual: every command appears with a
    /// description, and there are NO buttons (no "试试"/"看卡", no `button`
    /// elements) — reading is its only job.
    #[test]
    fn help_card_is_a_buttonless_command_manual() {
        let card = build_help_card();
        let text = card.to_string();
        // Every top-level command is listed once.
        for cmd in [
            "/new",
            "/dir",
            "/switch",
            "/topic",
            "/name",
            "/agent",
            "/model",
            "/autoaccept",
            "/stop",
            "/compact",
            "/help",
            "/restart",
            "/restart-opencode",
            "/update",
        ] {
            assert!(text.contains(cmd), "missing command {cmd} in help card: {text}");
        }
        // Pure reference: no execution/preview buttons anywhere.
        assert!(
            !text.contains("试试") && !text.contains("看卡"),
            "help card must be buttonless: {text}"
        );
        assert!(
            !text.contains("\"tag\":\"button\"") && !text.contains("\"tag\": \"button\""),
            "help card must not embed buttons: {text}"
        );
    }

    /// The `/help` card must stay schema-V2-compatible: the `note` element is
    /// no longer supported (Feishu rejects the card with ErrCode 200861), which
    /// silently killed `/help` (the 400 only landed in the log). The footer hint
    /// renders as `markdown` instead.
    #[test]
    fn help_card_has_no_schema_v2_unsupported_note() {
        let card = build_help_card();
        let text = card.to_string();
        assert!(
            !text.contains("\"tag\":\"note\"") && !text.contains("\"tag\": \"note\""),
            "help card must not use the schema-V2-unsupported note element: {text}"
        );
    }
}
