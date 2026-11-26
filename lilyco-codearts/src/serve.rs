//! `lcodearts serve` —— 把 CodeArts 账号开成一个 **OpenAI 兼容**的本地端口，
//! 并在后台按北京时间自动签到。
//!
//! 为什么需要这一层：dsh（DeepSeek Harness）只认几条线上协议，而 CodeArts 上游
//! 给的是标准 OpenAI SSE。这里把 `/v1/models`、`/v1/chat/completions` 包装好，
//! dsh 侧加一条 `api: openai-completions` 的 route 指到 `http://127.0.0.1:8793/v1`
//! 即可。
//!
//! 几个上游怪癖（见 CodeArts2API/docs/UPSTREAM.md）：
//! - `glm-5.3` 只支持 `stream=true`：这里**统一向上游发流式**，非流式客户端在本地聚合；
//! - 上游可能用 `HTTP 200 + error_code` 表达失败：聚合与透传都保留原始错误；
//! - `model-id` / `model-name` 必须与请求体 `model` 完全一致，由 `api` 层负责。
//!
//! 自动签到：启动后立即检查一次，此后每 `--checkin-interval` 秒检查一次；北京时间
//! `--checkin-after-hour` 点前跳过；每次以服务端可领取状态为准（领取接口幂等）。
//! `--no-auto-checkin` 可关。

use axum::extract::DefaultBodyLimit;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{extract::State, Json, Router};
use lilyco::prelude::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{api, checkin, cred, util};

const DEFAULT_PORT: u16 = 8793;
const DEFAULT_HOST: &str = "127.0.0.1";
const MODELS_CACHE_TTL: Duration = Duration::from_secs(300);

struct Config {
    api_key: Option<String>,
    models_cache: Mutex<Option<(Instant, Vec<Value>)>>,
}

#[derive(App)]
#[app(
    name = "serve",
    run = "run_serve",
    safety = "t1",
    about = "Expose the CodeArts account as a local OpenAI-compatible endpoint: GET /v1/models, POST /v1/chat/completions (streaming SSE or locally aggregated JSON), GET /v1/quota and GET /health. Requests spend credits according to the upstream model ratio. A background thread claims the daily USER_LOGIN activity (normally +1000 credits) after the configured Beijing hour, skipping days already claimed. Safety T1: every chat request consumes credits and the background thread claims a remote benefit."
)]
pub struct Serve {
    #[arg(about = "Port to listen on (0 means 8793)", default = 0)]
    port: u16,
    #[arg(
        about = "Bind address (empty means 127.0.0.1; this endpoint fronts your account credits)",
        default = ""
    )]
    host: String,
    #[arg(
        about = "Optional Bearer API key for /v1/* (also reads LCODEARTS_SERVE_API_KEY)",
        default = ""
    )]
    api_key: String,
    #[arg(
        about = "Disable the background daily check-in (enabled by default)",
        default = false
    )]
    no_auto_checkin: bool,
    #[arg(
        about = "In auto check-in, do not try before this hour (Beijing time)",
        default = 8
    )]
    checkin_after_hour: u32,
    #[arg(
        about = "Auto check-in poll interval in seconds (minimum 60)",
        default = 300
    )]
    checkin_interval: u64,
}

fn run_serve(app: &Serve, ctx: &Context) -> Result<Value, AppError> {
    let host = match app.host.trim() {
        "" => DEFAULT_HOST,
        h => h,
    };
    let port = if app.port == 0 {
        DEFAULT_PORT
    } else {
        app.port
    };
    let bind = format!("{host}:{port}");

    let cred = cred::load()?;
    if cred.expired() {
        return Err(AppError::InvalidInput(
            "本地记录的 CodeArts 凭据已过期：运行 `lcodearts login` 重新授权".to_string(),
        ));
    }
    let api_key = {
        let from_arg = app.api_key.trim().to_string();
        if !from_arg.is_empty() {
            Some(from_arg)
        } else {
            std::env::var("LCODEARTS_SERVE_API_KEY")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        }
    };
    if !app.no_auto_checkin {
        checkin::spawn_auto_loop(app.checkin_after_hour, app.checkin_interval);
        ctx.log(
            LogLevel::Info,
            format!(
                "自动签到已开启：北京时间 {}:00 后检查，每 {} 秒一次",
                app.checkin_after_hour.min(23),
                app.checkin_interval.max(60)
            ),
        );
    }

    ctx.log(
        LogLevel::Info,
        format!("OpenAI 兼容端: http://{bind}/v1（模型来自 /v1/model/builtin）"),
    );
    ctx.emit(Progress::Started {
        total: None,
        message: Some(format!("serve {bind}")),
    });

    let cfg = Arc::new(Config {
        api_key,
        models_cache: Mutex::new(None),
    });
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| AppError::Runtime(format!("创建 tokio 运行时失败: {e}")))?;
    rt.block_on(serve_on(bind.clone(), cfg))?;
    Ok(json!({ "bind": bind }))
}

