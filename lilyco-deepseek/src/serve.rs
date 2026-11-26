//! `ldeepseek serve` —— 基于 `lilyco-webchat-core` 的 OpenAI 兼容本地端点（默认 `127.0.0.1:8791/v1`）。

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
}

struct Config {
    cdp: String,
    api_key: Option<String>,
    queue: SerialQueue,
    sessions: SessionStore,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
    let full_prompt = transcript(&messages);
    let (sys, turns) = split_chat_messages(&messages);
    let anchor = session_anchor(&sys, &turns);
    let (bound_cid, send_prompt) = match anchor
        .and_then(|k| cfg.sessions.get(k))
        .filter(|b| turns.len() > b.sent_turns)
    {
        Some(b) => (Some(b.conversation_id), format_delta_turns(&turns[b.sent_turns..])),
        None => (None, cap_transcript(&full_prompt, DEFAULT_MAX_CHAT_CHARS)),
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
        let fp = full_prompt.clone();
        let joined = tokio::task::spawn_blocking(move || {
            let _hold = queue_guard;
            let (tx, _rx) = std::sync::mpsc::channel();
            let ctx = Context::from_sender(tx);
            let res = crate::ask_web(
                &send_prompt,
                deep_think,
                bound_cid.as_deref(),
                &cfg2.cdp,
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
                let text = res["text"].as_str().unwrap_or("").to_string();
                let usage = estimated_usage(&fp, &text);
                Json(json!({
                    "id": id,
                    "object": "chat.completion",
                    "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "message": { "role": "assistant", "content": text }, "finish_reason": "stop" }],
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
        while let Some(item) = rx.recv().await {
            match item {
                Ok(delta) => {
                    full_out.push_str(&delta);
                    let chunk = json!({
                        "id": id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{ "index": 0, "delta": { "content": delta }, "finish_reason": Value::Null }],
                    }).to_string();
                    yield Ok::<Event, Infallible>(Event::default().data(chunk));
                }
                Err(msg) => {
                    yield Ok(Event::default().data(json!({ "error": { "message": msg, "type": "ldeepseek_error" } }).to_string()));
                    break;
                }
            }
        }
        let usage = estimated_usage(&fp, &full_out);
        let end = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
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
