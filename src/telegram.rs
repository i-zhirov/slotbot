use anyhow::{Context, Result};
use serde::Serialize;

/// Minimal Telegram Bot API client (sendMessage only).
#[derive(Clone)]
pub struct Telegram {
    http: reqwest::Client,
    token: String,
    chat_ids: Vec<String>,
    dry_run: bool,
}

impl Telegram {
    pub fn new(token: &str, chat_ids: &[String], dry_run: bool) -> Result<Self> {
        if chat_ids.is_empty() {
            anyhow::bail!("no Telegram chat ids configured");
        }
        let mut builder = reqwest::Client::builder();
        // Optional outbound proxy for networks where api.telegram.org is
        // blocked, e.g. SLOTBOT_HTTPS_PROXY=socks5h://127.0.0.1:1080
        if let Ok(proxy) = std::env::var("SLOTBOT_HTTPS_PROXY") {
            if !proxy.is_empty() {
                builder = builder.proxy(
                    reqwest::Proxy::all(&proxy)
                        .with_context(|| format!("invalid SLOTBOT_HTTPS_PROXY: {proxy}"))?,
                );
            }
        }
        let http = builder.build().context("failed to build HTTP client")?;
        Ok(Telegram {
            http,
            token: token.to_string(),
            chat_ids: chat_ids.to_vec(),
            dry_run,
        })
    }

    /// Send the same message to every configured chat id.
    pub async fn send(&self, text: &str) -> Result<()> {
        for chat_id in &self.chat_ids {
            if self.dry_run {
                println!("=== DRY RUN: would send to {chat_id} ===\n{text}\n");
                continue;
            }
            let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
            let body = SendMessage {
                chat_id: chat_id.clone(),
                text: text.to_string(),
                parse_mode: "HTML",
                disable_web_page_preview: true,
            };

            let resp = self
                .http
                .post(&url)
                .json(&body)
                .send()
                .await
                .with_context(|| format!("Telegram request failed (chat {chat_id})"))?;

            let status = resp.status();
            let text_resp = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!(
                    "Telegram -> HTTP {status} (chat {chat_id}): {}",
                    text_resp.chars().take(300).collect::<String>()
                );
            }
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct SendMessage {
    chat_id: String,
    text: String,
    parse_mode: &'static str,
    disable_web_page_preview: bool,
}
