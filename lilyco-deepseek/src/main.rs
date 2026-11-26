//! `ldeepseek` —— 基于 `lilyco-webchat-core` 的 `chat.deepseek.com` 四端域工具 + OpenAI 兼容网关。
//!
//! 复用了 `lilyco-webchat-core` 的全部通用能力：
//! - 纯 Rust CDP 浏览器驱动（含 `page_count <= 1` 防误杀保护、坐标点击、长文本 DOM 写入）
//! - `SerialQueue`（并发=1 串行队列）+ `verify_bearer`（可选 API Key 鉴权）
//! - `SessionStore`（锚点会话绑定 + 增量续聊）+ `estimate_tokens` + `cap_transcript` + `Diff`

use std::sync::Arc;
use std::time::{Duration, Instant};

use lilyco::prelude::*;
use lilyco_webchat_core::cdp::{self, Client};
use serde_json::{json, Value};

#[cfg(feature = "server")]
mod serve;

const SITE_URL: &str = "https://chat.deepseek.com/";
const ENV_CDP: &str = "LDEEPSEEK_CDP";
const COMPOSER_TIMEOUT: Duration = Duration::from_secs(30);
const REPLY_TIMEOUT: Duration = Duration::from_secs(240);

/// 检查本机 CDP 浏览器是否就绪（只读 T0）
#[derive(App)]
#[app(
    name = "check",
    run = "run_check",
    about = "Check whether a local Chromium CDP endpoint (ego-lite 9229 by default) is reachable for chat.deepseek.com. Read-only (safety T0)."
)]
pub struct Check {
    #[arg(about = "Explicit CDP endpoint, e.g. http://127.0.0.1:9229", default = "")]
    cdp: String,
}

fn run_check(app: &Check, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    let candidates = cdp::candidate_endpoints(&app.cdp, ENV_CDP);
    let mut probed = Vec::new();
    let mut active: Option<String> = None;
    for ep in &candidates {
        match cdp::probe(ep) {
            Ok(()) => {
                if active.is_none() {
                    active = Some(ep.clone());
                }
                probed.push(json!({ "endpoint": ep, "ok": true, "pages": cdp::page_count(ep) }));
            }
            Err(e) => {
                probed.push(json!({ "endpoint": ep, "ok": false, "error": format!("{e}") }));
            }
        }
    }
    let out = json!({
        "site": SITE_URL,
        "active_cdp": active,
        "candidates": probed,
    });
    if let Some(ep) = out["active_cdp"].as_str() {
        ctx.log(LogLevel::Info, format!("CDP 就绪: {ep}"));
    } else {
        ctx.log(
            LogLevel::Warn,
            "未发现可用的 CDP 浏览器（请先启动 ego-browser）".to_string(),
        );
    }
    ctx.done(out.clone(), start.elapsed().as_millis() as u64);
    Ok(out)
}

/// 向 `chat.deepseek.com` 发一条消息（T1）
#[derive(App)]
#[app(
    name = "ask",
    run = "run_ask",
    safety = "t1",
    about = "Send one prompt to chat.deepseek.com through a logged-in Chromium CDP tab (optional --deep-think for R1 reasoning mode). Safety T1."
)]
pub struct Ask {
    #[arg(about = "Prompt text to send")]
    prompt: String,
    #[arg(about = "Enable DeepThink (R1) toggle on the DeepSeek web composer", default = false)]
    deep_think: bool,
    #[arg(about = "Existing chat.deepseek.com conversation id to continue", default = "")]
    conversation_id: String,
    #[arg(about = "Explicit CDP endpoint, e.g. http://127.0.0.1:9229", default = "")]
    cdp: String,
}

fn run_ask(app: &Ask, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    let prompt = app.prompt.trim();
    if prompt.is_empty() {
        return Err(AppError::InvalidArg("prompt 不能为空".to_string()));
    }
    let cid = (!app.conversation_id.trim().is_empty()).then_some(app.conversation_id.trim());
    let out = ask_web(prompt, app.deep_think, cid, &app.cdp, ctx, &mut |_| {})?;
    if let Some(txt) = out["text"].as_str() {
        ctx.log(LogLevel::Info, txt.to_string());
    }
    ctx.done(out.clone(), start.elapsed().as_millis() as u64);
    Ok(out)
}

