use crate::opencode::Client;
use crate::opencode::client::OpenCodeEvent;
use reqwest::Response;
use tokio::sync::mpsc;

/// Connects to the OpenCode global SSE stream and yields typed events.
pub struct SseStream {
    rx: mpsc::Receiver<crate::error::Result<OpenCodeEvent>>,
}

impl SseStream {
    pub async fn connect(client: &Client) -> crate::error::Result<Self> {
        let (tx, rx) = mpsc::channel(256);

        let response = client
            .http
            .get(client.url("/api/event"))
            .send()
            .await?
            .error_for_status()?;

        let _url = client.url("/api/event");

        tokio::spawn(async move {
            if let Err(e) = sse_loop(response, tx).await {
                tracing::error!("SSE stream error: {}", e);
            }
        });

        Ok(Self { rx })
    }

    pub async fn next_event(&mut self) -> Option<crate::error::Result<OpenCodeEvent>> {
        self.rx.recv().await
    }
}

async fn sse_loop(
    response: Response,
    tx: mpsc::Sender<crate::error::Result<OpenCodeEvent>>,
) -> crate::error::Result<()> {
    use futures_util::StreamExt;

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let text = String::from_utf8_lossy(&chunk);
        buffer.push_str(&text);

        while let Some(boundary) = buffer.find("\n\n") {
            let block = buffer[..boundary].to_string();
            buffer = buffer[boundary + 2..].to_string();

            if block.trim().is_empty() {
                continue;
            }

            let data: String = block
                .lines()
                .filter(|line| line.starts_with("data:"))
                .map(|line| line.strip_prefix("data:").unwrap().trim())
                .collect::<Vec<_>>()
                .join("\n");

            if data.is_empty() {
                continue;
            }

            tracing::info!("SSE raw: {}", &data[..data.len().min(300)]);

            match serde_json::from_str::<OpenCodeEvent>(&data) {
                Ok(event) => {
                    if let OpenCodeEvent::Unknown = &event {
                        tracing::debug!("SSE unknown event, skipping");
                        continue;
                    }
                    tracing::info!("SSE parsed: {:?}", std::mem::discriminant(&event));
                    if tx.send(Ok(event)).await.is_err() {
                        return Ok(());
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to parse SSE data: {} — raw: {}",
                        e,
                        &data[..data.len().min(200)]
                    );
                }
            }
        }
    }

    Ok(())
}
