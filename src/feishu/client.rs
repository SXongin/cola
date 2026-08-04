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
        tracing::debug!("WS endpoint response: {} body={}", status, &text[..text.len().min(500)]);

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

    /// Send a text message to a user (by open_id) or chat.
    pub async fn send_text(&self, receive_id_type: &str, receive_id: &str, text: &str) -> crate::error::Result<String> {
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
