//! 纯 Rust CDP（Chrome DevTools Protocol）通用驱动层：
//! - 端点探测（显式 `--cdp` → 环境变量 → ego-lite 9229 → 常见调试端口）
//! - 开/关标签页（自带 `page_count <= 1` 保护，绝不误杀整个 Chromium）
//! - 坐标点击、按键、长文本 DOM 快速写入与 ProseMirror/富文本空白漂移容忍校验

use std::time::{Duration, Instant};

use lilyco::prelude::AppError;
use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::util::clip;

const FALLBACK_PORTS: &[u16] = &[9229, 9222, 9223, 9333];
const CDP_HTTP_TIMEOUT: Duration = Duration::from_secs(6);
const CDP_WS_TIMEOUT: Duration = Duration::from_secs(25);

fn agent() -> ureq::Agent {
    let cfg = ureq::Agent::config_builder()
        .timeout_global(Some(CDP_HTTP_TIMEOUT))
        .http_status_as_error(false)
        .build();
    cfg.into()
}

/// 读 `~/.config/ego-lite/config.json` 里的 `cdpPort`（不存在就返回 None）
pub fn ego_lite_port() -> Option<u16> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let path = std::path::PathBuf::from(home)
        .join(".config")
        .join("ego-lite")
        .join("config.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    v.get("cdpPort")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
}

pub fn normalize_endpoint(endpoint: &str) -> String {
    let e = endpoint.trim().trim_end_matches('/');
    if e.starts_with("http://") || e.starts_with("https://") {
        e.to_string()
    } else {
        format!("http://{e}")
    }
}

/// 候选 CDP 端点（显式给定则**只**试那一个，否则按 env → ego-lite → 常见端口排序去重）
pub fn candidate_endpoints(explicit: &str, env_var: &str) -> Vec<String> {
    if !explicit.trim().is_empty() {
        return vec![normalize_endpoint(explicit)];
    }
    let mut out = Vec::new();
    let mut push = |e: String| {
        if !out.contains(&e) {
            out.push(e);
        }
    };
    if !env_var.is_empty() {
        if let Ok(e) = std::env::var(env_var) {
            if !e.trim().is_empty() {
                push(normalize_endpoint(&e));
            }
        }
    }
    if let Some(p) = ego_lite_port() {
        push(format!("http://127.0.0.1:{p}"));
    }
    for p in FALLBACK_PORTS {
        push(format!("http://127.0.0.1:{p}"));
    }
    out
}

/// 探一下端口是否是活着的 CDP
pub fn probe(cdp: &str) -> Result<(), AppError> {
    let version = agent()
        .get(format!("{cdp}/json/version"))
        .call()
        .map_err(|e| AppError::Runtime(format!("连不上 CDP {cdp}: {e}")))?;
    if !version.status().is_success() {
        return Err(AppError::Runtime(format!(
            "{cdp}/json/version → HTTP {}（这个端口不是 CDP）",
            version.status()
        )));
    }
    Ok(())
}

/// 新开一个指定 URL 的标签页，返回 `(target_id, ws_url)`
pub fn create_target_url(cdp: &str, url: &str) -> Result<(String, String), AppError> {
    let created = agent()
        .put(format!("{cdp}/json/new?{url}"))
        .send_empty()
        .map_err(|e| AppError::Runtime(format!("开新标签页失败: {e}")))?;
    let status = created.status();
    let body = created
        .into_body()
        .read_to_string()
        .map_err(|e| AppError::Runtime(format!("读新标签页返回失败: {e}")))?;
    if !status.is_success() {
        return Err(AppError::Runtime(format!(
            "{cdp}/json/new → HTTP {status}: {}",
            clip(&body, 200)
        )));
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| AppError::Runtime(format!("新标签页返回不是 JSON: {e}")))?;
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Runtime("新标签页没有 id".to_string()))?
        .to_string();
    let ws = v
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Runtime("新标签页没有 webSocketDebuggerUrl".to_string()))?
        .to_string();
    Ok((id, ws))
}

/// 统计当前打开的 `page` 标签页数量
pub fn page_count(cdp: &str) -> Option<usize> {
    let raw = agent()
        .get(format!("{cdp}/json/list"))
        .call()
        .ok()?
        .into_body()
        .read_to_string()
        .ok()?;
    let items: Value = serde_json::from_str(&raw).ok()?;
    let n = items
        .as_array()?
        .iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .count();
    Some(n)
}

/// 安全关闭临时标签页：Linux 上只剩 ≤ 1 个 `page` 时绝不关闭，避免把整个 Chromium 进程关掉。
pub fn close_target(cdp: &str, id: &str) {
    if page_count(cdp).unwrap_or(0) <= 1 {
        return;
    }
    let _ = agent().get(format!("{cdp}/json/close/{id}")).call();
}

