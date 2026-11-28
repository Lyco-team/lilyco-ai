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
const CDP_WS_TIMEOUT: Duration = Duration::from_secs(10);

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
    // `/json/new?<url>` 把参数整个当 URL；`#` 必须编码成 %23，否则会被当片段
    // 截断，新建页变成 about:blank（lchatgpt 踩过这个坑）。
    let encoded = url.replace('#', "%23");
    let created = agent()
        .put(format!("{cdp}/json/new?{encoded}"))
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

    /// 把当前标签页切到前台，让用户能直接看到悬浮提示卡并交互
    pub fn bring_to_front(&mut self) {
        let _ = self.call("Page.bringToFront", json!({}));
    }

    /// 导航到指定 URL（用于先注入 Cookies 再刷新进入目标站）
    pub fn navigate(&mut self, url: &str) -> Result<(), AppError> {
        self.call("Page.navigate", json!({ "url": url }))?;
        Ok(())
    }

    /// 在受控页面右上角注入「🤖 Lilyco 智能助手控制中」悬浮徽章（幂等）
    pub fn inject_control_badge(&mut self, subtitle: &str) {
        let sub = serde_json::to_string(subtitle).unwrap_or_else(|_| "\"\"".to_string());
        let js = format!(
            r#"(() => {{
  const id = 'lilyco-control-badge';
  let el = document.getElementById(id);
  if (!el) {{
    el = document.createElement('div');
    el.id = id;
    el.style.cssText = [
      'position:fixed', 'top:14px', 'right:16px', 'z-index:2147483647',
      'display:flex', 'align-items:center', 'gap:8px',
      'padding:8px 14px', 'border-radius:999px',
      'background:rgba(15,23,42,0.90)', 'color:#f8fafc',
      'font:600 13px/1.2 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif',
      'box-shadow:0 8px 24px rgba(0,0,0,0.28)',
      'border:1px solid rgba(56,189,248,0.55)',
      'backdrop-filter:blur(10px)', 'pointer-events:none'
    ].join(';');
    document.documentElement.appendChild(el);
  }}
  const sub = {sub};
  el.textContent = sub ? ('🤖 Lilyco 智能助手控制中 · ' + sub) : '🤖 Lilyco 智能助手控制中';
}})()"#
        );
        let _ = self.eval_js(&js);
    }

    /// 注入带交互按钮的「人工交接悬浮窗」：提示任务内容、保存路径与「导出 Cookies 并保存」按钮。
    /// 页面跳转/登录后如果悬浮窗消失，轮询时可反复调用以自动补回。
    pub fn inject_handoff_card(&mut self, task: &str, save_path: &str) {
        let task_js = serde_json::to_string(task).unwrap_or_else(|_| "\"\"".to_string());
        let path_js = serde_json::to_string(save_path).unwrap_or_else(|_| "\"\"".to_string());
        let js = format!(
            r#"(() => {{
  const id = 'lilyco-handoff-card';
  if (document.getElementById(id)) return;
  const card = document.createElement('div');
  card.id = id;
  card.style.cssText = [
    'position:fixed', 'top:18px', 'right:18px', 'z-index:2147483647',
    'width:340px', 'max-width:calc(100vw - 36px)',
    'padding:16px 18px', 'border-radius:14px',
    'background:rgba(15,23,42,0.95)', 'color:#f8fafc',
    'font:13px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif',
    'box-shadow:0 16px 40px rgba(0,0,0,0.42)',
    'border:1px solid rgba(56,189,248,0.65)',
    'backdrop-filter:blur(12px)'
  ].join(';');
  const title = document.createElement('div');
  title.style.cssText = 'font-weight:700;font-size:14px;color:#38bdf8;margin-bottom:8px;display:flex;align-items:center;gap:6px;';
  title.textContent = '🤖 Lilyco 智能助手控制中';
  const body = document.createElement('div');
  body.style.cssText = 'font-size:13px;color:#e2e8f0;margin-bottom:8px;';
  body.textContent = 'AI 给你发布了任务：' + {task_js};
  const hint = document.createElement('div');
  hint.style.cssText = 'font-size:12px;color:#94a3b8;margin-bottom:12px;word-break:break-all;';
  hint.textContent = '完成后点击下方按钮导出 Cookies，保存到：' + {path_js};
  const btn = document.createElement('button');
  btn.id = 'lilyco-handoff-export-btn';
  btn.type = 'button';
  btn.style.cssText = [
    'width:100%', 'padding:10px 14px', 'border:none', 'border-radius:9px',
    'background:linear-gradient(135deg,#0ea5e9,#2563eb)', 'color:#ffffff',
    'font-weight:600', 'font-size:13px', 'cursor:pointer'
  ].join(';');
  btn.textContent = '导出 Cookies 并保存';
  btn.onclick = () => {{
    window.__lilycoExportRequested = true;
    btn.disabled = true;
    btn.textContent = '正在导出 Cookies...';
    btn.style.opacity = '0.75';
  }};
  card.appendChild(title);
  card.appendChild(body);
  card.appendChild(hint);
  card.appendChild(btn);
  document.documentElement.appendChild(card);
}})()"#
        );
        let _ = self.eval_js(&js);
    }

    /// 更新人工交接悬浮窗为完成态
    pub fn mark_handoff_done(&mut self, summary: &str) {
        let msg = serde_json::to_string(summary).unwrap_or_else(|_| "\"\"".to_string());
        let js = format!(
            r#"(() => {{
  const btn = document.getElementById('lilyco-handoff-export-btn');
  if (btn) {{
    btn.disabled = true;
    btn.style.background = '#16a34a';
    btn.style.opacity = '1';
    btn.textContent = {msg};
  }}
}})()"#
        );
        let _ = self.eval_js(&js);
    }

    /// 通过 CDP 导出匹配域名的全部 Cookies + 当前页 `localStorage`（用于无头服务器复用登录态）
    pub fn export_cookies_and_storage(&mut self, domain_filter: &str) -> Result<Value, AppError> {
        let raw_cookies = self
            .call("Storage.getCookies", json!({}))
            .or_else(|_| self.call("Network.getAllCookies", json!({})))?;
        let filter = domain_filter.trim().to_ascii_lowercase();
        let cookies: Vec<Value> = raw_cookies
            .get("cookies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|c| {
                if filter.is_empty() {
                    return true;
                }
                c.get("domain")
                    .and_then(Value::as_str)
                    .map(|d| d.to_ascii_lowercase().contains(&filter))
                    .unwrap_or(false)
            })
            .collect();
        let ls = self
            .eval_json(
                r#"(() => {
  const out = {};
  try {
    for (let i = 0; i < localStorage.length; i++) {
      const k = localStorage.key(i);
      if (k !== null) out[k] = localStorage.getItem(k);
    }
  } catch (_) {}
  return JSON.stringify(out);
})()"#,
            )
            .unwrap_or_else(|_| json!({}));
        let origin = self
            .eval_js("location.origin")
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let exported_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(json!({
            "domain": domain_filter,
            "origin": origin,
            "exported_at": exported_at,
            "cookie_count": cookies.len(),
            "cookies": cookies,
            "local_storage": ls,
        }))
    }

    /// 截图当前视口到 PNG（用于登录二维码/登录页目检）。
    /// 纯 CDP，不依赖浏览器控制台或外部工具。
    pub fn capture_screenshot(&mut self, path: &std::path::Path) -> Result<(), AppError> {
        let res = self.call(
            "Page.captureScreenshot",
            json!({ "format": "png", "captureBeyondViewport": false }),
        )?;
        let data = res
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::Runtime("截图响应缺少 data".to_string()))?;
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| AppError::Runtime(format!("解码截图失败: {e}")))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                AppError::Runtime(format!("创建截图目录 {} 失败: {e}", parent.display()))
            })?;
        }
        std::fs::write(path, bytes)
            .map_err(|e| AppError::Runtime(format!("写截图 {} 失败: {e}", path.display())))?;
        Ok(())
    }

    /// 清掉某个域名下的所有 cookies（多账号登录前退出现有账号）
    pub fn clear_cookies_for_domain(&mut self, domain_filter: &str) -> Result<usize, AppError> {
        let raw = self
            .call("Storage.getCookies", json!({}))
            .or_else(|_| self.call("Network.getAllCookies", json!({})))?;
        let filter = domain_filter.trim().to_ascii_lowercase();
        let cookies: Vec<Value> = raw
            .get("cookies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut removed = 0usize;
        for c in cookies {
            let domain = c.get("domain").and_then(Value::as_str).unwrap_or("");
            if filter.is_empty() || !domain.to_ascii_lowercase().contains(&filter) {
                continue;
            }
            let name = c.get("name").and_then(Value::as_str).unwrap_or("");
            let path = c.get("path").and_then(Value::as_str).unwrap_or("/");
            if name.is_empty() {
                continue;
            }
            let _ = self.call(
                "Network.deleteCookies",
                json!({ "name": name, "domain": domain, "path": path }),
            );
            removed += 1;
        }
        Ok(removed)
    }

    /// 清掉某个 origin 的 localStorage / sessionStorage / cookies
    pub fn clear_origin(&mut self, origin: &str) -> Result<(), AppError> {
        self.call(
            "Storage.clearDataForOrigin",
            json!({ "origin": origin, "storageTypes": "all" }),
        )?;
        Ok(())
    }

    /// 把导出的 Cookies + localStorage 注入到当前（无头或本机）浏览器实例
    pub fn import_cookies_and_storage(
        &mut self,
        session_file: &std::path::Path,
    ) -> Result<usize, AppError> {
        let raw = std::fs::read_to_string(session_file).map_err(|e| {
            AppError::Runtime(format!("读登录态文件 {} 失败: {e}", session_file.display()))
        })?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| AppError::Runtime(format!("解析登录态 JSON 失败: {e}")))?;
        let cookies = v
            .get("cookies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !cookies.is_empty() {
            let _ = self.call("Network.enable", json!({}));
            self.call("Network.setCookies", json!({ "cookies": cookies }))?;
        }
        if let Some(ls) = v.get("local_storage").and_then(Value::as_object) {
            if !ls.is_empty() {
                let encoded = serde_json::to_string(ls).unwrap_or_else(|_| "{}".to_string());
                let js = format!(
                    r#"(() => {{
  try {{
    const data = {encoded};
    for (const [k, v] of Object.entries(data)) {{
      if (typeof v === 'string') localStorage.setItem(k, v);
    }}
  }} catch (_) {{}}
}})()"#
                );
                let _ = self.eval_js(&js);
            }
        }
        Ok(cookies.len())
    }
}

