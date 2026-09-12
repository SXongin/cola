use crate::config::FeishuConfig;
use serde::Deserialize;

/// The Feishu open platform host used in production.
const DEFAULT_BASE_URL: &str = "https://open.feishu.cn";

/// A minimal Feishu REST client.
pub struct Client {
    http: reqwest::Client,
    /// Scheme + host every REST call is built on. Production uses the Feishu
    /// open platform; wire tests point it at a local fake server (ADR-0031).
    base_url: String,
    app_id: String,
    app_secret: String,
    access_token: std::sync::Mutex<Option<CachedToken>>,
}

/// Read a response body, surfacing the HTTP status and a body snippet on any
/// failure. Feishu (or an intermediary proxy) can answer with non-JSON bodies
/// — HTML error pages, block pages — and a bare decode error ("error decoding
/// response body") hides that; the diagnostic makes the actual cause visible
/// in the logs.
async fn read_body_with_diag(resp: reqwest::Response, what: &str) -> crate::error::Result<String> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(crate::error::BridgeError::Feishu(format!(
            "{what} HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        )));
    }
    Ok(text)
}

struct CachedToken {
    token: String,
    expires_at: chrono::DateTime<chrono::Utc>,
}

impl Client {
    pub fn new(cfg: FeishuConfig) -> Self {
        Self::with_base_url(cfg, DEFAULT_BASE_URL)
    }

    /// Build a client against an alternate Feishu-compatible base URL. Normal
    /// production code uses [`Client::new`]; wire tests point this at a local
    /// fake server so the HTTP layer itself is exercised end to end (ADR-0031).
    /// A trailing slash is tolerated.
    pub fn with_base_url(cfg: FeishuConfig, base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            app_id: cfg.app_id,
            app_secret: cfg.app_secret,
            access_token: std::sync::Mutex::new(None),
        }
    }

    /// Build a full endpoint URL from an absolute path beginning with `/`.
    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
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

        let text = read_body_with_diag(
            self.http
                .post(self.endpoint("/open-apis/auth/v3/tenant_access_token/internal"))
                .json(&body)
                .send()
                .await?,
            "token",
        )
        .await?;
        let resp: TokenResponse = serde_json::from_str(&text).map_err(|e| {
            crate::error::BridgeError::Feishu(format!(
                "parse token response: {e} — body: {}",
                text.chars().take(200).collect::<String>()
            ))
        })?;

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

        let text = read_body_with_diag(
            self.http
                .post(self.endpoint(&format!("/open-apis/im/v1/messages/{message_id}/reply")))
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await?,
            "reply card",
        )
        .await?;
        let resp: MessageResponse = serde_json::from_str(&text).map_err(|e| {
            crate::error::BridgeError::Feishu(format!(
                "parse reply card response: {e} — body: {}",
                text.chars().take(200).collect::<String>()
            ))
        })?;

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
            .post(self.endpoint("/callback/ws/endpoint"))
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
            .get(self.endpoint("/open-apis/bot/v3/info"))
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
            .post(self.endpoint(&format!(
                "/open-apis/im/v1/messages?receive_id_type={receive_id_type}"
            )))
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
            .post(self.endpoint(&format!("/open-apis/im/v1/messages/{message_id}/reply")))
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

    /// Reply to a message **in thread form** (`reply_in_thread: true`) with an
    /// interactive card, which creates a topic around the seed message when it
    /// is not already inside one. Returns `(message_id, thread_id)`: the id of
    /// the created reply (which lives INSIDE the topic, usable as an anchor to
    /// reply into it later) and the topic's `thread_id`. Used by `/topic` to
    /// open a real, UI-separated conversation and by `/topic --adopt` to place
    /// the Session Snapshot card as the topic's first message and anchor
    /// (ADR-0028).
    ///
    /// `thread_id` is `None` when the response carries no topic (the chat does
    /// not support topic replies) — the caller must not persist a broken
    /// mapping in that case.
    pub async fn reply_card_in_thread(
        &self,
        message_id: &str,
        card: &serde_json::Value,
    ) -> crate::error::Result<(String, Option<String>)> {
        let token = self.get_access_token().await?;
        let body = serde_json::json!({
            "msg_type": "interactive",
            "reply_in_thread": true,
            "content": card.to_string()
        });

        let resp: MessageResponse = self
            .http
            .post(self.endpoint(&format!("/open-apis/im/v1/messages/{message_id}/reply")))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        if resp.code != 0 {
            Err(crate::error::BridgeError::Feishu(format!(
                "reply card in thread error {}: {}",
                resp.code, resp.msg
            )))
        } else {
            Ok((resp.data.message_id, resp.data.thread_id))
        }
    }

    /// Reply to a message **in thread form** (`reply_in_thread: true`) with a
    /// markdown card, which creates a topic around the seed message. Returns
    /// `(message_id, thread_id)`: the id of the created reply (which lives
    /// INSIDE the topic, usable as an anchor to reply into it later) and the
    /// new topic's `thread_id`. Used by `/topic` to open a real, UI-separated
    /// conversation.
    ///
    /// `thread_id` is `None` when the response carries no topic (the chat does
    /// not support topic replies) — the caller must not persist a broken
    /// mapping in that case.
    pub async fn reply_in_thread(
        &self,
        message_id: &str,
        text: &str,
    ) -> crate::error::Result<(String, Option<String>)> {
        self.reply_card_in_thread(message_id, &markdown_card(text)).await
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
            .post(self.endpoint(&format!("/open-apis/im/v1/messages/{message_id}/reply")))
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
        let url = self.endpoint(&format!(
            "/open-apis/contact/v3/users/{open_id}?user_id_type=open_id"
        ));
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

    /// The display name of a Feishu chat (`im:chat:readonly`), used by the
    /// `/attach` rejection card to name the thread that currently owns a
    /// session. Best-effort `Ok(None)` on failure so callers fall back to the
    /// raw chat id.
    ///
    /// Response shape: `GET /im/v1/chats/{chat_id}` → `data.name`.
    pub async fn chat_name(&self, chat_id: &str) -> crate::error::Result<Option<String>> {
        let token = self.get_access_token().await?;
        let url = self.endpoint(&format!("/open-apis/im/v1/chats/{chat_id}"));
        let resp = self.http.get(url).bearer_auth(&token).send().await?;
        let text = resp.text().await?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| crate::error::BridgeError::Feishu(format!("parse chat info: {e} — body: {text}")))?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            tracing::debug!(
                "chat_name lookup failed ({code}): {}",
                &text[..text.len().min(200)]
            );
            return Ok(None);
        }
        let name = v["data"]["name"].as_str().map(|s| s.to_string());
        if name.is_none() {
            tracing::debug!(
                "chat_name lookup returned no name: {}",
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
            .patch(self.endpoint(&format!("/open-apis/im/v1/messages/{message_id}")))
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

    /// List the most recent messages in a chat or topic (newest first, a single
    /// page of 50). Production use: `resolve_topic_anchor` scans a topic's
    /// messages for the newest cola message to anchor a card reply — the send
    /// API cannot target a `thread_id`.
    #[allow(dead_code)]
    pub async fn list_messages(
        &self,
        container_id_type: &str,
        container_id: &str,
    ) -> crate::error::Result<Vec<ChatMessage>> {
        let token = self.get_access_token().await?;
        let resp = self
            .http
            .get(self.endpoint("/open-apis/im/v1/messages"))
            .query(&[
                ("container_id_type", container_id_type),
                ("container_id", container_id),
                // Newest first, so the newest cola message is on the first
                // page even once a topic passes 50 messages.
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

    /// Fetch a message by id (`GET /im/v1/messages/{id}`), returning the fields
    /// cola needs for quote injection. `card_msg_content_type=raw_card_content`
    /// keeps the raw card JSON (so interactive cards yield extractable text)
    /// instead of a template that discards the content.
    ///
    /// Requires the `im:message` permission. Any failure (missing permission,
    /// deleted message) surfaces as an error; callers degrade to text-only.
    pub async fn get_message(&self, message_id: &str) -> crate::error::Result<FeishuMessage> {
        let token = self.get_access_token().await?;
        let resp = self
            .http
            .get(self.endpoint(&format!(
                "/open-apis/im/v1/messages/{message_id}?card_msg_content_type=raw_card_content"
            )))
            .bearer_auth(&token)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await?;
        let text = resp.text().await?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            crate::error::BridgeError::Feishu(format!("parse get_message: {e} — body: {text}"))
        })?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(crate::error::BridgeError::Feishu(format!(
                "get message error {code}: {}",
                &text[..text.len().min(300)]
            )));
        }
        let Some(item) = v["data"]["items"].get(0) else {
            return Err(crate::error::BridgeError::Feishu(format!(
                "get message error: no data for {message_id} — body: {}",
                &text[..text.len().min(300)]
            )));
        };
        let mentions: Vec<crate::feishu::event::Mention> = serde_json::from_value(
            item.get("mentions")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
        )
        .unwrap_or_default();
        Ok(FeishuMessage {
            msg_type: item
                .get("msg_type")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string(),
            content: item["body"]["content"].as_str().unwrap_or_default().to_string(),
            mentions,
        })
    }

    /// Download an image embedded in a message (`GET /im/v1/messages/{id}/resources/{key}?type=image`),
    /// returning its bytes and the server-declared content type. Requires the
    /// `im:message` permission (already held); callers degrade to a `[图片]`
    /// placeholder on error.
    pub async fn download_image(
        &self,
        message_id: &str,
        image_key: &str,
    ) -> crate::error::Result<ImageAttachment> {
        let token = self.get_access_token().await?;
        let resp = self
            .http
            .get(self.endpoint(&format!(
                "/open-apis/im/v1/messages/{message_id}/resources/{image_key}?type=image"
            )))
            .bearer_auth(&token)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(crate::error::BridgeError::Feishu(format!(
                "download image failed: {status}: {}",
                &text[..text.len().min(300)]
            )));
        }
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/png")
            .to_string();
        let bytes = resp.bytes().await?;
        Ok(ImageAttachment {
            mime,
            data: bytes.to_vec(),
        })
    }
}

/// A minimal interactive card carrying one markdown element — the shape
/// `reply_in_thread` and the topic cover card use (ADR-0023).
pub(crate) fn markdown_card(text: &str) -> serde_json::Value {
    serde_json::json!({
        "config": { "wide_screen_mode": true },
        "elements": [{ "tag": "markdown", "content": text }]
    })
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
    /// Present on replies sent in thread form (`reply_in_thread: true`): the
    /// id of the topic created around the seed message.
    #[serde(default)]
    thread_id: Option<String>,
}

/// A message returned by `list_messages` — the newest-first page
/// `resolve_topic_anchor` scans for a reply anchor.
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

/// A message fetched by id (`get_message`), carrying the fields cola needs for
/// quote injection: the message type, raw content and mentions.
#[derive(Debug, Clone)]
pub struct FeishuMessage {
    pub msg_type: String,
    pub content: String,
    pub mentions: Vec<crate::feishu::event::Mention>,
}

/// A downloaded Feishu image, ready to be attached to a prompt as a vision
/// file part.
#[derive(Debug, Clone)]
pub struct ImageAttachment {
    pub mime: String,
    pub data: Vec<u8>,
}

/// The prompt-relevant content of a Feishu message: extracted text plus any
/// images downloaded from it. Produced for quoted/replied parents so the model
/// sees what the reply answers.
#[derive(Debug, Clone)]
pub struct MessageContext {
    pub text: String,
    pub images: Vec<ImageAttachment>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{RecordedRequest, TestHttpServer};

    const TOKEN_PATH: &str = "/open-apis/auth/v3/tenant_access_token/internal";

    fn test_config() -> FeishuConfig {
        FeishuConfig {
            app_id: "cli_test".into(),
            app_secret: "secret_test".into(),
        }
    }

    async fn token_server(response: serde_json::Value) -> TestHttpServer {
        let server = TestHttpServer::start().await;
        server.route("POST", TOKEN_PATH, 200, response.to_string());
        server
    }

    /// A server with a working token route plus a client pointed at it.
    async fn wire_client() -> (TestHttpServer, Client) {
        let server = token_server(serde_json::json!({
            "code": 0,
            "msg": "ok",
            "tenant_access_token": "t-abc",
            "expire": 7200,
        }))
        .await;
        let client = Client::with_base_url(test_config(), server.base_url());
        (server, client)
    }

    /// The request under test (the token fetch comes first in the log).
    fn last_request(server: &TestHttpServer) -> RecordedRequest {
        server.requests().pop().expect("a request should have been sent")
    }

    fn body_json(request: &RecordedRequest) -> serde_json::Value {
        serde_json::from_str(&request.body).expect("request body should be JSON")
    }

    /// Parse the JSON string inside a message `content` field (a card or text).
    fn send_content(request: &RecordedRequest) -> serde_json::Value {
        let body = body_json(request);
        serde_json::from_str(body["content"].as_str().expect("content should be a string"))
            .expect("content should be JSON")
    }

    /// Unwrap a client error, asserting it is a Feishu error, and return its
    /// message.
    fn feishu_error(err: crate::error::BridgeError) -> String {
        match err {
            crate::error::BridgeError::Feishu(message) => message,
            other => panic!("expected BridgeError::Feishu, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_access_token_posts_credentials_and_returns_token() {
        let server = token_server(serde_json::json!({
            "code": 0,
            "msg": "ok",
            "tenant_access_token": "t-abc",
            "expire": 7200,
        }))
        .await;
        let client = Client::with_base_url(test_config(), server.base_url());

        let token = client.get_access_token().await.unwrap();
        assert_eq!(token, "t-abc");

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, TOKEN_PATH);
        assert_eq!(requests[0].header("content-type"), Some("application/json"));
        let body: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
        assert_eq!(body["app_id"], "cli_test");
        assert_eq!(body["app_secret"], "secret_test");
    }

    #[tokio::test]
    async fn get_access_token_caches_until_expiry() {
        let server = token_server(serde_json::json!({
            "code": 0,
            "msg": "ok",
            "tenant_access_token": "t-abc",
            "expire": 7200,
        }))
        .await;
        let client = Client::with_base_url(test_config(), server.base_url());

        assert_eq!(client.get_access_token().await.unwrap(), "t-abc");
        assert_eq!(client.get_access_token().await.unwrap(), "t-abc");
        assert_eq!(server.request_count(), 1, "a cached token must not re-request");
        assert_eq!(server.requests()[0].path, TOKEN_PATH);
    }

    #[tokio::test]
    async fn get_access_token_refetches_after_expiry() {
        // expire == 60 leaves zero cache lifetime, so the next call refetches.
        let server = token_server(serde_json::json!({
            "code": 0,
            "msg": "ok",
            "tenant_access_token": "t-abc",
            "expire": 60,
        }))
        .await;
        let client = Client::with_base_url(test_config(), server.base_url());

        assert_eq!(client.get_access_token().await.unwrap(), "t-abc");
        assert_eq!(client.get_access_token().await.unwrap(), "t-abc");
        assert_eq!(server.request_count(), 2, "an expired token must be refetched");
        for request in server.requests() {
            assert_eq!(request.path, TOKEN_PATH);
        }
    }

    #[tokio::test]
    async fn get_access_token_maps_business_error_code() {
        let server = token_server(serde_json::json!({
            "code": 10003,
            "msg": "invalid app_secret",
        }))
        .await;
        let client = Client::with_base_url(test_config(), server.base_url());

        let message = feishu_error(client.get_access_token().await.unwrap_err());
        assert!(
            message.contains("token error 10003"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("invalid app_secret"),
            "unexpected error: {message}"
        );
        assert_eq!(server.request_count(), 1);
        assert_eq!(server.requests()[0].path, TOKEN_PATH);
    }

    #[tokio::test]
    async fn reply_card_sends_interactive_content_and_returns_message_id() {
        let (server, client) = wire_client().await;
        server.route(
            "POST",
            "/open-apis/im/v1/messages/om_42/reply",
            200,
            r#"{"code":0,"msg":"ok","data":{"message_id":"om_reply"}}"#,
        );
        let card = serde_json::json!({"elements": [{"tag": "markdown", "content": "hi"}]});

        let id = client.reply_card("om_42", &card).await.unwrap();
        assert_eq!(id, "om_reply");

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/open-apis/im/v1/messages/om_42/reply");
        assert_eq!(request.query, "");
        assert_eq!(request.header("authorization"), Some("Bearer t-abc"));
        let body = body_json(&request);
        assert_eq!(body["msg_type"], "interactive");
        assert_eq!(body["content"], card.to_string());
    }

    #[tokio::test]
    async fn send_card_carries_receive_id_type_and_receive_id() {
        let (server, client) = wire_client().await;
        server.route(
            "POST",
            "/open-apis/im/v1/messages",
            200,
            r#"{"code":0,"msg":"ok","data":{"message_id":"om_sent"}}"#,
        );
        let card = serde_json::json!({"elements": []});

        let id = client.send_card("chat_id", "oc_123", &card).await.unwrap();
        assert_eq!(id, "om_sent");

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/open-apis/im/v1/messages");
        assert_eq!(request.query_param("receive_id_type").as_deref(), Some("chat_id"));
        assert_eq!(request.header("authorization"), Some("Bearer t-abc"));
        let body = body_json(&request);
        assert_eq!(body["receive_id"], "oc_123");
        assert_eq!(body["msg_type"], "interactive");
        assert_eq!(body["content"], card.to_string());
    }

    #[tokio::test]
    async fn send_card_maps_business_error_code() {
        let (server, client) = wire_client().await;
        // Feishu error envelopes still carry a `data` object; the client maps
        // the non-zero code to a diagnostic error.
        server.route(
            "POST",
            "/open-apis/im/v1/messages",
            200,
            r#"{"code":230001,"msg":"invalid receive_id","data":{"message_id":""}}"#,
        );

        let message = feishu_error(
            client
                .send_card("chat_id", "oc_bad", &serde_json::json!({}))
                .await
                .unwrap_err(),
        );
        assert!(
            message.contains("send card error 230001"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("invalid receive_id"),
            "unexpected error: {message}"
        );
    }

    #[tokio::test]
    async fn update_message_patches_the_card() {
        let (server, client) = wire_client().await;
        server.route(
            "PATCH",
            "/open-apis/im/v1/messages/om_42",
            200,
            r#"{"code":0,"msg":"ok"}"#,
        );
        let card = serde_json::json!({"elements": [{"tag": "markdown", "content": "updated"}]});

        client.update_message("om_42", &card).await.unwrap();

        let request = last_request(&server);
        assert_eq!(request.method, "PATCH");
        assert_eq!(request.path, "/open-apis/im/v1/messages/om_42");
        assert_eq!(request.query, "");
        assert_eq!(request.header("authorization"), Some("Bearer t-abc"));
        assert_eq!(body_json(&request)["content"], card.to_string());
    }

    #[tokio::test]
    async fn reply_text_wraps_text_in_a_markdown_card() {
        let (server, client) = wire_client().await;
        server.route(
            "POST",
            "/open-apis/im/v1/messages/om_42/reply",
            200,
            r#"{"code":0,"msg":"ok","data":{"message_id":"om_reply"}}"#,
        );

        let id = client.reply_text("om_42", "hello <world>").await.unwrap();
        assert_eq!(id, "om_reply");

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/open-apis/im/v1/messages/om_42/reply");
        assert_eq!(request.query, "");
        assert_eq!(request.header("authorization"), Some("Bearer t-abc"));
        assert_eq!(body_json(&request)["msg_type"], "interactive");
        let content = send_content(&request);
        assert_eq!(content["config"]["wide_screen_mode"], true);
        assert_eq!(content["elements"][0]["tag"], "markdown");
        assert_eq!(content["elements"][0]["content"], "hello <world>");
    }

    #[tokio::test]
    async fn reply_card_in_thread_requests_a_thread_reply_and_parses_thread_id() {
        let (server, client) = wire_client().await;
        server.route(
            "POST",
            "/open-apis/im/v1/messages/om_42/reply",
            200,
            r#"{"code":0,"msg":"ok","data":{"message_id":"om_thread","thread_id":"omt_1"}}"#,
        );
        let card = serde_json::json!({"elements": []});

        let (id, thread_id) = client.reply_card_in_thread("om_42", &card).await.unwrap();
        assert_eq!(id, "om_thread");
        assert_eq!(thread_id.as_deref(), Some("omt_1"));

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/open-apis/im/v1/messages/om_42/reply");
        assert_eq!(request.query, "");
        assert_eq!(request.header("authorization"), Some("Bearer t-abc"));
        let body = body_json(&request);
        assert_eq!(body["msg_type"], "interactive");
        assert_eq!(body["reply_in_thread"], true);
        assert_eq!(body["content"], card.to_string());
    }

    #[tokio::test]
    async fn reply_in_thread_without_topic_returns_none() {
        let (server, client) = wire_client().await;
        server.route(
            "POST",
            "/open-apis/im/v1/messages/om_42/reply",
            200,
            r#"{"code":0,"msg":"ok","data":{"message_id":"om_thread"}}"#,
        );

        let (id, thread_id) = client.reply_in_thread("om_42", "topic title").await.unwrap();
        assert_eq!(id, "om_thread");
        assert_eq!(thread_id, None);

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/open-apis/im/v1/messages/om_42/reply");
        assert_eq!(request.query, "");
        assert_eq!(request.header("authorization"), Some("Bearer t-abc"));
        assert_eq!(body_json(&request)["msg_type"], "interactive");
        assert_eq!(body_json(&request)["reply_in_thread"], true);
        assert_eq!(send_content(&request)["elements"][0]["content"], "topic title");
    }

    #[tokio::test]
    async fn reply_completion_notice_mentions_and_escapes_the_name() {
        let (server, client) = wire_client().await;
        server.route(
            "POST",
            "/open-apis/im/v1/messages/om_42/reply",
            200,
            r#"{"code":0,"msg":"ok","data":{"message_id":"om_notice"}}"#,
        );

        let id = client
            .reply_completion_notice("om_42", "ou_1", Some("Alice <Admin>"), "任务完成")
            .await
            .unwrap();
        assert_eq!(id, "om_notice");

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/open-apis/im/v1/messages/om_42/reply");
        assert_eq!(request.query, "");
        assert_eq!(request.header("authorization"), Some("Bearer t-abc"));
        assert_eq!(body_json(&request)["msg_type"], "text");
        assert_eq!(
            send_content(&request)["text"],
            "<at user_id=\"ou_1\">Alice &lt;Admin&gt;</at> 任务完成"
        );
    }

    #[tokio::test]
    async fn reply_completion_notice_without_name_is_plain_text() {
        let (server, client) = wire_client().await;
        server.route(
            "POST",
            "/open-apis/im/v1/messages/om_42/reply",
            200,
            r#"{"code":0,"msg":"ok","data":{"message_id":"om_notice"}}"#,
        );

        client
            .reply_completion_notice("om_42", "ou_1", None, "任务完成")
            .await
            .unwrap();

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/open-apis/im/v1/messages/om_42/reply");
        assert_eq!(request.query, "");
        assert_eq!(request.header("authorization"), Some("Bearer t-abc"));
        assert_eq!(body_json(&request)["msg_type"], "text");
        assert_eq!(send_content(&request)["text"], "任务完成");
    }

    #[tokio::test]
    async fn list_messages_sends_the_newest_first_query_and_parses_items() {
        let (server, client) = wire_client().await;
        server.route(
            "GET",
            "/open-apis/im/v1/messages",
            200,
            serde_json::json!({
                "code": 0,
                "msg": "ok",
                "data": {
                    "items": [{
                        "message_id": "om_1",
                        "msg_type": "interactive",
                        "create_time": "1720000000000",
                        "chat_id": "oc_1",
                        "sender": {"id": "cli_bot", "sender_type": "app"},
                        "body": {"content": "card"},
                    }],
                },
            })
            .to_string(),
        );

        let messages = client.list_messages("thread", "omt_1").await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message_id, "om_1");
        assert_eq!(messages[0].msg_type, "interactive");
        assert_eq!(messages[0].chat_id, "oc_1");
        assert_eq!(
            messages[0]
                .sender
                .as_ref()
                .and_then(|sender| sender.sender_type.as_deref()),
            Some("app")
        );

        let request = last_request(&server);
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/open-apis/im/v1/messages");
        assert_eq!(
            request.query_param("container_id_type").as_deref(),
            Some("thread")
        );
        assert_eq!(request.query_param("container_id").as_deref(), Some("omt_1"));
        assert_eq!(
            request.query_param("sort_type").as_deref(),
            Some("ByCreateTimeDesc")
        );
        assert_eq!(request.query_param("page_size").as_deref(), Some("50"));
        assert_eq!(request.header("authorization"), Some("Bearer t-abc"));
    }

    #[tokio::test]
    async fn get_message_requests_raw_card_content_and_parses_mentions() {
        let (server, client) = wire_client().await;
        server.route(
            "GET",
            "/open-apis/im/v1/messages/om_7",
            200,
            serde_json::json!({
                "code": 0,
                "msg": "ok",
                "data": {
                    "items": [{
                        "msg_type": "text",
                        "body": {"content": r#"{"text":"hi"}"#},
                        "mentions": [{
                            "key": "@_user_1",
                            "id": {"open_id": "ou_1"},
                            "name": "Alice",
                        }],
                    }],
                },
            })
            .to_string(),
        );

        let message = client.get_message("om_7").await.unwrap();
        assert_eq!(message.msg_type, "text");
        assert_eq!(message.content, r#"{"text":"hi"}"#);
        assert_eq!(message.mentions.len(), 1);
        assert_eq!(message.mentions[0].name.as_deref(), Some("Alice"));
        assert_eq!(
            message.mentions[0]
                .id
                .as_ref()
                .and_then(|id| id.open_id.as_deref()),
            Some("ou_1")
        );

        let request = last_request(&server);
        assert_eq!(request.path, "/open-apis/im/v1/messages/om_7");
        assert_eq!(
            request.query_param("card_msg_content_type").as_deref(),
            Some("raw_card_content")
        );
    }

    #[tokio::test]
    async fn download_image_passes_through_bytes_and_content_type() {
        let (server, client) = wire_client().await;
        server.route_raw(
            "GET",
            "/open-apis/im/v1/messages/om_7/resources/img_1",
            200,
            "image/png",
            b"PNGDATA".to_vec(),
        );

        let image = client.download_image("om_7", "img_1").await.unwrap();
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.data, b"PNGDATA");

        let request = last_request(&server);
        assert_eq!(request.path, "/open-apis/im/v1/messages/om_7/resources/img_1");
        assert_eq!(request.query_param("type").as_deref(), Some("image"));
    }

    #[tokio::test]
    async fn download_image_maps_http_error() {
        let (server, client) = wire_client().await;
        server.route_raw(
            "GET",
            "/open-apis/im/v1/messages/om_7/resources/img_1",
            500,
            "text/plain",
            "boom",
        );

        let message = feishu_error(client.download_image("om_7", "img_1").await.unwrap_err());
        assert!(
            message.contains("download image failed"),
            "unexpected error: {message}"
        );
        assert!(message.contains("500"), "unexpected error: {message}");
        assert!(message.contains("boom"), "unexpected error: {message}");
    }

    #[tokio::test]
    async fn user_name_reads_the_nested_name() {
        let (server, client) = wire_client().await;
        server.route(
            "GET",
            "/open-apis/contact/v3/users/ou_1",
            200,
            r#"{"code":0,"msg":"ok","data":{"user":{"name":"Alice"}}}"#,
        );

        assert_eq!(client.user_name("ou_1").await.unwrap().as_deref(), Some("Alice"));

        let request = last_request(&server);
        assert_eq!(request.path, "/open-apis/contact/v3/users/ou_1");
        assert_eq!(request.query_param("user_id_type").as_deref(), Some("open_id"));
    }

    #[tokio::test]
    async fn user_name_degrades_to_none_on_business_error() {
        let (server, client) = wire_client().await;
        server.route(
            "GET",
            "/open-apis/contact/v3/users/ou_1",
            200,
            r#"{"code":99991,"msg":"no permission"}"#,
        );

        assert_eq!(client.user_name("ou_1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn chat_name_reads_the_name() {
        let (server, client) = wire_client().await;
        server.route(
            "GET",
            "/open-apis/im/v1/chats/oc_1",
            200,
            r#"{"code":0,"msg":"ok","data":{"name":"Team"}}"#,
        );

        assert_eq!(client.chat_name("oc_1").await.unwrap().as_deref(), Some("Team"));
        assert_eq!(last_request(&server).path, "/open-apis/im/v1/chats/oc_1");
    }

    #[tokio::test]
    async fn chat_name_degrades_to_none_on_business_error() {
        let (server, client) = wire_client().await;
        server.route(
            "GET",
            "/open-apis/im/v1/chats/oc_1",
            200,
            r#"{"code":99991,"msg":"no permission"}"#,
        );

        assert_eq!(client.chat_name("oc_1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn bot_open_id_reads_bot_open_id() {
        let (server, client) = wire_client().await;
        server.route(
            "GET",
            "/open-apis/bot/v3/info",
            200,
            r#"{"code":0,"msg":"ok","bot":{"open_id":"ou_bot"}}"#,
        );

        assert_eq!(client.bot_open_id().await.unwrap(), "ou_bot");

        let request = last_request(&server);
        assert_eq!(request.path, "/open-apis/bot/v3/info");
        assert_eq!(request.header("authorization"), Some("Bearer t-abc"));
    }

    #[tokio::test]
    async fn bot_open_id_maps_business_error() {
        let (server, client) = wire_client().await;
        server.route(
            "GET",
            "/open-apis/bot/v3/info",
            200,
            r#"{"code":10002,"msg":"bad credentials"}"#,
        );

        let message = feishu_error(client.bot_open_id().await.unwrap_err());
        assert!(
            message.contains("bot info error 10002"),
            "unexpected error: {message}"
        );
    }

    #[tokio::test]
    async fn get_ws_endpoint_posts_credentials_and_returns_url() {
        let server = TestHttpServer::start().await;
        server.route(
            "POST",
            "/callback/ws/endpoint",
            200,
            r#"{"code":0,"msg":"ok","data":{"URL":"wss://ws.example"}}"#,
        );
        let client = Client::with_base_url(test_config(), server.base_url());

        assert_eq!(client.get_ws_endpoint().await.unwrap(), "wss://ws.example");

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/callback/ws/endpoint");
        let body = body_json(&request);
        assert_eq!(body["AppID"], "cli_test");
        assert_eq!(body["AppSecret"], "secret_test");
    }

    #[tokio::test]
    async fn get_ws_endpoint_maps_business_error() {
        let server = TestHttpServer::start().await;
        server.route(
            "POST",
            "/callback/ws/endpoint",
            200,
            r#"{"code":1,"msg":"nope","data":{"URL":""}}"#,
        );
        let client = Client::with_base_url(test_config(), server.base_url());

        let message = feishu_error(client.get_ws_endpoint().await.unwrap_err());
        assert!(
            message.contains("ws endpoint error 1"),
            "unexpected error: {message}"
        );
    }

    #[tokio::test]
    async fn reply_card_reports_an_html_error_page() {
        let (server, client) = wire_client().await;
        server.route_raw(
            "POST",
            "/open-apis/im/v1/messages/om_42/reply",
            403,
            "text/html",
            "<html>blocked by proxy</html>",
        );

        let message = feishu_error(
            client
                .reply_card("om_42", &serde_json::json!({}))
                .await
                .unwrap_err(),
        );
        assert!(
            message.contains("reply card HTTP 403"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("blocked by proxy"),
            "unexpected error: {message}"
        );
    }

    #[tokio::test]
    async fn reply_card_reports_http_5xx() {
        let (server, client) = wire_client().await;
        server.route(
            "POST",
            "/open-apis/im/v1/messages/om_42/reply",
            500,
            r#"{"code":-1,"msg":"server exploded"}"#,
        );

        let message = feishu_error(
            client
                .reply_card("om_42", &serde_json::json!({}))
                .await
                .unwrap_err(),
        );
        assert!(
            message.contains("reply card HTTP 500"),
            "unexpected error: {message}"
        );
        assert!(message.contains("server exploded"), "unexpected error: {message}");
    }

    #[tokio::test]
    async fn token_endpoint_non_json_body_is_a_parse_error() {
        let server = TestHttpServer::start().await;
        server.route_raw("POST", TOKEN_PATH, 200, "text/html", "<html>nope</html>");
        let client = Client::with_base_url(test_config(), server.base_url());

        let message = feishu_error(client.get_access_token().await.unwrap_err());
        assert!(
            message.contains("parse token response"),
            "unexpected error: {message}"
        );
    }
}
