//! HTTP 客户端、provider 凭据解析、retry / failover、OpenAI 兼容 chat completion 调用与 SSE 流。
//!
//! 本模块对外暴露：
//! - `ProviderConnectionInput` / `resolve_provider_credentials` —— 来自前端的 provider 临时配置或 settings.json 的解析。
//! - `build_http_client` —— 60s 超时的 reqwest Client 构造。
//! - `effective_retry_attempts` —— 把 settings.retry_enabled + retry_attempts 折成实际尝试次数。
//! - `extract_status_code` / `is_failover_error` —— failover 判定（仅 401/402/403/429）。
//! - `send_with_retry` —— 网络抖动 / 5xx / 429 退避重试。
//! - `send_with_failover` —— 在 api_keys 列表上轮换；401/402/403/429 直接切 key。
//! - `call_openai_text` / `call_openai_ocr` / `call_vision_api` —— chat completion 三类调用。
//! - `stream_chat_call` / `stream_translate_combined` / `stream_vision_response` —— SSE 流解析。
//! - `build_ocr_request_body` —— 视觉 + 流式 body 构造。

use std::{
  collections::HashSet,
  fs,
  future::Future,
  path::Path,
  sync::atomic::{AtomicU64, Ordering},
  time::Duration,
};

use base64::{engine::general_purpose, Engine as _};
use reqwest::{header::HeaderMap, Client, RequestBuilder, StatusCode};
use serde::Deserialize;
use tauri::{AppHandle, Emitter, State};

use crate::apple_intelligence::APPLE_INTELLIGENCE_BASE_URL;
use crate::prompts::COMBINED_TRANSLATE_SEPARATOR;
use crate::lens_commands::resolve_explain_image_path;
use crate::settings::{
  self, default_system_prompt, no_think_instruction, ExplainMessage, Settings,
};
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelEndpointKind {
  ChatCompletions,
  Responses,
  Messages,
  LegacyBase,
}

fn normalized_provider_url(url: &str) -> String {
  url.trim().trim_end_matches('/').to_string()
}

fn provider_endpoint_kind(url: &str) -> ModelEndpointKind {
  let normalized = normalized_provider_url(url).to_ascii_lowercase();
  if normalized.ends_with("/responses") {
    ModelEndpointKind::Responses
  } else if normalized.ends_with("/messages") {
    ModelEndpointKind::Messages
  } else if normalized.ends_with("/chat/completions") {
    ModelEndpointKind::ChatCompletions
  } else {
    ModelEndpointKind::LegacyBase
  }
}

fn chat_completions_url(url: &str) -> String {
  let normalized = normalized_provider_url(url);
  if provider_endpoint_kind(&normalized) == ModelEndpointKind::ChatCompletions {
    normalized
  } else {
    format!("{normalized}/chat/completions")
  }
}

fn responses_api_url(url: &str) -> String {
  let normalized = normalized_provider_url(url);
  if provider_endpoint_kind(&normalized) == ModelEndpointKind::Responses {
    normalized
  } else {
    format!("{normalized}/responses")
  }
}

fn messages_api_url(url: &str) -> String {
  let normalized = normalized_provider_url(url);
  if provider_endpoint_kind(&normalized) == ModelEndpointKind::Messages {
    normalized
  } else {
    format!("{normalized}/messages")
  }
}

pub fn models_url_from_provider_url(url: &str) -> String {
  let normalized = normalized_provider_url(url);
  let lower = normalized.to_ascii_lowercase();
  let base = if lower.ends_with("/chat/completions") {
    &normalized[..normalized.len() - "/chat/completions".len()]
  } else if lower.ends_with("/responses") {
    &normalized[..normalized.len() - "/responses".len()]
  } else if lower.ends_with("/messages") {
    &normalized[..normalized.len() - "/messages".len()]
  } else {
    normalized.as_str()
  };
  format!("{base}/models")
}

fn claude_auth_headers(request: RequestBuilder, key: &str) -> RequestBuilder {
  request
    .bearer_auth(key)
    .header("anthropic-version", "2023-06-01")
}

fn responses_role(role: &str) -> &'static str {
  match role {
    "system" | "developer" => "developer",
    "assistant" => "assistant",
    _ => "user",
  }
}

fn responses_input_text(text: impl Into<String>) -> serde_json::Value {
  serde_json::json!({ "type": "input_text", "text": text.into() })
}

fn responses_output_text(text: impl Into<String>) -> serde_json::Value {
  serde_json::json!({ "type": "output_text", "text": text.into() })
}

fn responses_input_image(url: impl Into<String>) -> serde_json::Value {
  serde_json::json!({ "type": "input_image", "image_url": url.into() })
}

fn responses_text_content_for_role(role: &str, text: impl Into<String>) -> serde_json::Value {
  if role == "assistant" {
    responses_output_text(text)
  } else {
    responses_input_text(text)
  }
}

fn responses_text_message(role: &str, text: impl Into<String>) -> serde_json::Value {
  serde_json::json!({
    "role": responses_role(role),
    "content": [responses_text_content_for_role(responses_role(role), text)]
  })
}

fn responses_tools(web_search: bool) -> serde_json::Value {
  if web_search {
    serde_json::json!([{ "type": "web_search" }])
  } else {
    serde_json::json!([])
  }
}

fn apply_responses_web_search(body: &mut serde_json::Value, web_search: bool) {
  if web_search {
    body["tools"] = responses_tools(true);
    body["tool_choice"] = serde_json::json!("auto");
  }
}

fn tavily_search_tool_def() -> serde_json::Value {
  serde_json::json!({
    "type": "function",
    "function": {
      "name": "tavily_web_search",
      "description": "Search the web for real-time information. Use this when the user asks about current events, news, recent or changing facts, or when you are unsure and need sources before answering.",
      "parameters": {
        "type": "object",
        "properties": {
          "query": {
            "type": "string",
            "description": "The search query"
          }
        },
        "required": ["query"]
      }
    }
  })
}

pub async fn tavily_search(client: &Client, api_key: &str, query: &str) -> Result<String, String> {
  let body = serde_json::json!({
    "api_key": api_key,
    "query": query,
    "max_results": 5
  });
  let response = client
    .post("https://api.tavily.com/search")
    .timeout(Duration::from_secs(30))
    .json(&body)
    .send()
    .await
    .map_err(|e| format!("Tavily request failed: {e}"))?;
  let status = response.status();
  let raw = response.text().await
    .map_err(|e| format!("Tavily read body: {e}"))?;
  if !status.is_success() {
    return Err(format!("Tavily HTTP {}: {}", status.as_u16(), raw.chars().take(200).collect::<String>()));
  }
  let value: serde_json::Value = serde_json::from_str(&raw)
    .map_err(|e| format!("Tavily parse JSON: {e}"))?;
  let results = value.get("results").and_then(|v| v.as_array());
  let mut output = String::new();
  if let Some(items) = results {
    for item in items {
      let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
      let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
      let content = item.get("content").and_then(|v| v.as_str()).unwrap_or("");
      output.push_str(&format!("## {title}\nURL: {url}\n{content}\n\n"));
    }
  }
  if output.is_empty() {
    output = "No search results found.".to_string();
  }
  Ok(output)
}

fn extract_tavily_tool_call_query(
  response: &serde_json::Value,
  use_responses: bool,
  use_messages: bool,
) -> Option<(String, String)> {
  if use_responses {
    // Responses API: output[].type == "function_call", name == "tavily_web_search"
    let output = response.get("output").and_then(|v| v.as_array())?;
    let call = output.iter().find(|item| {
      item.get("type").and_then(|v| v.as_str()) == Some("function_call")
        && item.get("name").and_then(|v| v.as_str()) == Some("tavily_web_search")
    })?;
    let call_id = call.get("call_id").and_then(|v| v.as_str()).unwrap_or("call_1").to_string();
    let args_str = call.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
    let query = serde_json::from_str::<serde_json::Value>(args_str)
      .ok()
      .and_then(|v| v.get("query").and_then(|q| q.as_str()).map(str::to_string))
      .unwrap_or_else(|| "news".to_string());
    Some((call_id, query))
  } else if use_messages {
    // Messages API: content[].type == "tool_use", name == "tavily_web_search"
    let content = response.get("content").and_then(|v| v.as_array())?;
    let call = content.iter().find(|item| {
      item.get("type").and_then(|v| v.as_str()) == Some("tool_use")
        && item.get("name").and_then(|v| v.as_str()) == Some("tavily_web_search")
    })?;
    let call_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("call_1").to_string();
    let query = call.get("input")
      .and_then(|v| v.get("query"))
      .and_then(|v| v.as_str())
      .unwrap_or("news")
      .to_string();
    Some((call_id, query))
  } else {
    // Chat Completions: choices[0].message.tool_calls[].function.name == "tavily_web_search"
    let tool_calls = response.get("choices")
      .and_then(|c| c.get(0))
      .and_then(|c| c.get("message"))
      .and_then(|m| m.get("tool_calls"))
      .and_then(|t| t.as_array())?;
    let call = tool_calls.iter().find(|c| {
      c.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()) == Some("tavily_web_search")
    })?;
    let call_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("call_1").to_string();
    let args_str = call.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or("{}");
    let query = serde_json::from_str::<serde_json::Value>(args_str)
      .ok()
      .and_then(|v| v.get("query").and_then(|q| q.as_str()).map(str::to_string))
      .unwrap_or_else(|| "news".to_string());
    Some((call_id, query))
  }
}

fn latest_user_query(messages: &[ExplainMessage]) -> String {
  let raw = messages
    .iter()
    .rev()
    .find(|message| message.role == "user")
    .map(|message| message.content.trim())
    .unwrap_or("");
  if raw.is_empty() {
    return String::new();
  }
  for marker in ["[用户问题]\n", "[Question]\n"] {
    if let Some(idx) = raw.rfind(marker) {
      return raw[idx + marker.len()..].trim().to_string();
    }
  }
  raw.to_string()
}

fn query_likely_needs_web_search(messages: &[ExplainMessage]) -> bool {
  let query = latest_user_query(messages);
  if query.is_empty() {
    return false;
  }

  let lower = query.to_ascii_lowercase();
  let keywords = [
    "联网",
    "上网",
    "搜索",
    "搜一下",
    "查一下",
    "查查",
    "检索",
    "不知道就搜",
    "不确定就搜",
    "最新",
    "今天",
    "今日",
    "昨天",
    "明天",
    "刚刚",
    "实时",
    "当前",
    "目前",
    "最近",
    "近期",
    "本周",
    "本月",
    "今年",
    "新闻",
    "热搜",
    "天气",
    "股价",
    "汇率",
    "比分",
    "赛程",
    "价格",
    "行情",
    "web search",
    "search web",
    "browse",
    "look up",
    "google",
    "latest",
    "today",
    "current",
    "currently",
    "now",
    "recent",
    "recently",
    "news",
    "weather",
    "stock",
    "price",
    "score",
    "schedule",
    "exchange rate",
    "up-to-date",
    "update",
  ];
  keywords.iter().any(|keyword| query.contains(keyword) || lower.contains(keyword))
}

fn tavily_tool_arguments(query: &str) -> String {
  serde_json::json!({ "query": query }).to_string()
}

fn append_tavily_tool_result(
  body: &mut serde_json::Value,
  first_response: &serde_json::Value,
  call_id: &str,
  query: &str,
  search_result: &str,
  use_responses: bool,
  use_messages: bool,
) {
  if use_responses {
    let mut new_input = body
      .get("input")
      .and_then(|v| v.as_array())
      .cloned()
      .unwrap_or_default();
    let mut replayed_tool_call = false;
    if let Some(output) = first_response.get("output").and_then(|v| v.as_array()) {
      for item in output {
        if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
          new_input.push(item.clone());
          replayed_tool_call = true;
        }
      }
    }
    if !replayed_tool_call {
      new_input.push(serde_json::json!({
        "type": "function_call",
        "name": "tavily_web_search",
        "call_id": call_id,
        "arguments": tavily_tool_arguments(query)
      }));
    }
    new_input.push(serde_json::json!({
      "type": "function_call_output",
      "call_id": call_id,
      "output": search_result
    }));
    if let Some(obj) = body.as_object_mut() {
      obj.remove("previous_response_id");
    }
    body["input"] = serde_json::json!(new_input);
    return;
  }

  if use_messages {
    let mut new_msgs = body
      .get("messages")
      .and_then(|v| v.as_array())
      .cloned()
      .unwrap_or_default();
    let assistant_content = first_response
      .get("content")
      .and_then(|v| v.as_array())
      .filter(|items| !items.is_empty())
      .map(|items| serde_json::json!(items))
      .unwrap_or_else(|| {
        serde_json::json!([{
          "type": "tool_use",
          "id": call_id,
          "name": "tavily_web_search",
          "input": { "query": query }
        }])
      });
    new_msgs.push(serde_json::json!({
      "role": "assistant",
      "content": assistant_content
    }));
    new_msgs.push(serde_json::json!({
      "role": "user",
      "content": [{
        "type": "tool_result",
        "tool_use_id": call_id,
        "content": search_result
      }]
    }));
    body["messages"] = serde_json::json!(new_msgs);
    return;
  }

  let mut new_msgs = body
    .get("messages")
    .and_then(|v| v.as_array())
    .cloned()
    .unwrap_or_default();
  let mut assistant_message = first_response
    .get("choices")
    .and_then(|c| c.get(0))
    .and_then(|c| c.get("message"))
    .cloned()
    .unwrap_or_else(|| {
      serde_json::json!({
        "role": "assistant",
        "tool_calls": [{
          "id": call_id,
          "type": "function",
          "function": {
            "name": "tavily_web_search",
            "arguments": tavily_tool_arguments(query)
          }
        }]
      })
    });
  if let Some(obj) = assistant_message.as_object_mut() {
    obj.insert("role".to_string(), serde_json::json!("assistant"));
  }
  new_msgs.push(assistant_message);
  new_msgs.push(serde_json::json!({
    "role": "tool",
    "tool_call_id": call_id,
    "content": search_result
  }));
  body["messages"] = serde_json::json!(new_msgs);
}