/// 驱动 `chat.deepseek.com` 页面完成一次问答，支持流式增量回调
pub fn ask_web(
    prompt: &str,
    deep_think: bool,
    conversation_id: Option<&str>,
    explicit_cdp: &str,
    ctx: &Context,
    on_text: &mut dyn FnMut(&str),
) -> Result<Value, AppError> {
    let started = Instant::now();
    let mut tried = Vec::new();
    for ep in cdp::candidate_endpoints(explicit_cdp, ENV_CDP) {
        if let Err(e) = cdp::probe(&ep) {
            tried.push(format!("  - {ep}: {e}"));
            continue;
        }
        let url = match conversation_id {
            Some(id) => format!("https://chat.deepseek.com/a/chat/s/{id}"),
            None => SITE_URL.to_string(),
        };
        let (target_id, ws_url) = cdp::create_target_url(&ep, &url)?;
        let res = run_deepseek_tab(&ws_url, prompt, deep_think, ctx, on_text);
        if res.is_ok() {
            cdp::close_target(&ep, &target_id);
        }
        let (conv_id, text) = res?;
        return Ok(json!({
            "site": "deepseek",
            "deep_think": deep_think,
            "conversation_id": conv_id,
            "text": text,
            "cdp": ep,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        }));
    }
    Err(AppError::InvalidInput(format!(
        "连不上可用的 Chromium CDP（请先启动 ego-browser 并登录 chat.deepseek.com）\n试过的端点：\n{}",
        tried.join("\n")
    )))
}

const READY_JS: &str = r#"(() => {
  const el = document.querySelector('textarea, div[contenteditable="true"]');
  return JSON.stringify({ ready: !!el, href: location.href });
})()"#;

fn set_input_js(prompt: &str) -> String {
    let encoded = serde_json::to_string(prompt).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(() => {{
  const ta = document.querySelector('textarea');
  if (ta) {{
    ta.focus();
    const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value')?.set;
    if (setter) setter.call(ta, {encoded}); else ta.value = {encoded};
    ta.dispatchEvent(new Event('input', {{ bubbles: true }}));
    return JSON.stringify({{ ok: true, kind: 'textarea' }});
  }}
  const ce = document.querySelector('div[contenteditable="true"]');
  if (ce) {{
    ce.focus();
    ce.textContent = {encoded};
    ce.dispatchEvent(new InputEvent('input', {{ bubbles: true, inputType: 'insertText' }}));
    return JSON.stringify({{ ok: true, kind: 'contenteditable' }});
  }}
  return JSON.stringify({{ ok: false }});
}})()"#
    )
}