async fn serve_on(bind: String, cfg: Arc<Config>) -> Result<(), AppError> {
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| AppError::Runtime(format!("绑定 `{bind}` 失败: {e}")))?;
    eprintln!("lcodearts serve: 监听 http://{bind}/v1（Ctrl-C 结束）");
    axum::serve(listener, router(cfg))
        .await
        .map_err(|e| AppError::Runtime(format!("HTTP 端异常退出: {e}")))
}

fn router(cfg: Arc<Config>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/v1", get(v1_index))
        .route("/v1/", get(v1_index))
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/quota", get(quota))
        .route("/v1/chat/completions", post(chat_completions))
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .with_state(cfg)
}

/// 根路径的说明页。`/v1/*` 是 API，不是聊天控制台；用户直接打开根地址时给他
/// baseURL、端点和 curl 示例，而不是一个空 404。
async fn index(State(cfg): State<Arc<Config>>, headers: HeaderMap) -> Html<String> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1:8793");
    let auth = if cfg.api_key.is_some() {
        "已开启：请求需要 <code>Authorization: Bearer &lt;key&gt;</code>"
    } else {
        "未开启（服务只监听回环地址）"
    };
    Html(
        INDEX_HTML
            .replace("__HOST__", host)
            .replace("__AUTH__", auth),
    )
}