fn apply_responses_reasoning(body: &mut serde_json::Value, thinking_enabled: bool, reasoning_effort: Option<&str>) {
  if !thinking_enabled {
    body["reasoning"] = serde_json::json!({ "effort": "low" });
  } else if let Some(effort) = reasoning_effort.filter(|e| !e.is_empty()) {
    body["reasoning"] = serde_json::json!({ "effort": effort, "summary": "auto" });
  }
}

fn chat_content_to_responses_content(
  role: &str,
  content: &serde_json::Value,
) -> Vec<serde_json::Value> {
  if let Some(text) = content.as_str() {
    return vec![responses_text_content_for_role(role, text)];
  }

  let mut items = Vec::new();
  if let Some(array) = content.as_array() {
    for item in array {
      let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
      match item_type {
        "text" | "input_text" | "output_text" => {
          if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
            items.push(responses_text_content_for_role(role, text));
          }
        }
        "image_url" | "input_image" => {
          if role == "assistant" {
            continue;
          }
          let url = item
            .get("image_url")
            .and_then(|v| {
              v.as_str()
                .or_else(|| v.get("url").and_then(|url| url.as_str()))
            })
            .or_else(|| item.get("image_url").and_then(|v| v.get("image_url")).and_then(|v| v.as_str()))
            .or_else(|| item.get("url").and_then(|v| v.as_str()));
          if let Some(url) = url {
            items.push(responses_input_image(url));
          }
        }
        _ => {}
      }
    }
  }
  items
}

fn chat_messages_to_responses_input(messages: &serde_json::Value) -> serde_json::Value {
  let input = messages
    .as_array()
    .map(|items| {
      items
        .iter()
        .filter_map(|message| {
          let role = message
            .get("role")
            .and_then(|v| v.as_str())
            .map(responses_role)
            .unwrap_or("user");
          let content = chat_content_to_responses_content(
            role,
            message.get("content").unwrap_or(&serde_json::Value::Null),
          );
          if content.is_empty() {
            None
          } else {
            Some(serde_json::json!({ "role": role, "content": content }))
          }
        })
        .collect::<Vec<_>>()
    })
    .unwrap_or_default();
  serde_json::json!(input)
}

fn claude_text_content(text: impl Into<String>) -> serde_json::Value {
  serde_json::json!({ "type": "text", "text": text.into() })
}

fn claude_image_content(url: &str) -> Option<serde_json::Value> {
  let rest = url.strip_prefix("data:")?;
  let (media_type, data) = rest.split_once(";base64,")?;
  Some(serde_json::json!({
    "type": "image",
    "source": {
      "type": "base64",
      "media_type": media_type,
      "data": data
    }
  }))
}

fn chat_content_to_claude_content(content: &serde_json::Value) -> Vec<serde_json::Value> {
  if let Some(text) = content.as_str() {
    return vec![claude_text_content(text)];
  }

  let mut items = Vec::new();
  if let Some(array) = content.as_array() {
    for item in array {
      let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
      match item_type {
        "text" | "input_text" | "output_text" => {
          if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
            items.push(claude_text_content(text));
          }
        }
        "image_url" | "input_image" => {
          let url = item
            .get("image_url")
            .and_then(|v| {
              v.as_str()
                .or_else(|| v.get("url").and_then(|url| url.as_str()))
            })
            .or_else(|| item.get("url").and_then(|v| v.as_str()));
          if let Some(image) = url.and_then(claude_image_content) {
            items.push(image);
          }
        }
        "image" | "tool_use" | "tool_result" | "thinking" | "redacted_thinking" => {
          items.push(item.clone());
        }
        _ => {}
      }
    }
  }
  items
}

fn chat_messages_to_claude_body(
  model: &str,
  messages: &serde_json::Value,
  max_tokens: u64,
  temperature: Option<f64>,
  stream: bool,
  thinking_enabled: bool,
) -> serde_json::Value {
  let mut system_parts = Vec::new();
  let mut claude_messages = Vec::new();
  if let Some(items) = messages.as_array() {
    for message in items {
      let role = message.get("role").and_then(|v| v.as_str()).unwrap_or("user");
      let content_value = message.get("content").unwrap_or(&serde_json::Value::Null);
      if matches!(role, "system" | "developer") {
        if let Some(text) = content_value.as_str() {
          system_parts.push(text.to_string());
        } else {
          let text = chat_content_to_claude_content(content_value)
            .into_iter()
            .filter_map(|item| item.get("text").and_then(|v| v.as_str()).map(str::to_string))
            .collect::<Vec<_>>()
            .join("\n");
          if !text.trim().is_empty() {
            system_parts.push(text);
          }
        }
        continue;
      }

      let content = chat_content_to_claude_content(content_value);
      if content.is_empty() {
        continue;
      }
      claude_messages.push(serde_json::json!({
        "role": if role == "assistant" { "assistant" } else { "user" },
        "content": content
      }));
    }
  }

  let mut body = serde_json::json!({
    "model": model,
    "messages": claude_messages,
    "max_tokens": max_tokens,
  });
  if !system_parts.is_empty() {
    body["system"] = serde_json::json!(system_parts.join("\n\n"));
  }
  if thinking_enabled && max_tokens > 1024 {
    body["thinking"] = serde_json::json!({
      "type": "enabled",
      "budget_tokens": 1024
    });
  } else if let Some(value) = temperature {
    body["temperature"] = serde_json::json!(value);
  }
  if stream {
    body["stream"] = serde_json::json!(true);
  }
  body
}

fn claude_output_text(value: &serde_json::Value) -> Option<String> {
  let text = value
    .get("content")
    .and_then(|v| v.as_array())
    .map(|parts| {
      parts
        .iter()
        .filter(|part| part.get("type").and_then(|v| v.as_str()).unwrap_or("") == "text")
        .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
        .collect::<String>()
    })
    .unwrap_or_default();
  if text.trim().is_empty() { None } else { Some(text) }
}

fn parse_claude_output_text(
  raw: &str,
  value: &serde_json::Value,
  label: &str,
) -> Result<String, String> {
  claude_output_text(value)
    .map(|s| s.trim().to_string())
    .ok_or_else(|| format!("Invalid {label} response: {}", raw.chars().take(500).collect::<String>()))
}

fn response_output_text(value: &serde_json::Value) -> Option<String> {
  if let Some(text) = value.get("output_text").and_then(|v| v.as_str()) {
    return Some(text.to_string());
  }
  let mut out = String::new();
  if let Some(items) = value.get("output").and_then(|v| v.as_array()) {
    for item in items {
      if item.get("type").and_then(|v| v.as_str()) != Some("message") {
        continue;
      }
      if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
        for part in content {
          let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
          if !part_type.is_empty() && part_type != "output_text" && part_type != "text" {
            continue;
          }
          if let Some(text) = part
            .get("text")
            .or_else(|| part.get("output_text"))
            .and_then(|v| v.as_str())
          {
            out.push_str(text);
          }
        }
      }
    }
  }
  if out.trim().is_empty() { None } else { Some(out) }
}

fn parse_response_output_text(
  raw: &str,
  value: &serde_json::Value,
  label: &str,
) -> Result<String, String> {
  response_output_text(value)
    .map(|s| s.trim().to_string())
    .ok_or_else(|| format!("Invalid {label} response: {}", raw.chars().take(500).collect::<String>()))
}

// ===== Provider 凭据 =====

/// 供应商连接输入参数，用于测试连接或获取模型列表时临时传入
/// api_keys 优先；api_key 为兼容旧前端发的单 key 字段（v2.3.x 时的 ProviderConnectionInput）
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConnectionInput {
  pub id: Option<String>,
  pub base_url: String,
  #[serde(default)]
  pub api_keys: Vec<String>,
  #[serde(default)]
  pub api_key: Option<String>,
}

impl ProviderConnectionInput {
  /// 整理出非空 key 列表：优先 api_keys，回退到 api_key。
  pub fn merged_keys(&self) -> Vec<String> {
    let mut keys: Vec<String> = self
      .api_keys
      .iter()
      .map(|k| k.trim().to_string())
      .filter(|k| !k.is_empty())
      .collect();
    if keys.is_empty() {
      if let Some(legacy) = self.api_key.as_deref() {
        let trimmed = legacy.trim().to_string();
        if !trimmed.is_empty() {
          keys.push(trimmed);
        }
      }
    }
    keys
  }
}

/// 解析供应商的凭据信息（base_url + 多 key 列表）
/// 优先使用传入的 ProviderConnectionInput（如测试连接时），否则从 settings 中查找对应的供应商
pub fn resolve_provider_credentials(
  settings: &Settings,
  provider_id: &str,
  provider: Option<ProviderConnectionInput>,
) -> Result<(String, Vec<String>), String> {
  if let Some(input) = provider {
    let id_matches = input
      .id
      .as_ref()
      .map(|id| id.is_empty() || id == provider_id)
      .unwrap_or(true);

    if id_matches {
      return Ok((input.base_url.clone(), input.merged_keys()));
    }
  }

  let provider = settings
    .get_provider(provider_id)
    .ok_or_else(|| "Provider not found".to_string())?;
  Ok((provider.base_url.clone(), provider.api_keys.clone()))
}

/// 构建 HTTP 客户端，设置 60 秒超时
pub fn build_http_client() -> Client {
  Client::builder()
    .connect_timeout(Duration::from_secs(30))
    .read_timeout(Duration::from_secs(300))
    .build()
    .unwrap_or_else(|err| {
      eprintln!("Failed to build HTTP client: {err}");
      Client::new()
    })
}

// ===== Retry / Failover =====

/// 重试延迟基础值（毫秒）
const RETRY_BASE_DELAY_MS: u64 = 500;
/// 重试延迟最大值（毫秒）
const RETRY_MAX_DELAY_MS: u64 = 10_000;

/// 获取实际的重试次数
/// 如果重试功能被禁用，则返回 1（即只尝试一次）
pub fn effective_retry_attempts(settings: &Settings) -> usize {
  if settings.retry_enabled {
    settings.retry_attempts as usize
  } else {
    1
  }
}

/// 从响应头中解析 Retry-After 值（秒），转换为毫秒延迟
fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
  headers
    .get("retry-after")
    .and_then(|value| value.to_str().ok())
    .and_then(|value| value.parse::<u64>().ok())
}