const SNAPSHOT_JS: &str = r#"(() => {
  const nodes = Array.from(document.querySelectorAll('.ds-markdown, [class*="markdown"]'));
  const last = nodes.length ? (nodes[nodes.length - 1].innerText || '').trim() : '';
  const generating = !!document.querySelector('button[aria-label*="Stop"], button[aria-label*="停止"], [class*="stop"]');
  const m = location.pathname.match(/\/a\/chat\/s\/([^/?#]+)/);
  return JSON.stringify({
    count: nodes.length,
    text: last,
    generating,
    conversation_id: m ? m[1] : null,
  });
})()"#;

fn run_deepseek_tab(
    ws_url: &str,
    prompt: &str,
    deep_think: bool,
    ctx: &Context,
    on_text: &mut dyn FnMut(&str),
) -> Result<(Option<String>, String), AppError> {
    let mut client = Client::connect(ws_url)?;
    let deadline = Instant::now() + COMPOSER_TIMEOUT;
    let mut ready = false;
    while Instant::now() < deadline {
        let p = client.eval_json(READY_JS).unwrap_or(Value::Null);
        if p.get("ready").and_then(Value::as_bool).unwrap_or(false) {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(600));
    }
    if !ready {
        return Err(AppError::Runtime(
            "等了 30 秒 chat.deepseek.com 输入框还没出来（请确认已登录）".to_string(),
        ));
    }

    if deep_think {
        // 如果页面上有「深度思考 / DeepThink」开关且未激活，点一下开启
        let _ = client.eval_js(
            r#"(() => {
              const btn = Array.from(document.querySelectorAll('button, [role="button"]')).find(b => {
                const t = (b.innerText || '').trim();
                return t.includes('深度思考') || t.toLowerCase().includes('deepthink');
              });
              if (btn && btn.getAttribute('aria-pressed') !== 'true' && !btn.className.includes('selected')) {
                btn.click();
              }
            })()"#,
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    let before = client.eval_json(SNAPSHOT_JS).unwrap_or(Value::Null);
    let before_count = before.get("count").and_then(Value::as_u64).unwrap_or(0);

    let set = client.eval_json(&set_input_js(prompt))?;
    if !set.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return Err(AppError::Runtime("找不到 DeepSeek 输入框".to_string()));
    }
    std::thread::sleep(Duration::from_millis(350));
    ctx.log(LogLevel::Info, "已写入 DeepSeek 输入框，发送消息".to_string());
    client.press_key("Enter", "Enter", 13)?;

    let reply_deadline = Instant::now() + REPLY_TIMEOUT;
    let mut best = String::new();
    let mut conv_id: Option<String> = None;
    let mut stable_ticks = 0u8;
    std::thread::sleep(Duration::from_millis(2000));

    while Instant::now() < reply_deadline {
        if let Ok(snap) = client.eval_json(SNAPSHOT_JS) {
            if let Some(cid) = snap.get("conversation_id").and_then(Value::as_str) {
                conv_id = Some(cid.to_string());
            }
            let count = snap.get("count").and_then(Value::as_u64).unwrap_or(0);
            let txt = snap.get("text").and_then(Value::as_str).unwrap_or("");
            let generating = snap
                .get("generating")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if count > before_count && !txt.is_empty() {
                if txt != best {
                    best = txt.to_string();
                    stable_ticks = 0;
                    on_text(&best);
                } else if !generating {
                    stable_ticks += 1;
                    if stable_ticks >= 2 {
                        return Ok((conv_id, best));
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1200));
    }

    if !best.is_empty() {
        Ok((conv_id, best))
    } else {
        Err(AppError::Runtime(format!(
            "等了 {} 秒 DeepSeek 还没返回回答",
            REPLY_TIMEOUT.as_secs()
        )))
    }
}

pub fn build_registry(policy: Arc<dyn SafetyPolicy>) -> Registry {
    let mut reg = Registry::new().with_policy(policy);
    #[allow(unused_mut)]
    let mut cmds = vec![
        RegisteredCommand::from_app::<Check>(),
        RegisteredCommand::from_app::<Ask>(),
    ];
    #[cfg(feature = "server")]
    cmds.push(RegisteredCommand::from_app::<serve::Serve>());
    for c in cmds {
        let name = c.schema.name.clone();
        reg.register(c)
            .unwrap_or_else(|e| panic!("注册命令 `{name}` 失败: {e}"));
    }
    reg
}

fn policy_for(backend: lilyco::Backend) -> Arc<dyn SafetyPolicy> {
    match backend {
        lilyco::Backend::Mcp => Arc::new(DenyElevated),
        #[allow(unreachable_patterns)]
        _ => Arc::new(Interactive),
    }
}

fn main() {
    let backend = lilyco::detect_registry_backend();
    let reg = build_registry(policy_for(backend));
    lilyco::run_registry_with("ldeepseek", reg, backend);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_expected_commands() {
        let reg = build_registry(Arc::new(Interactive));
        let names: Vec<&str> = reg.visible().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"check"));
        assert!(names.contains(&"ask"));
        #[cfg(feature = "server")]
        assert!(names.contains(&"serve"));
    }
}
