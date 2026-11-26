//! 自动二维码登录：打开登录页 → 截取/定时刷新二维码 → 轮询登录态 → 保存账号。
//!
//! 和 `ldeepseek login` 的区别：
//! - 不需要用户点「导出 Cookies」，登录成功后自动导出；
//! - 二维码不是一次性截图，页面自动刷新 + 每 `--refresh` 秒重新截图；
//! - 多账号：`--account` 区分，`--fresh` 会先清掉该站点旧 cookies 再登录新账号。

use crate::accounts;
use crate::providers::Provider;
use lilyco::prelude::*;
use lilyco_webchat_core::cdp::{self, Client};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};

pub struct LoginOptions<'a> {
    pub provider: Provider,
    pub account: &'a str,
    pub cdp: &'a str,
    pub login_url: &'a str,
    pub timeout: u64,
    pub refresh: u64,
    pub fresh: bool,
    pub force: bool,
    pub open_image: bool,
}

/// 选一个可用 CDP（显式 → `LWEBAI_CDP` → ego-lite → 9229/9222/9223/9333）
pub fn pick_cdp(hint: &str) -> Result<String, AppError> {
    let mut tried = Vec::new();
    for ep in cdp::candidate_endpoints(hint, "LWEBAI_CDP") {
        match cdp::probe(&ep) {
            Ok(()) => return Ok(ep),
            Err(e) => tried.push(format!("  - {ep}: {e}")),
        }
    }
    Err(AppError::InvalidInput(format!(
        "连不上可用的 Chromium CDP（先启动 ego-browser 或桌面客户端并开启 --remote-debugging-port）\n试过的端点：\n{}",
        tried.join("\n")
    )))
}

pub fn run(opts: LoginOptions<'_>, ctx: &Context) -> Result<Value, AppError> {
    let started = Instant::now();
    let ep = pick_cdp(opts.cdp)?;
    let account_path = accounts::account_path(opts.provider, opts.account)?;
    if account_path.exists() && !opts.force {
        return Err(AppError::InvalidInput(format!(
            "账号已存在：{}。换一个 --account 名称，或加 --force 覆盖",
            account_path.display()
        )));
    }
    let qr_path = accounts::qr_path(opts.provider, opts.account)?;
    let login_url = if opts.login_url.trim().is_empty() {
        opts.provider.login_url()
    } else {
        opts.login_url.trim()
    };

    ctx.log(
        LogLevel::Info,
        format!(
            "{}：打开 {}（CDP {}），自动截取/刷新登录二维码",
            opts.provider.display(),
            login_url,
            ep
        ),
    );

    let (_target_id, ws_url) = cdp::create_target_url(&ep, "about:blank")?;
    let mut client = Client::connect(&ws_url)?;
    client.bring_to_front();
    if opts.fresh {
        let removed = client
            .clear_cookies_for_domain(opts.provider.domain_filter())
            .unwrap_or(0);
        let _ = client.clear_origin(opts.provider.origin());
        ctx.log(
            LogLevel::Info,
            format!(
                "已清掉 {} 的旧登录态（{} 条 cookies），准备登录新账号",
                opts.provider.id(),
                removed
            ),
        );
    }
    client.navigate(login_url)?;
    client.inject_control_badge("等待扫码登录");
    std::thread::sleep(Duration::from_millis(2200));
    let _ = client.eval_js(opts.provider.activate_login_js());
    std::thread::sleep(Duration::from_millis(1200));

    // 不 fresh 且当前已登录：直接收进账号仓库（不用再扫一次）
    if !opts.fresh {
        if let Ok(bundle) = client.export_cookies_and_storage(opts.provider.domain_filter()) {
            if opts.provider.logged_in(&bundle) {
                let saved = accounts::save_bundle(opts.provider, opts.account, &bundle)?;
                let count = bundle["cookie_count"].as_u64().unwrap_or(0);
                client.inject_control_badge("✅ 已登录，账号已保存");
                ctx.log(
                    LogLevel::Info,
                    format!(
                        "检测到已登录，已保存 {} 条 cookies 到 {}",
                        count,
                        saved.display()
                    ),
                );
                return Ok(json!({
                    "ok": true, "provider": opts.provider.id(), "account": opts.account,
                    "saved_to": saved.display().to_string(), "cookie_count": count,
                    "existing": true, "elapsed_ms": started.elapsed().as_millis() as u64,
                }));
            }
        }
    }

    if let Err(e) = client.capture_screenshot(&qr_path) {
        ctx.log(LogLevel::Warn, format!("截图失败: {e}"));
    } else if opts.open_image {
        open_image(&qr_path);
        ctx.log(
            LogLevel::Info,
            format!(
                "二维码已保存并打开：{}（页面会自动刷新，工具每 {} 秒更新截图）",
                qr_path.display(),
                opts.refresh.max(5)
            ),
        );
    }

    let deadline = Instant::now() + Duration::from_secs(opts.timeout.max(30));
    let refresh_every = Duration::from_secs(opts.refresh.max(5));
    let mut last_shot = Instant::now();
    let mut last_open = Instant::now();
    loop {
        if Instant::now() > deadline {
            return Err(AppError::Runtime(format!(
                "等了 {} 秒仍未登录。二维码截图：{}（页面/二维码过期会自动刷新；若是密码登录，请在页面完成登录）",
                opts.timeout.max(30),
                qr_path.display()
            )));
        }
        if let Ok(bundle) = client.export_cookies_and_storage(opts.provider.domain_filter()) {
            if opts.provider.logged_in(&bundle) {
                let saved = accounts::save_bundle(opts.provider, opts.account, &bundle)?;
                let count = bundle["cookie_count"].as_u64().unwrap_or(0);
                client.inject_control_badge("✅ 已登录，账号已保存");
                ctx.log(
                    LogLevel::Info,
                    format!("登录成功：{} 条 cookies → {}", count, saved.display()),
                );
                return Ok(json!({
                    "ok": true, "provider": opts.provider.id(), "account": opts.account,
                    "saved_to": saved.display().to_string(), "cookie_count": count,
                    "qr_image": qr_path.display().to_string(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }));
            }
        }
        if last_shot.elapsed() >= refresh_every {
            let _ = client.eval_js(opts.provider.refresh_qr_js());
            match client.capture_screenshot(&qr_path) {
                Ok(()) => {
                    if opts.open_image && last_open.elapsed() >= refresh_every {
                        open_image(&qr_path);
                        last_open = Instant::now();
                    }
                }
                Err(e) => ctx.log(LogLevel::Warn, format!("刷新截图失败: {e}")),
            }
            last_shot = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(2000));
    }
}

fn open_image(path: &Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", &path.display().to_string()])
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_path_rejects_bad_names() {
        assert!(accounts::validate_name("../x").is_err());
        assert!(accounts::validate_name("lilyco").is_ok());
        let _ = Provider::Doubao;
    }
}