/// 判断 HTTP 状态码是否可重试
/// 包括 429（限流）和所有服务器错误（5xx）
fn is_retryable_status(status: StatusCode) -> bool {
  status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// 判断请求错误是否可重试
/// 包括超时和连接错误
fn is_retryable_error(error: &reqwest::Error) -> bool {
  error.is_timeout() || error.is_connect()
}

/// 计算重试延迟
/// 优先使用服务器返回的 Retry-After 头；否则使用指数退避策略
fn retry_delay_ms(attempt: usize, retry_after: Option<u64>) -> u64 {
  if let Some(seconds) = retry_after {
    return seconds.saturating_mul(1000);
  }

  let delay = RETRY_BASE_DELAY_MS.saturating_mul(2u64.saturating_pow((attempt - 1) as u32));
  delay.min(RETRY_MAX_DELAY_MS)
}

fn parse_leading_status_code(value: &str) -> Option<u16> {
  let end = value
    .find(|c: char| !c.is_ascii_digit())
    .unwrap_or(value.len());
  if end == 0 {
    return None;
  }
  value[..end].parse().ok()
}

/// 从 HTTP 错误信息中提取状态码
/// 格式约定：`"{label} Error: {status} - {body}"`，
/// status 形如 `"429 Too Many Requests"`，第一段数字即可
/// 兼容少数防御性分支使用的 `"{label} HTTP {status}: {body}"`。
/// 网络错误（reqwest::Error 路径）格式为 `"{label} Error: <reqwest msg>"`，无前导数字 → 返回 None
pub fn extract_status_code(err_msg: &str) -> Option<u16> {
  if let Some(idx) = err_msg.find(" Error: ") {
    let rest = &err_msg[idx + " Error: ".len()..];
    if let Some(code) = parse_leading_status_code(rest) {
      return Some(code);
    }
  }

  if let Some(idx) = err_msg.find(" HTTP ") {
    let rest = &err_msg[idx + " HTTP ".len()..];
    return parse_leading_status_code(rest);
  }

  None
}

/// 判断错误信息是否触发 key failover
/// 严格按 HTTP 状态码：401/402/403/429 才换 key —— 与 key 直接相关的错误：
/// - 401 鉴权失败（key 被吊销 / 错误）
/// - 402 需要付费（账户欠费）
/// - 403 权限不足 / 被封禁
/// - 429 限流（key 维度配额耗尽）
/// 其它 4xx（如 400 malformed body）属于请求本身问题，换 key 也无济于事 → 不触发
/// 5xx 由 send_with_retry 内部退避重试，不会到这里
/// 网络错误（timeout / connect 失败）非 key 问题，extract_status_code 返回 None → 不触发
pub fn is_failover_error(err_msg: &str) -> bool {
  matches!(extract_status_code(err_msg), Some(401 | 402 | 403 | 429))
}

fn is_failover_status(status: StatusCode) -> bool {
  matches!(status.as_u16(), 401 | 402 | 403 | 429)
}

fn is_retryable_status_for_failover(status: StatusCode) -> bool {
  is_retryable_status(status) && !is_failover_status(status)
}

/// 多 key failover 包装：在 api_keys 列表上依次尝试，遇到 failover-eligible 错误自动切下一 key
/// 内层每次尝试仍走 send_with_retry_for_failover（处理网络抖动 / 服务端 5xx 等通用重试；
/// 401/402/403/429 立即交回外层换 key，避免在同一 key 上耗尽重试次数）。
pub async fn send_with_failover<F, Fut>(
  state: &AppState,
  label: &str,
  attempts: usize,
  provider_id: &str,
  api_keys: &[String],
  send: F,
) -> Result<reqwest::Response, String>
where
  F: Fn(&str) -> Fut,
  Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
  let total = api_keys.len();
  if total == 0 {
    return Err(format!("{} Error: No API key configured", label));
  }

  let mut tried: HashSet<usize> = HashSet::new();
  let mut last_err: Option<String> = None;

  while tried.len() < total {
    let idx = match state.pick_active_key(provider_id, total, &tried) {
      Some(i) => i,
      None => break,
    };
    tried.insert(idx);
    let key = api_keys[idx].as_str();

    match send_with_retry_for_failover(label, attempts, || send(key)).await {
      Ok(resp) => {
        state.mark_key_ok(provider_id, idx);
        return Ok(resp);
      }
      Err(err_msg) => {
        if is_failover_error(&err_msg) && tried.len() < total {
          state.mark_key_failed(provider_id, idx);
          eprintln!(
            "[failover] {} key #{}/{} failed, switching to next: {}",
            label,
            idx + 1,
            total,
            err_msg
          );
          last_err = Some(err_msg);
          continue;
        }
        // 非 failover 错误（或已穷举所有 key）→ 直接返回
        if is_failover_error(&err_msg) {
          state.mark_key_failed(provider_id, idx);
        }
        return Err(err_msg);
      }
    }
  }

  Err(
    last_err.unwrap_or_else(|| format!("{} Error: all {} keys exhausted", label, total)),
  )
}

/// 带重试机制的 HTTP 发送函数
/// 对可重试的错误（限流、服务器错误、超时、连接失败）进行指数退避重试
pub async fn send_with_retry<F, Fut>(
  label: &str,
  attempts: usize,
  mut send: F,
) -> Result<reqwest::Response, String>
where
  F: FnMut() -> Fut,
  Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
  send_with_retry_status_policy(label, attempts, &mut send, is_retryable_status).await
}

async fn send_with_retry_for_failover<F, Fut>(
  label: &str,
  attempts: usize,
  mut send: F,
) -> Result<reqwest::Response, String>
where
  F: FnMut() -> Fut,
  Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
  send_with_retry_status_policy(label, attempts, &mut send, is_retryable_status_for_failover).await
}

async fn send_with_retry_status_policy<F, Fut, P>(
  label: &str,
  attempts: usize,
  send: &mut F,
  should_retry_status: P,
) -> Result<reqwest::Response, String>
where
  F: FnMut() -> Fut,
  Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
  P: Fn(StatusCode) -> bool,
{
  let attempts = attempts.max(1);
  let mut last_error: Option<String> = None;

  for attempt in 1..=attempts {
    match send().await {
      Ok(response) => {
        let status = response.status();
        if status.is_success() {
          return Ok(response);
        }

        let retry_after = parse_retry_after(response.headers());
        let text = response.text().await.unwrap_or_default();
        let err_msg = format!("{} Error: {} - {}", label, status, text);

        if should_retry_status(status) && attempt < attempts {
          last_error = Some(err_msg);
          let delay = retry_delay_ms(attempt, retry_after);
          eprintln!("{} retrying in {}ms (attempt {}/{})", label, delay, attempt, attempts);
          tokio::time::sleep(Duration::from_millis(delay)).await;
          continue;
        }

        return Err(format!("{} (attempt {}/{})", err_msg, attempt, attempts));
      }
      Err(err) => {
        let err_msg = format!("{} Error: {}", label, err);
        if is_retryable_error(&err) && attempt < attempts {
          last_error = Some(err_msg);
          let delay = retry_delay_ms(attempt, None);
          eprintln!("{} retrying in {}ms (attempt {}/{})", label, delay, attempt, attempts);
          tokio::time::sleep(Duration::from_millis(delay)).await;
          continue;
        }
        return Err(format!("{} (attempt {}/{})", err_msg, attempt, attempts));
      }
    }
  }

  Err(last_error.map(|msg| {
    format!("{} (attempt {}/{})", msg, attempts, attempts)
  }).unwrap_or_else(|| {
    format!("{} Error: exceeded retry attempts ({})", label, attempts)
  }))
}

// ===== Chat completion 调用 =====

/// 调用 OpenAI 兼容的文本聊天接口
/// 发送单轮 user 消息，temperature 设为 0.2,返回模型生成的文本内容
pub async fn call_openai_text(
  state: &State<'_, AppState>,
  config: &settings::ModelProvider,
  model: &str,
  prompt: String,
  retry_attempts: usize,
  thinking_enabled: bool,
) -> Result<String, String> {
  // Apple Intelligence(端上)路由：跳过 HTTP，直接调 sidecar。model/retry/thinking 三个参数全部忽略。
  if config.base_url == APPLE_INTELLIGENCE_BASE_URL {
    let _ = (model, retry_attempts, thinking_enabled);
    return state.apple_intelligence.call_text(&prompt).await;
  }
  if model.trim().is_empty() {
    return Err("Please select a model first".to_string());
  }
  match provider_endpoint_kind(&config.base_url) {
    ModelEndpointKind::Responses => {
    let url = responses_api_url(&config.base_url);
    let mut body = serde_json::json!({
      "model": model,
      "input": [responses_text_message("user", prompt.clone())],
      "max_output_tokens": 2000
    });
    apply_responses_reasoning(&mut body, thinking_enabled, config.reasoning_effort.as_deref());

    let response = send_with_failover(
      state,
      "OpenAI Responses",
      retry_attempts,
      &config.id,
      &config.api_keys,
      |key| {
        state
          .http
          .post(url.clone())
          .bearer_auth(key)
          .json(&body)
          .send()
      },
    )
    .await?;
    let raw = response.text().await.map_err(|e| format!("OpenAI Responses read body: {e}"))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
      .map_err(|e| format!("OpenAI Responses parse JSON: {} (body: {})", e, raw.chars().take(500).collect::<String>()))?;
    return parse_response_output_text(&raw, &value, "OpenAI Responses");
    }
    ModelEndpointKind::Messages => {
      let url = messages_api_url(&config.base_url);
      let messages = serde_json::json!([{ "role": "user", "content": prompt }]);
      let body = chat_messages_to_claude_body(model, &messages, 2000, Some(0.2), false, thinking_enabled);

      let response = send_with_failover(
        state,
        "Claude Messages",
        retry_attempts,
        &config.id,
        &config.api_keys,
        |key| {
          claude_auth_headers(state.http.post(url.clone()), key)
            .json(&body)
            .send()
        },
      )
      .await?;
      let raw = response.text().await.map_err(|e| format!("Claude Messages read body: {e}"))?;
      let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("Claude Messages parse JSON: {} (body: {})", e, raw.chars().take(500).collect::<String>()))?;
      return parse_claude_output_text(&raw, &value, "Claude Messages");
    }
    _ => {}
  }

  let url = chat_completions_url(&config.base_url);
  let mut body = serde_json::json!({
    "model": model,
    "messages": [{ "role": "user", "content": prompt }],
    "temperature": 0.2
  });
  if !thinking_enabled {
    body["thinking"] = serde_json::json!({ "type": "disabled" });
  }

  let response = send_with_failover(
    state,
    "OpenAI API",
    retry_attempts,
    &config.id,
    &config.api_keys,
    |key| {
      state
        .http
        .post(url.clone())
        .bearer_auth(key)
        .json(&body)
        .send()
    },
  )
  .await?;

  let value: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
  let content = value
    .get("choices")
    .and_then(|choices| choices.get(0))
    .and_then(|choice| choice.get("message"))
    .and_then(|message| message.get("content"))
    .and_then(|content| content.as_str())
    .ok_or_else(|| "Invalid response".to_string())?;

  Ok(content.trim().to_string())
}

/// 调用 OpenAI 兼容的 OCR/视觉接口
/// 将图片转为 Base64 后作为 image_url 类型内容发送，temperature 设为 0 以提高识别稳定性
pub async fn call_openai_ocr(
  state: &State<'_, AppState>,
  config: &settings::ModelProvider,
  model: &str,
  image_path: &Path,
  prompt: &str,
  retry_attempts: usize,
  thinking_enabled: bool,
) -> Result<String, String> {
  if config.base_url == APPLE_INTELLIGENCE_BASE_URL {
    let _ = (state, model, image_path, prompt, retry_attempts, thinking_enabled);
    return Err("Apple Intelligence 暂不支持图像输入,请为截图/视觉功能配置云端 provider".into());
  }
  if model.trim().is_empty() {
    return Err("Please select a model first".to_string());
  }
  let bytes = fs::read(image_path).map_err(|e| e.to_string())?;
  let base64 = general_purpose::STANDARD.encode(bytes);
  match provider_endpoint_kind(&config.base_url) {
    ModelEndpointKind::Responses => {
    let url = responses_api_url(&config.base_url);
    let mut body = serde_json::json!({
      "model": model,
      "input": [
        {
          "role": "user",
          "content": [
            responses_input_image(format!("data:image/png;base64,{base64}")),
            responses_input_text(prompt)
          ]
        }
      ],
      "max_output_tokens": 2000
    });
    apply_responses_reasoning(&mut body, thinking_enabled, config.reasoning_effort.as_deref());

    let response = send_with_failover(
      state,
      "OpenAI OCR",
      retry_attempts,
      &config.id,
      &config.api_keys,
      |key| {
        state
          .http
          .post(url.clone())
          .bearer_auth(key)
          .json(&body)
          .send()
      },
    )
    .await?;

    let status = response.status();
    if !status.is_success() {
      let body_text = response.text().await.unwrap_or_default();
      let snippet: String = body_text.chars().take(500).collect();
      return Err(format!("OCR HTTP {}: {}", status.as_u16(), snippet));
    }
    let raw = response.text().await.map_err(|e| format!("OCR read body: {}", e))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
      .map_err(|e| format!("OCR parse JSON: {} (body: {})", e, raw.chars().take(500).collect::<String>()))?;
    return parse_response_output_text(&raw, &value, "OCR");
    }
    ModelEndpointKind::Messages => {
      let url = messages_api_url(&config.base_url);
      let messages = serde_json::json!([
        {
          "role": "user",
          "content": [
            { "type": "image_url", "image_url": { "url": format!("data:image/png;base64,{base64}") } },
            { "type": "text", "text": prompt }
          ]
        }
      ]);
      let body = chat_messages_to_claude_body(model, &messages, 2000, Some(0.2), false, thinking_enabled);

      let response = send_with_failover(
        state,
        "Claude OCR",
        retry_attempts,
        &config.id,
        &config.api_keys,
        |key| {
          claude_auth_headers(state.http.post(url.clone()), key)
            .json(&body)
            .send()
        },
      )
      .await?;

      let status = response.status();
      if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let snippet: String = body_text.chars().take(500).collect();
        return Err(format!("OCR HTTP {}: {}", status.as_u16(), snippet));
      }
      let raw = response.text().await.map_err(|e| format!("OCR read body: {}", e))?;
      let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("OCR parse JSON: {} (body: {})", e, raw.chars().take(500).collect::<String>()))?;
      return parse_claude_output_text(&raw, &value, "OCR");
    }
    _ => {}
  }

  let url = chat_completions_url(&config.base_url);

  // 与 lens 的 vision body 对齐：image 在 text 前、显式 max_tokens。
  // thinking 按调用方传入：截图翻译默认 false（节省时间），lens 默认 true。
  let mut body = serde_json::json!({
    "model": model,
    "messages": [
      {
        "role": "user",
        "content": [
          {
            "type": "image_url",
            "image_url": { "url": format!("data:image/png;base64,{base64}") }
          },
          {
            "type": "text",
            "text": prompt
          }
        ]
      }
    ],
    "temperature": 0.2,
    "max_tokens": 2000
  });
  if !thinking_enabled {
    body["thinking"] = serde_json::json!({ "type": "disabled" });
  }

  let response = send_with_failover(
    state,
    "OpenAI OCR",
    retry_attempts,
    &config.id,
    &config.api_keys,
    |key| {
      state
        .http
        .post(url.clone())
        .bearer_auth(key)
        .json(&body)
        .send()
    },
  )
  .await?;

  // 显式检查 HTTP 状态：非 2xx 把原始 body 文本带回，避免后续 .json() 抛出含糊的 "error decoding response body"
  let status = response.status();
  if !status.is_success() {
    let body_text = response.text().await.unwrap_or_default();
    let snippet: String = body_text.chars().take(500).collect();
    return Err(format!("OCR HTTP {}: {}", status.as_u16(), snippet));
  }

  let raw = response.text().await.map_err(|e| format!("OCR read body: {}", e))?;
  let value: serde_json::Value = serde_json::from_str(&raw)
    .map_err(|e| format!("OCR parse JSON: {} (body: {})", e, raw.chars().take(500).collect::<String>()))?;
  let content = value
    .get("choices")
    .and_then(|choices| choices.get(0))
    .and_then(|choice| choice.get("message"))
    .and_then(|message| message.get("content"))
    .and_then(|content| content.as_str())
    .ok_or_else(|| format!("Invalid OCR response: {}", raw.chars().take(500).collect::<String>()))?;

  Ok(content.trim().to_string())
}