/// 人工交接登录：打开目标网页、切到前台、弹出「Lilyco 智能助手控制中」悬浮窗，
/// 等用户登录并点击「导出 Cookies 并保存」后自动落盘（权限 0600），供无头服务器直接导入复用。
pub fn handoff_login(
    cdp: &str,
    url: &str,
    domain_filter: &str,
    task_desc: &str,
    out_path: &std::path::Path,
    timeout: Duration,
) -> Result<Value, AppError> {
    probe(cdp)?;
    let (target_id, ws_url) = create_target_url(cdp, url)?;
    let mut client = Client::connect(&ws_url)?;
    client.bring_to_front();
    let path_str = out_path.display().to_string();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        client.inject_handoff_card(task_desc, &path_str);
        let clicked = client
            .eval_js("Boolean(window.__lilycoExportRequested)")
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if clicked {
            let bundle = client.export_cookies_and_storage(domain_filter)?;
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    AppError::Runtime(format!("创建目录 {} 失败: {e}", parent.display()))
                })?;
            }
            let text = serde_json::to_string_pretty(&bundle)
                .map_err(|e| AppError::Runtime(format!("序列化登录态失败: {e}")))?;
            std::fs::write(out_path, &text)
                .map_err(|e| AppError::Runtime(format!("写入 {} 失败: {e}", out_path.display())))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(out_path, std::fs::Permissions::from_mode(0o600));
            }
            let count = bundle["cookie_count"].as_u64().unwrap_or(0);
            client.mark_handoff_done(&format!("✅ 已导出 {count} 条 Cookies 并保存"));
            std::thread::sleep(Duration::from_millis(1200));
            close_target(cdp, &target_id);
            return Ok(json!({
                "ok": true,
                "domain": domain_filter,
                "cookie_count": count,
                "saved_to": path_str,
                "cdp": cdp,
            }));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    close_target(cdp, &target_id);
    Err(AppError::Runtime(format!(
        "等待 {} 秒未点击「导出 Cookies 并保存」",
        timeout.as_secs()
    )))
}

