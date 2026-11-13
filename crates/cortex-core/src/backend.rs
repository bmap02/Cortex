//! Talks to the model server. Two wire formats:
//! - Ollama native: POST {base}/api/chat, newline-delimited JSON stream
//! - OpenAI-compatible: POST {base}/chat/completions, server-sent events

use crate::cancel::CancelHandle;
use crate::config::{BackendKind, Config};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

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

#[derive(Default)]
pub struct Reply {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub interrupted: bool,
}

/// One update from a model download: what Ollama is doing, and bytes done of the current part
/// (both 0 for steps without a size, such as "verifying sha256 digest").
#[derive(Debug, Clone, PartialEq)]
pub struct PullProgress {
    pub status: String,
    pub completed: u64,
    pub total: u64,
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
    /// The server's embedding model (Some(None): it has none), once it has answered.
    embedding_model: std::sync::Mutex<Option<Option<String>>>,
}

impl Backend {
    pub fn new(cfg: &Config) -> Backend {
        Backend {
            kind: cfg.backend,
            base: cfg.base_url.trim_end_matches('/').to_string(),
            key: cfg.api_key(),
            agent: ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).build(),
            embedding_model: std::sync::Mutex::new(None),
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

    /// Ollama only: download a model. `on_progress` gets each update; `cancel` stops it. Pulls
    /// are started by the user, so they aren't subject to the agent's network switch.
    pub fn pull(&self, model: &str, cancel: &CancelHandle, on_progress: &mut dyn FnMut(&PullProgress)) -> Result<(), BackendError> {
        self.ollama_only()?;
        let resp = self.req("POST", "/api/pull").send_json(json!({"model": model, "stream": true})).map_err(Self::err)?;
        Self::follow_status(resp, cancel, on_progress, "the download ended before it finished")
    }


    /// Vectors for `texts` from embedding model `model`, one per text, in order.
    pub fn embed(&self, model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError> {
        let bad = |what: &str| BackendError::Other(format!("the server sent {what} instead of embeddings"));
        let path = match self.kind { BackendKind::Ollama => "/api/embed", BackendKind::Openai => "/embeddings" };
        // the first call loads the model; after that one is quick
        let v: Value = self.req("POST", path).timeout(Duration::from_secs(60)).send_json(json!({"model": model, "input": texts}))
            .map_err(Self::err)?.into_json().map_err(|_| bad("something unreadable"))?;
        let list: Vec<Value> = match self.kind {
            BackendKind::Ollama => v["embeddings"].as_array().cloned().ok_or_else(|| bad("no list"))?,
            BackendKind::Openai => {
                let mut data = v["data"].as_array().cloned().ok_or_else(|| bad("no list"))?;
                data.sort_by_key(|d| d["index"].as_u64().unwrap_or(0));
                data.into_iter().map(|d| d["embedding"].clone()).collect()
            }
        };
        let vectors: Vec<Vec<f32>> = list.iter().map(|e| e.as_array().map(|xs| xs.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect()))
            .collect::<Option<_>>().ok_or_else(|| bad("a vector that isn't one"))?;
        if vectors.len() != texts.len() { return Err(bad(&format!("{} vectors for {} texts", vectors.len(), texts.len()))); }
        Ok(vectors)
    }

    /// Ollama only: whether the server has `model` (also under its :latest tag).
    pub fn has_model(&self, model: &str) -> bool {
        self.list_models().map(|m| m.iter().any(|m| m.name == model || m.name == format!("{model}:latest"))).unwrap_or(false)
    }

    fn ollama_only(&self) -> Result<(), BackendError> {
        if self.kind == BackendKind::Ollama { return Ok(()); }
        Err(BackendError::Other("Cortex installs models through Ollama; load this one in your server instead".into()))
    }

    /// Read a streamed status reply (pull, create) to the end. Ok once Ollama says "success".
    fn follow_status(resp: ureq::Response, cancel: &CancelHandle, on_progress: &mut dyn FnMut(&PullProgress), unfinished: &str) -> Result<(), BackendError> {
        let mut finished = false;
        for line in BufReader::new(resp.into_reader()).lines() {
            if cancel.is_cancelled() { return Err(BackendError::Other("download stopped".into())); }
            let line = line.map_err(|e| BackendError::Other(e.to_string()))?;
            let v: Value = serde_json::from_str(&line).unwrap_or_default();
            if let Some(e) = v["error"].as_str() { return Err(BackendError::Other(e.into())); }
            let status = v["status"].as_str().unwrap_or("").to_string();
            finished = status == "success";
            on_progress(&PullProgress { status, completed: v["completed"].as_u64().unwrap_or(0), total: v["total"].as_u64().unwrap_or(0) });
        }
        if finished { Ok(()) } else { Err(BackendError::Other(unfinished.into())) }
    }

    /// Ollama only: send a model file to the server as a blob (skipped if it has it already).
    pub fn upload_blob(&self, path: &std::path::Path, sha256: &str, size: u64, cancel: &CancelHandle,
                       on_progress: &mut dyn FnMut(&PullProgress)) -> Result<(), BackendError> {
        self.ollama_only()?;
        let blob = format!("/api/blobs/sha256:{sha256}");
        if self.req("HEAD", &blob).call().is_ok() { return Ok(()); }
        let file = std::fs::File::open(path).map_err(|e| BackendError::Other(format!("couldn't read {}: {e}", path.display())))?;
        let reader = ProgressReader { inner: file, done: 0, total: size, cancel, on_progress };
        self.req("POST", &blob).set("Content-Length", &size.to_string()).send(reader).map_err(|e| match e {
            ureq::Error::Transport(_) if cancel.is_cancelled() => BackendError::Other("import stopped".into()),
            e => Self::err(e),
        })?;
        Ok(())
    }

    /// Ollama only: create model `name` from an uploaded GGUF blob.
    pub fn create_model(&self, name: &str, file_name: &str, sha256: &str, on_progress: &mut dyn FnMut(&PullProgress)) -> Result<(), BackendError> {
        self.ollama_only()?;
        let body = json!({"model": name, "files": {file_name: format!("sha256:{sha256}")}, "stream": true});
        let resp = self.req("POST", "/api/create").send_json(body).map_err(Self::err)?;
        // not cancellable: once the file is in, creating the model takes a moment and is all or nothing
        Self::follow_status(resp, &CancelHandle::new(), on_progress, "Ollama stopped before the model was created")
    }

    /// Stream a chat completion. `on_text` receives text as it arrives; `cancel` ends the
    /// stream early (the reply comes back marked `interrupted`).
    pub fn chat(&self, cfg: &Config, model: &str, msgs: &[Msg], tools: &[Value], cancel: &CancelHandle, on_text: &mut dyn FnMut(&str)) -> Result<Reply, BackendError> {
        // Some models now and then write a tool call the server can't parse; another sample usually works.
        const ATTEMPTS: usize = 3;
        for attempt in 1..=ATTEMPTS {
            let result = match self.kind {
                BackendKind::Ollama => self.chat_ollama(cfg, model, msgs, tools, cancel, on_text),
                BackendKind::Openai => self.chat_openai(cfg, model, msgs, tools, cancel, on_text),
            };
            match result {
                Err(BackendError::Other(m)) if attempt < ATTEMPTS && is_tool_parse_error(&m) && !cancel.is_cancelled() => continue,
                other => return other,
            }
        }
        unreachable!("the last attempt always returns")
    }

    fn chat_ollama(&self, cfg: &Config, model: &str, msgs: &[Msg], tools: &[Value], cancel: &CancelHandle, on_text: &mut dyn FnMut(&str)) -> Result<Reply, BackendError> {
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
        if let Some(think) = cfg.think { body["think"] = json!(think); }

        let resp = self.req("POST", "/api/chat").send_json(body).map_err(Self::err)?;
        let mut reply = Reply::default();
        let mut thinking = String::new();
        for line in BufReader::new(resp.into_reader()).lines() {
            if cancel.is_cancelled() { reply.interrupted = true; break; }
            let line = line.map_err(|e| BackendError::Other(e.to_string()))?;
            if line.trim().is_empty() { continue; }
            let v: Value = serde_json::from_str(&line).map_err(|e| BackendError::Other(format!("bad stream chunk: {e}")))?;
            if let Some(e) = v["error"].as_str() { return Err(BackendError::Other(e.into())); }
            if let Some(t) = v["message"]["content"].as_str()
                && !t.is_empty() { on_text(t); reply.content.push_str(t); }
            if let Some(t) = v["message"]["thinking"].as_str() { thinking.push_str(t); }
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
        reply.use_thinking_if_empty(&thinking, on_text);
        Ok(reply)
    }

    fn chat_openai(&self, cfg: &Config, model: &str, msgs: &[Msg], tools: &[Value], cancel: &CancelHandle, on_text: &mut dyn FnMut(&str)) -> Result<Reply, BackendError> {
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
        let mut reply = Reply::default();
        // tool call fragments arrive spread over many chunks, keyed by index
        let mut partial: Vec<(String, String, String)> = vec![]; // (id, name, args json text)
        let mut streamed_chunks = 0usize;
        let mut thinking = String::new();

        for line in BufReader::new(resp.into_reader()).lines() {
            if cancel.is_cancelled() { reply.interrupted = true; break; }
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
            if let Some(t) = delta["content"].as_str()
                && !t.is_empty() {
                    first_token.get_or_insert_with(Instant::now);
                    streamed_chunks += 1;
                    on_text(t);
                    reply.content.push_str(t);
                }
            // llama.cpp, vLLM and LM Studio stream reasoning in one of these fields
            if let Some(t) = delta["reasoning_content"].as_str().or(delta["reasoning"].as_str()) { thinking.push_str(t); }
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
        reply.use_thinking_if_empty(&thinking, on_text);
        Ok(reply)
    }
}

/// The server rejected the model's tool call as malformed (e.g. Ollama's
/// "XML syntax error ... element <function> closed by </parameter>").
fn is_tool_parse_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("xml syntax error") || (m.contains("tool call") && (m.contains("parse") || m.contains("invalid")))
}

impl Reply {
    /// Some models (or their Ollama templates) put the whole answer in the thinking/reasoning
    /// channel and leave the reply empty. Show that text as the answer rather than nothing.
    fn use_thinking_if_empty(&mut self, thinking: &str, on_text: &mut dyn FnMut(&str)) {
        let t = thinking.trim();
        if self.content.trim().is_empty() && self.tool_calls.is_empty() && !self.interrupted && !t.is_empty() {
            on_text(t);
            self.content = t.to_string();
        }
    }
}

/// Whether a model's name says it makes embeddings rather than chat: nomic-embed-text,
/// mxbai-embed-large, snowflake-arctic-embed, granite-embedding, LM Studio's text-embedding-…,
/// all-minilm and the bge family.
pub fn is_embedding_model(name: &str) -> bool {
    let n = name.to_lowercase();
    let base = n.rsplit('/').next().unwrap_or(&n);
    n.contains("embed") || base.starts_with("all-minilm") || base.starts_with("bge")
}

/// Recall's embeddings come from the model server. Its embedding model is looked for the first time
/// one is needed and remembered for the session; with none, recall uses keywords alone. A server
/// that doesn't answer isn't taken to have none: it's asked again next time.
impl crate::memory::Embedder for Backend {
    fn model(&self) -> Option<String> {
        let mut known = self.embedding_model.lock().unwrap_or_else(|e| e.into_inner());
        if known.is_none() && let Ok(models) = self.list_models() {
            *known = Some(models.into_iter().map(|m| m.name).find(|n| is_embedding_model(n)));
        }
        known.clone().flatten()
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let model = crate::memory::Embedder::model(self).ok_or("no embedding model")?;
        Backend::embed(self, &model, texts).map_err(|e| e.to_string())
    }
}

/// Reads a file being uploaded, reporting progress and stopping when cancelled.
struct ProgressReader<'a, R> {
    inner: R,
    done: u64,
    total: u64,
    cancel: &'a CancelHandle,
    on_progress: &'a mut dyn FnMut(&PullProgress),
}

impl<R: std::io::Read> std::io::Read for ProgressReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // not ErrorKind::Interrupted: copy loops retry those, and the upload would never end
        if self.cancel.is_cancelled() { return Err(std::io::Error::other("import stopped")); }
        let n = self.inner.read(buf)?;
        self.done += n as u64;
        (self.on_progress)(&PullProgress { status: "uploading to Ollama".into(), completed: self.done, total: self.total });
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognizes_malformed_tool_calls() {
        assert!(is_tool_parse_error("server returned 500: XML syntax error on line 7: element <function> closed by </parameter>"));
        assert!(is_tool_parse_error("error parsing tool call: invalid JSON"));
        assert!(!is_tool_parse_error("can't reach the server: connection refused"));
    }
    #[test]
    fn embedding_models_are_told_apart_from_chat_models_by_name() {
        for m in ["nomic-embed-text:latest", "mxbai-embed-large", "snowflake-arctic-embed:110m", "granite-embedding:30m",
                  "all-minilm", "bge-m3", "text-embedding-nomic-embed-text-v1.5", "hf.co/CompendiumLabs/bge-base-en-v1.5-gguf"] {
            assert!(super::is_embedding_model(m), "{m}");
        }
        for m in ["qwen2.5:7b", "llama3.1:8b", "lukey03/qwen3.5-9b-abliterated-vision:latest", "gemma3:4b"] {
            assert!(!super::is_embedding_model(m), "{m}");
        }
    }

    #[test]
    fn empty_reply_falls_back_to_thinking() {
        let mut shown = String::new();
        let mut r = Reply::default();
        r.use_thinking_if_empty("  Here are the files.\n", &mut |t| shown.push_str(t));
        assert_eq!((r.content.as_str(), shown.as_str()), ("Here are the files.", "Here are the files."));
        // a real answer, or a tool call, wins over thinking
        let mut r = Reply { content: "answer".into(), ..Reply::default() };
        r.use_thinking_if_empty("reasoning", &mut |_| {});
        assert_eq!(r.content, "answer");
        let mut r = Reply { tool_calls: vec![ToolCall { id: "1".into(), name: "list_dir".into(), args: json!({}) }], ..Reply::default() };
        r.use_thinking_if_empty("reasoning", &mut |_| {});
        assert!(r.content.is_empty());
    }
}