/// 调用视觉 API（截图解释 / Lens 共用）
/// 支持流式输出：如果 stream 为 true，通过 stream_vision_response 逐段 emit `event_name` 事件。
/// `provider_id_override` 非空时使用指定 provider/model（用于 lens 选择独立模型）；空则走 explain 配置。
#[allow(clippy::too_many_arguments)]
pub async fn call_vision_api(
  app: &AppHandle,
  state: &State<'_, AppState>,
  image_id: &str,
  messages: Vec<ExplainMessage>,
  language: &str,
  retry_attempts: usize,
  stream: bool,
  stream_kind: &str,
  event_name: &str,
  provider_id_override: Option<&str>,
  model_override: Option<&str>,
  system_prompt_override: Option<&str>,
  thinking_enabled: bool,
) -> Result<String, String> {
  let settings = state.settings_read().clone();
  let provider_id = provider_id_override
    .filter(|s| !s.is_empty())
    .unwrap_or(&settings.translator_provider_id);
  let provider = settings.get_provider(provider_id)
    .ok_or_else(|| "Vision provider not found".to_string())?;

  // image_id 为空 → 走纯文本对话路径（不附图）
  let has_image = !image_id.is_empty();

  // 优先用调用方传入的 system_prompt_override；否则用默认模板（区分有/无图片）
  // 关闭思考时在 system 末尾追加显式禁止指令，作为参数层不生效时的兜底
  let system_prompt_to_use = {
    let base = match system_prompt_override.filter(|s| !s.is_empty()) {
      Some(s) => s.to_string(),
      None => default_system_prompt(language, has_image),
    };
    if !thinking_enabled {
      format!("{}{}", base, no_think_instruction(language))
    } else {
      base
    }
  };

  if provider.base_url == APPLE_INTELLIGENCE_BASE_URL {
    if has_image {
      return Err("Apple Intelligence 暂不支持图像输入,请为 Lens / 截图视觉功能配置云端 provider".into());
    }
    let prompt = build_apple_text_prompt(&system_prompt_to_use, &messages);
    if stream {
      return stream_apple_text_response(
        app,
        state,
        &prompt,
        image_id,
        stream_kind,
        event_name,
      )
      .await;
    }
    return state.apple_intelligence.call_text(&prompt).await;
  }

  let mut api_messages = Vec::new();
  api_messages.push(serde_json::json!({
    "role": "system",
    "content": system_prompt_to_use
  }));

  if has_image {
    let image_path = resolve_explain_image_path(app, state, image_id)?;
    let bytes = fs::read(image_path).map_err(|e| e.to_string())?;
    let base64 = general_purpose::STANDARD.encode(bytes);
    if let Some(first) = messages.first() {
      api_messages.push(serde_json::json!({
        "role": "user",
        "content": [
          { "type": "image_url", "image_url": { "url": format!("data:image/png;base64,{base64}") } },
          { "type": "text", "text": first.content }
        ]
      }));
      for message in messages.iter().skip(1) {
        api_messages.push(serde_json::json!({
          "role": message.role,
          "content": message.content
        }));
      }
    }
  } else {
    // 纯文本：每条 message 直接 push（无图）
    for message in messages.iter() {
      api_messages.push(serde_json::json!({
        "role": message.role,
        "content": message.content,
      }));
    }
  }

  let model = model_override
    .filter(|s| !s.is_empty())
    .unwrap_or(&settings.translator_model);
  if model.trim().is_empty() {
    return Err("Please select a model first".to_string());
  }
  let endpoint_kind = provider_endpoint_kind(&provider.base_url);
  let use_responses = endpoint_kind == ModelEndpointKind::Responses;
  let use_messages = endpoint_kind == ModelEndpointKind::Messages;
  let url = match endpoint_kind {
    ModelEndpointKind::Responses => responses_api_url(&provider.base_url),
    ModelEndpointKind::Messages => messages_api_url(&provider.base_url),
    _ => chat_completions_url(&provider.base_url),
  };
  let mut body = serde_json::json!({
    "model": model,
    "messages": api_messages,
    "temperature": 0.7,
    "max_tokens": 2000
  });
  let tavily_key = settings.tavily_api_key.clone();
  let use_tavily = !tavily_key.is_empty() && query_likely_needs_web_search(&messages);
  if use_responses {
    let input = chat_messages_to_responses_input(&body["messages"]);
    body = serde_json::json!({
      "model": model,
      "input": input,
      "max_output_tokens": 2000
    });
    apply_responses_reasoning(&mut body, thinking_enabled, provider.reasoning_effort.as_deref());
  } else if use_messages {
    body = chat_messages_to_claude_body(model, &body["messages"], 2000, Some(0.7), stream, thinking_enabled);
  }
  // Tavily function tool: 所有端点 + web search 开启 + tavily key 存在时注入
  if use_tavily {
    if use_responses {
      // Responses API: 注入为 function tool
      let existing = body.get("tools").and_then(|v| v.as_array()).cloned().unwrap_or_default();
      let mut tools = existing;
      tools.push(serde_json::json!({
        "type": "function",
        "name": "tavily_web_search",
        "description": "Search the web for real-time information. Use this when the user asks about current events, news, recent or changing facts, or when you are unsure and need sources before answering.",
        "parameters": {
          "type": "object",
          "properties": {
            "query": { "type": "string", "description": "The search query" }
          },
          "required": ["query"]
        }
      }));
      body["tools"] = serde_json::json!(tools);
      body["tool_choice"] = serde_json::json!("auto");
    } else if use_messages {
      // Messages API: 注入为 Claude tool
      let existing = body.get("tools").and_then(|v| v.as_array()).cloned().unwrap_or_default();
      let mut tools = existing;
      tools.push(serde_json::json!({
        "name": "tavily_web_search",
        "description": "Search the web for real-time information. Use this when the user asks about current events, news, recent or changing facts, or when you are unsure and need sources before answering.",
        "input_schema": {
          "type": "object",
          "properties": {
            "query": { "type": "string", "description": "The search query" }
          },
          "required": ["query"]
        }
      }));
      body["tools"] = serde_json::json!(tools);
    } else {
      // Chat Completions: 注入为 OpenAI function tool
      let existing = body.get("tools").and_then(|v| v.as_array()).cloned().unwrap_or_default();
      let mut tools = existing;
      tools.push(tavily_search_tool_def());
      body["tools"] = serde_json::json!(tools);
      body["tool_choice"] = serde_json::json!("auto");
    }
  }
  if stream {
    body["stream"] = serde_json::json!(true);
  }

  // Tavily: 第一次请求不流式，检测是否需要搜索
  if use_tavily {
    let mut first_body = body.clone();
    if let Some(obj) = first_body.as_object_mut() {
      obj.remove("stream");
    }
    let first_response = send_with_failover(
      state,
      "Vision API",
      retry_attempts,
      &provider.id,
      &provider.api_keys,
      |key| {
        let request = state.http.post(url.clone());
        let request = if use_messages {
          claude_auth_headers(request, key)
        } else {
          request.bearer_auth(key)
        };
        request.json(&first_body).send()
      },
    )
    .await?;
    let first_status = first_response.status();
    if !first_status.is_success() {
      let err_text = first_response.text().await.unwrap_or_default();
      return Err(format!("Vision API HTTP {}: {}", first_status.as_u16(), err_text.chars().take(500).collect::<String>()));
    }
    let first_raw = first_response.text().await.unwrap_or_default();
    let first_val: serde_json::Value = serde_json::from_str(&first_raw)
      .map_err(|e| format!("Vision API parse JSON: {} (body: {})", e, first_raw.chars().take(500).collect::<String>()))?;
    if let Some((call_id, query)) = extract_tavily_tool_call_query(&first_val, use_responses, use_messages) {
        // 模型要搜索 — 调 Tavily，注入结果，再流式发第二次
        if stream {
          let _ = app.emit(event_name, serde_json::json!({
            "imageId": image_id, "kind": stream_kind, "delta": "", "searchQuery": query,
          }));
        }
        let search_result = tavily_search(&state.http, &tavily_key, &query).await
          .unwrap_or_else(|e| format!("Search failed: {e}"));
        if stream {
          let _ = app.emit(event_name, serde_json::json!({
            "imageId": image_id, "kind": stream_kind, "delta": "", "searchResults": search_result,
          }));
        }
        // 注入 tool result 到 body，去掉 tools
        append_tavily_tool_result(
          &mut body,
          &first_val,
          &call_id,
          &query,
          &search_result,
          use_responses,
          use_messages,
        );
        if let Some(obj) = body.as_object_mut() {
          obj.remove("tools");
          obj.remove("tool_choice");
        }
        // 第二次请求继续走下面的正常流式路径
      } else {
        // 模型不需要搜索 — 直接 emit 非流式结果
        let text = if use_responses {
          parse_response_output_text(&first_raw, &first_val, "Vision API")?
        } else if use_messages {
          parse_claude_output_text(&first_raw, &first_val, "Vision API")?
        } else {
          first_val
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("Invalid vision response: {}", first_raw.chars().take(500).collect::<String>()))?
            .trim()
            .to_string()
        };
        // emit 完整文本作为一次性 delta
        if stream {
          let _ = app.emit(event_name, serde_json::json!({"imageId": image_id, "kind": stream_kind, "delta": text}));
          let _ = app.emit(event_name, serde_json::json!({"imageId": image_id, "kind": stream_kind, "delta": "", "done": true, "reason": "done", "full": text}));
        }
        return Ok(text);
      }
  }

  // 关闭思考模式：仅塞 DeepSeek/Kimi 官方文档约定的 thinking={type:"disabled"} 字段。
  // 不再注入 chat_template_kwargs / enable_thinking / reasoning_effort —— 这些是 vLLM/Qwen/OpenAI
  // 私有字段，第三方代理（如 OpenRouter / 反代）做严格校验时会以 400 拒绝整个请求（实测 DeepSeek
  // 路径上 chat_template_kwargs 直接报错）。提示词层的 no-think 指令是更稳的兜底。
  if !use_responses && !use_messages && !thinking_enabled {
    body["thinking"] = serde_json::json!({ "type": "disabled" });
  }

  let response = send_with_failover(
    state,
    "Vision API",
    retry_attempts,
    &provider.id,
    &provider.api_keys,
    |key| {
      let request = state.http.post(url.clone());
      let request = if use_messages {
        claude_auth_headers(request, key)
      } else {
        request.bearer_auth(key)
      };
      request.json(&body).send()
    },
  )
  .await?;

  // 先检查 HTTP 状态：非 2xx 直接读出 body 文本作为错误，避免后续 .json() / chunk() 拿到非预期格式时抛出含糊的 "error decoding response body"。
  let status = response.status();
  if !status.is_success() {
    let body_text = response.text().await.unwrap_or_default();
    let snippet = body_text.chars().take(500).collect::<String>();
    return Err(format!("Vision API HTTP {}: {}", status.as_u16(), snippet));
  }

  if stream {
    // 启动新流：递增代号，存到本流持有的快照里；后续 chunk 循环只要发现全局代号 != 自己的快照就退出。
    let generation = state
      .explain_stream_generation
      .fetch_add(1, Ordering::SeqCst)
      + 1;
    if use_responses {
      return stream_responses_response(
        app,
        response,
        image_id,
        stream_kind,
        event_name,
        &state.explain_stream_generation,
        generation,
      )
      .await;
    }
    if use_messages {
      return stream_claude_response(
        app,
        response,
        image_id,
        stream_kind,
        event_name,
        &state.explain_stream_generation,
        generation,
      )
      .await;
    }
    return stream_vision_response(
      app,
      response,
      image_id,
      stream_kind,
      event_name,
      &state.explain_stream_generation,
      generation,
    )
    .await;
  }

  // 非流式：先读 raw text，再 parse JSON，把原始 body 作为错误信息便于诊断。
  let raw = response.text().await.map_err(|e| format!("Vision API read body: {}", e))?;
  let value: serde_json::Value = serde_json::from_str(&raw)
    .map_err(|e| format!("Vision API parse JSON: {} (body: {})", e, raw.chars().take(500).collect::<String>()))?;
  if use_responses {
    return parse_response_output_text(&raw, &value, "Vision API");
  }
  if use_messages {
    return parse_claude_output_text(&raw, &value, "Vision API");
  }
  let content = value
    .get("choices")
    .and_then(|choices| choices.get(0))
    .and_then(|choice| choice.get("message"))
    .and_then(|message| message.get("content"))
    .and_then(|content| content.as_str())
    .ok_or_else(|| format!("Invalid vision response: {}", raw.chars().take(500).collect::<String>()))?;

  Ok(content.trim().to_string())
}

