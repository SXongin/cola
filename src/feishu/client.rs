use crate::config::FeishuConfig;
use serde::Deserialize;

/// A minimal Feishu REST client.
pub struct Client {
    http: reqwest::Client,
    app_id: String,
    app_secret: String,
    access_token: std::sync::Mutex<Option<CachedToken>>,
}

struct CachedToken {
    token: String,
    expires_at: chrono::DateTime<chrono::Utc>,
}

impl Client {
    pub fn new(cfg: FeishuConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            app_id: cfg.app_id,
            app_secret: cfg.app_secret,
            access_token: std::sync::Mutex::new(None),
        }
    }

    /// Obtain a tenant access token, caching it until expiry.
    pub async fn get_access_token(&self) -> crate::error::Result<String> {
        {
            let cached = self.access_token.lock().unwrap();
            if let Some(ref token) = *cached
                && token.expires_at > chrono::Utc::now()
            {
                return Ok(token.token.clone());
            }
        }

        let body = serde_json::json!({
            "app_id": self.app_id,
            "app_secret": self.app_secret,
        });

        let resp: TokenResponse = self
            .http
            .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if resp.code != 0 {
            return Err(crate::error::BridgeError::Feishu(format!(
                "token error {}: {}",
                resp.code, resp.msg
            )));
        }

        let token = resp.tenant_access_token.clone();
        let expires_at = chrono::Utc::now() + chrono::Duration::seconds(resp.expire as i64 - 60);

        let mut cached = self.access_token.lock().unwrap();
        *cached = Some(CachedToken {
            token: token.clone(),
            expires_at,
        });

        Ok(token)
    }

    /// Reply to a Feishu message with an interactive card.
    pub async fn reply_card(
        &self,
        message_id: &str,
        card: &serde_json::Value,
    ) -> crate::error::Result<String> {
        let token = self.get_access_token().await?;
        let body = serde_json::json!({
            "msg_type": "interactive",
            "content": card.to_string()
        });

        let resp: MessageResponse = self
            .http
            .post(format!(
                "https://open.feishu.cn/open-apis/im/v1/messages/{}/reply",
                message_id
            ))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        if resp.code != 0 {
            Err(crate::error::BridgeError::Feishu(format!(
                "reply card error {}: {}",
                resp.code, resp.msg
            )))
        } else {
            Ok(resp.data.message_id)
        }
    }

    /// Get the WebSocket endpoint URL for long-connection mode.
    pub async fn get_ws_endpoint(&self) -> crate::error::Result<String> {
        let body = serde_json::json!({
            "AppID": self.app_id,
            "AppSecret": self.app_secret,
        });

        let resp = self
            .http
            .post("https://open.feishu.cn/callback/ws/endpoint")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;
        tracing::debug!(
            "WS endpoint response: {} body={}",
            status,
            &text[..text.len().min(500)]
        );

        let resp_data: WsEndpointResponse = serde_json::from_str(&text).map_err(|e| {
            crate::error::BridgeError::Feishu(format!("parse ws endpoint: {e} — body: {text}"))
        })?;

        if resp_data.code != 0 {
            Err(crate::error::BridgeError::Feishu(format!(
                "ws endpoint error {}: {}",
                resp_data.code, resp_data.msg
            )))
        } else {
            Ok(resp_data.data.url)
        }
    }

    /// The bot's own open_id, used to recognise @mentions of cola itself.
    /// Fetches it from the bot info API on each call; the caller caches the
    /// result on `App.bot_open_id` at startup.
    pub async fn bot_open_id(&self) -> crate::error::Result<String> {
        let token = self.get_access_token().await?;
        let resp = self
            .http
            .get("https://open.feishu.cn/open-apis/bot/v3/info")
            .bearer_auth(&token)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;
        tracing::debug!(
            "bot info response: {} body={}",
            status,
            &text[..text.len().min(500)]
        );

        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| crate::error::BridgeError::Feishu(format!("parse bot info: {e} — body: {text}")))?;
        let code = parsed["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            return Err(crate::error::BridgeError::Feishu(format!(
                "bot info error {code} — body: {text}"
            )));
        }
        parsed["bot"]["open_id"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| {
                crate::error::BridgeError::Feishu(format!("bot info missing bot.open_id — body: {text}"))
            })
    }

    /// Send a text message to a user (by open_id) or chat. Only the live e2e
    /// harness uses this.
    #[cfg(test)]
    pub async fn send_text(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        text: &str,
    ) -> crate::error::Result<String> {
        let token = self.get_access_token().await?;
        let body = serde_json::json!({
            "receive_id": receive_id,
            "msg_type": "text",
            "content": serde_json::json!({"text": text}).to_string()
        });

        let resp: MessageResponse = self
            .http
            .post(format!(
                "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type={}",
                receive_id_type
            ))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        if resp.code != 0 {
            Err(crate::error::BridgeError::Feishu(format!(
                "send error {}: {}",
                resp.code, resp.msg
            )))
        } else {
            Ok(resp.data.message_id)
        }
    }

    /// Send an interactive card to a user (by open_id) or chat.
    pub async fn send_card(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        card: &serde_json::Value,
    ) -> crate::error::Result<String> {
        let token = self.get_access_token().await?;
        let body = serde_json::json!({
            "receive_id": receive_id,
            "msg_type": "interactive",
            "content": card.to_string()
        });

        let resp: MessageResponse = self
            .http
            .post(format!(
                "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type={}",
                receive_id_type
            ))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        if resp.code != 0 {
            Err(crate::error::BridgeError::Feishu(format!(
                "send card error {}: {}",
                resp.code, resp.msg
            )))
        } else {
            Ok(resp.data.message_id)
        }
    }

    /// Reply to a Feishu message, returning the sent message_id for card updates.
    pub async fn reply_text(&self, message_id: &str, text: &str) -> crate::error::Result<String> {
        let token = self.get_access_token().await?;
        let body = serde_json::json!({
            "msg_type": "interactive",
            "content": serde_json::json!({
                "config": { "wide_screen_mode": true },
                "elements": [{
                    "tag": "markdown",
                    "content": text
                }]
            }).to_string()
        });

        let resp: MessageResponse = self
            .http
            .post(format!(
                "https://open.feishu.cn/open-apis/im/v1/messages/{}/reply",
                message_id
            ))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        if resp.code != 0 {
            Err(crate::error::BridgeError::Feishu(format!(
                "reply error {}: {}",
                resp.code, resp.msg
            )))
        } else {
            Ok(resp.data.message_id)
        }
    }

    /// Reply to a message with a completion notice: a short text message. When
    /// the requester's display name is known, @-mention them so the group gets
    /// a real notification; otherwise a plain reply still notifies the author.
    pub async fn reply_completion_notice(
        &self,
        message_id: &str,
        open_id: &str,
        name: Option<&str>,
        text: &str,
    ) -> crate::error::Result<String> {
        let token = self.get_access_token().await?;
        let mut content = String::new();
        if let Some(name) = name {
            let escaped = name.replace('<', "&lt;").replace('>', "&gt;");
            content.push_str(&format!("<at user_id=\"{}\">{}</at> ", open_id, escaped));
        }
        content.push_str(text);
        let body = serde_json::json!({
            "msg_type": "text",
            "content": serde_json::json!({"text": content}).to_string()
        });

        let resp: MessageResponse = self
            .http
            .post(format!(
                "https://open.feishu.cn/open-apis/im/v1/messages/{}/reply",
                message_id
            ))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        if resp.code != 0 {
            Err(crate::error::BridgeError::Feishu(format!(
                "reply completion notice error {}: {}",
                resp.code, resp.msg
            )))
        } else {
            Ok(resp.data.message_id)
        }
    }

    /// The display name of a Feishu user (`contact:user.base:readonly`).
    /// Returns `Ok(None)` on any failure (missing permission, deleted user) so
    /// callers fall back to a plain reply instead of erroring.
    ///
    /// Response shape: `GET /contact/v3/users/{open_id}` → `data.user.name`
    /// (NOT `data.name` — reading the wrong path silently returned None).
    pub async fn user_name(&self, open_id: &str) -> crate::error::Result<Option<String>> {
        let token = self.get_access_token().await?;
        let url = format!(
            "https://open.feishu.cn/open-apis/contact/v3/users/{}?user_id_type=open_id",
            open_id
        );
        let resp = self.http.get(url).bearer_auth(&token).send().await?;
        let text = resp.text().await?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| crate::error::BridgeError::Feishu(format!("parse user info: {e} — body: {text}")))?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            tracing::debug!(
                "user_name lookup failed ({code}): {}",
                &text[..text.len().min(200)]
            );
            return Ok(None);
        }
        let name = v["data"]["user"]["name"].as_str().map(|s| s.to_string());
        if name.is_none() {
            tracing::debug!(
                "user_name lookup returned no name: {}",
                &text[..text.len().min(300)]
            );
        }
        Ok(name)
    }

    /// Update (patch) an existing message card.
    pub async fn update_message(
        &self,
        message_id: &str,
        card_json: &serde_json::Value,
    ) -> crate::error::Result<()> {
        let token = self.get_access_token().await?;
        let body = serde_json::json!({
            "content": card_json.to_string()
        });

        let resp: ApiResponse = self
            .http
            .patch(format!(
                "https://open.feishu.cn/open-apis/im/v1/messages/{}",
                message_id
            ))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        if resp.code != 0 {
            Err(crate::error::BridgeError::Feishu(format!(
                "update error {}: {}",
                resp.code, resp.msg
            )))
        } else {
            Ok(())
        }
    }

    /// List messages in a chat (used by the live end-to-end harness: a second
    /// Feishu bot reads what the cola bot actually sent). Queried without a
    /// time window — Feishu's start_time/end_time filter returns empty for
    /// recent messages on this API, so the harness filters client-side.
    #[allow(dead_code)]
    pub async fn list_messages(
        &self,
        container_id_type: &str,
        container_id: &str,
    ) -> crate::error::Result<Vec<ChatMessage>> {
        let token = self.get_access_token().await?;
        let resp = self
            .http
            .get("https://open.feishu.cn/open-apis/im/v1/messages")
            .query(&[
                ("container_id_type", container_id_type),
                ("container_id", container_id),
                // Newest first, so recent cards (which the tests wait for) are
                // on the first page even once the group passes 50 messages.
                ("sort_type", "ByCreateTimeDesc"),
                ("page_size", "50"),
            ])
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            eprintln!(
                "list_messages failed: {} — body: {}",
                status,
                &text[..text.len().min(500)]
            );
            return Err(crate::error::BridgeError::Feishu(format!(
                "list messages failed: {}",
                status
            )));
        }
        let resp: MessagesResponse = resp.json().await?;

        if resp.code != 0 {
            Err(crate::error::BridgeError::Feishu(format!(
                "list messages error {}: {}",
                resp.code, resp.msg
            )))
        } else {
            Ok(resp.data.items)
        }
    }
}

