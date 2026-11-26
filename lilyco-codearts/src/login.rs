//! 浏览器登录华为云，换回临时 AK/SK/STS。
//!
//! 流程抄自 AIcivilization/dsh-model 的 `codearts-login.js`（MIT）：
//!
//! 1. 本地起一个回调服务器（默认 127.0.0.1:8791，占用就换随机端口）；
//! 2. 打开 `auth.huaweicloud.com/authui/login.html?service=<跳转地址>`；
//! 3. 登录后华为云把浏览器转回 `http://127.0.0.1:<port>/authentication?secret=<secret>`；
//! 4. 拿 ticket_id + secret 轮询 `snap-manager/v1/login/ticket` 换凭据。
//!
//! 临时凭据（STS）会过期，过期后重跑一次 `lcodearts login` 即可；长期 AK/SK 可以
//! 走环境变量 `LCODEARTS_AK/SK`，不依赖这条链路。

use lilyco::prelude::AppError;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use crate::cred::Credential;
use crate::util;

const AUTH_BASE: &str = "https://auth.huaweicloud.com/authui/login.html";
const DOER_REDIRECT: &str = "https://devcloud.cn-north-4.huaweicloud.com/doer/redirect";
const TICKET_ENDPOINT: &str =
    "https://snap-access.cn-north-4.myhuaweicloud.com/snap-manager/v1/login/ticket";
const PLUGIN_NAME: &str = "snap_jetbrains";
const PLUGIN_VERSION: &str = "26.3.3";
const CALLBACK_PATH: &str = "/authentication";
const DEFAULT_PORT: u16 = 8791;
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) lcodearts/0.3";

pub struct LoginOutcome {
    pub credential: Credential,
    pub login_url: String,
    pub saved_to: std::path::PathBuf,
}

/// 完整登录：起回调、开浏览器、等授权、换凭据、落盘。
pub fn run(timeout_secs: u64, open_browser: bool) -> Result<LoginOutcome, AppError> {
    let ticket = util::uuid_v4();
    let listener = bind_listener().map_err(|e| {
        AppError::Runtime(format!(
            "回调服务器起不来（127.0.0.1:{DEFAULT_PORT} 或随机端口）：{e}"
        ))
    })?;
    let port = listener
        .local_addr()
        .map_err(|e| AppError::Runtime(format!("读回调端口失败: {e}")))?
        .port();
    let login_url = build_login_url(port, &ticket);
    // 浏览器没能自动打开时（远程/无桌面），用户至少能从终端复制这个地址
    eprintln!("lcodearts login: 请打开 {login_url}");

    if open_browser {
        let _ = std::process::Command::new("xdg-open")
            .arg(&login_url)
            .spawn();
    }

    let secret = wait_for_secret(&listener, timeout_secs).map_err(|e| {
        AppError::Runtime(format!("等待华为云授权失败：{e}\n登录地址：{login_url}"))
    })?;
    let mut credential = poll_for_credential(&ticket, &secret, 120)?;
    credential.source = "huawei-oauth".to_string();
    let saved_to = crate::cred::save(&credential)?;
    Ok(LoginOutcome {
        credential,
        login_url,
        saved_to,
    })
}

fn bind_listener() -> std::io::Result<TcpListener> {
    // 必须非阻塞：wait_for_secret 靠 accept 的 WouldBlock + sleep 轮询来实现在 timeout
    // 内退出；阻塞 accept 会让超时永远不生效。
    let listener = match TcpListener::bind(("127.0.0.1", DEFAULT_PORT)) {
        Ok(l) => l,
        Err(_) => TcpListener::bind(("127.0.0.1", 0))?,
    };
    listener.set_nonblocking(true)?;
    Ok(listener)
}

pub fn build_login_url(port: u16, ticket: &str) -> String {
    let callback = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let redirect = format!(
        "{DOER_REDIRECT}?IdeaType=jetbrains&auth_callback_url={}&plugin-name={PLUGIN_NAME}\
         &plugin-version={PLUGIN_VERSION}&ticket_id={}",
        js_encode(&callback),
        js_encode(ticket)
    );
    format!("{AUTH_BASE}?service={}", js_encode(&redirect))
}

/// 阻塞等回调；非 /authentication 的请求（favicon 等）直接 404 后继续等。
fn wait_for_secret(listener: &TcpListener, timeout_secs: u64) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(secret) = handle_connection(stream) {
                    return Ok(secret);
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    std::thread::sleep(Duration::from_millis(100));
                } else {
                    return Err(format!("accept 失败: {e}"));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(format!("{timeout_secs} 秒内没等到授权回调"))
}