fn build_apple_text_prompt(system_prompt: &str, messages: &[ExplainMessage]) -> String {
  let mut parts = Vec::new();
  let system_prompt = system_prompt.trim();
  if !system_prompt.is_empty() {
    parts.push(format!("System:\n{}", system_prompt));
  }

  for message in messages {
    let role = match message.role.as_str() {
      "assistant" => "Assistant",
      "system" => "System",
      _ => "User",
    };
    let content = message.content.trim();
    if !content.is_empty() {
      parts.push(format!("{}:\n{}", role, content));
    }
  }

  parts.push("Assistant:".to_string());
  parts.join("\n\n")
}

async fn stream_apple_text_response(
  app: &AppHandle,
  state: &State<'_, AppState>,
  prompt: &str,
  image_id: &str,
  kind: &str,
  event_name: &str,
) -> Result<String, String> {
  let generation = state
    .explain_stream_generation
    .fetch_add(1, Ordering::SeqCst)
    + 1;
  let generation_atom = &state.explain_stream_generation;
  let apple = state.apple_intelligence.clone();
  let app_for_emit = app.clone();
  let image_id_for_emit = image_id.to_string();
  let kind_for_emit = kind.to_string();
  let event_name_for_emit = event_name.to_string();
  let mut full = String::new();

  let result = apple
    .stream_text(prompt, |delta| {
      if generation_atom.load(Ordering::SeqCst) != generation {
        return;
      }
      full.push_str(delta);
      let _ = app_for_emit.emit(
        &event_name_for_emit,
        serde_json::json!({
          "imageId": image_id_for_emit.clone(),
          "kind": kind_for_emit.clone(),
          "delta": delta,
        }),
      );
    })
    .await;

  let full = full.trim().to_string();
  let reason = match result {
    Ok(_) if generation_atom.load(Ordering::SeqCst) != generation => "cancelled",
    Ok(_) => "done",
    Err(_) => "error",
  };
  let _ = app.emit(
    event_name,
    serde_json::json!({
      "imageId": image_id,
      "kind": kind,
      "delta": "",
      "done": true,
      "reason": reason,
      "full": full.clone(),
    }),
  );

  result.map(|_| full)
}

// ===== SSE 流 =====

/// 构造带 image 的 OCR/视觉请求 body（model 由调用方注入），开启 stream
pub fn build_ocr_request_body(
  image_path: &Path,
  prompt: &str,
  thinking_enabled: bool,
) -> Result<serde_json::Value, String> {
  let bytes = fs::read(image_path).map_err(|e| e.to_string())?;
  let base64 = general_purpose::STANDARD.encode(bytes);
  let mut body = serde_json::json!({
    "messages": [{
      "role": "user",
      "content": [
        { "type": "image_url", "image_url": { "url": format!("data:image/png;base64,{base64}") } },
        { "type": "text", "text": prompt }
      ]
    }],
    "temperature": 0.2,
    "max_tokens": 2000,
    "stream": true
  });
  if !thinking_enabled {
    body["thinking"] = serde_json::json!({ "type": "disabled" });
  }
  Ok(body)
}

/// 通用流式 chat 调用：发送 body（model 在外层注入）→ 解析 SSE → 通过 stream_vision_response emit。
/// 复用 explain_stream_generation 作取消代号（lens-stream / lens-translate-stream 都共用）。
#[allow(clippy::too_many_arguments)]
pub async fn stream_chat_call(
  app: &AppHandle,
  state: &State<'_, AppState>,
  provider: &settings::ModelProvider,
  model: &str,
  mut body: serde_json::Value,
  retry_attempts: usize,
  image_id: &str,
  kind: &str,
  event_name: &str,
) -> Result<String, String> {
  if provider.base_url == APPLE_INTELLIGENCE_BASE_URL {
    let _ = (app, state, model, &mut body, retry_attempts, image_id, kind, event_name);
    return Err("Apple Intelligence 暂不支持图像输入,请为截图翻译配置云端 provider".into());
  }
  if model.trim().is_empty() {
    return Err("Please select a model first".to_string());
  }
  body["model"] = serde_json::json!(model);
  let thinking_disabled = body.get("thinking")
    .and_then(|t| t.get("type"))
    .and_then(|v| v.as_str())
    == Some("disabled");
  let endpoint_kind = provider_endpoint_kind(&provider.base_url);
  let use_responses = endpoint_kind == ModelEndpointKind::Responses;
  let use_messages = endpoint_kind == ModelEndpointKind::Messages;
  let url = if use_responses {
    let input = chat_messages_to_responses_input(body.get("messages").unwrap_or(&serde_json::Value::Null));
    let responses_body = serde_json::json!({
      "model": model,
      "input": input,
      "stream": true,
      "max_output_tokens": body.get("max_tokens").cloned().unwrap_or_else(|| serde_json::json!(2000))
    });
    body = responses_body;
    apply_responses_web_search(&mut body, provider.web_search_enabled.unwrap_or(false));
    apply_responses_reasoning(&mut body, !thinking_disabled, provider.reasoning_effort.as_deref());
    responses_api_url(&provider.base_url)
  } else if use_messages {
    body = chat_messages_to_claude_body(
      model,
      body.get("messages").unwrap_or(&serde_json::Value::Null),
      body.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(2000),
      body.get("temperature").and_then(|v| v.as_f64()),
      true,
      !thinking_disabled,
    );
    messages_api_url(&provider.base_url)
  } else {
    chat_completions_url(&provider.base_url)
  };

  let response = send_with_failover(
    state,
    "Stream chat",
    retry_attempts,
    &provider.id,
    &provider.api_keys,
    |key| {
      let request = state.http.post(url.clone());
      let request = if use_messages {
        claude_auth_headers(request, key)
      } else {
        request.bearer_auth(key)
      };
      request.json(&body).send()
    },
  )
  .await?;

  let status = response.status();
  if !status.is_success() {
    let body_text = response.text().await.unwrap_or_default();
    let snippet: String = body_text.chars().take(500).collect();
    return Err(format!("Stream HTTP {}: {}", status.as_u16(), snippet));
  }

  let generation = state
    .explain_stream_generation
    .fetch_add(1, Ordering::SeqCst)
    + 1;
  if use_responses {
    return stream_responses_response(
      app,
      response,
      image_id,
      kind,
      event_name,
      &state.explain_stream_generation,
      generation,
    )
    .await;
  }
  if use_messages {
    return stream_claude_response(
      app,
      response,
      image_id,
      kind,
      event_name,
      &state.explain_stream_generation,
      generation,
    )
    .await;
  }
  stream_vision_response(
    app,
    response,
    image_id,
    kind,
    event_name,
    &state.explain_stream_generation,
    generation,
  )
  .await
}