async fn v1_index() -> Json<Value> {
    Json(json!({
        "object": "api",
        "message": "OpenAI-compatible CodeArts endpoint. Open / for usage.",
        "base_url": "/v1",
        "endpoints": ["/v1/models", "/v1/chat/completions", "/v1/quota", "/health"],
    }))
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="zh-CN"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>lcodearts · CodeArts 本地网关</title>
<style>
:root{color-scheme:light dark}
body{margin:0;font:15px/1.6 -apple-system,system-ui,"Segoe UI",sans-serif;background:#0b1020;color:#e6edf7}
main{max-width:860px;margin:0 auto;padding:48px 24px}
h1{font-size:32px;margin:0 0 8px}
h2{font-size:15px;margin:0 0 10px;color:#c7d5ea}
.badge{display:inline-block;background:#11331f;color:#5ee08a;border:1px solid #1f6b3a;border-radius:999px;padding:2px 10px;font-size:12px;margin-left:8px;vertical-align:middle}
.lead{color:#9fb0c7;margin:0 0 28px}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(260px,1fr));gap:12px;margin:0 0 24px}
.card{background:#131a2d;border:1px solid #24304a;border-radius:12px;padding:16px 18px}
.card h2{font-size:13px;text-transform:uppercase;letter-spacing:.08em;color:#8fa3bf}
code,pre{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}
code{background:#1b2440;border:1px solid #2a3860;border-radius:6px;padding:1px 5px;font-size:13px}
pre{background:#0f1628;border:1px solid #24304a;border-radius:10px;padding:14px;overflow:auto;font-size:13px;line-height:1.5}
table{width:100%;border-collapse:collapse;margin:0 0 24px;font-size:13.5px}
th,td{text-align:left;padding:8px 10px;border-bottom:1px solid #24304a}
th{color:#8fa3bf;font-weight:600}
a{color:#7cc4ff;text-decoration:none}a:hover{text-decoration:underline}
.note{color:#9fb0c7;font-size:13px;border-left:3px solid #4f7cff;padding-left:12px;margin:24px 0 0}
footer{color:#66758c;font-size:12px;margin-top:36px}
</style>
<main>
<h1>lcodearts <span class="badge">运行中</span></h1>
<p class="lead">华为云码道（CodeArts）的本地 OpenAI 兼容网关，并在后台自动领取每日积分。</p>
<div class="grid">
<div class="card"><h2>客户端 baseURL</h2><p><code>http://__HOST__/v1</code></p><p>dsh / opencode 选择 <code>openai-completions</code>。</p></div>
<div class="card"><h2>鉴权</h2><p>__AUTH__</p></div>
</div>
<h2>端点</h2>
<table><tr><th>路径</th><th>说明</th></tr>
<tr><td><a href="/v1/models">/v1/models</a></td><td>当前账号可用模型与积分倍率</td></tr>
<tr><td>/v1/chat/completions</td><td>OpenAI 兼容聊天（SSE / 非流式）</td></tr>
<tr><td><a href="/v1/quota">/v1/quota</a></td><td>积分套餐与剩余额度</td></tr>
<tr><td><a href="/health">/health</a></td><td>存活检查</td></tr></table>
<h2>快速验证</h2>
<pre>curl http://__HOST__/v1/models
curl -X POST http://__HOST__/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"glm-5.3","messages":[{"role":"user","content":"你好"}],"stream":true}'</pre>
<p class="note">这是 API 端点，不是聊天控制台。浏览器直接打开 <code>/v1/models</code> 会返回 JSON；根路径只做说明。</p>
<footer>自动签到：北京时间 8:00 后每 5 分钟检查一次 · 模型与积分来自 CodeArts 上游</footer>
</main>
</html>"#;

fn api_error(status: StatusCode, message: impl Into<String>, kind: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "message": message.into(), "type": kind } })),
    )
        .into_response()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn check_auth(headers: &HeaderMap, cfg: &Config) -> Result<(), Box<Response>> {
    let Some(expected) = cfg.api_key.as_deref() else {
        return Ok(());
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    if ct_eq(provided.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(Box::new(api_error(
            StatusCode::UNAUTHORIZED,
            "缺少或错误的 Bearer API key",
            "invalid_request_error",
        )))
    }
}

async fn health() -> Json<Value> {
    Json(json!({
        "ok": true,
        "service": "lcodearts",
        "models_endpoint": "/v1/model/builtin (upstream)",
        "auto_checkin": "background thread; see --no-auto-checkin",
    }))
}

async fn models(State(cfg): State<Arc<Config>>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_auth(&headers, &cfg) {
        return *resp;
    }
    let cached = cfg.models_cache.lock().ok().and_then(|g| {
        g.as_ref()
            .filter(|(at, _)| at.elapsed() < MODELS_CACHE_TTL)
            .map(|(_, list)| list.clone())
    });
    let list = match cached {
        Some(list) => list,
        None => {
            // 每次请求重新读凭据：`login` / `auth-import` 之后无需重启 serve
            let credential = match cred::load() {
                Ok(c) => c,
                Err(e) => {
                    return api_error(StatusCode::BAD_GATEWAY, format!("{e}"), "credentials_error")
                }
            };
            if credential.expired() {
                return api_error(
                    StatusCode::BAD_GATEWAY,
                    "CodeArts 凭据已过期：运行 `lcodearts login` 或 `auth-import`",
                    "credentials_error",
                );
            }
            match tokio::task::spawn_blocking(move || api::Api::new(credential).models()).await {
                Ok(Ok(list)) => {
                    if let Ok(mut g) = cfg.models_cache.lock() {
                        *g = Some((Instant::now(), list.clone()));
                    }
                    list
                }
                Ok(Err(e)) => {
                    return api_error(StatusCode::BAD_GATEWAY, format!("{e}"), "upstream_error")
                }
                Err(e) => {
                    return api_error(
                        StatusCode::BAD_GATEWAY,
                        format!("模型线程崩了: {e}"),
                        "upstream_error",
                    )
                }
            }
        }
    };
    let data: Vec<Value> = list
        .iter()
        .map(|m| {
            json!({
                "id": m.get("model_id").cloned().unwrap_or(Value::Null),
                "object": "model",
                "owned_by": "codearts",
                "name": m.get("model_name").cloned().unwrap_or(Value::Null),
                "description": m.get("model_desc").cloned().unwrap_or(Value::Null),
                "enabled": m.get("enable").and_then(Value::as_bool).unwrap_or(false),
                "supports_images": m.get("supports_images").and_then(Value::as_bool).unwrap_or(false),
                "credit_ratio": credit_ratio(m),
            })
        })
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

fn credit_ratio(m: &Value) -> String {
    m.get("credit")
        .and_then(Value::as_array)
        .and_then(|tiers| {
            tiers
                .iter()
                .find_map(|t| t.get("ratio_display").and_then(Value::as_str))
        })
        .unwrap_or("")
        .to_string()
}

async fn quota(State(cfg): State<Arc<Config>>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_auth(&headers, &cfg) {
        return *resp;
    }
    let credential = match cred::load() {
        Ok(c) => c,
        Err(e) => return api_error(StatusCode::BAD_GATEWAY, format!("{e}"), "credentials_error"),
    };
    if credential.expired() {
        return api_error(
            StatusCode::BAD_GATEWAY,
            "CodeArts 凭据已过期：运行 `lcodearts login` 或 `auth-import`",
            "credentials_error",
        );
    }
    match tokio::task::spawn_blocking(move || api::Api::new(credential).credits()).await {
        Ok(Ok(raw)) => {
            let s = api::CreditSnapshot::from_value(&raw);
            Json(json!({
                "ok": true,
                "package": {
                    "name": s.package_name, "status": s.status,
                    "is_credit_package": s.is_credit_package,
                    "is_token_package": s.is_token_package,
                    "end_date": s.end_date,
                },
                "credits": {
                    "total": s.total, "used": s.used, "remain": s.remain,
                    "expiring": s.expiring, "percent": s.percent,
                },
                "raw": raw,
            }))
            .into_response()
        }
        Ok(Err(e)) => api_error(StatusCode::BAD_GATEWAY, format!("{e}"), "upstream_error"),
        Err(e) => api_error(
            StatusCode::BAD_GATEWAY,
            format!("额度线程崩了: {e}"),
            "upstream_error",
        ),
    }
}

async fn chat_completions(
    State(cfg): State<Arc<Config>>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    if let Err(resp) = check_auth(&headers, &cfg) {
        return *resp;
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if model.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "请求体缺少 model",
            "invalid_request_error",
        );
    }
    let want_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    // glm-5.3 非流式直接 400（空 body），统一向上游发流式；非流式在本地聚合
    body["stream"] = json!(true);
    let credential = match cred::load() {
        Ok(c) => c,
        Err(e) => return api_error(StatusCode::BAD_GATEWAY, format!("{e}"), "credentials_error"),
    };
    if credential.expired() {
        return api_error(
            StatusCode::BAD_GATEWAY,
            "CodeArts 凭据已过期：运行 `lcodearts login` 或 `auth-import`",
            "credentials_error",
        );
    }
    let upstream_body = body.clone();
    let upstream = tokio::task::spawn_blocking(move || {
        api::Api::new(credential).chat_response(&upstream_body)
    })
    .await;
    let upstream = match upstream {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return api_error(StatusCode::BAD_GATEWAY, format!("{e}"), "upstream_error"),
        Err(e) => {
            return api_error(
                StatusCode::BAD_GATEWAY,
                format!("聊天线程崩了: {e}"),
                "upstream_error",
            )
        }
    };

    if want_stream {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<String, String>>(256);
        tokio::task::spawn_blocking(move || {
            let mut reader = BufReader::new(upstream.into_body().into_reader());
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() || trimmed.starts_with(':') {
                            continue;
                        }
                        let Some(payload) = trimmed.strip_prefix("data:") else {
                            continue;
                        };
                        let payload = payload.trim().to_string();
                        if payload.is_empty() {
                            continue;
                        }
                        let done = payload == "[DONE]";
                        if tx.blocking_send(Ok(payload)).is_err() {
                            break;
                        }
                        if done {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.blocking_send(Err(format!("读上游 SSE 失败: {e}")));
                        break;
                    }
                }
            }
        });
        let stream_body = async_stream::stream! {
            while let Some(item) = rx.recv().await {
                match item {
                    Ok(payload) => yield Ok::<Event, Infallible>(Event::default().data(payload)),
                    Err(e) => {
                        yield Ok(Event::default().data(
                            json!({ "error": { "message": e, "type": "upstream_error" } })
                                .to_string(),
                        ));
                        break;
                    }
                }
            }
        };
        Sse::new(stream_body)
            .keep_alive(KeepAlive::default())
            .into_response()
    } else {
        let model_out = model;
        match tokio::task::spawn_blocking(move || {
            aggregate_reader(
                BufReader::new(upstream.into_body().into_reader()),
                &model_out,
            )
        })
        .await
        {
            Ok(Ok(v)) => Json(v).into_response(),
            Ok(Err(e)) => api_error(StatusCode::BAD_GATEWAY, format!("{e}"), "upstream_error"),
            Err(e) => api_error(
                StatusCode::BAD_GATEWAY,
                format!("聚合线程崩了: {e}"),
                "upstream_error",
            ),
        }
    }
}

#[derive(Default)]
struct PartialTool {
    id: String,
    name: String,
    arguments: String,
}

/// 把 OpenAI SSE 聚合成一条非流式 `chat.completion`。
fn aggregate_reader(mut reader: impl BufRead, fallback_model: &str) -> Result<Value, AppError> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tools: BTreeMap<i64, PartialTool> = BTreeMap::new();
    let mut id = String::new();
    let mut created = 0i64;
    let mut model = fallback_model.to_string();
    let mut finish = "stop".to_string();
    let mut usage = Value::Null;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| AppError::Runtime(format!("读上游 SSE 失败: {e}")))?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(':') {
            continue;
        }
        let Some(payload) = trimmed.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            continue;
        }
        if payload == "[DONE]" {
            break;
        }
        let v: Value = serde_json::from_str(payload).map_err(|e| {
            AppError::Runtime(format!(
                "上游 SSE 不是 JSON: {e}；原文：{}",
                util::clip(payload, 200)
            ))
        })?;
        if let Some(code) = v.get("error_code").and_then(Value::as_str) {
            if !code.is_empty() && code != "0000" {
                return Err(api::translate_code(code, payload));
            }
        }
        if let Some(err) = v.get("error") {
            return Err(AppError::Runtime(format!(
                "上游返回错误: {}",
                err.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误")
            )));
        }
        if id.is_empty() {
            if let Some(s) = v.get("id").and_then(Value::as_str) {
                id = s.to_string();
            }
        }
        if created == 0 {
            if let Some(c) = v.get("created").and_then(Value::as_i64) {
                created = c;
            }
        }
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            if !m.is_empty() {
                model = m.to_string();
            }
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            usage = u.clone();
        }
        if let Some(choice) = v
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        {
            if let Some(delta) = choice.get("delta") {
                if let Some(s) = delta.get("content").and_then(Value::as_str) {
                    content.push_str(s);
                }
                if let Some(s) = delta.get("reasoning_content").and_then(Value::as_str) {
                    reasoning.push_str(s);
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let idx = call.get("index").and_then(Value::as_i64).unwrap_or(0);
                        let t = tools.entry(idx).or_default();
                        if let Some(s) = call.get("id").and_then(Value::as_str) {
                            if !s.is_empty() {
                                t.id = s.to_string();
                            }
                        }
                        if let Some(f) = call.get("function") {
                            if let Some(n) = f.get("name").and_then(Value::as_str) {
                                if !n.is_empty() {
                                    t.name = n.to_string();
                                }
                            }
                            if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                                t.arguments.push_str(a);
                            }
                        }
                    }
                }
            }
            if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
                if !f.is_empty() {
                    finish = f.to_string();
                }
            }
        }
    }

    if id.is_empty() {
        id = format!("chatcmpl-{}", util::random_hex(12));
    }
    if created == 0 {
        created = util::now_secs();
    }
    let mut message = json!({
        "role": "assistant",
        "content": if content.is_empty() { Value::Null } else { json!(content) },
    });
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tools.is_empty() {
        let calls: Vec<Value> = tools
            .into_values()
            .map(|t| {
                json!({
                    "id": t.id,
                    "type": "function",
                    "function": { "name": t.name, "arguments": t.arguments }
                })
            })
            .collect();
        message["tool_calls"] = json!(calls);
    }
    Ok(json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": usage,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(raw: &str) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(raw.as_bytes().to_vec())
    }

    #[test]
    fn aggregates_text_and_reasoning() {
        let raw = concat!(
            "data: {\"id\":\"c1\",\"created\":1700000000,\"model\":\"glm-5.3\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"你\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"好\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let v = aggregate_reader(sse(raw), "fallback").unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "你好");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert_eq!(v["model"], "glm-5.3");
        assert_eq!(v["usage"]["completion_tokens"], 2);
    }

    #[test]
    fn aggregates_tool_call_deltas() {
        let raw = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"run\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let v = aggregate_reader(sse(raw), "m").unwrap();
        let call = &v["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["function"]["name"], "run");
        assert_eq!(call["function"]["arguments"], "{\"cmd\":\"ls\"}");
    }

    #[test]
    fn upstream_error_inside_sse_is_surfaced() {
        let raw =
            concat!("data: {\"error_code\":\"APIG.0301\",\"error_msg\":\"bad signature\"}\n\n",);
        let err = aggregate_reader(sse(raw), "m").unwrap_err();
        assert!(format!("{err}").contains("签名"));
    }
}
