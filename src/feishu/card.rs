use serde_json::json;

/// Card state for Feishu interactive message cards.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum CardState {
    #[default]
    Loading,
    Reasoning,
    Streaming,
    Done,
    Error,
}

    /// Builds Feishu interactive card JSON (v2 schema with collapsible panels).
    /// The Feishu API stores a generic fallback copy; clients that support v2
    /// render the real card with foldable panels.
    pub struct CardBuilder {
    title: String,
    state: CardState,
    text: String,
    reasoning: Option<String>,
    tools: Vec<ToolPanel>,
    footer: Option<String>,
    question: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolPanel {
    pub name: String,
    pub status: String,
    pub input: Option<String>,
    pub output: Option<String>,
}

impl ToolPanel {
    fn status_icon(&self) -> &'static str {
        match self.status.as_str() {
            "running" | "pending" => "⏳",
            "completed" => "✅",
            "failed" => "❌",
            _ => "🔧",
        }
    }
}

impl CardBuilder {
    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_string(),
            state: CardState::Loading,
            text: String::new(),
            reasoning: None,
            tools: Vec::new(),
            footer: None,
            question: None,
        }
    }

    pub fn with_question(mut self, question: &str) -> Self {
        self.question = Some(question.to_string());
        self
    }

    pub fn with_state(mut self, state: CardState) -> Self {
        self.state = state;
        self
    }

    pub fn with_text(mut self, text: &str) -> Self {
        self.text = text.to_string();
        self
    }

    pub fn with_reasoning(mut self, reasoning: &str) -> Self {
        self.reasoning = Some(reasoning.to_string());
        self
    }

    pub fn with_tool(mut self, tool: ToolPanel) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn with_footer(mut self, footer: &str) -> Self {
        self.footer = Some(footer.to_string());
        self
    }

    /// Build the Feishu card JSON payload (v2 schema).
    pub fn build(&self) -> serde_json::Value {
        let mut elements: Vec<serde_json::Value> = Vec::new();

        // Question being answered — visible so the user knows which prompt
        // this reply corresponds to (queued prompts answer in order)
        if let Some(ref q) = self.question {
            elements.push(json!({
                "tag": "markdown",
                "content": format!("**问**: {}", truncate_md(q, 200))
            }));
            elements.push(json!({"tag": "hr"}));
        }

        // Reasoning — folded collapsible panel
        if let Some(ref r) = self.reasoning
            && !r.is_empty()
        {
            elements.push(collapsible_panel("💭 推理过程", &truncate_md(r, 800)));
        }

        // Tools — one folded panel each, header shows icon + name
        for tool in &self.tools {
            let mut content = String::new();
            if let Some(ref i) = tool.input {
                content.push_str(&format!("**Input**\n{}\n", truncate_md(i, 400)));
            }
            if let Some(ref o) = tool.output {
                content.push_str(&format!("**Output**\n{}", truncate_md(o, 800)));
            }
            if content.is_empty() {
                content = "_(no details)_".to_string();
            }
            elements.push(collapsible_panel(
                &format!("{} {}", tool.status_icon(), tool.name),
                &content,
            ));
        }

        // Result text — always visible
        if !self.text.is_empty() {
            elements.push(json!({ "tag": "markdown", "content": self.text }));
        } else if self.state == CardState::Streaming || self.state == CardState::Loading {
            elements.push(json!({ "tag": "markdown", "content": "⏳ ..." }));
        }

        if let Some(ref footer) = self.footer {
            elements.push(json!({"tag": "hr"}));
            elements.push(json!({ "tag": "markdown", "content": footer }));
        }

        let (header_title, template) = self.header_info();
        json!({
            "schema": "2.0",
            "config": { "wide_screen_mode": true },
            "header": {
                "title": { "tag": "plain_text", "content": header_title },
                "template": template
            },
            "body": { "elements": elements }
        })
    }

    /// Dynamic header: title + color template based on state.
    fn header_info(&self) -> (String, &'static str) {
        match self.state {
            CardState::Loading => (format!("⏳ {} 思考中...", self.title), "blue"),
            CardState::Reasoning => (format!("💭 {} 推理中...", self.title), "blue"),
            CardState::Streaming => {
                if let Some(tool) = self.tools.iter().find(|t| t.status == "running") {
                    (format!("🔧 {} {}", tool.status_icon(), tool.name), "orange")
                } else {
                    (format!("✍️ {} 回复中...", self.title), "blue")
                }
            }
            CardState::Done => {
                if let Some(tool) = self.tools.iter().find(|t| t.status == "failed") {
                    (format!("❌ {} 失败", self.title), "red")
                } else {
                    (format!("✅ {} 完成", self.title), "green")
                }
            }
            CardState::Error => (format!("❌ {} 出错", self.title), "red"),
        }
    }
}

