//! `ldeepseek serve` —— 基于 `lilyco-webchat-core` 的 OpenAI 兼容本地端点（默认 `127.0.0.1:8791/v1`）。

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{extract::State, Json, Router};
use lilyco::prelude::*;
use lilyco_webchat_core::guard::{verify_bearer, SerialQueue};
use lilyco_webchat_core::session::{
    cap_transcript, estimated_usage, format_delta_turns, session_anchor, split_chat_messages,
    transcript, BoundSession, Diff, SessionStore, DEFAULT_MAX_CHAT_CHARS,
};
use serde_json::{json, Value};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8791;
const TOOLS_PROMPT_MAX_CHARS: usize = 40_000;
pub const MODEL_CHAT: &str = "ldeepseek:chat";
pub const MODEL_REASONER: &str = "ldeepseek:reasoner";

#[derive(App)]
#[app(
    name = "serve",
    run = "run_serve",
    safety = "t1",
    about = "Expose chat.deepseek.com as a local OpenAI-compatible HTTP endpoint (127.0.0.1:8791/v1) with serial queue, optional Bearer auth, session continuation, and token estimation. Safety T1."
)]
pub struct Serve {
    #[arg(about = "Port to listen on (0 means 8791)", default = 0)]
    port: u16,
    #[arg(about = "Bind host (empty means 127.0.0.1)", default = "")]
    host: String,
    #[arg(about = "CDP endpoint, e.g. http://127.0.0.1:9229", default = "")]
    cdp: String,
    #[arg(about = "Optional Bearer API key (also reads LDEEPSEEK_SERVE_API_KEY)", default = "")]
    api_key: String,
    #[arg(about = "Max queued requests (0 means 16)", default = 0)]
    max_queue: u16,
    #[arg(
        about = "Optional exported cookies/localStorage JSON file for headless server mode",
        default = ""
    )]
    session_file: std::path::PathBuf,
}