/// 截图翻译合并模式流：单次调用模型，按 `<<<ORIGINAL>>>` 分隔符把 SSE delta 拆成两段。
/// 分隔符前的 chunk emit kind="translated"；分隔符后的 chunk emit kind="original"。
/// 返回 (translated, original) 完整文本。
///
/// 关键点：
/// - 分隔符可能跨 SSE chunk 边界 → 用 tail 缓冲住末尾 (SEPARATOR.len()-1) 字节防止把分隔符前缀当成译文 emit 出去
/// - tail 切片必须落在 UTF-8 char boundary，否则 String::drain 会 panic（用户截图常含 CJK，每字 3 字节）
#[allow(clippy::too_many_arguments)]
pub async fn stream_translate_combined(
  app: &AppHandle,
  state: &State<'_, AppState>,
  provider: &settings::ModelProvider,
  model: &str,
  mut body: serde_json::Value,
  retry_attempts: usize,
  image_id: &str,
  event_name: &str,
) -> Result<(String, String), String> {
  if provider.base_url == APPLE_INTELLIGENCE_BASE_URL {
    let _ = (app, state, model, &mut body, retry_attempts, image_id, event_name);
    return Err("Apple Intelligence 暂不支持图像输入,请为截图翻译配置云端 provider".into());
  }
  if model.trim().is_empty() {
    return Err("Please select a model first".to_string());
  }
  body["model"] = serde_json::json!(model);
  let thinking_disabled = body.get("thinking")
    .and_then(|t| t.get("type"))
    .and_then(|v| v.as_str())
    == Some("disabled");
  let endpoint_kind = provider_endpoint_kind(&provider.base_url);
  let use_responses = endpoint_kind == ModelEndpointKind::Responses;
  let use_messages = endpoint_kind == ModelEndpointKind::Messages;
  let url = if use_responses {
    let input = chat_messages_to_responses_input(body.get("messages").unwrap_or(&serde_json::Value::Null));
    let mut responses_body = serde_json::json!({
      "model": model,
      "input": input,
      "stream": true,
      "max_output_tokens": body.get("max_tokens").cloned().unwrap_or_else(|| serde_json::json!(2000))
    });
    apply_responses_web_search(&mut responses_body, provider.web_search_enabled.unwrap_or(false));
    apply_responses_reasoning(&mut responses_body, !thinking_disabled, provider.reasoning_effort.as_deref());
    body = responses_body;
    responses_api_url(&provider.base_url)
  } else if use_messages {
    body = chat_messages_to_claude_body(
      model,
      body.get("messages").unwrap_or(&serde_json::Value::Null),
      body.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(2000),
      body.get("temperature").and_then(|v| v.as_f64()),
      true,
      !thinking_disabled,
    );
    messages_api_url(&provider.base_url)
  } else {
    chat_completions_url(&provider.base_url)
  };

  let mut response = send_with_failover(
    state,
    "Stream translate combined",
    retry_attempts,
    &provider.id,
    &provider.api_keys,
    |key| {
      let request = state.http.post(url.clone());
      let request = if use_messages {
        claude_auth_headers(request, key)
      } else {
        request.bearer_auth(key)
      };
      request.json(&body).send()
    },
  )
  .await?;

  let status = response.status();
  if !status.is_success() {
    let body_text = response.text().await.unwrap_or_default();
    let snippet: String = body_text.chars().take(500).collect();
    return Err(format!("Stream HTTP {}: {}", status.as_u16(), snippet));
  }

  let my_gen = state
    .explain_stream_generation
    .fetch_add(1, Ordering::SeqCst)
    + 1;

  if use_responses {
    return stream_translate_combined_responses(
      app,
      response,
      image_id,
      event_name,
      &state.explain_stream_generation,
      my_gen,
    )
    .await;
  }

  if use_messages {
    return stream_translate_combined_claude(
      app,
      response,
      image_id,
      event_name,
      &state.explain_stream_generation,
      my_gen,
    )
    .await;
  }

  let sep = COMBINED_TRANSLATE_SEPARATOR;
  let sep_len = sep.len();

  let mut sse_buf = String::new();
  let mut tail = String::new();
  let mut translated = String::new();
  let mut original = String::new();
  let mut sep_seen = false;

  let emit_done = |reason: &str| {
    let _ = app.emit(
      event_name,
      serde_json::json!({
        "imageId": image_id, "delta": "", "done": true, "reason": reason,
      }),
    );
  };

  loop {
    if state.explain_stream_generation.load(Ordering::SeqCst) != my_gen {
      // 取消：把 tail 当作 translated flush（避免末尾几个字符丢失），再 emit done
      if !tail.is_empty() && !sep_seen {
        translated.push_str(&tail);
        let _ = app.emit(
          event_name,
          serde_json::json!({ "imageId": image_id, "kind": "translated", "delta": tail }),
        );
      }
      emit_done("cancelled");
      return Ok((translated, original));
    }

    let chunk = match response.chunk().await {
      Ok(Some(c)) => c,
      Ok(None) => break,
      Err(e) => {
        emit_done("error");
        return Err(e.to_string());
      }
    };

    let text = String::from_utf8_lossy(&chunk);
    sse_buf.push_str(&text);

    while let Some(pos) = sse_buf.find('\n') {
      let line: String = sse_buf.drain(..=pos).collect();
      let line = line.trim();
      if !line.starts_with("data:") {
        continue;
      }
      let data = line.trim_start_matches("data:").trim();
      if data.is_empty() {
        continue;
      }
      if data == "[DONE]" {
        // flush tail
        if !sep_seen && !tail.is_empty() {
          translated.push_str(&tail);
          let _ = app.emit(
            event_name,
            serde_json::json!({ "imageId": image_id, "kind": "translated", "delta": tail }),
          );
        }
        emit_done("done");
        return Ok((translated, original));
      }

      let value: serde_json::Value = match serde_json::from_str(data) {
        Ok(val) => val,
        Err(_) => continue,
      };

      let delta_obj = value
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("delta"));

      // 推理链 emit（恒定 kind="translated"，前端在主面板渲染）
      if let Some(r) = delta_obj
        .and_then(|d| d.get("reasoning_content").or_else(|| d.get("reasoning")))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
      {
        let _ = app.emit(
          event_name,
          serde_json::json!({
            "imageId": image_id, "kind": "translated", "delta": "", "reasoningDelta": r,
          }),
        );
      }

      let content = delta_obj
        .and_then(|d| d.get("content"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

      let Some(c) = content else { continue };

      if sep_seen {
        original.push_str(c);
        let _ = app.emit(
          event_name,
          serde_json::json!({ "imageId": image_id, "kind": "original", "delta": c }),
        );
        continue;
      }

      tail.push_str(c);
      if let Some(idx) = tail.find(sep) {
        // 分隔符命中：拆 before / after，trim 掉分隔符相邻的换行，分别发出
        let before: String = tail.drain(..idx).collect();
        // 移除分隔符本身
        tail.drain(..sep_len);
        let after: String = std::mem::take(&mut tail);

        let before_emit = before.trim_end_matches('\n').to_string();
        if !before_emit.is_empty() {
          translated.push_str(&before_emit);
          let _ = app.emit(
            event_name,
            serde_json::json!({ "imageId": image_id, "kind": "translated", "delta": before_emit }),
          );
        }
        sep_seen = true;
        let after_emit = after.trim_start_matches('\n').to_string();
        if !after_emit.is_empty() {
          original.push_str(&after_emit);
          let _ = app.emit(
            event_name,
            serde_json::json!({ "imageId": image_id, "kind": "original", "delta": after_emit }),
          );
        }
      } else {
        // 没命中：emit 安全前缀（保留末尾 sep_len-1 字节防止跨 chunk 分隔符被切碎）
        let max_emit = tail.len().saturating_sub(sep_len.saturating_sub(1));
        if max_emit == 0 {
          continue;
        }
        // 找一个合法 char boundary（CJK 字符多字节，不能切到字符中间）
        let mut safe = max_emit;
        while safe > 0 && !tail.is_char_boundary(safe) {
          safe -= 1;
        }
        if safe == 0 {
          continue;
        }
        let to_emit: String = tail.drain(..safe).collect();
        translated.push_str(&to_emit);
        let _ = app.emit(
          event_name,
          serde_json::json!({ "imageId": image_id, "kind": "translated", "delta": to_emit }),
        );
      }
    }
  }

  // SSE 流结束（连接关闭）但没收到 [DONE]：flush tail
  if !sep_seen && !tail.is_empty() {
    translated.push_str(&tail);
    let _ = app.emit(
      event_name,
      serde_json::json!({ "imageId": image_id, "kind": "translated", "delta": tail }),
    );
  }
  emit_done("done");
  Ok((translated, original))
}

/// 流式解析视觉 API 的 SSE 响应
/// 逐 chunk 读取响应体，解析 "data:" 行，提取 delta 中的 content 并通过 `event_name` emit。
/// 支持取消：调用方持有 `my_generation`，全局代号 `generation_atom` 一旦变化即视为被新流或外部取消作废。
pub async fn stream_vision_response(
  app: &AppHandle,
  mut response: reqwest::Response,
  image_id: &str,
  kind: &str,
  event_name: &str,
  generation_atom: &AtomicU64,
  my_generation: u64,
) -> Result<String, String> {
  let mut buffer = String::new();
  let mut full = String::new();

  let emit_done = |reason: &str, full_text: &str| {
    let _ = app.emit(
      event_name,
      serde_json::json!({
        "imageId": image_id,
        "kind": kind,
        "delta": "",
        "done": true,
        "reason": reason,
        "full": full_text,
      }),
    );
  };

  loop {
    if generation_atom.load(Ordering::SeqCst) != my_generation {
      emit_done("cancelled", full.trim());
      return Ok(full.trim().to_string());
    }

    let chunk = match response.chunk().await {
      Ok(Some(c)) => c,
      Ok(None) => break,
      Err(e) => {
        emit_done("error", full.trim());
        return Err(e.to_string());
      }
    };

    let text = String::from_utf8_lossy(&chunk);
    buffer.push_str(&text);

    while let Some(pos) = buffer.find('\n') {
      let line: String = buffer.drain(..=pos).collect();
      let line = line.trim();
      if !line.starts_with("data:") {
        continue;
      }
      let data = line.trim_start_matches("data:").trim();
      if data.is_empty() {
        continue;
      }
      if data == "[DONE]" {
        emit_done("done", full.trim());
        return Ok(full.trim().to_string());
      }

      let value: serde_json::Value = match serde_json::from_str(data) {
        Ok(val) => val,
        Err(_) => continue,
      };

      let delta_obj = value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("delta"));

      // 推理模型（DeepSeek-R1 / Kimi 等）把链路放在 delta.reasoning_content
      // 部分实现用 delta.reasoning。两种字段都尝试取，只要有就 emit。
      let reasoning = delta_obj
        .and_then(|d| d.get("reasoning_content").or_else(|| d.get("reasoning")))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

      if let Some(r) = reasoning {
        let _ = app.emit(
          event_name,
          serde_json::json!({
            "imageId": image_id,
            "kind": kind,
            "delta": "",
            "reasoningDelta": r,
          }),
        );
      }

      let content = delta_obj
        .and_then(|d| d.get("content"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

      if let Some(content) = content {
        full.push_str(content);
        let _ = app.emit(
          event_name,
          serde_json::json!({ "imageId": image_id, "kind": kind, "delta": content }),
        );
      }
    }
  }

  emit_done("done", full.trim());
  Ok(full.trim().to_string())
}

fn claude_stream_delta_text(value: &serde_json::Value) -> Option<String> {
  let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
  match event_type {
    "content_block_delta" => value
      .get("delta")
      .and_then(|delta| {
        if delta.get("type").and_then(|v| v.as_str()) == Some("text_delta") {
          delta.get("text").and_then(|v| v.as_str())
        } else {
          None
        }
      })
      .map(str::to_string),
    "message_delta" => value
      .get("delta")
      .and_then(|delta| delta.get("text"))
      .and_then(|v| v.as_str())
      .map(str::to_string),
    _ => None,
  }
}

fn claude_stream_delta_reasoning(value: &serde_json::Value) -> Option<String> {
  let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
  match event_type {
    "content_block_delta" => value
      .get("delta")
      .and_then(|delta| {
        if delta.get("type").and_then(|v| v.as_str()) == Some("thinking_delta") {
          delta.get("thinking").and_then(|v| v.as_str())
        } else {
          None
        }
      })
      .map(str::to_string),
    _ => None,
  }
}

fn claude_stream_error(value: &serde_json::Value) -> Option<String> {
  if value.get("type").and_then(|v| v.as_str()) != Some("error") {
    return None;
  }
  let error = value.get("error").unwrap_or(value);
  let message = error
    .get("message")
    .and_then(|v| v.as_str())
    .unwrap_or("Claude stream error");
  let error_type = error.get("type").and_then(|v| v.as_str()).unwrap_or("");
  if error_type.is_empty() {
    Some(message.to_string())
  } else {
    Some(format!("{error_type}: {message}"))
  }
}

pub async fn stream_claude_response(
  app: &AppHandle,
  mut response: reqwest::Response,
  image_id: &str,
  kind: &str,
  event_name: &str,
  generation_atom: &AtomicU64,
  my_generation: u64,
) -> Result<String, String> {
  let mut buffer = String::new();
  let mut full = String::new();

  let emit_done = |reason: &str, full_text: &str| {
    let _ = app.emit(
      event_name,
      serde_json::json!({
        "imageId": image_id,
        "kind": kind,
        "delta": "",
        "done": true,
        "reason": reason,
        "full": full_text,
      }),
    );
  };

  loop {
    if generation_atom.load(Ordering::SeqCst) != my_generation {
      emit_done("cancelled", full.trim());
      return Ok(full.trim().to_string());
    }

    let chunk = match response.chunk().await {
      Ok(Some(c)) => c,
      Ok(None) => break,
      Err(e) => {
        emit_done("error", full.trim());
        return Err(e.to_string());
      }
    };

    buffer.push_str(&String::from_utf8_lossy(&chunk));
    while let Some(pos) = buffer.find('\n') {
      let line: String = buffer.drain(..=pos).collect();
      let line = line.trim();
      if !line.starts_with("data:") {
        continue;
      }
      let data = line.trim_start_matches("data:").trim();
      if data.is_empty() {
        continue;
      }
      let value: serde_json::Value = match serde_json::from_str(data) {
        Ok(val) => val,
        Err(_) => continue,
      };

      if let Some(err) = claude_stream_error(&value) {
        emit_done("error", full.trim());
        return Err(err);
      }
      if value.get("type").and_then(|v| v.as_str()) == Some("message_stop") {
        emit_done("done", full.trim());
        return Ok(full.trim().to_string());
      }
      // web search: 检测 server_tool_use (搜索开始) 事件
      if value.get("type").and_then(|v| v.as_str()) == Some("content_block_start") {
        let block = value.get("content_block");
        let block_type = block.and_then(|b| b.get("type")).and_then(|v| v.as_str()).unwrap_or("");
        if block_type == "server_tool_use" {
          let tool_name = block.and_then(|b| b.get("name")).and_then(|v| v.as_str()).unwrap_or("");
          if tool_name == "web_search" || tool_name == "tavily_web_search" {
            let _ = app.emit(
              event_name,
              serde_json::json!({
                "imageId": image_id,
                "kind": kind,
                "delta": "",
                "reasoningDelta": "正在联网搜索...\n",
              }),
            );
          }
        }
      }
      if let Some(reasoning) = claude_stream_delta_reasoning(&value).filter(|s| !s.is_empty()) {
        let _ = app.emit(
          event_name,
          serde_json::json!({
            "imageId": image_id,
            "kind": kind,
            "delta": "",
            "reasoningDelta": reasoning,
          }),
        );
      }
      if let Some(delta) = claude_stream_delta_text(&value).filter(|s| !s.is_empty()) {
        full.push_str(&delta);
        let _ = app.emit(
          event_name,
          serde_json::json!({ "imageId": image_id, "kind": kind, "delta": delta }),
        );
      }
    }
  }

  emit_done("done", full.trim());
  Ok(full.trim().to_string())
}

#[allow(clippy::too_many_arguments)]
fn emit_combined_text_delta(
  app: &AppHandle,
  event_name: &str,
  image_id: &str,
  sep: &str,
  sep_seen: &mut bool,
  tail: &mut String,
  translated: &mut String,
  original: &mut String,
  text: &str,
) {
  if text.is_empty() {
    return;
  }
  if *sep_seen {
    original.push_str(text);
    let _ = app.emit(
      event_name,
      serde_json::json!({ "imageId": image_id, "kind": "original", "delta": text }),
    );
    return;
  }

  tail.push_str(text);
  if let Some(idx) = tail.find(sep) {
    let before: String = tail.drain(..idx).collect();
    tail.drain(..sep.len());
    let after: String = std::mem::take(tail);

    let before_emit = before.trim_end_matches('\n').to_string();
    if !before_emit.is_empty() {
      translated.push_str(&before_emit);
      let _ = app.emit(
        event_name,
        serde_json::json!({ "imageId": image_id, "kind": "translated", "delta": before_emit }),
      );
    }
    *sep_seen = true;
    let after_emit = after.trim_start_matches('\n').to_string();
    if !after_emit.is_empty() {
      original.push_str(&after_emit);
      let _ = app.emit(
        event_name,
        serde_json::json!({ "imageId": image_id, "kind": "original", "delta": after_emit }),
      );
    }
  } else {
    let max_emit = tail.len().saturating_sub(sep.len().saturating_sub(1));
    if max_emit == 0 {
      return;
    }
    let mut safe = max_emit;
    while safe > 0 && !tail.is_char_boundary(safe) {
      safe -= 1;
    }
    if safe == 0 {
      return;
    }
    let to_emit: String = tail.drain(..safe).collect();
    translated.push_str(&to_emit);
    let _ = app.emit(
      event_name,
      serde_json::json!({ "imageId": image_id, "kind": "translated", "delta": to_emit }),
    );
  }
}

fn flush_combined_tail(
  app: &AppHandle,
  event_name: &str,
  image_id: &str,
  sep_seen: bool,
  tail: &mut String,
  translated: &mut String,
) {
  if !sep_seen && !tail.is_empty() {
    let text = std::mem::take(tail);
    translated.push_str(&text);
    let _ = app.emit(
      event_name,
      serde_json::json!({ "imageId": image_id, "kind": "translated", "delta": text }),
    );
  }
}

pub async fn stream_translate_combined_claude(
  app: &AppHandle,
  mut response: reqwest::Response,
  image_id: &str,
  event_name: &str,
  generation_atom: &AtomicU64,
  my_generation: u64,
) -> Result<(String, String), String> {
  let sep = COMBINED_TRANSLATE_SEPARATOR;
  let mut sse_buf = String::new();
  let mut tail = String::new();
  let mut translated = String::new();
  let mut original = String::new();
  let mut sep_seen = false;

  let emit_done = |reason: &str| {
    let _ = app.emit(
      event_name,
      serde_json::json!({
        "imageId": image_id, "delta": "", "done": true, "reason": reason,
      }),
    );
  };

  loop {
    if generation_atom.load(Ordering::SeqCst) != my_generation {
      flush_combined_tail(app, event_name, image_id, sep_seen, &mut tail, &mut translated);
      emit_done("cancelled");
      return Ok((translated, original));
    }

    let chunk = match response.chunk().await {
      Ok(Some(c)) => c,
      Ok(None) => break,
      Err(e) => {
        emit_done("error");
        return Err(e.to_string());
      }
    };

    sse_buf.push_str(&String::from_utf8_lossy(&chunk));
    while let Some(pos) = sse_buf.find('\n') {
      let line: String = sse_buf.drain(..=pos).collect();
      let line = line.trim();
      if !line.starts_with("data:") {
        continue;
      }
      let data = line.trim_start_matches("data:").trim();
      if data.is_empty() {
        continue;
      }
      let value: serde_json::Value = match serde_json::from_str(data) {
        Ok(val) => val,
        Err(_) => continue,
      };
      if let Some(err) = claude_stream_error(&value) {
        emit_done("error");
        return Err(err);
      }
      if value.get("type").and_then(|v| v.as_str()) == Some("message_stop") {
        flush_combined_tail(app, event_name, image_id, sep_seen, &mut tail, &mut translated);
        emit_done("done");
        return Ok((translated, original));
      }
      if let Some(reasoning) = claude_stream_delta_reasoning(&value).filter(|s| !s.is_empty()) {
        let _ = app.emit(
          event_name,
          serde_json::json!({
            "imageId": image_id, "kind": "translated", "delta": "", "reasoningDelta": reasoning,
          }),
        );
      }
      if let Some(delta) = claude_stream_delta_text(&value).filter(|s| !s.is_empty()) {
        emit_combined_text_delta(
          app,
          event_name,
          image_id,
          sep,
          &mut sep_seen,
          &mut tail,
          &mut translated,
          &mut original,
          &delta,
        );
      }
    }
  }

  flush_combined_tail(app, event_name, image_id, sep_seen, &mut tail, &mut translated);
  emit_done("done");
  Ok((translated, original))
}

pub async fn stream_translate_combined_responses(
  app: &AppHandle,
  mut response: reqwest::Response,
  image_id: &str,
  event_name: &str,
  generation_atom: &AtomicU64,
  my_generation: u64,
) -> Result<(String, String), String> {
  let sep = COMBINED_TRANSLATE_SEPARATOR;
  let mut sse_buf = String::new();
  let mut sse_event_type = String::new();
  let mut tail = String::new();
  let mut translated = String::new();
  let mut original = String::new();
  let mut sep_seen = false;
  let mut text_delta_seen = false;
  let mut reasoning_delta_seen = false;

  let emit_done = |reason: &str| {
    let _ = app.emit(
      event_name,
      serde_json::json!({
        "imageId": image_id, "delta": "", "done": true, "reason": reason,
      }),
    );
  };

  loop {
    if generation_atom.load(Ordering::SeqCst) != my_generation {
      flush_combined_tail(app, event_name, image_id, sep_seen, &mut tail, &mut translated);
      emit_done("cancelled");
      return Ok((translated, original));
    }

    let chunk = match response.chunk().await {
      Ok(Some(c)) => c,
      Ok(None) => break,
      Err(e) => {
        emit_done("error");
        return Err(e.to_string());
      }
    };

    sse_buf.push_str(&String::from_utf8_lossy(&chunk));
    while let Some(pos) = sse_buf.find('\n') {
      let line: String = sse_buf.drain(..=pos).collect();
      let line = line.trim();
      if let Some(event_type) = line.strip_prefix("event:") {
        sse_event_type = event_type.trim().to_string();
        continue;
      }
      if !line.starts_with("data:") {
        continue;
      }
      let data = line.trim_start_matches("data:").trim();
      if data.is_empty() {
        continue;
      }
      if data == "[DONE]" {
        flush_combined_tail(app, event_name, image_id, sep_seen, &mut tail, &mut translated);
        emit_done("done");
        return Ok((translated, original));
      }

      let value: serde_json::Value = match serde_json::from_str(data) {
        Ok(val) => val,
        Err(_) => continue,
      };
      let event_type_owned = value
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or(sse_event_type.as_str())
        .to_string();
      sse_event_type.clear();

      match event_type_owned.as_str() {
        event if event.contains("output_text.delta") || event.contains("response.text.delta") => {
          if let Some(delta) = response_stream_delta_text(&value) {
            text_delta_seen = true;
            emit_combined_text_delta(
              app,
              event_name,
              image_id,
              sep,
              &mut sep_seen,
              &mut tail,
              &mut translated,
              &mut original,
              &delta,
            );
          }
        }
        event if response_stream_is_reasoning_event(event, &value) => {
          let is_delta_event = event.contains("delta") || value.get("delta").is_some();
          if is_delta_event || !reasoning_delta_seen {
            if let Some(delta) = response_stream_delta_text(&value) {
              reasoning_delta_seen = true;
              let _ = app.emit(
                event_name,
                serde_json::json!({
                  "imageId": image_id, "kind": "translated", "delta": "", "reasoningDelta": delta,
                }),
              );
            }
          }
        }
        "response.output_text.done" | "response.text.done" | "response.content_part.done" => {
          if !text_delta_seen {
            if let Some(text) = response_stream_done_text(&value) {
              text_delta_seen = true;
              emit_combined_text_delta(
                app,
                event_name,
                image_id,
                sep,
                &mut sep_seen,
                &mut tail,
                &mut translated,
                &mut original,
                &text,
              );
            }
          }
        }
        "response.completed" => {
          if !text_delta_seen {
            if let Some(text) = value.get("response").and_then(response_output_text) {
              emit_combined_text_delta(
                app,
                event_name,
                image_id,
                sep,
                &mut sep_seen,
                &mut tail,
                &mut translated,
                &mut original,
                &text,
              );
            }
          }
          flush_combined_tail(app, event_name, image_id, sep_seen, &mut tail, &mut translated);
          emit_done("done");
          return Ok((translated, original));
        }
        "response.failed" | "response.incomplete" | "error" => {
          emit_done("error");
          return Err(format!("Responses stream error: {}", value));
        }
        _ => {
          if !text_delta_seen {
            if let Some(delta) = response_stream_message_item_text(&value) {
              text_delta_seen = true;
              emit_combined_text_delta(
                app,
                event_name,
                image_id,
                sep,
                &mut sep_seen,
                &mut tail,
                &mut translated,
                &mut original,
                &delta,
              );
            }
          }
        }
      }
    }
  }

  flush_combined_tail(app, event_name, image_id, sep_seen, &mut tail, &mut translated);
  emit_done("done");
  Ok((translated, original))
}

fn response_stream_delta_text(value: &serde_json::Value) -> Option<String> {
  value
    .get("delta")
    .or_else(|| value.get("text"))
    .and_then(|v| v.as_str())
    .map(str::to_string)
    .or_else(|| response_stream_message_item_text(value))
}

fn response_stream_message_item_text(value: &serde_json::Value) -> Option<String> {
  value
    .get("item")
    .filter(|item| item.get("type").and_then(|v| v.as_str()) == Some("message"))
    .and_then(|item| item.get("content"))
    .and_then(|content| content.as_array())
    .map(|parts| {
      parts
        .iter()
        .filter_map(|part| {
          let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
          if !part_type.is_empty() && part_type != "output_text" && part_type != "text" {
            return None;
          }
          part
            .get("text")
            .or_else(|| part.get("output_text"))
            .and_then(|v| v.as_str())
        })
        .collect::<String>()
    })
    .filter(|text| !text.is_empty())
}

fn response_stream_message_item_text_delta(
  current: &str,
  value: &serde_json::Value,
) -> Option<String> {
  response_stream_message_item_text(value)
    .and_then(|text| {
      response_text_extension(current, &text)
        .filter(|delta| !delta.is_empty())
        .map(str::to_string)
    })
}

fn response_stream_is_reasoning_event(event_type: &str, value: &serde_json::Value) -> bool {
  if event_type.contains("reasoning") {
    return true;
  }
  for key in ["item", "part", "content_part", "delta"] {
    let Some(item_type) = value
      .get(key)
      .and_then(|item| item.get("type"))
      .and_then(|v| v.as_str())
    else {
      continue;
    };
    if item_type == "reasoning"
      || item_type == "summary_text"
      || item_type == "summary_text_delta"
    {
      return true;
    }
  }
  false
}

fn response_stream_done_text(value: &serde_json::Value) -> Option<String> {
  value
    .get("text")
    .or_else(|| value.get("output_text"))
    .and_then(|v| v.as_str())
    .map(str::to_string)
    .or_else(|| value.get("response").and_then(response_output_text))
    .or_else(|| response_output_text(value))
}

fn response_text_extension<'a>(current: &str, candidate: &'a str) -> Option<&'a str> {
  if current.is_empty() {
    return Some(candidate);
  }
  if let Some(rest) = candidate.strip_prefix(current) {
    return Some(rest);
  }
  let trimmed = current.trim_end();
  if trimmed.len() != current.len() {
    return candidate.strip_prefix(trimmed);
  }
  None
}

