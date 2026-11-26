//! 简单命令：login / auth / quota / models。
//!
//! 每条命令都是一个 `#[derive(App)]` 结构体，所以同一份 handler 自动出现在
//! CLI / TUI / Web / MCP 四个面；命令的分级与 lchatgpt 一致：
//! 读接口 T0，登录/写本地凭据 T1。

use lilyco::prelude::*;
use serde_json::{json, Value};
use std::time::Instant;

use crate::{api, cred, login};

/// 浏览器授权换临时 AK/SK（T1）
#[derive(App)]
#[app(
    name = "login",
    run = "run_login",
    safety = "t1",
    about = "Open the Huawei Cloud sign-in page in a browser, receive the OAuth callback on 127.0.0.1, exchange ticket+secret for a temporary AK/SK/STS and save it (0600) for quota, models, check-in and the OpenAI-compatible endpoint. Safety T1: writes the local credential store."
)]
pub struct Login {
    #[arg(
        about = "Seconds to wait for the browser callback before giving up",
        default = 180
    )]
    timeout: u64,
    #[arg(
        about = "Only print the sign-in URL; do not open a browser (for headless/remote hosts)",
        default = false
    )]
    no_browser: bool,
}

fn run_login(app: &Login, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    ctx.log(
        LogLevel::Info,
        "正在打开华为云登录页；如果浏览器没有自动弹出，请手动打开下面打印的地址".to_string(),
    );
    let outcome = login::run(app.timeout, !app.no_browser)?;
    let out = json!({
        "ok": true,
        "login_url": outcome.login_url,
        "saved_to": outcome.saved_to.display().to_string(),
        "access_key": outcome.credential.masked(),
        "user_name": outcome.credential.user_name,
        "expires_at": outcome.credential.expires_at,
        "source": outcome.credential.source,
        "elapsed_ms": start.elapsed().as_millis() as u64,
    });
    ctx.log(
        LogLevel::Info,
        format!(
            "凭据已保存到 {}（{}，到期 {}）",
            outcome.saved_to.display(),
            outcome.credential.masked(),
            outcome
                .credential
                .expires_at
                .as_deref()
                .unwrap_or("未提供（长期 AK/SK）")
        ),
    );
    ctx.done(out.clone(), start.elapsed().as_millis() as u64);
    Ok(out)
}

/// 查看当前凭据状态（T0）
#[derive(App)]
#[app(
    name = "auth",
    run = "run_auth",
    safety = "t0",
    about = "Show where the CodeArts credential comes from (env or local store), its masked AK, expiry and Huawei Cloud user. Read-only (safety T0); never prints the SK."
)]
pub struct Auth {}

fn run_auth(_app: &Auth, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    match cred::load() {
        Ok(c) => {
            let out = json!({
                "configured": true,
                "access_key": c.masked(),
                "source": c.source_label(),
                "security_token": c.security_token.is_some(),
                "expires_at": c.expires_at,
                "expired": c.expired(),
                "user_name": c.user_name,
                "domain_id": c.domain_id,
            });
            ctx.log(
                LogLevel::Info,
                format!(
                    "已配置：{}（来源 {}，STS {}，过期：{}）",
                    c.masked(),
                    c.source_label(),
                    if c.security_token.is_some() {
                        "有"
                    } else {
                        "无"
                    },
                    if c.expired() {
                        "是（请重新 login）"
                    } else {
                        "否"
                    }
                ),
            );
            ctx.done(out.clone(), start.elapsed().as_millis() as u64);
            Ok(out)
        }
        Err(e) => {
            let out = json!({
                "configured": false,
                "message": format!("{e}"),
                "hint": "运行 `lcodearts login` 授权，或设置 LCODEARTS_AK / LCODEARTS_SK",
            });
            ctx.log(LogLevel::Warn, format!("{e}"));
            ctx.done(out.clone(), start.elapsed().as_millis() as u64);
            Ok(out)
        }
    }
}

/// 积分套餐与余量（T0）
#[derive(App)]
#[app(
    name = "quota",
    run = "run_quota",
    safety = "t0",
    about = "Read the CodeArts credit package from GET /snap-manager/v1/statistics/plugin: package name/status, whether it is a credit or token package, total/used/remaining/expiring credits, percent left and the reset date. Read-only (safety T0); consumes no credits."
)]
pub struct Quota {}