/// 专用常驻页：跨请求复用同一张标签页，绝不碰用户自己开的页面。
/// 这是 ego-browser `task space` 的轻量版——共享 Chromium profile（登录态继承），
/// 隔离的是「哪些标签归工具控制」：靠持久化的 target id + URL hash 标记认领。
/// 常驻页没有「新建后 React 监听未挂稳」的加载竞态，也便于观察页面状态。
fn dedicated_id_path(tool: &str) -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))?;
    Some(base.join(tool).join("dedicated-target.json"))
}

fn read_dedicated_id(tool: &str) -> Option<String> {
    let path = dedicated_id_path(tool)?;
    let raw = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    v.get("id").and_then(Value::as_str).map(str::to_string)
}

fn write_dedicated_id(tool: &str, id: &str, cdp: &str) {
    let Some(path) = dedicated_id_path(tool) else { return };
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() { return; }
    let body = json!({ "id": id, "cdp": cdp }).to_string();
    if std::fs::write(&path, body).is_err() { return; }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

/// 找由 `tool` 控制的专用常驻标签页；没有就新建并记住它的 target id。
/// `tool` 决定持久化目录名与 hash 标记（如 `ldeepseek` → `#ldeepseek-dedicated`）。
/// 只认两类标签：持久化的 target id（导航不变，最稳）或 URL 带 hash 标记（兼容旧版）。
/// 绝不碰用户自己开的页面。返回的 page 各请求复用、永不关闭。
pub fn dedicated_target(cdp: &str, tool: &str, site_url: &str) -> Result<(String, String), AppError> {
    let mark = format!("#{tool}-dedicated");
    let list = agent()
        .get(format!("{cdp}/json/list"))
        .call()
        .map_err(|e| AppError::Runtime(format!("读 {cdp}/json/list 失败: {e}")))?;
    let raw = list
        .into_body()
        .read_to_string()
        .map_err(|e| AppError::Runtime(format!("读标签页列表失败: {e}")))?;
    let targets: Value = serde_json::from_str(&raw)
        .map_err(|e| AppError::Runtime(format!("CDP 列表不是 JSON: {e}")))?;

    let saved_id = read_dedicated_id(tool);
    let mut found: Option<(String, String)> = None;
    if let Some(items) = targets.as_array() {
        // 第一轮：持久化的 target id（SPA 导航会抹掉 hash，target id 稳定）
        if let Some(ref sid) = saved_id {
            for t in items {
                let kind = t.get("type").and_then(Value::as_str).unwrap_or("");
                let ws = t.get("webSocketDebuggerUrl").and_then(Value::as_str).unwrap_or("");
                if kind == "page" && t.get("id").and_then(Value::as_str) == Some(sid.as_str()) && !ws.is_empty() {
                    found = Some((sid.clone(), ws.to_string()));
                    break;
                }
            }
        }
        // 第二轮：URL hash 标记（兼容旧版 / 持久化文件丢失）
        if found.is_none() {
            for t in items {
                let url = t.get("url").and_then(Value::as_str).unwrap_or("");
                let kind = t.get("type").and_then(Value::as_str).unwrap_or("");
                let ws = t.get("webSocketDebuggerUrl").and_then(Value::as_str).unwrap_or("");
                if kind == "page" && url.contains(&mark) && !ws.is_empty() {
                    found = Some((
                        t.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                        ws.to_string(),
                    ));
                    break;
                }
            }
        }
    }

    let (id, ws) = match found {
        Some(pair) => pair,
        None => {
            let pair = create_target_url(cdp, &format!("{site_url}{mark}"))?;
            write_dedicated_id(tool, &pair.0, cdp);
            pair
        }
    };
    Ok((id, ws))
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
