//! Talks to the model server. Two wire formats:
//! - Ollama native: POST {base}/api/chat, newline-delimited JSON stream
//! - OpenAI-compatible: POST {base}/chat/completions, server-sent events

use crate::config::{BackendKind, Config};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub static INTERRUPT: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Msg {
    pub role: String, // system | user | assistant | tool
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl Msg {
    pub fn new(role: &str, content: impl Into<String>) -> Msg {
        Msg { role: role.into(), content: content.into(), tool_calls: vec![], tool_call_id: None, tool_name: None }
    }
    pub fn tool_result(call: &ToolCall, content: String) -> Msg {
        Msg { role: "tool".into(), content, tool_calls: vec![], tool_call_id: Some(call.id.clone()), tool_name: Some(call.name.clone()) }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub tok_per_s: f64,
}

pub struct Reply {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub interrupted: bool,
}

pub struct ModelInfo {
    pub name: String,
    pub detail: String, // size / quantization when the server reports it
}

#[derive(Debug)]
pub enum BackendError {
    /// The model or server doesn't accept the `tools` field.
    ToolsUnsupported(String),
    Other(String),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::ToolsUnsupported(m) | BackendError::Other(m) => write!(f, "{m}"),
        }
    }
}

pub struct Backend {
    kind: BackendKind,
    base: String,
    key: Option<String>,
    agent: ureq::Agent,
}

