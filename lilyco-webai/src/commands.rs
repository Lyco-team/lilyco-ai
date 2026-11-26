//! lwebai 的四个面命令：login / accounts / use / remove / status。

use crate::accounts;
use crate::login::{self, LoginOptions};
use crate::providers::Provider;
use lilyco::prelude::*;
use serde_json::{json, Value};
use std::time::Instant;

/// 登录并保存账号（T1）
#[derive(App)]
#[app(
    name = "login",
    run = "run_login",
    safety = "t1",
    about = "Open a provider's real login page over CDP, automatically capture the QR/login screen to a PNG, refresh that screenshot every --refresh seconds (the site also refreshes the QR), poll for login cookies/localStorage, then save the account as cookies + localStorage (0600). Use --fresh to clear the provider's existing session first when adding another account. No Python/Go backend and no third-party proxy: this is the same pure-Rust CDP driver used by ldeepseek/lchatgpt. Safety T1: captures a real login credential and writes the local account store."
)]
pub struct Login {
    #[arg(
        about = "doubao / qianwen / kimi / zcode (aliases: 豆包, qwen, 千问, zai)",
        default = "doubao"
    )]
    provider: String,
    #[arg(about = "Account name for multi-account storage", default = "default")]
    account: String,
    #[arg(
        about = "Explicit CDP endpoint (empty auto-detects ego-lite/9229; can point at a desktop client's remote debugging port)",
        default = ""
    )]
    cdp: String,
    #[arg(
        about = "Override the login URL (e.g. a desktop client's local page); empty uses the provider's web login",
        default = ""
    )]
    login_url: String,
    #[arg(about = "Seconds to wait for login", default = 300)]
    timeout: u64,
    #[arg(
        about = "Seconds between QR screenshot refreshes (minimum 5)",
        default = 20
    )]
    refresh: u64,
    #[arg(
        about = "Clear the provider's existing cookies/localStorage first (for logging in a second account)",
        default = false
    )]
    fresh: bool,
    #[arg(about = "Overwrite an existing account file", default = false)]
    force: bool,
    #[arg(
        about = "Do not open the QR PNG in the system image viewer",
        default = false
    )]
    no_open: bool,
}

fn run_login(app: &Login, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    let provider = Provider::parse(&app.provider)?;
    let out = login::run(
        LoginOptions {
            provider,
            account: &app.account,
            cdp: &app.cdp,
            login_url: &app.login_url,
            timeout: app.timeout,
            refresh: app.refresh,
            fresh: app.fresh,
            force: app.force,
            open_image: !app.no_open,
        },
        ctx,
    )?;
    ctx.done(out.clone(), start.elapsed().as_millis() as u64);
    Ok(out)
}

/// 多账号列表（T0）
#[derive(App)]
#[app(
    name = "accounts",
    run = "run_accounts",
    safety = "t0",
    about = "List saved accounts from ~/.local/share/lwebai/accounts (provider, account name, cookie count, localStorage keys, saved_at, path) and show the current default account. Read-only (safety T0)."
)]
pub struct Accounts {
    #[arg(about = "Only show this provider (empty means all four)", default = "")]
    provider: String,
}

fn run_accounts(app: &Accounts, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    let filter = if app.provider.trim().is_empty() {
        None
    } else {
        Some(Provider::parse(&app.provider)?)
    };
    let list = accounts::list(filter)?;
    let current = accounts::current();
    for a in &list {
        ctx.log(
            LogLevel::Info,
            format!(
                "{}/{}  cookies={}  saved_at={}",
                a["provider"].as_str().unwrap_or(""),
                a["account"].as_str().unwrap_or(""),
                a["cookie_count"],
                a["saved_at"]
            ),
        );
    }
    let out = json!({
        "ok": true,
        "count": list.len(),
        "accounts": list,
        "current": current.map(|(p, a)| json!({"provider": p, "account": a})),
        "elapsed_ms": start.elapsed().as_millis() as u64,
    });
    ctx.done(out.clone(), start.elapsed().as_millis() as u64);
    Ok(out)
}

