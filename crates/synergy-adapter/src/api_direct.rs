//! Direct API adapter for Leader mode.
//!
//! Instead of embedding a GUI app or spawning a CLI tool, this adapter
//! calls an LLM API directly via HTTP (reqwest). Synergy renders its own
//! chat UI in the Leader panel.
//!
//! Supported providers:
//! - Anthropic (Claude)
//! - OpenAI (GPT)
//! - Google (Gemini)
//! - Local (Ollama / LM Studio via OpenAI-compatible endpoint)

use crate::adapter::{AppAdapter, AppHandle, AppStatus, AppType, LaunchConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Supported API providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiProvider {
    Anthropic,
    OpenAi,
    Google,
    Local,
}

impl ApiProvider {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "anthropic" | "claude" => Self::Anthropic,
            "openai" | "gpt" => Self::OpenAi,
            "google" | "gemini" => Self::Google,
            _ => Self::Local,
        }
    }

    pub fn default_base_url(&self) -> &str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::OpenAi => "https://api.openai.com",
            Self::Google => "https://generativelanguage.googleapis.com",
            Self::Local => "http://localhost:11434",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    pub provider: String,
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

/// Internal message buffer for the API conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,   // "user" | "assistant" | "system"
    pub content: String,
}

/// Shared state for the API adapter (conversation history + config).
pub struct ApiState {
    pub config: ApiConfig,
    pub messages: Vec<ChatMessage>,
    pub last_response: Option<String>,
    pub client: reqwest::Client,
}

pub struct DirectApiAdapter {
    state: Arc<Mutex<Option<ApiState>>>,
}

impl DirectApiAdapter {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
        }
    }
}

impl Default for DirectApiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AppAdapter for DirectApiAdapter {
    fn id(&self) -> &str {
        "api-direct"
    }

    fn display_name(&self) -> &str {
        "Direct API (LLM)"
    }

    fn app_type(&self) -> AppType {
        AppType::Cli // Renders in Synergy's own chat UI
    }

    async fn launch(&self, config: &LaunchConfig) -> Result<AppHandle> {
        // Parse API config from LaunchConfig args
        // Expected: args = ["provider", "model", "api_key", "base_url?"]
        let provider = config.args.first().cloned().unwrap_or_else(|| "anthropic".to_owned());
        let model = config.args.get(1).cloned().unwrap_or_else(|| "claude-sonnet-4-20250514".to_owned());
        let api_key = config.args.get(2).cloned();
        let base_url = config.args.get(3).cloned();

        let api_config = ApiConfig {
            provider: provider.clone(),
            model,
            api_key,
            base_url,
        };

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?;

        let state = ApiState {
            config: api_config,
            messages: Vec::new(),
            last_response: None,
            client,
        };

        *self.state.lock().await = Some(state);

        Ok(AppHandle {
            pty_session: None,
            window_hwnd: None,
            user_data: None,
        })
    }

    async fn send_command(&self, _handle: &mut AppHandle, text: &str) -> Result<()> {
        let mut guard = self.state.lock().await;
        let state = guard.as_mut().ok_or_else(|| anyhow!("API not initialized"))?;

        state.messages.push(ChatMessage {
            role: "user".to_owned(),
            content: text.to_owned(),
        });

        let provider = ApiProvider::from_str(&state.config.provider);
        let response = match provider {
            ApiProvider::Anthropic => call_anthropic(state).await?,
            ApiProvider::OpenAi | ApiProvider::Local => call_openai_compatible(state).await?,
            ApiProvider::Google => call_openai_compatible(state).await?, // Gemini supports OpenAI format
        };

        state.messages.push(ChatMessage {
            role: "assistant".to_owned(),
            content: response.clone(),
        });
        state.last_response = Some(response);

        Ok(())
    }

    async fn read_output(&self, _handle: &mut AppHandle) -> Option<String> {
        let mut guard = self.state.lock().await;
        let state = guard.as_mut()?;
        state.last_response.take()
    }

    async fn detect_status(&self, _output_buffer: &str) -> AppStatus {
        AppStatus::Idle
    }
}

// ─── Anthropic API ───────────────────────────────────────────────────────────

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
}

#[derive(Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
}

#[derive(Deserialize)]
struct AnthropicContent {
    text: Option<String>,
}

async fn call_anthropic(state: &ApiState) -> Result<String> {
    let base = state
        .config
        .base_url
        .as_deref()
        .unwrap_or(ApiProvider::Anthropic.default_base_url());
    let url = format!("{}/v1/messages", base);

    let api_key = state
        .config
        .api_key
        .as_deref()
        .ok_or_else(|| anyhow!("Anthropic API key required"))?;

    let messages: Vec<AnthropicMessage> = state
        .messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| AnthropicMessage {
            role: m.role.clone(),
            content: m.content.clone(),
        })
        .collect();

    let body = AnthropicRequest {
        model: state.config.model.clone(),
        max_tokens: 4096,
        messages,
    };

    let resp = state
        .client
        .post(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Anthropic API error {}: {}", status, text));
    }

    let data: AnthropicResponse = resp.json().await?;
    let text = data
        .content
        .into_iter()
        .filter_map(|c| c.text)
        .collect::<Vec<_>>()
        .join("");

    Ok(text)
}

// ─── OpenAI-compatible API ───────────────────────────────────────────────────

#[derive(Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<OpenAiMessage>,
    max_tokens: Option<u32>,
}

#[derive(Serialize, Deserialize)]
struct OpenAiMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

async fn call_openai_compatible(state: &ApiState) -> Result<String> {
    let provider = ApiProvider::from_str(&state.config.provider);
    let base = state
        .config
        .base_url
        .as_deref()
        .unwrap_or(provider.default_base_url());

    let url = if provider == ApiProvider::Local {
        format!("{}/v1/chat/completions", base)
    } else {
        format!("{}/v1/chat/completions", base)
    };

    let messages: Vec<OpenAiMessage> = state
        .messages
        .iter()
        .map(|m| OpenAiMessage {
            role: m.role.clone(),
            content: m.content.clone(),
        })
        .collect();

    let body = OpenAiRequest {
        model: state.config.model.clone(),
        messages,
        max_tokens: Some(4096),
    };

    let mut req = state
        .client
        .post(&url)
        .header("content-type", "application/json");

    if let Some(ref key) = state.config.api_key {
        req = req.header("authorization", format!("Bearer {}", key));
    }

    let resp = req.json(&body).send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("API error {}: {}", status, text));
    }

    let data: OpenAiResponse = resp.json().await?;
    let text = data
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .unwrap_or_default();

    Ok(text)
}