fn run_quota(_app: &Quota, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    let c = cred::load()?;
    if c.expired() {
        return Err(AppError::InvalidInput(
            "本地记录的 CodeArts 凭据已过期：运行 `lcodearts login` 重新授权".to_string(),
        ));
    }
    let api = api::Api::new(c);
    let raw = api.credits()?;
    let snapshot = api::CreditSnapshot::from_value(&raw);
    let human = match snapshot.remain {
        Some(remain) => {
            let total = snapshot.total.unwrap_or(0.0);
            format!(
                "{}：剩余 {remain:.0}/{total:.0} 积分{}",
                if snapshot.package_name.is_empty() {
                    "CodeArts"
                } else {
                    &snapshot.package_name
                },
                snapshot
                    .end_date
                    .is_empty()
                    .then(String::new)
                    .unwrap_or_else(|| format!("（{} 重置）", snapshot.end_date))
            )
        }
        None => format!(
            "{}：没有积分套餐（token 套餐或未开通）",
            if snapshot.package_name.is_empty() {
                "CodeArts"
            } else {
                &snapshot.package_name
            }
        ),
    };
    ctx.log(LogLevel::Info, human);
    let out = json!({
        "ok": true,
        "package": {
            "name": snapshot.package_name,
            "status": snapshot.status,
            "is_credit_package": snapshot.is_credit_package,
            "is_token_package": snapshot.is_token_package,
            "end_date": snapshot.end_date,
        },
        "credits": {
            "total": snapshot.total,
            "used": snapshot.used,
            "remain": snapshot.remain,
            "expiring": snapshot.expiring,
            "percent": snapshot.percent,
        },
        "raw": raw,
        "elapsed_ms": start.elapsed().as_millis() as u64,
    });
    ctx.done(out.clone(), start.elapsed().as_millis() as u64);
    Ok(out)
}

/// 可用模型（T0）
#[derive(App)]
#[app(
    name = "models",
    run = "run_models",
    safety = "t0",
    about = "List the models available to this CodeArts account from GET /v1/model/builtin, with the credit ratio each model charges. Read-only (safety T0)."
)]
pub struct Models {}

fn run_models(_app: &Models, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    let c = cred::load()?;
    if c.expired() {
        return Err(AppError::InvalidInput(
            "本地记录的 CodeArts 凭据已过期：运行 `lcodearts login` 重新授权".to_string(),
        ));
    }
    let api = api::Api::new(c);
    let raw = api.models()?;
    let models: Vec<Value> = raw
        .iter()
        .map(|m| {
            json!({
                "id": m.get("model_id").cloned().unwrap_or(Value::Null),
                "name": m.get("model_name").cloned().unwrap_or(Value::Null),
                "description": m.get("model_desc").cloned().unwrap_or(Value::Null),
                "enabled": m.get("enable").and_then(Value::as_bool).unwrap_or(false),
                "supports_images": m.get("supports_images").and_then(Value::as_bool).unwrap_or(false),
                "credit_ratio": credit_ratio(m),
            })
        })
        .collect();
    for m in &models {
        ctx.log(
            LogLevel::Info,
            format!(
                "{}  {}{}",
                m["id"].as_str().unwrap_or("?"),
                m["credit_ratio"].as_str().unwrap_or(""),
                if m["enabled"].as_bool().unwrap_or(false) {
                    ""
                } else {
                    "  (disabled)"
                }
            ),
        );
    }
    let out = json!({
        "ok": true,
        "count": models.len(),
        "models": models,
        "elapsed_ms": start.elapsed().as_millis() as u64,
    });
    ctx.done(out.clone(), start.elapsed().as_millis() as u64);
    Ok(out)
}

/// 导入长期 AK/SK（T1）
///
/// 长期 AK/SK 必须在华为云 IAM「我的凭证 → 访问密钥」创建。本命令只负责把
/// 已有的凭据安全写入本地 0600 仓库，不生成、不上传、不写日志。
/// 支持三种输入：纯文本两行（AK / SK，可选第三行 STS）、华为云下载的 CSV、JSON
/// （`accessKey/secretKey` 或登录响应的 `credential` / `result` 形状）。
#[derive(App)]
#[app(
    name = "auth-import",
    run = "run_auth_import",
    safety = "t1",
    about = "Import a long-term Huawei Cloud AK/SK into the local 0600 store (no network, no upload, never logs the SK). Reads stdin by default: first line AK, second line SK, optional third line STS. Also accepts a Huawei Cloud IAM CSV (User Name, Access Key Id, Secret Access Key) or JSON (accessKey/secretKey, access_key_id/secret_access_key, or the credential/result shape). Long-term AK/SK has no STS and expires only per IAM policy, so daily auto check-in keeps working without re-login. Safety T1: writes the local credential store."
)]
pub struct AuthImport {
    #[arg(
        about = "Read from this file instead of stdin (supports TXT / CSV / JSON)",
        default = ""
    )]
    file: String,
}

fn run_auth_import(app: &AuthImport, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    let raw = if app.file.trim().is_empty() {
        ctx.log(
            LogLevel::Info,
            "从 stdin 读取：第一行 AK，第二行 SK（可选第三行 STS）；Ctrl-D 结束".to_string(),
        );
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
            .map_err(|e| AppError::Runtime(format!("读 stdin 失败: {e}")))?;
        s
    } else {
        std::fs::read_to_string(app.file.trim())
            .map_err(|e| AppError::Runtime(format!("读文件失败 {}: {e}", app.file.trim())))?
    };
    let credential = parse_import(&raw)?;
    let saved = cred::save(&credential)?;
    let long_term = credential.security_token.is_none();
    let out = json!({
        "ok": true,
        "saved_to": saved.display().to_string(),
        "access_key": credential.masked(),
        "long_term": long_term,
        "has_security_token": !long_term,
        "source": credential.source,
        "elapsed_ms": start.elapsed().as_millis() as u64,
    });
    ctx.log(
        LogLevel::Info,
        format!(
            "已导入 {} 到 {}（{}）",
            credential.masked(),
            saved.display(),
            if long_term {
                "长期 AK/SK，不会自动过期"
            } else {
                "含 STS，到期后需重新导入或 login"
            }
        ),
    );
    ctx.done(out.clone(), start.elapsed().as_millis() as u64);
    Ok(out)
}