impl Backend {
    pub fn new(cfg: &Config) -> Backend {
        Backend {
            kind: cfg.backend,
            base: cfg.base_url.trim_end_matches('/').to_string(),
            key: cfg.api_key(),
            agent: ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).build(),
        }
    }

    fn req(&self, method: &str, path: &str) -> ureq::Request {
        let mut r = self.agent.request(method, &format!("{}{}", self.base, path));
        if let Some(k) = &self.key {
            r = r.set("Authorization", &format!("Bearer {k}"));
        }
        r
    }

    fn err(e: ureq::Error) -> BackendError {
        match e {
            ureq::Error::Status(code, resp) => {
                let body = resp.into_string().unwrap_or_default();
                let msg = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v.get("error")
                            .map(|e| e.get("message").and_then(|m| m.as_str()).or(e.as_str()).unwrap_or("").to_string())
                    })
                    .filter(|s| !s.is_empty())
                    .unwrap_or(body);
                let low = msg.to_lowercase();
                if low.contains("does not support tools") || low.contains("tool") && low.contains("not support") {
                    BackendError::ToolsUnsupported(msg)
                } else {
                    BackendError::Other(format!("server returned {code}: {msg}"))
                }
            }
            ureq::Error::Transport(t) => BackendError::Other(format!("can't reach the server: {t}")),
        }
    }

    pub fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError> {
        match self.kind {
            BackendKind::Ollama => {
                let v: Value = self.req("GET", "/api/tags").call().map_err(Self::err)?.into_json().map_err(|e| BackendError::Other(e.to_string()))?;
                Ok(v["models"].as_array().cloned().unwrap_or_default().iter().map(|m| {
                    let size = m["size"].as_f64().map(|b| format!("{:.1} GB", b / 1e9)).unwrap_or_default();
                    let d = &m["details"];
                    let detail = [d["parameter_size"].as_str().unwrap_or(""), d["quantization_level"].as_str().unwrap_or(""), &size]
                        .iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" · ");
                    ModelInfo { name: m["name"].as_str().unwrap_or("?").into(), detail }
                }).collect())
            }
            BackendKind::Openai => {
                let v: Value = self.req("GET", "/models").call().map_err(Self::err)?.into_json().map_err(|e| BackendError::Other(e.to_string()))?;
                Ok(v["data"].as_array().cloned().unwrap_or_default().iter()
                    .map(|m| ModelInfo { name: m["id"].as_str().unwrap_or("?").into(), detail: String::new() })
                    .collect())
            }
        }
    }

    /// The model's maximum context length, when the server reports it
    /// (Ollama /api/show; llama.cpp and some others in /models).
    pub fn model_max_context(&self, model: &str) -> Option<usize> {
        match self.kind {
            BackendKind::Ollama => {
                let v: Value = self.req("POST", "/api/show").send_json(json!({"model": model})).ok()?.into_json().ok()?;
                v["model_info"].as_object()?.iter()
                    .find(|(k, _)| k.ends_with(".context_length"))
                    .and_then(|(_, v)| v.as_u64()).map(|n| n as usize)
            }
            BackendKind::Openai => {
                let v: Value = self.req("GET", "/models").call().ok()?.into_json().ok()?;
                let m = v["data"].as_array()?.iter().find(|m| m["id"].as_str() == Some(model))?;
                ["context_length", "max_context_length", "context_window"].iter()
                    .find_map(|k| m[*k].as_u64())
                    .or_else(|| m["meta"]["n_ctx_train"].as_u64())
                    .map(|n| n as usize)
            }
        }
    }

    /// Ollama only: download a model, printing progress.
    pub fn pull(&self, model: &str) -> Result<(), BackendError> {
        if self.kind != BackendKind::Ollama {
            return Err(BackendError::Other("pulling models is only supported with the Ollama backend".into()));
        }
        let resp = self.req("POST", "/api/pull").send_json(json!({"model": model, "stream": true})).map_err(Self::err)?;
        let mut last = String::new();
        for line in BufReader::new(resp.into_reader()).lines() {
            let line = line.map_err(|e| BackendError::Other(e.to_string()))?;
            let v: Value = serde_json::from_str(&line).unwrap_or_default();
            if let Some(e) = v["error"].as_str() { return Err(BackendError::Other(e.into())); }
            let status = v["status"].as_str().unwrap_or("").to_string();
            match (v["completed"].as_f64(), v["total"].as_f64()) {
                (Some(c), Some(t)) if t > 0.0 => eprint!("\r  {status}  {:>5.1}%  ({:.2}/{:.2} GB)   ", c / t * 100.0, c / 1e9, t / 1e9),
                _ if status != last => { eprintln!("\r  {status}{}", " ".repeat(30)); last = status; }
                _ => {}
            }
        }
        eprintln!();
        Ok(())
    }

    /// Stream a chat completion. `on_text` receives text as it arrives.
    pub fn chat(&self, cfg: &Config, model: &str, msgs: &[Msg], tools: &[Value], on_text: &mut dyn FnMut(&str)) -> Result<Reply, BackendError> {
        INTERRUPT.store(false, Ordering::SeqCst);
        match self.kind {
            BackendKind::Ollama => self.chat_ollama(cfg, model, msgs, tools, on_text),
            BackendKind::Openai => self.chat_openai(cfg, model, msgs, tools, on_text),
        }
    }

    fn chat_ollama(&self, cfg: &Config, model: &str, msgs: &[Msg], tools: &[Value], on_text: &mut dyn FnMut(&str)) -> Result<Reply, BackendError> {
        let messages: Vec<Value> = msgs.iter().map(|m| {
            let mut o = json!({"role": m.role, "content": m.content});
            if !m.tool_calls.is_empty() {
                o["tool_calls"] = m.tool_calls.iter().map(|c| json!({"function": {"name": c.name, "arguments": c.args}})).collect();
            }
            if let Some(n) = &m.tool_name { o["tool_name"] = json!(n); }
            o
        }).collect();
        let mut body = json!({
            "model": model, "messages": messages, "stream": true,
            "options": {"num_ctx": cfg.num_ctx, "temperature": cfg.temperature},
        });
        if !tools.is_empty() { body["tools"] = json!(tools); }

        let resp = self.req("POST", "/api/chat").send_json(body).map_err(Self::err)?;
        let mut reply = Reply { content: String::new(), tool_calls: vec![], usage: Usage::default(), interrupted: false };
        for line in BufReader::new(resp.into_reader()).lines() {
            if INTERRUPT.load(Ordering::SeqCst) { reply.interrupted = true; break; }
            let line = line.map_err(|e| BackendError::Other(e.to_string()))?;
            if line.trim().is_empty() { continue; }
            let v: Value = serde_json::from_str(&line).map_err(|e| BackendError::Other(format!("bad stream chunk: {e}")))?;
            if let Some(e) = v["error"].as_str() { return Err(BackendError::Other(e.into())); }
            if let Some(t) = v["message"]["content"].as_str() {
                if !t.is_empty() { on_text(t); reply.content.push_str(t); }
            }
            for c in v["message"]["tool_calls"].as_array().cloned().unwrap_or_default() {
                let args = match &c["function"]["arguments"] {
                    Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
                    other => other.clone(),
                };
                reply.tool_calls.push(ToolCall {
                    id: format!("call_{}", reply.tool_calls.len()),
                    name: c["function"]["name"].as_str().unwrap_or("").into(),
                    args,
                });
            }
            if v["done"].as_bool() == Some(true) {
                let ec = v["eval_count"].as_u64().unwrap_or(0) as usize;
                let ed = v["eval_duration"].as_f64().unwrap_or(0.0) / 1e9;
                reply.usage = Usage {
                    prompt_tokens: v["prompt_eval_count"].as_u64().unwrap_or(0) as usize,
                    completion_tokens: ec,
                    tok_per_s: if ed > 0.0 { ec as f64 / ed } else { 0.0 },
                };
            }
        }
        Ok(reply)
    }

    fn chat_openai(&self, cfg: &Config, model: &str, msgs: &[Msg], tools: &[Value], on_text: &mut dyn FnMut(&str)) -> Result<Reply, BackendError> {
        let messages: Vec<Value> = msgs.iter().map(|m| {
            let mut o = json!({"role": m.role, "content": m.content});
            if !m.tool_calls.is_empty() {
                o["tool_calls"] = m.tool_calls.iter().map(|c| json!({
                    "id": c.id, "type": "function",
                    "function": {"name": c.name, "arguments": c.args.to_string()}
                })).collect();
            }
            if let Some(id) = &m.tool_call_id { o["tool_call_id"] = json!(id); }
            o
        }).collect();
        let mut body = json!({
            "model": model, "messages": messages, "stream": true,
            "temperature": cfg.temperature,
            "stream_options": {"include_usage": true},
        });
        if !tools.is_empty() { body["tools"] = json!(tools); }

        let start = Instant::now();
        let mut first_token: Option<Instant> = None;
        let resp = self.req("POST", "/chat/completions").send_json(body).map_err(Self::err)?;
        let mut reply = Reply { content: String::new(), tool_calls: vec![], usage: Usage::default(), interrupted: false };
        // tool call fragments arrive spread over many chunks, keyed by index
        let mut partial: Vec<(String, String, String)> = vec![]; // (id, name, args json text)
        let mut streamed_chunks = 0usize;

        for line in BufReader::new(resp.into_reader()).lines() {
            if INTERRUPT.load(Ordering::SeqCst) { reply.interrupted = true; break; }
            let line = line.map_err(|e| BackendError::Other(e.to_string()))?;
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" { break; }
            let v: Value = match serde_json::from_str(data) { Ok(v) => v, Err(_) => continue };
            if let Some(e) = v.get("error") { return Err(BackendError::Other(e.to_string())); }
            if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                reply.usage.prompt_tokens = u["prompt_tokens"].as_u64().unwrap_or(0) as usize;
                reply.usage.completion_tokens = u["completion_tokens"].as_u64().unwrap_or(0) as usize;
            }
            let delta = &v["choices"][0]["delta"];
            if let Some(t) = delta["content"].as_str() {
                if !t.is_empty() {
                    first_token.get_or_insert_with(Instant::now);
                    streamed_chunks += 1;
                    on_text(t);
                    reply.content.push_str(t);
                }
            }
            for tc in delta["tool_calls"].as_array().cloned().unwrap_or_default() {
                let i = tc["index"].as_u64().unwrap_or(partial.len() as u64) as usize;
                while partial.len() <= i { partial.push(Default::default()); }
                if let Some(id) = tc["id"].as_str() { partial[i].0 = id.into(); }
                if let Some(n) = tc["function"]["name"].as_str() { partial[i].1.push_str(n); }
                if let Some(a) = tc["function"]["arguments"].as_str() { partial[i].2.push_str(a); }
            }
        }
        for (i, (id, name, args)) in partial.into_iter().enumerate() {
            if name.is_empty() { continue; }
            reply.tool_calls.push(ToolCall {
                id: if id.is_empty() { format!("call_{i}") } else { id },
                name,
                args: serde_json::from_str(if args.trim().is_empty() { "{}" } else { &args }).unwrap_or(json!({"_raw": args})),
            });
        }
        let gen_secs = first_token.map(|t| t.elapsed()).unwrap_or_else(|| start.elapsed()).as_secs_f64();
        let toks = if reply.usage.completion_tokens > 0 { reply.usage.completion_tokens } else { streamed_chunks };
        reply.usage.tok_per_s = if gen_secs > 0.0 { toks as f64 / gen_secs } else { 0.0 };
        Ok(reply)
    }
}