/// 切换默认账号（T1）
#[derive(App)]
#[app(
    name = "use",
    run = "run_use",
    safety = "t1",
    about = "Set the default provider/account in ~/.local/share/lwebai/current.json after verifying the account file exists. Safety T1: writes local account selection."
)]
pub struct Use {
    #[arg(about = "doubao / qianwen / kimi / zcode", default = "doubao")]
    provider: String,
    #[arg(about = "Saved account name", default = "default")]
    account: String,
}

fn run_use(app: &Use, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    let provider = Provider::parse(&app.provider)?;
    let _ = accounts::load(provider, &app.account)?;
    let path = accounts::set_current(provider, &app.account)?;
    let out = json!({
        "ok": true, "provider": provider.id(), "account": app.account,
        "current_file": path.display().to_string(),
        "elapsed_ms": start.elapsed().as_millis() as u64,
    });
    ctx.log(
        LogLevel::Info,
        format!("默认账号已切换为 {}/{}", provider.id(), app.account),
    );
    ctx.done(out.clone(), start.elapsed().as_millis() as u64);
    Ok(out)
}

/// 删除账号（T1）
#[derive(App)]
#[app(
    name = "remove",
    run = "run_remove",
    safety = "t1",
    about = "Delete a saved account file (and clear current.json if it points at it). Safety T1: removes local credential data."
)]
pub struct Remove {
    #[arg(about = "doubao / qianwen / kimi / zcode", default = "doubao")]
    provider: String,
    #[arg(about = "Saved account name", default = "default")]
    account: String,
}

fn run_remove(app: &Remove, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    let provider = Provider::parse(&app.provider)?;
    let path = accounts::remove(provider, &app.account)?;
    let out = json!({
        "ok": true, "provider": provider.id(), "account": app.account,
        "removed": path.display().to_string(),
        "elapsed_ms": start.elapsed().as_millis() as u64,
    });
    ctx.log(LogLevel::Info, format!("已删除 {}", path.display()));
    ctx.done(out.clone(), start.elapsed().as_millis() as u64);
    Ok(out)
}

/// CDP 与账号状态（T0）
#[derive(App)]
#[app(
    name = "status",
    run = "run_status",
    safety = "t0",
    about = "Check CDP reachability, count saved accounts per provider and show the current default. Read-only (safety T0)."
)]
pub struct Status {
    #[arg(about = "Explicit CDP endpoint", default = "")]
    cdp: String,
}

fn run_status(app: &Status, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    let (cdp, cdp_error) = match login::pick_cdp(&app.cdp) {
        Ok(ep) => (Some(ep), None::<String>),
        Err(e) => (None, Some(format!("{e}"))),
    };
    let list = accounts::list(None).unwrap_or_default();
    let mut counts = serde_json::Map::new();
    for p in Provider::ALL {
        let n = list
            .iter()
            .filter(|a| a["provider"].as_str() == Some(p.id()))
            .count();
        counts.insert(p.id().to_string(), json!(n));
    }
    let out = json!({
        "ok": cdp.is_some(),
        "cdp": cdp,
        "cdp_error": cdp_error,
        "accounts": counts,
        "current": accounts::current().map(|(p,a)| json!({"provider":p,"account":a})),
        "store": accounts::home().map(|p| p.display().to_string()),
        "elapsed_ms": start.elapsed().as_millis() as u64,
    });
    if out["ok"].as_bool().unwrap_or(false) {
        ctx.log(
            LogLevel::Info,
            format!(
                "CDP {} · 账号 {}",
                out["cdp"].as_str().unwrap_or(""),
                list.len()
            ),
        );
    } else {
        ctx.log(
            LogLevel::Warn,
            cdp_error.unwrap_or_else(|| "CDP 不可用".to_string()),
        );
    }
    ctx.done(out.clone(), start.elapsed().as_millis() as u64);
    Ok(out)
}
