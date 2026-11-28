//! `lchatgpt import-cookies` —— 把一份浏览器的 Cookie 请求头注入 lchatgpt 专用标签页。
//!
//! 用途：当现有浏览器登录态被风控、或想换用另一份「干净」的 cookies 时，
//! 把从浏览器 Network 面板复制的 `Cookie:` 头（形如 `name=value; name2=value2`）
//! 从 **stdin** 喂进来，工具解析后经 CDP `Network.setCookies` 注入专用页并重载。
//!
//! 安全约定：cookie 原文只经过 stdin → 内存 → CDP，不写进代码、日志、仓库或任何
//! 持久文件；返回值与日志里只有条数和域名，不含任何 cookie 值。工具本身是 T1，
//! 因为它会改写浏览器的登录态。

use crate::browser;
use lilyco::prelude::*;
use serde_json::{json, Value};
use std::io::Read;

/// 导入 Cookie 头到专用页
#[derive(App)]
#[app(
    name = "import-cookies",
    run = "run_import_cookies",
    safety = "t1",
    about = "Inject a browser Cookie header (as copied from the Network panel: 'name=value; name2=value2') into lchatgpt's dedicated ChatGPT tab so a clean login state replaces a rate-limited one. The raw cookie is read from stdin only — never hardcoded, logged, or written to any file — and is sent straight to the browser over CDP Network.setCookies, then the dedicated tab is reloaded. Returns { imported, domain, cdp, target_id } with no cookie values echoed. Safety T1 because it changes the browser's login state; MCP surfaces refuse it. Pair with `ask` (which reuses the same dedicated tab) to continue without re-login."
)]
pub struct ImportCookies {
    #[arg(
        about = "CDP endpoint of the browser (default: $LCHATGPT_CDP, then ego-lite config, then 9229/9222/9223/9333)",
        default = ""
    )]
    cdp: String,
}

/// Cookie 头里这些是属性而不是 cookie 名，跳过
fn is_attribute(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "path" | "domain" | "expires" | "max-age" | "samesite" | "priority" | "partitioned"
    )
}

/// 把 `name=value; ...` 解析成 CDP CookieParam 数组。
/// `__Secure-`/`__Host-` 前缀按 Chrome 规则补 secure；`__Host-` 必须 host-only。
fn parse_cookie_header(raw: &str) -> Vec<Value> {
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() + 365 * 24 * 3600)
        .unwrap_or(0);
    let mut out = Vec::new();
    for part in raw.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, value) = match part.split_once('=') {
            Some(pair) => pair,
            None => continue,
        };
        let name = name.trim();
        if name.is_empty() || is_attribute(name) {
            continue;
        }
        let mut value = value.trim();
        if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            value = &value[1..value.len() - 1];
        }
        let host_only = name.starts_with("__Host-");
        let secure = name.starts_with("__Secure-") || host_only;
        let http_only = name.contains("session-token") || secure;
        out.push(json!({
            "name": name,
            "value": value,
            "domain": if host_only { "chatgpt.com" } else { ".chatgpt.com" },
            "path": "/",
            "secure": secure,
            "httpOnly": http_only,
            "sameSite": if secure { "None" } else { "Lax" },
            "expires": expires,
        }));
    }
    out
}

fn run_import_cookies(app: &ImportCookies, ctx: &Context) -> Result<Value, AppError> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|e| AppError::Runtime(format!("读 stdin 失败: {e}")))?;
    let cookies = parse_cookie_header(&raw);
    if cookies.is_empty() {
        return Err(AppError::InvalidArg(
            "stdin 里没解析出 cookie。请粘贴浏览器 Cookie 头（形如 name=value; name2=value2）"
                .to_string(),
        ));
    }

    let mut chosen = None;
    let mut last_err = String::new();
    for cdp in browser::candidate_endpoints(&app.cdp) {
        match browser::probe(&cdp) {
            Ok(()) => {
                chosen = Some(cdp);
                break;
            }
            Err(e) => {
                last_err = e.to_string().lines().next().unwrap_or("").to_string();
            }
        }
    }
    let cdp = chosen.ok_or_else(|| {
        AppError::Runtime(format!("没有可用的浏览器 CDP 端点（最后一次: {last_err}）"))
    })?;

    let (target_id, ws) = browser::dedicated_target(&cdp, None)?;
    let mut client = browser::Client::connect(&ws)?;
    client.set_cookies(&cookies)?;
    client.reload()?;

    let count = cookies.len();
    ctx.log(
        LogLevel::Info,
        format!("已向 lchatgpt 专用页注入 {count} 条 cookie 并重载（值不落日志）"),
    );
    Ok(json!({
        "imported": count,
        "domain": ".chatgpt.com",
        "cdp": cdp,
        "target_id": target_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cookie_header_with_secure_prefixes() {
        let cookies = parse_cookie_header(
            "a=1; __Secure-next-auth.session-token.0=eyJhb.abc; __Host-next-auth.csrf-token=x%7Cy; Path=/",
        );
        assert_eq!(cookies.len(), 3);
        assert_eq!(cookies[0]["name"], "a");
        assert_eq!(cookies[1]["secure"], true);
        assert_eq!(cookies[1]["httpOnly"], true);
        assert_eq!(cookies[1]["domain"], ".chatgpt.com");
        assert_eq!(cookies[2]["secure"], true);
        assert_eq!(cookies[2]["domain"], "chatgpt.com");
        assert!(cookies.iter().all(|c| c.get("value").is_some()));
    }

    #[test]
    fn quoted_values_are_unwrapped() {
        let cookies = parse_cookie_header("_uasid=\"Z0FBQUFBQnJDV2Q==\"");
        assert_eq!(cookies[0]["value"], "Z0FBQUFBQnJDV2Q==");
    }
}