/// 极简 CDP WebSocket 客户端
pub struct Client {
    ws: WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    next_id: u64,
}

impl Client {
    pub fn connect(ws_url: &str) -> Result<Self, AppError> {
        let (ws, _resp) = tungstenite::connect(ws_url)
            .map_err(|e| AppError::Runtime(format!("连不上 CDP WebSocket: {e}")))?;
        let mut client = Self { ws, next_id: 1 };
        if let MaybeTlsStream::Plain(stream) = client.ws.get_mut() {
            let _ = stream.set_read_timeout(Some(CDP_WS_TIMEOUT));
            let _ = stream.set_write_timeout(Some(CDP_WS_TIMEOUT));
        }
        Ok(client)
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, AppError> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({ "id": id, "method": method, "params": params }).to_string();
        self.ws
            .send(Message::Text(payload.into()))
            .map_err(|e| AppError::Runtime(format!("CDP 发送 {method} 失败: {e}")))?;
        let deadline = Instant::now() + CDP_WS_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                return Err(AppError::Runtime(format!("CDP {method} 超时")));
            }
            let msg = self
                .ws
                .read()
                .map_err(|e| AppError::Runtime(format!("CDP 读包失败: {e}")))?;
            let text = match msg {
                Message::Text(t) => t.as_str().to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                _ => continue,
            };
            let v: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = v.get("error") {
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误");
                return Err(AppError::Runtime(format!("CDP {method} 报错: {msg}")));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    pub fn insert_text(&mut self, text: &str) -> Result<(), AppError> {
        self.call("Input.insertText", json!({ "text": text }))?;
        Ok(())
    }

    pub fn click_point(&mut self, x: f64, y: f64) -> Result<(), AppError> {
        self.call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1 }),
        )?;
        self.call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1 }),
        )?;
        Ok(())
    }

    pub fn press_key(&mut self, key: &str, code: &str, vk: u32) -> Result<(), AppError> {
        self.call(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyDown", "key": key, "code": code, "windowsVirtualKeyCode": vk }),
        )?;
        self.call(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": vk }),
        )?;
        Ok(())
    }

    pub fn eval_js(&mut self, expression: &str) -> Result<Value, AppError> {
        let res = self.call(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
        )?;
        if res.get("exceptionDetails").is_some() {
            let msg = res
                .pointer("/exceptionDetails/exception/description")
                .or_else(|| res.pointer("/exceptionDetails/text"))
                .and_then(Value::as_str)
                .unwrap_or("页面脚本抛错");
            return Err(AppError::Runtime(format!("页面求值出错: {msg}")));
        }
        Ok(res.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    pub fn eval_json(&mut self, expression: &str) -> Result<Value, AppError> {
        let v = self.eval_js(expression)?;
        match v {
            Value::String(s) => serde_json::from_str(&s)
                .map_err(|e| AppError::Runtime(format!("页面返回的不是 JSON: {e}"))),
            other => Ok(other),
        }
    }
}

/// 折叠空白并剥离零宽字符（ProseMirror / 富文本输入框读回校验用）
pub fn squash(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|&c| c != '\u{200b}' && c != '\u{feff}')
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 校验输入框读回文本与目标提示是否一致（长提示容忍首尾 80 字一致 + 1% 排版漂移）
pub fn same_text(got: &str, want: &str) -> bool {
    if want.trim().is_empty() {
        return false;
    }
    let g = squash(got);
    let w = squash(want);
    if g == w {
        return true;
    }
    let gc: Vec<char> = g.chars().collect();
    let wc: Vec<char> = w.chars().collect();
    if wc.len() < 200 || gc.len() < 200 {
        return false;
    }
    let edge = 80;
    let diff = gc.len().abs_diff(wc.len());
    diff * 100 <= wc.len()
        && gc[..edge] == wc[..edge]
        && gc[gc.len() - edge..] == wc[wc.len() - edge..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_endpoint_is_tried_alone() {
        assert_eq!(
            candidate_endpoints("127.0.0.1:9229", "NONEXISTENT_ENV"),
            vec!["http://127.0.0.1:9229"]
        );
    }

    #[test]
    fn same_text_tolerates_minor_drift_on_long_prompts() {
        let head = "System context header. ".repeat(5);
        let mid = "body text line. ".repeat(15);
        let tail = "User: final question here. ".repeat(4);
        let want = format!("{head}{mid}{tail}");
        let got = format!("{head}{mid}\u{200b}x{tail}");
        assert!(same_text(&got, &want));
        assert!(!same_text(&head, &want));
    }
}