fn parse_import(raw: &str) -> Result<cred::Credential, AppError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AppError::InvalidInput(
            "输入为空：需要 AK/SK（两行文本、华为云 CSV 或 JSON）".to_string(),
        ));
    }
    if trimmed.starts_with('{') {
        let v: Value = serde_json::from_str(trimmed)
            .map_err(|e| AppError::InvalidInput(format!("JSON 解析失败: {e}")))?;
        if let Ok(c) = serde_json::from_value::<cred::Credential>(v.clone()) {
            if !c.access_key.trim().is_empty() && !c.secret_key.trim().is_empty() {
                return finish_import(c);
            }
        }
        if let Some(c) = login::parse_credential(&v) {
            return finish_import(c);
        }
        return Err(AppError::InvalidInput(
            "JSON 里找不到 AK/SK（支持 accessKey/secretKey、access_key_id/secret_access_key、credential/result）"
                .to_string(),
        ));
    }

    let lines: Vec<&str> = trimmed.lines().collect();
    if lines.len() >= 2 && lines[0].to_ascii_lowercase().contains("access key id") {
        // 华为云 IAM 下载的 CSV：User Name,Access Key Id,Secret Access Key
        let header = split_csv(lines[0]);
        let row = split_csv(lines[1]);
        let find = |needle: &str| {
            header
                .iter()
                .position(|h| h.to_ascii_lowercase().contains(needle))
                .and_then(|i| row.get(i).cloned())
        };
        let ak = find("access key id").unwrap_or_default();
        let sk = find("secret access key").unwrap_or_default();
        if ak.is_empty() || sk.is_empty() {
            return Err(AppError::InvalidInput(
                "CSV 缺少 Access Key Id / Secret Access Key 列".to_string(),
            ));
        }
        return finish_import(cred::Credential {
            access_key: ak,
            secret_key: sk,
            security_token: None,
            expires_at: None,
            user_name: find("user name"),
            domain_id: None,
            source: "manual-csv".into(),
            saved_at: None,
        });
    }

    let parts: Vec<&str> = lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if parts.len() < 2 {
        return Err(AppError::InvalidInput(
            "纯文本至少需要两行：第一行 AK，第二行 SK（可选第三行 STS）".to_string(),
        ));
    }
    finish_import(cred::Credential {
        access_key: parts[0].to_string(),
        secret_key: parts[1].to_string(),
        security_token: parts.get(2).map(|s| s.to_string()),
        expires_at: None,
        user_name: None,
        domain_id: None,
        source: "manual".into(),
        saved_at: None,
    })
}

fn finish_import(mut c: cred::Credential) -> Result<cred::Credential, AppError> {
    c.access_key = c.access_key.trim().to_string();
    c.secret_key = c.secret_key.trim().to_string();
    c.security_token = c
        .security_token
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if c.access_key.is_empty() || c.secret_key.is_empty() {
        return Err(AppError::InvalidInput("AK 或 SK 为空".to_string()));
    }
    c.source = "manual".to_string();
    Ok(c)
}

/// 极简 CSV 行拆分，支持双引号包裹与 `""` 转义（IAM 下载文件只有这三列）
fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if in_quote && chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quote = !in_quote;
                }
            }
            ',' if !in_quote => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    out.push(cur.trim().to_string());
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_accepts_two_lines_and_csv() {
        let c = parse_import("AK123\nSK456\n").unwrap();
        assert_eq!(c.access_key, "AK123");
        assert_eq!(c.secret_key, "SK456");
        assert!(c.security_token.is_none());

        let csv = "User Name,Access Key Id,Secret Access Key\n\"lilyco42\",\"AKCSV\",\"SKCSV\"\n";
        let c = parse_import(csv).unwrap();
        assert_eq!(c.access_key, "AKCSV");
        assert_eq!(c.secret_key, "SKCSV");
        assert_eq!(c.user_name.as_deref(), Some("lilyco42"));
    }

    #[test]
    fn import_accepts_login_json_shape() {
        let v = r#"{"credential":{"access":"AKJ","secret":"SKJ","securitytoken":"STJ"}}"#;
        let c = parse_import(v).unwrap();
        assert_eq!(c.access_key, "AKJ");
        assert_eq!(c.security_token.as_deref(), Some("STJ"));
    }

    #[test]
    fn ratio_prefers_first_display_value() {
        let m = json!({
            "credit": [
                {"ratio": "0.5"},
                {"ratio_display": "0.7x"}
            ]
        });
        assert_eq!(credit_ratio(&m), "0.7x");
    }
}