#[derive(Debug, Deserialize)]
struct WsEndpointResponse {
    code: i32,
    msg: String,
    data: WsEndpointData,
}

#[derive(Debug, Deserialize)]
struct WsEndpointData {
    #[serde(rename = "URL")]
    url: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    code: i32,
    msg: String,
    #[serde(default)]
    tenant_access_token: String,
    #[serde(default)]
    expire: i64,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    code: i32,
    msg: String,
}

#[derive(Debug, Deserialize)]
struct MessageResponse {
    code: i32,
    msg: String,
    data: MessageData,
}

#[derive(Debug, Deserialize)]
struct MessageData {
    message_id: String,
}

/// A message returned by `list_messages` — used by the live E2E harness.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ChatMessage {
    #[serde(rename = "message_id")]
    pub message_id: String,
    #[serde(rename = "msg_type")]
    pub msg_type: String,
    #[serde(rename = "create_time")]
    pub create_time: String,
    #[serde(rename = "chat_id")]
    pub chat_id: String,
    #[serde(default)]
    pub sender: Option<ChatMessageSender>,
    #[serde(default)]
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ChatMessageSender {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "sender_type", default)]
    pub sender_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct MessagesResponse {
    code: i32,
    msg: String,
    data: MessagesData,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct MessagesData {
    #[serde(default)]
    items: Vec<ChatMessage>,
}