struct Config {
    cdp: String,
    api_key: Option<String>,
    session_file: Option<std::path::PathBuf>,
    queue: SerialQueue,
    sessions: SessionStore,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn cap_string(s: String, max: usize) -> String {
    let total = s.chars().count();
    if total <= max { return s; }
    let half = max / 2;
    let tail_at = total.saturating_sub(half);
    let head: String = s.chars().take(half).collect();
    let tail: String = s.chars().skip(tail_at).collect();
    format!("{head}\n\n[... 工具定义过长，已折叠中间约 {} 字符 ...]\n\n{tail}", total.saturating_sub(half + half))
}

fn tool_name_set(tools: &[Value]) -> HashSet<String> {
    tools
        .iter()
        .filter_map(|t| t.get("function").and_then(|f| f.get("name")).and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn tools_prompt(tools: &[Value]) -> String {
    if tools.is_empty() { return String::new(); }
    let mut lines = Vec::new();
    lines.push("你正在以 OpenAI 兼容工具代理模式运行。".to_string());
    lines.push("当且仅当你需要调用工具时，只输出一个 JSON 代码块（三反引号 json），内容必须是严格 JSON，不要输出任何解释文字：".to_string());
    lines.push(r#"{"tool_calls":[{"id":"call_1","type":"function","function":{"name":"<工具名>","arguments":"<参数 JSON 对象序列化后的字符串>"}}]}"#.to_string());
    lines.push("arguments 必须是 JSON 对象字符串，例如 {\"description\":\"...\",\"command\":\"...\"} 序列化后的结果。若无需调用工具，直接正常回答。".to_string());
    lines.push("可用工具定义如下：".to_string());
    for t in tools {
        if let Some(f) = t.get("function") {
            let compact = serde_json::to_string(f).unwrap_or_default();
            if !compact.is_empty() { lines.push(format!("- {compact}")); }
        }
    }
    cap_string(lines.join("\n"), TOOLS_PROMPT_MAX_CHARS)
}

fn extract_json_candidates(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let fence = "```";
    let mut cursor = 0usize;
    while let Some(rel) = text[cursor..].find(fence) {
        let start = cursor + rel;
        let after = start + 3;
        let line_end = text[after..].find('\n').map(|p| after + p + 1).unwrap_or(after);
        let lang = text[after..line_end].trim();
        if !lang.is_empty() && !lang.eq_ignore_ascii_case("json") { cursor = after; continue; }
        if let Some(close_rel) = text[line_end..].find(fence) {
            let close = line_end + close_rel;
            let candidate = text[line_end..close].trim().to_string();
            if !candidate.is_empty() { out.push((candidate.clone(), text[start..close + 3].to_string())); }
            cursor = close + 3;
        } else { break; }
    }
    let trimmed = text.trim();
    if !trimmed.is_empty() { out.push((trimmed.to_string(), trimmed.to_string())); }
    out
}

fn normalize_tool_call(c: &Value) -> Option<(String, String)> {
    let name_v = c.get("function").and_then(|f| f.get("name"))
        .or_else(|| c.get("name")).or_else(|| c.get("tool")).or_else(|| c.get("tool_name"))?;
    let name = name_v.as_str()?.to_string();
    if name.trim().is_empty() { return None; }
    let args_v = c.get("function").and_then(|f| f.get("arguments"))
        .or_else(|| c.get("arguments")).or_else(|| c.get("args")).or_else(|| c.get("input"));
    let args = match args_v {
        Some(Value::String(x)) => x.clone(),
        Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string()),
        None => "{}".to_string(),
    };
    let args = if args.trim().is_empty() { "{}".to_string() } else { args };
    if serde_json::from_str::<Value>(&args).is_err() { return None; }
    Some((name, args))
}

fn parse_tool_candidate(candidate: &str, allowed: &HashSet<String>) -> Option<Vec<Value>> {
    let v: Value = serde_json::from_str(candidate).ok()?;
    let calls: Vec<Value> = match &v {
        Value::Array(a) => a.clone(),
        Value::Object(o) => {
            if let Some(tc) = o.get("tool_calls").and_then(Value::as_array) { tc.clone() }
            else if let Some(tc) = o.get("calls").and_then(Value::as_array) { tc.clone() }
            else { vec![v.clone()] }
        },
        _ => return None,
    };
    let mut out = Vec::new();
    for (i, c) in calls.iter().enumerate() {
        if let Some((name, args)) = normalize_tool_call(c) {
            if !allowed.contains(&name) { continue; }
            let id = c.get("id").and_then(Value::as_str)
                .filter(|x| !x.trim().is_empty()).map(str::to_string)
                .unwrap_or_else(|| format!("call_lds_{}", i + 1));
            out.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": args }
            }));
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn parse_tool_calls(text: &str, allowed: &HashSet<String>) -> (Vec<Value>, String) {
    if allowed.is_empty() { return (Vec::new(), text.to_string()); }
    for (candidate, span) in extract_json_candidates(text) {
        if let Some(calls) = parse_tool_candidate(&candidate, allowed) {
            let visible = text.replace(&span, "").trim().to_string();
            return (calls, visible);
        }
    }
    (Vec::new(), text.to_string())
}

fn api_error(status: StatusCode, msg: impl Into<String>, kind: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "message": msg.into(), "type": kind, "code": null } })),
    )
        .into_response()
}

fn check_auth(headers: &HeaderMap, cfg: &Config) -> Result<(), Box<Response>> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if verify_bearer(raw, cfg.api_key.as_deref()) {
        Ok(())
    } else {
        Err(Box::new(api_error(
            StatusCode::UNAUTHORIZED,
            "缺少或无效的 Authorization: Bearer <api_key>",
            "authentication_error",
        )))
    }
}

fn run_serve(app: &Serve, ctx: &Context) -> Result<Value, AppError> {
    let host = if app.host.trim().is_empty() {
        DEFAULT_HOST
    } else {
        app.host.trim()
    };
    let port = if app.port == 0 { DEFAULT_PORT } else { app.port };
    let bind = format!("{host}:{port}");
    let api_key = if !app.api_key.trim().is_empty() {
        Some(app.api_key.trim().to_string())
    } else {
        std::env::var("LDEEPSEEK_SERVE_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let cfg = Arc::new(Config {
        cdp: app.cdp.trim().to_string(),
        api_key,
        session_file: crate::resolve_session_file(&app.session_file),
        queue: SerialQueue::new(if app.max_queue == 0 {
            16
        } else {
            app.max_queue as usize
        }),
        sessions: SessionStore::default(),
    });
    ctx.log(
        LogLevel::Info,
        format!("ldeepseek serve: http://{bind}/v1 ({MODEL_CHAT} / {MODEL_REASONER})"),
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| AppError::Runtime(format!("创建 tokio 运行时失败: {e}")))?;
    let bind_addr = bind.clone();
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .map_err(|e| AppError::Runtime(format!("绑定 {bind_addr} 失败: {e}")))?;
        let app = Router::new()
            .route("/health", get(health))
            .route("/v1/models", get(models))
            .route("/v1/chat/completions", post(chat_completions))
            .with_state(cfg);
        axum::serve(listener, app)
            .await
            .map_err(|e| AppError::Runtime(format!("HTTP 异常退出: {e}")))
    })?;
    Ok(json!({ "bind": bind }))
}