fn handle_connection(mut stream: TcpStream) -> Option<String> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).ok()?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let first = req.lines().next().unwrap_or("");
    let path = first.split_whitespace().nth(1).unwrap_or("");
    let (path_only, query) = path.split_once('?').unwrap_or((path, ""));

    if !path_only.starts_with(CALLBACK_PATH) {
        let body = "not found";
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        return None;
    }

    let secret = query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == "secret").then(|| v.to_string())
    });

    let body = if secret.is_some() {
        "<!doctype html><meta charset=\"utf-8\"><title>CodeArts 登录</title>\
         <div style=\"font:15px/1.6 system-ui,sans-serif;max-width:440px;margin:80px auto;text-align:center\">\
         <h2 style=\"color:#0f9d58\">✓ 已收到华为云授权</h2>\
         <p style=\"color:#666\">正在换取凭据，可以关闭这个页面，回到终端查看结果。</p></div>"
    } else {
        "缺少 secret，登录可能没走完，请回终端重试。"
    };
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
    secret
}

/// 轮询 ticket 接口换 AK/SK（最多 `attempts` 次、每秒一次）。
pub fn poll_for_credential(
    ticket: &str,
    secret: &str,
    attempts: u32,
) -> Result<Credential, AppError> {
    let url = format!(
        "{TICKET_ENDPOINT}?ticket_id={}&secret={}",
        js_encode(ticket),
        js_encode(secret)
    );
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .new_agent();

    for i in 0..attempts.max(1) {
        if i > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        let res = agent
            .get(&url)
            .header("Content-Type", "application/json;charset=UTF-8")
            .header("plugin-name", PLUGIN_NAME)
            .header("plugin-version", PLUGIN_VERSION)
            .header("User-Agent", USER_AGENT)
            .call();
        let Ok(res) = res else { continue };
        if !(200..300).contains(&res.status().as_u16()) {
            continue;
        }
        let Ok(text) = res.into_body().read_to_string() else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(cred) = parse_credential(&v) {
            return Ok(cred);
        }
    }
    Err(AppError::Runtime(
        "等华为云返回凭据超时（授权可能没完成），再运行一次 `lcodearts login`".to_string(),
    ))
}

/// 华为云有两种返回形状（credential / result），字段名还有大小写变体，全部要认。
pub fn parse_credential(v: &Value) -> Option<Credential> {
    let user_name = v
        .get("user_name")
        .or_else(|| v.get("userName"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let domain_id = v
        .get("domain_id")
        .or_else(|| v.get("domainId"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let (ak, sk, st, exp) = if let Some(c) = v.get("credential") {
        (
            pick(c, &["access", "accessKeyId", "access_key_id"])?,
            pick(c, &["secret", "secretAccessKey", "secret_access_key"])?,
            pick(c, &["securitytoken", "securityToken", "security_token"]),
            pick(c, &["expires_at", "expiresAt", "expiration"]),
        )
    } else if let Some(r) = v.get("result") {
        (
            pick(r, &["accessKeyId", "access_key_id", "access"])?,
            pick(r, &["secretAccessKey", "secret_access_key", "secret"])?,
            pick(r, &["securityToken", "security_token", "securitytoken"]),
            pick(r, &["expiration", "expiresAt", "expires_at"]),
        )
    } else {
        return None;
    };
    Some(Credential {
        access_key: ak,
        secret_key: sk,
        security_token: st,
        expires_at: exp,
        user_name,
        domain_id,
        source: "huawei-oauth".to_string(),
        saved_at: None,
    })
}

fn pick(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        v.get(*k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

/// encodeURIComponent 等价：只保留 `A-Za-z0-9-._~`，其余 UTF-8 字节 → `%XX`
fn js_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn credential_shapes_are_both_accepted() {
        let v = json!({
            "credential": {
                "access": "AK1", "secret": "SK1",
                "securitytoken": "STS1", "expires_at": "2026-11-27T00:00:00Z"
            },
            "user_name": "lilyco42"
        });
        let c = parse_credential(&v).unwrap();
        assert_eq!(c.access_key, "AK1");
        assert_eq!(c.security_token.as_deref(), Some("STS1"));
        assert_eq!(c.user_name.as_deref(), Some("lilyco42"));

        let v = json!({
            "result": {
                "accessKeyId": "AK2", "secretAccessKey": "SK2",
                "securityToken": "STS2", "expiration": "2026-11-27T00:00:00Z"
            }
        });
        let c = parse_credential(&v).unwrap();
        assert_eq!(c.access_key, "AK2");
        assert_eq!(c.secret_key, "SK2");
    }

    #[test]
    fn credential_without_ak_sk_is_rejected() {
        assert!(parse_credential(&json!({"credential": {"access": "AK"}})).is_none());
    }

    #[test]
    fn login_url_encodes_callback_and_ticket() {
        let url = build_login_url(8791, "ticket/with+chars");
        assert!(url.starts_with(AUTH_BASE));
        // service 参数整体再做一次 encodeURIComponent，所以回调里的 % 变成 %25
        assert!(
            url.contains(
                "auth_callback_url%3Dhttp%253A%252F%252F127.0.0.1%253A8791%252Fauthentication"
            ),
            "{url}"
        );
        assert!(
            url.contains("ticket_id%3Dticket%252Fwith%252Bchars"),
            "{url}"
        );
    }
}