async fn emit_responses_text_delta(
  app: &AppHandle,
  event_name: &str,
  image_id: &str,
  kind: &str,
  full: &mut String,
  delta: &str,
) {
  if delta.is_empty() {
    return;
  }
  full.push_str(delta);
  let _ = app.emit(
    event_name,
    serde_json::json!({ "imageId": image_id, "kind": kind, "delta": delta }),
  );
}

async fn emit_responses_reasoning_delta(
  app: &AppHandle,
  event_name: &str,
  image_id: &str,
  kind: &str,
  delta: &str,
) {
  if delta.is_empty() {
    return;
  }
  let _ = app.emit(
    event_name,
    serde_json::json!({
      "imageId": image_id,
      "kind": kind,
      "delta": "",
      "reasoningDelta": delta,
    }),
  );
}

/// 流式解析 OpenAI Responses API 的 SSE 响应。
pub async fn stream_responses_response(
  app: &AppHandle,
  mut response: reqwest::Response,
  image_id: &str,
  kind: &str,
  event_name: &str,
  generation_atom: &AtomicU64,
  my_generation: u64,
) -> Result<String, String> {
  let mut buffer = String::new();
  let mut full = String::new();
  let mut web_search_notice_emitted = false;
  let mut sse_event_type = String::new();
  let mut reasoning_delta_seen = false;

  let emit_done = |reason: &str, full_text: &str| {
    let _ = app.emit(
      event_name,
      serde_json::json!({
        "imageId": image_id,
        "kind": kind,
        "delta": "",
        "done": true,
        "reason": reason,
        "full": full_text,
      }),
    );
  };

  loop {
    if generation_atom.load(Ordering::SeqCst) != my_generation {
      emit_done("cancelled", full.trim());
      return Ok(full.trim().to_string());
    }

    let chunk = match response.chunk().await {
      Ok(Some(c)) => c,
      Ok(None) => break,
      Err(e) => {
        emit_done("error", full.trim());
        return Err(format!("Responses stream read body: {e}"));
      }
    };

    buffer.push_str(&String::from_utf8_lossy(&chunk));
    while let Some(pos) = buffer.find('\n') {
      let line: String = buffer.drain(..=pos).collect();
      let line = line.trim();
      if let Some(event_type) = line.strip_prefix("event:") {
        sse_event_type = event_type.trim().to_string();
        continue;
      }
      if !line.starts_with("data:") {
        continue;
      }
      let data = line.trim_start_matches("data:").trim();
      if data.is_empty() {
        continue;
      }
      if data == "[DONE]" {
        emit_done("done", full.trim());
        return Ok(full.trim().to_string());
      }

      let value: serde_json::Value = match serde_json::from_str(data) {
        Ok(val) => val,
        Err(_) => continue,
      };
      let event_type_owned = value
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or(sse_event_type.as_str())
        .to_string();
      sse_event_type.clear();

      if !web_search_notice_emitted {
        let item_type = value
          .get("item")
          .and_then(|item| item.get("type"))
          .and_then(|v| v.as_str())
          .unwrap_or("");
        if event_type_owned.contains("web_search") || item_type.contains("web_search") {
          web_search_notice_emitted = true;
          emit_responses_reasoning_delta(
            app,
            event_name,
            image_id,
            kind,
            "正在联网搜索...\n",
          )
          .await;
        }
      }

      match event_type_owned.as_str() {
        event if event.contains("output_text.delta") || event.contains("response.text.delta") => {
          if let Some(delta) = response_stream_delta_text(&value) {
            emit_responses_text_delta(app, event_name, image_id, kind, &mut full, &delta).await;
          }
        }
        event if response_stream_is_reasoning_event(event, &value) => {
          let is_delta_event = event.contains("delta") || value.get("delta").is_some();
          if is_delta_event || !reasoning_delta_seen {
            if let Some(delta) = response_stream_delta_text(&value) {
              reasoning_delta_seen = true;
              emit_responses_reasoning_delta(app, event_name, image_id, kind, &delta).await;
            }
          }
        }
        "response.output_text.done" | "response.text.done" | "response.content_part.done" => {
          if let Some(text) = response_stream_done_text(&value) {
            if let Some(delta) = response_text_extension(&full, &text).filter(|s| !s.is_empty()) {
              emit_responses_text_delta(app, event_name, image_id, kind, &mut full, delta).await;
            }
          }
        }
        "response.completed" => {
          if full.trim().is_empty() {
            if let Some(text) = value.get("response").and_then(response_output_text) {
              if let Some(delta) = response_text_extension(&full, &text).filter(|s| !s.is_empty()) {
                emit_responses_text_delta(app, event_name, image_id, kind, &mut full, delta).await;
              }
            }
          }
          emit_done("done", full.trim());
          return Ok(full.trim().to_string());
        }
        "response.failed" | "response.incomplete" | "error" => {
          let message = value
            .get("error")
            .or_else(|| value.get("response").and_then(|v| v.get("error")))
            .map(|v| v.to_string())
            .unwrap_or_else(|| value.to_string());
          emit_done("error", full.trim());
          return Err(format!("Responses stream error: {}", message.chars().take(500).collect::<String>()));
        }
        _ => {
          if let Some(delta) = response_stream_message_item_text_delta(&full, &value) {
            emit_responses_text_delta(app, event_name, image_id, kind, &mut full, &delta).await;
          }
        }
      }
    }
  }

  emit_done("done", full.trim());
  Ok(full.trim().to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  // ===== extract_status_code =====

  #[test]
  fn extract_status_code_parses_typical_send_with_retry_format() {
    // send_with_retry 拼出来的标准格式
    let s = "OpenAI API Error: 429 Too Many Requests - {\"error\":\"rate_limit\"}";
    assert_eq!(extract_status_code(s), Some(429));
  }

  #[test]
  fn extract_status_code_handles_each_failover_status() {
    assert_eq!(extract_status_code("X Error: 401 Unauthorized - body"), Some(401));
    assert_eq!(extract_status_code("X Error: 402 Payment Required - body"), Some(402));
    assert_eq!(extract_status_code("X Error: 403 Forbidden - body"), Some(403));
    assert_eq!(extract_status_code("X Error: 429 Too Many Requests - body"), Some(429));
  }

  #[test]
  fn response_output_text_ignores_reasoning_summary_blocks() {
    let value = serde_json::json!({
      "output": [
        {
          "type": "reasoning",
          "content": [
            { "type": "summary_text", "text": "I need to respond in Chinese." }
          ]
        },
        {
          "type": "message",
          "content": [
            { "type": "output_text", "text": "我是 ChatGPT。" }
          ]
        }
      ]
    });
    assert_eq!(response_output_text(&value).as_deref(), Some("我是 ChatGPT。"));
  }

  #[test]
  fn append_tavily_tool_result_for_responses_replays_input_instead_of_previous_response_id() {
    let mut body = serde_json::json!({
      "model": "gpt-4.1",
      "input": [
        {
          "role": "user",
          "content": [
            { "type": "input_text", "text": "today news" }
          ]
        }
      ],
      "previous_response_id": "resp_old"
    });
    let first_response = serde_json::json!({
      "id": "resp_new",
      "output": [
        {
          "type": "reasoning",
          "summary": [
            { "type": "summary_text", "text": "Searching for events" }
          ]
        },
        {
          "type": "function_call",
          "name": "tavily_web_search",
          "call_id": "call_123",
          "arguments": "{\"query\":\"today news\"}"
        }
      ]
    });

    append_tavily_tool_result(
      &mut body,
      &first_response,
      "call_123",
      "today news",
      "search result body",
      true,
      false,
    );

    assert!(body.get("previous_response_id").is_none());
    let input = body.get("input").and_then(|v| v.as_array()).expect("input array");
    assert_eq!(input.len(), 3);
    assert_eq!(input[1].get("type").and_then(|v| v.as_str()), Some("function_call"));
    assert_eq!(input[2].get("type").and_then(|v| v.as_str()), Some("function_call_output"));
    assert_eq!(input[2].get("call_id").and_then(|v| v.as_str()), Some("call_123"));
  }

  #[test]
  fn response_text_extension_only_returns_real_suffixes() {
    assert_eq!(response_text_extension("", "hello"), Some("hello"));
    assert_eq!(response_text_extension("hello", "hello"), Some(""));
    assert_eq!(response_text_extension("hello", "hello world"), Some(" world"));
    assert_eq!(response_text_extension("hello ", "hello world"), Some("world"));
    assert_eq!(response_text_extension("hello", "hi"), None);
  }

  #[test]
  fn response_stream_reasoning_done_events_are_not_message_text() {
    let reasoning_done = serde_json::json!({
      "type": "response.reasoning_summary_text.done",
      "text": "Preparing to respond in Chinese"
    });
    assert!(response_stream_is_reasoning_event(
      "response.reasoning_summary_text.done",
      &reasoning_done,
    ));
    assert_eq!(response_stream_message_item_text(&reasoning_done), None);

    let summary_part_done = serde_json::json!({
      "type": "response.content_part.done",
      "part": {
        "type": "summary_text",
        "text": "The user asked for current events."
      }
    });
    assert!(response_stream_is_reasoning_event(
      "response.content_part.done",
      &summary_part_done,
    ));
    assert_eq!(response_stream_message_item_text(&summary_part_done), None);
  }

  #[test]
  fn response_stream_message_item_text_only_reads_message_items() {
    let message_item = serde_json::json!({
      "type": "response.output_item.done",
      "item": {
        "type": "message",
        "content": [
          { "type": "output_text", "text": "answer" }
        ]
      }
    });
    assert_eq!(response_stream_message_item_text(&message_item).as_deref(), Some("answer"));

    let reasoning_item = serde_json::json!({
      "type": "response.output_item.done",
      "item": {
        "type": "reasoning",
        "summary": [
          { "type": "summary_text", "text": "hidden reasoning" }
        ]
      }
    });
    assert!(response_stream_is_reasoning_event("response.output_item.done", &reasoning_item));
    assert_eq!(response_stream_message_item_text(&reasoning_item), None);
  }

  #[test]
  fn response_stream_message_item_text_delta_only_returns_unseen_suffix() {
    let done_item = serde_json::json!({
      "type": "response.output_item.done",
      "item": {
        "type": "message",
        "content": [
          { "type": "output_text", "text": "hello" }
        ]
      }
    });
    assert_eq!(response_stream_message_item_text_delta("hello", &done_item), None);

    let extended_done_item = serde_json::json!({
      "type": "response.output_item.done",
      "item": {
        "type": "message",
        "content": [
          { "type": "output_text", "text": "hello world" }
        ]
      }
    });
    assert_eq!(
      response_stream_message_item_text_delta("hello", &extended_done_item).as_deref(),
      Some(" world"),
    );
  }

  #[test]
  fn extract_status_code_handles_defensive_http_format() {
    assert_eq!(extract_status_code("Stream HTTP 429: rate limited"), Some(429));
    assert_eq!(extract_status_code("Stream HTTP 401: unauthorized"), Some(401));
    assert_eq!(extract_status_code("Vision API HTTP 403: forbidden"), Some(403));
  }

  #[test]
  fn extract_status_code_handles_non_failover_status() {
    assert_eq!(extract_status_code("X Error: 400 Bad Request - body"), Some(400));
    assert_eq!(extract_status_code("X Error: 500 Internal Server Error - body"), Some(500));
  }

  #[test]
  fn extract_status_code_returns_none_for_network_error() {
    // reqwest::Error 路径无前导数字
    let s = "Stream chat Error: error sending request: connection refused (attempt 3/3)";
    assert_eq!(extract_status_code(s), None);
  }

  #[test]
  fn extract_status_code_returns_none_when_marker_missing() {
    assert_eq!(extract_status_code("just some message"), None);
    assert_eq!(extract_status_code(""), None);
  }

  // ===== is_failover_error =====

  #[test]
  fn is_failover_error_only_triggers_on_auth_quota_codes() {
    assert!(is_failover_error("X Error: 401 - body"));
    assert!(is_failover_error("X Error: 402 - body"));
    assert!(is_failover_error("X Error: 403 - body"));
    assert!(is_failover_error("X Error: 429 - body"));
    assert!(is_failover_error("Stream HTTP 429: rate limited"));
    assert!(is_failover_error("Stream HTTP 401: unauthorized"));
  }

  #[test]
  fn is_failover_error_does_not_trigger_on_400_or_5xx() {
    // 400 是请求 body 问题，不应换 key
    assert!(!is_failover_error("X Error: 400 Bad Request - body"));
    assert!(!is_failover_error("Stream HTTP 400: bad request"));
    // 500 由 send_with_retry 内部退避重试，不应到 failover 层
    assert!(!is_failover_error("X Error: 500 Internal Server Error - body"));
    assert!(!is_failover_error("X Error: 503 Service Unavailable - body"));
  }

  #[test]
  fn is_failover_error_does_not_trigger_on_network_failure() {
    // 网络问题不是 key 的锅
    assert!(!is_failover_error("Stream Error: error sending request: timed out"));
    assert!(!is_failover_error("X Error: connection closed"));
  }

  #[test]
  fn is_failover_error_does_not_trigger_on_body_keywords_alone() {
    // 旧版宽泛匹配 body 含 "billing" / "quota" 会误触发；现版严格按状态码
    assert!(!is_failover_error("X Error: 400 - {\"message\":\"billing issue\"}"));
    assert!(!is_failover_error("X Error: 500 - {\"message\":\"quota exceeded\"}"));
  }

  #[test]
  fn failover_retry_policy_switches_key_on_429_without_inner_retry() {
    assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
    assert!(!is_retryable_status_for_failover(StatusCode::TOO_MANY_REQUESTS));
    assert!(is_retryable_status_for_failover(StatusCode::INTERNAL_SERVER_ERROR));
    assert!(is_retryable_status_for_failover(StatusCode::BAD_GATEWAY));
  }

  #[test]
  fn build_apple_text_prompt_keeps_system_and_conversation_roles() {
    let prompt = build_apple_text_prompt(
      "System prompt",
      &[
        ExplainMessage {
          role: "user".to_string(),
          content: "Question".to_string(),
        },
        ExplainMessage {
          role: "assistant".to_string(),
          content: "Earlier answer".to_string(),
        },
      ],
    );

    assert!(prompt.contains("System:\nSystem prompt"));
    assert!(prompt.contains("User:\nQuestion"));
    assert!(prompt.contains("Assistant:\nEarlier answer"));
    assert!(prompt.ends_with("Assistant:"));
  }
}