/// Build a collapsible panel (v2), folded by default.
fn collapsible_panel(title: &str, content: &str) -> serde_json::Value {
    json!({
        "tag": "collapsible_panel",
        "expanded": false,
        "header": {
            "title": { "tag": "plain_text", "content": title },
            "icon": { "tag": "standard_icon", "token": "down-small-ccm_outlined" },
            "icon_position": "right"
        },
        "elements": [
            { "tag": "markdown", "content": content }
        ]
    })
}

fn truncate_md(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(max_len).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_card_header() {
        let card = CardBuilder::new("cola").with_state(CardState::Loading).build();
        let header = &card["header"]["title"]["content"];
        assert!(header.as_str().unwrap().contains("思考中"));
        assert_eq!(card["schema"].as_str().unwrap(), "2.0");
    }

    #[test]
    fn reasoning_is_collapsible_panel() {
        let card = CardBuilder::new("cola")
            .with_state(CardState::Reasoning)
            .with_reasoning("Let me analyze this code...")
            .build();
        let header = &card["header"]["title"]["content"];
        assert!(header.as_str().unwrap().contains("推理中"));
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements[0]["tag"].as_str().unwrap(), "collapsible_panel");
        assert_eq!(elements[0]["expanded"].as_bool().unwrap(), false);
        assert!(elements[0].to_string().contains("analyze"));
    }

    #[test]
    fn streaming_card_shows_text() {
        let card = CardBuilder::new("cola")
            .with_state(CardState::Streaming)
            .with_text("pub fn main() {")
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements.last().unwrap()["content"].as_str().unwrap(), "pub fn main() {");
    }

    #[test]
    fn tool_panel_completed_is_collapsible() {
        let tool = ToolPanel {
            name: "read".into(),
            status: "completed".into(),
            input: Some("src/main.rs".into()),
            output: Some("fn main() {}".into()),
        };
        let card = CardBuilder::new("cola")
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        let panel = &elements[0];
        assert_eq!(panel["tag"].as_str().unwrap(), "collapsible_panel");
        assert!(panel["header"]["title"]["content"].as_str().unwrap().contains("✅ read"));
        assert!(panel.to_string().contains("fn main() {}"));
    }

    #[test]
    fn running_tool_shows_in_header() {
        let tool = ToolPanel {
            name: "bash".into(),
            status: "running".into(),
            input: Some("cargo test".into()),
            output: None,
        };
        let card = CardBuilder::new("cola")
            .with_state(CardState::Streaming)
            .with_tool(tool)
            .build();
        let header = card["header"]["title"]["content"].as_str().unwrap();
        assert!(header.contains("bash"));
        assert_eq!(card["header"]["template"].as_str().unwrap(), "orange");
    }

    #[test]
    fn done_card_green() {
        let card = CardBuilder::new("cola").with_state(CardState::Done).with_text("Done!").build();
        assert_eq!(card["header"]["template"].as_str().unwrap(), "green");
        assert!(card["header"]["title"]["content"].as_str().unwrap().contains("✅"));
    }
}