async fn health(State(cfg): State<Arc<Config>>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "service": "ldeepseek",
        "models": [MODEL_CHAT, MODEL_REASONER],
        "auth_required": cfg.api_key.is_some(),
        "queue_depth": cfg.queue.depth(),
        "max_queue": cfg.queue.max_queue(),
    }))
}

async fn models(State(cfg): State<Arc<Config>>, headers: HeaderMap) -> Response {
    if let Err(r) = check_auth(&headers, &cfg) {
        return *r;
    }
    Json(json!({
        "object": "list",
        "data": [
            { "id": MODEL_CHAT, "object": "model", "owned_by": "ldeepseek", "context_window": 64000 },
            { "id": MODEL_REASONER, "object": "model", "owned_by": "ldeepseek", "context_window": 64000 },
        ]
    }))
    .into_response()
}

async fn chat_completions(
    State(cfg): State<Arc<Config>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(r) = check_auth(&headers, &cfg) {
        return *r;
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(MODEL_CHAT)
        .to_string();
    let deep_think = model.contains("reasoner") || model.contains("r1");
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if messages.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "messages 不能为空",
            "invalid_request_error",
        );
    }
    let tools = body.get("tools").and_then(Value::as_array).cloned().unwrap_or_default();
    let tool_names = tool_name_set(&tools);
    let tools_txt = tools_prompt(&tools);
    let full_prompt = transcript(&messages);
    let combined_prompt = if tools_txt.is_empty() {
        full_prompt.clone()
    } else {
        format!("{tools_txt}\n\n---\n\n{full_prompt}")
    };
    let (sys, turns) = split_chat_messages(&messages);
    let anchor = session_anchor(&sys, &turns);
    let (bound_cid, send_prompt) = match anchor
        .and_then(|k| cfg.sessions.get(k))
        .filter(|b| turns.len() > b.sent_turns)
    {
        Some(b) => (Some(b.conversation_id), format_delta_turns(&turns[b.sent_turns..])),
        None => (None, cap_transcript(&combined_prompt, DEFAULT_MAX_CHAT_CHARS)),
    };
    let total_turns = turns.len();
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let queue_guard = match cfg.queue.enter() {
        Ok(g) => g,
        Err((cur, max)) => {
            return api_error(
                StatusCode::TOO_MANY_REQUESTS,
                format!("串行队列已满 ({cur}/{max})，请稍后再试"),
                "queue_full",
            )
        }
    };

    let created = now_secs();
    let id = format!("chatcmpl-ldeepseek-{created}");

    if !stream {
        let Some(_permit) = cfg.queue.acquire().await else {
            return api_error(StatusCode::SERVICE_UNAVAILABLE, "队列关闭", "service_unavailable");
        };
        let cfg2 = cfg.clone();
        let fp = combined_prompt.clone();
        let joined = tokio::task::spawn_blocking(move || {
            let _hold = queue_guard;
            let (tx, _rx) = std::sync::mpsc::channel();
            let ctx = Context::from_sender(tx);
            let res = crate::ask_web(
                &send_prompt,
                deep_think,
                bound_cid.as_deref(),
                &cfg2.cdp,
                cfg2.session_file.as_deref(),
                &ctx,
                &mut |_| {},
            )?;
            if let (Some(k), Some(cid)) = (anchor, res["conversation_id"].as_str()) {
                cfg2.sessions.insert(
                    k,
                    BoundSession {
                        conversation_id: cid.to_string(),
                        sent_turns: total_turns + 1,
                        branch_len: 0,
                    },
                );
            }
            Ok::<Value, AppError>(res)
        })
        .await;
        return match joined {
            Ok(Ok(res)) => {
                let raw = res["text"].as_str().unwrap_or("").to_string();
                let (calls, visible) = parse_tool_calls(&raw, &tool_names);
                let usage = estimated_usage(&fp, &raw);
                let mut message = json!({ "role": "assistant", "content": visible });
                let finish_reason = if calls.is_empty() {
                    "stop"
                } else {
                    message["tool_calls"] = json!(calls);
                    "tool_calls"
                };
                Json(json!({
                    "id": id,
                    "object": "chat.completion",
                    "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
                    "usage": usage,
                }))
                .into_response()
            }
            Ok(Err(e)) => api_error(StatusCode::BAD_GATEWAY, format!("{e}"), "upstream_error"),
            Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"), "internal_error"),
        };
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<String, String>>(64);
    let cfg2 = cfg.clone();
    let fp = full_prompt.clone();
    let gate = cfg.queue.gate();
    let worker = tokio::spawn(async move {
        let _guard = queue_guard;
        let _permit = gate.acquire_owned().await.ok();
        let tx2 = tx.clone();
        let res = tokio::task::spawn_blocking(move || {
            let (ptx, _prx) = std::sync::mpsc::channel();
            let ctx = Context::from_sender(ptx);
            let mut diff = Diff::default();
            let out = crate::ask_web(
                &send_prompt,
                deep_think,
                bound_cid.as_deref(),
                &cfg2.cdp,
                cfg2.session_file.as_deref(),
                &ctx,
                &mut |full| {
                    if let Some(d) = diff.next(full) {
                        let _ = tx2.blocking_send(Ok(d));
                    }
                },
            )?;
            if let (Some(k), Some(cid)) = (anchor, out["conversation_id"].as_str()) {
                cfg2.sessions.insert(
                    k,
                    BoundSession {
                        conversation_id: cid.to_string(),
                        sent_turns: total_turns + 1,
                        branch_len: 0,
                    },
                );
            }
            Ok::<String, AppError>(out["text"].as_str().unwrap_or("").to_string())
        })
        .await;
        if let Ok(Err(e)) = res {
            let _ = tx.send(Err(format!("{e}"))).await;
        }
    });

    let stream_body = async_stream::stream! {
        let mut full_out = String::new();
        let mut stream_error: Option<String> = None;
        while let Some(item) = rx.recv().await {
            match item {
                Ok(delta) => full_out.push_str(&delta),
                Err(msg) => { stream_error = Some(msg); break; }
            }
        }
        if let Some(msg) = stream_error {
            yield Ok(Event::default().data(json!({ "error": { "message": msg, "type": "ldeepseek_error" } }).to_string()));
            yield Ok(Event::default().data("[DONE]"));
            let _ = worker.await;
            return;
        }

        let (calls, visible) = parse_tool_calls(&full_out, &tool_names);
        if calls.is_empty() {
            if !visible.is_empty() {
                let chunk = json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "delta": { "role": "assistant", "content": visible }, "finish_reason": Value::Null }],
                }).to_string();
                yield Ok::<Event, Infallible>(Event::default().data(chunk));
            }
        } else {
            for (i, call) in calls.iter().enumerate() {
                let fncall = json!({
                    "index": i,
                    "id": call["id"],
                    "type": "function",
                    "function": {
                        "name": call["function"]["name"],
                        "arguments": call["function"]["arguments"],
                    }
                });
                let delta = if i == 0 {
                    json!({ "role": "assistant", "tool_calls": [fncall] })
                } else {
                    json!({ "tool_calls": [fncall] })
                };
                let chunk = json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "delta": delta, "finish_reason": Value::Null }],
                }).to_string();
                yield Ok::<Event, Infallible>(Event::default().data(chunk));
            }
        }
        let usage = estimated_usage(&fp, &full_out);
        let finish_reason = if calls.is_empty() { "stop" } else { "tool_calls" };
        let end = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{ "index": 0, "delta": {}, "finish_reason": finish_reason }],
            "usage": usage,
        }).to_string();
        yield Ok(Event::default().data(end));
        yield Ok(Event::default().data("[DONE]"));
        let _ = worker.await;
    };

    Sse::new(stream_body)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_call_json_fence() {
        let allowed = HashSet::from(["get_goal".to_string()]);
        let fence = String::from_utf8(vec![96, 96, 96]).unwrap();
        let payload = r#"{"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_goal","arguments":"{}"}}]}"#;
        let text = format!("explanation\n{0}json\n{payload}\n{0}", fence);
        let (calls, visible) = parse_tool_calls(&text, &allowed);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "get_goal");
        assert_eq!(calls[0]["function"]["arguments"], "{}");
        assert!(visible.contains("explanation"));
    }
}
