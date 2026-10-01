// Google Gemini API client — multi-turn chat with Google Search grounding,
// and files sent as inlineData (image/pdf) or text parts.
//
// Matches the architecture of claude.rs: the API key stays in the Windows
// Credential Manager, and file reading happens in the backend.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::secrets;

const BASE_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const MAX_INLINE_TEXT: u64 = 200_000;

#[allow(dead_code)]
pub const DEFAULT_MODEL: &str = "gemini-2.5-flash";

const SYSTEM_PROMPT: &str = "You are Mochi, a personal AI assistant living at the top of the user's screen. \
You have Google Search grounding and can help with research, coding, recommendations, tasks, and questions. \
Respond in the user's language. Be thorough, clear and complete. \
No markdown formatting (no **, no ##, no bullet dashes). Use plain text with line breaks.";

#[derive(Default)]
pub struct Chat {
    /// Full multi-turn history in Gemini format: [{"role": "user"|"model", "parts": [...]}]
    contents: Mutex<Vec<Value>>,
}

impl Chat {
    pub fn reset(&self) {
        self.contents.lock().unwrap().clear();
    }

    fn is_empty(&self) -> bool {
        self.contents.lock().unwrap().is_empty()
    }

    fn push(&self, message: Value) {
        self.contents.lock().unwrap().push(message);
    }

    fn pop(&self) {
        self.contents.lock().unwrap().pop();
    }

    fn snapshot(&self) -> Vec<Value> {
        self.contents.lock().unwrap().clone()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ChatContext {
    File { name: String, path: String },
    Window { app_name: String, title: String, url: Option<String> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatReply {
    pub text: String,
}

pub async fn send(
    chat: &Chat,
    model: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let key = secrets::get("gemini-api-key")
        .ok_or_else(|| "Gemini API key missing. Open settings to configure it.".to_string())?;

    let mut parts: Vec<Value> = Vec::new();

    // Attach file or window context on the first turn
    if chat.is_empty() {
        match &context {
            Some(ChatContext::File { name, path }) => {
                if let Some(part) = file_part(path) {
                    parts.push(part);
                }
                parts.push(json!({ "text": format!("File: {name}") }));
            }
            Some(ChatContext::Window { app_name, title, url }) => {
                let mut text = format!("Context — App: {app_name}, Window: {title}");
                if let Some(url) = url {
                    text.push_str(&format!(", URL: {url}"));
                }
                parts.push(json!({ "text": text }));
            }
            None => {}
        }
    }
    parts.push(json!({ "text": query }));

    chat.push(json!({ "role": "user", "parts": parts }));

    let request_body = json!({
        "contents": chat.snapshot(),
        "systemInstruction": {
            "parts": [{ "text": SYSTEM_PROMPT }]
        },
        "tools": [
            { "googleSearch": {} }
        ],
        "generationConfig": {
            "maxOutputTokens": 4096,
            "temperature": 0.7
        }
    });

    let endpoint = format!("{BASE_ENDPOINT}/{model}:generateContent?key={key}");
    let response = match call(&endpoint, &request_body).await {
        Ok(v) => v,
        Err(err) => {
            chat.pop();
            return Err(err);
        }
    };

    // Extract text from candidates
    let candidate = response
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first());

    let parts = candidate
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array);

    let Some(parts) = parts else {
        chat.pop();
        if let Some(block_reason) = candidate.and_then(|c| c.get("finishReason")).and_then(Value::as_str) {
            return Err(format!("Gemini declined: {block_reason}"));
        }
        return Err("Unexpected API response from Gemini.".into());
    };

    let text = parts
        .iter()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if text.is_empty() {
        chat.pop();
        return Err("No response text from Gemini.".into());
    }

    // Save assistant reply in conversation history
    chat.push(json!({
        "role": "model",
        "parts": parts.clone()
    }));

    Ok(ChatReply { text })
}

async fn call(url: &str, body: &Value) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .map_err(|e| e.to_string())?;

    let response = client
        .post(url)
        .header("content-type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("Gemini API {status}: {detail}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("Bad API response: {e}"))
}

/// Converts a dropped file to a Gemini inlineData part or inlined text part.
fn file_part(path: &str) -> Option<Value> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let mime_type = match ext.as_str() {
        "pdf" => Some("application/pdf"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    };

    if let Some(mime) = mime_type {
        let bytes = std::fs::read(path).ok()?;
        return Some(json!({
            "inlineData": {
                "mimeType": mime,
                "data": crate::claude::base64_for(&bytes)
            }
        }));
    }

    let len = std::fs::metadata(path).ok()?.len();
    if len > MAX_INLINE_TEXT {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    Some(json!({ "text": format!("File contents:\n{text}") }))
}
