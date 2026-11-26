//! 每日签到（运营活动 `USER_LOGIN`，通常 +1000 积分）。
//!
//! 链路抄自 2951461586/CodeArts2API 与 AIcivilization/dsh-model：
//! `GET /v1/ops/delivery?channel=IDE` → 找 `type=USER_LOGIN` → `POST /v1/ops/claim`。
//! 只领服务端明确 `claimable=true` 的活动；邀请 / 学生认证需要真实事件触发，
//! 服务端也会拒绝直接领取，所以这里绝不伪造触发。
//!
//! 自动签到的语义：
//! - 按**北京时间**算「今天」；
//! - 过了 `after_hour` 点才尝试；
//! - 本地记录今天已经 success / already 就跳过；
//! - 服务端本身幂等，重复调用不会重复发积分。

use lilyco::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Instant;

use crate::{api, cred, util};

const HISTORY_LIMIT: usize = 60;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckinState {
    #[serde(default)]
    pub last_date: String,
    #[serde(default)]
    pub last_status: String,
    #[serde(default)]
    pub last_message: String,
    #[serde(default)]
    pub last_credit: f64,
    #[serde(default)]
    pub last_attempt: String,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub date: String,
    pub at: String,
    pub status: String,
    pub message: String,
    #[serde(default)]
    pub credit: f64,
}

pub fn state_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LCODEARTS_CHECKIN_STATE") {
        let p = PathBuf::from(p);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    cred::store_path().and_then(|p| p.parent().map(|d| d.join("checkin.json")))
}

pub fn load_state() -> CheckinState {
    let Some(path) = state_path() else {
        return CheckinState::default();
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(state: &CheckinState) {
    let Some(path) = state_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if serde_json::to_string_pretty(state)
        .ok()
        .and_then(|body| std::fs::write(&tmp, body).ok())
        .is_some()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn record(state: &mut CheckinState, status: &str, message: &str, credit: f64) {
    let date = util::beijing_day();
    let at = util::iso8601_from_epoch_secs(util::now_secs());
    state.last_date = date.clone();
    state.last_status = status.to_string();
    state.last_message = message.to_string();
    state.last_credit = credit;
    state.last_attempt = at.clone();
    state.history.push(HistoryEntry {
        date,
        at,
        status: status.to_string(),
        message: message.to_string(),
        credit,
    });
    if state.history.len() > HISTORY_LIMIT {
        let start = state.history.len() - HISTORY_LIMIT;
        state.history.drain(..start);
    }
    save_state(state);
}

#[allow(dead_code)]
fn already_today(state: &CheckinState) -> bool {
    state.last_date == util::beijing_day()
        && matches!(state.last_status.as_str(), "success" | "already")
}

/// 执行一次签到。`dry_run` 只查询活动，不领取。
pub fn run_once(dry_run: bool) -> Result<Value, AppError> {
    let cred = cred::load()?;
    if cred.expired() {
        return Err(AppError::InvalidInput(
            "本地记录的 CodeArts 凭据已过期：运行 `lcodearts login` 重新授权".to_string(),
        ));
    }
    let api = api::Api::new(cred);
    let mut state = load_state();
    let today = util::beijing_day();

    let activities = api.activities()?;
    let summary: Vec<Value> = activities
        .iter()
        .map(|a| {
            json!({
                "campaign_id": a.campaign_id,
                "type": a.kind,
                "title": a.title,
                "status": a.status,
                "claimable": a.claimable,
                "amount": a.amount,
                "unit": a.unit_label(),
                "trigger_mode": a.trigger_mode,
            })
        })
        .collect();

    let candidates = pick_daily(&activities);
    if candidates.is_empty() {
        let msg = format!("没有找到签到活动（共 {} 个运营活动）", activities.len());
        record(&mut state, "no_task", &msg, 0.0);
        return Ok(json!({
            "ok": true, "date": today, "status": "no_task", "message": msg,
            "activities": summary,
        }));
    }

    let claimable: Vec<&api::Activity> =
        candidates.iter().copied().filter(|a| a.claimable).collect();
    if dry_run {
        let message = if claimable.is_empty() {
            format!("暂无可领取的签到（{} 项已处理）", candidates.len())
        } else {
            format!(
                "可领取 {} 项签到：{}",
                claimable.len(),
                claimable
                    .iter()
                    .map(|a| format!("「{}」+{:.0} {}", a.title, a.amount, a.unit_label()))
                    .collect::<Vec<_>>()
                    .join("；")
            )
        };
        return Ok(json!({
            "ok": true, "date": today,
            "status": if claimable.is_empty() { "already" } else { "would_claim" },
            "message": message,
            "campaigns": candidates.iter().map(|a| json!({
                "id": a.campaign_id, "title": a.title, "status": a.status,
                "claimable": a.claimable, "amount": a.amount, "unit": a.unit_label(),
            })).collect::<Vec<_>>(),
            "activities": summary,
        }));
    }

    if claimable.is_empty() {
        let all_claimed = candidates.iter().all(|a| a.already_claimed());
        let msg = if all_claimed {
            format!("今日的 {} 项签到都已领取", candidates.len())
        } else {
            format!("暂无可领取的签到（{} 项）", candidates.len())
        };
        record(
            &mut state,
            if all_claimed { "already" } else { "not_ready" },
            &msg,
            0.0,
        );
        return Ok(json!({
            "ok": true, "date": today,
            "status": if all_claimed { "already" } else { "not_ready" },
            "message": msg,
            "campaigns": candidates.iter().map(|a| json!({
                "id": a.campaign_id, "title": a.title, "status": a.status,
                "claimable": a.claimable, "amount": a.amount, "unit": a.unit_label(),
            })).collect::<Vec<_>>(),
            "activities": summary,
        }));
    }

    let mut claims = Vec::new();
    let mut total = 0.0;
    let mut success_count = 0usize;
    let mut failure_count = 0usize;
    for a in &claimable {
        let entry = match api.claim(&a.campaign_id) {
            Ok(v) => {
                let (status, message, credit) = classify_claim(&v, a.amount, a.unit_label());
                if status == "success" {
                    success_count += 1;
                    total += credit;
                }
                if status == "failed" {
                    failure_count += 1;
                }
                json!({
                    "campaign_id": a.campaign_id, "title": a.title,
                    "status": status, "message": message, "credit": credit,
                    "claim": v,
                })
            }
            Err(e) => {
                failure_count += 1;
                json!({
                    "campaign_id": a.campaign_id, "title": a.title,
                    "status": "failed", "message": format!("{e}"), "credit": 0.0,
                })
            }
        };
        claims.push(entry);
    }

    let (status, message) = if success_count > 0 {
        let mut m = format!("签到成功 +{total:.0} 积分（{success_count} 项）");
        if failure_count > 0 {
            m.push_str(&format!("；{failure_count} 项失败"));
        }
        ("success", m)
    } else if failure_count > 0 {
        ("failed", format!("{failure_count} 项签到失败"))
    } else {
        ("already", "今日已领取".to_string())
    };
    record(&mut state, status, &message, total);
    Ok(json!({
        "ok": status != "failed",
        "date": today,
        "status": status,
        "message": message,
        "credit": total,
        "claims": claims,
        "campaign": claims.first().cloned().unwrap_or(Value::Null),
        "activities": summary,
    }))
}

/// 挑出「每日签到」类活动。
///
/// USER_LOGIN 不只一个：实测同一账号还有「Skill创新赛报名礼」这类 type=USER_LOGIN
/// 的活动，且可能排在前面；同时会有**多个**每日签到（1000 分 / 100 分）。
/// 这里先按标题过滤掉报名礼，再按「每日 / 签到 / 登录 / 可领取」加权排序，
/// 返回所有候选，由调用方逐个领取（服务端幂等）。
fn pick_daily(activities: &[api::Activity]) -> Vec<&api::Activity> {
    let mut all: Vec<&api::Activity> = activities.iter().filter(|a| a.is_daily_login()).collect();
    all.sort_by_key(|a| {
        let mut score = 0i32;
        if a.title.contains("每日") {
            score += 100;
        }
        if a.title.contains("签到") {
            score += 50;
        }
        if a.title.contains("登录") {
            score += 20;
        }
        if a.claimable {
            score += 10;
        }
        -score
    });
    let focused: Vec<&api::Activity> = all
        .iter()
        .copied()
        .filter(|a| a.title.contains("每日") || a.title.contains("签到"))
        .collect();
    if focused.is_empty() {
        all
    } else {
        focused
    }
}

/// `POST /v1/ops/claim` 的响应分类（code=0 成功，40001 已领，其它失败）。
fn classify_claim(v: &Value, amount: f64, unit: &str) -> (&'static str, String, f64) {
    let code = v
        .get("code")
        .map(|c| match c {
            Value::Number(n) => n.as_i64().unwrap_or(-1),
            Value::String(s) => s.parse().unwrap_or(-1),
            _ => -1,
        })
        .unwrap_or(-1);
    let msg = v
        .get("message")
        .or_else(|| v.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match code {
        0 => ("success", format!("签到成功 +{amount:.0} {unit}"), amount),
        40001 => (
            "already",
            if msg.is_empty() {
                "今日已领取".to_string()
            } else {
                msg.to_string()
            },
            0.0,
        ),
        _ => (
            "failed",
            format!(
                "领取失败 [{code}]：{}",
                if msg.is_empty() {
                    "上游未给原因"
                } else {
                    msg
                }
            ),
            0.0,
        ),
    }
}

/// 自动签到的一次检查：过了时点、今天没成功过，才真正发请求。
pub fn auto_tick(after_hour: u32) -> Result<Value, AppError> {
    if util::beijing_minute_of_day() < after_hour.min(23) * 60 {
        return Ok(
            json!({ "status": "skipped", "message": format!("未到 {after_hour}:00（北京时间）") }),
        );
    }
    // 不按本地 state 跳过：服务端可能新增可领活动（如 100 分签到 + 1000 分赛事签到），
    // 每次都以服务端 claimable 为准；领取接口本身幂等。
    run_once(false)
}

/// `serve` 里的后台循环。任何一次失败都只打日志，不杀进程。
#[cfg(feature = "server")]
pub fn spawn_auto_loop(after_hour: u32, interval_secs: u64) {
    std::thread::spawn(move || loop {
        match auto_tick(after_hour) {
            Ok(v) => {
                let status = v.get("status").and_then(Value::as_str).unwrap_or("?");
                if status != "skipped" {
                    eprintln!(
                        "[lcodearts auto-checkin] {}",
                        v.get("message").and_then(Value::as_str).unwrap_or("")
                    );
                }
            }
            Err(e) => eprintln!("[lcodearts auto-checkin] 失败: {e}"),
        }
        std::thread::sleep(std::time::Duration::from_secs(interval_secs.max(60)));
    });
}

/// 手动签到命令（T1）。`--loop` 时按 `--interval` 循环，供 systemd / nohup 长跑。
#[derive(App)]
#[app(
    name = "checkin",
    run = "run_checkin",
    safety = "t1",
    about = "Claim the daily CodeArts activity (USER_LOGIN, normally +1000 credits): GET /v1/ops/delivery?channel=IDE, find type=USER_LOGIN, POST /v1/ops/claim with an idempotent key. Only claims campaigns the server marks claimable; invite/student-certification campaigns need real events and are never faked. --dry-run only lists activities. --repeat keeps watching until stopped and skips days already claimed. Safety T1 (spends/claims a remote benefit, not credits)."
)]
pub struct CheckinCmd {
    #[arg(
        about = "Only query activities and report what would be claimed",
        default = false
    )]
    dry_run: bool,
    #[arg(
        about = "Keep running: check every --interval seconds (for systemd/nohup)",
        default = false
    )]
    repeat: bool,
    #[arg(about = "Loop interval in seconds (minimum 60)", default = 300)]
    interval: u64,
    #[arg(
        about = "In loop mode, do not try before this hour (Beijing time)",
        default = 8
    )]
    after_hour: u32,
}

fn run_checkin(app: &CheckinCmd, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    if app.repeat {
        ctx.log(
            LogLevel::Info,
            format!(
                "自动签到循环已启动：每 {} 秒检查一次，北京时间 {}:00 后尝试；Ctrl-C 退出",
                app.interval.max(60),
                app.after_hour
            ),
        );
        loop {
            match auto_tick(app.after_hour) {
                Ok(v) => {
                    let status = v.get("status").and_then(Value::as_str).unwrap_or("?");
                    if status != "skipped" {
                        ctx.log(
                            LogLevel::Info,
                            v.get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        );
                    }
                }
                Err(e) => ctx.log(LogLevel::Warn, format!("本次签到失败: {e}")),
            }
            std::thread::sleep(std::time::Duration::from_secs(app.interval.max(60)));
        }
    }

    let out = run_once(app.dry_run)?;
    if app.dry_run {
        if let Some(acts) = out.get("activities").and_then(Value::as_array) {
            for a in acts {
                ctx.log(
                    LogLevel::Info,
                    format!(
                        "#{} [{}] {} | {} | claimable={} | {} {}",
                        a.get("campaign_id")
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                        a.get("type").and_then(Value::as_str).unwrap_or(""),
                        a.get("title").and_then(Value::as_str).unwrap_or(""),
                        a.get("status").and_then(Value::as_str).unwrap_or(""),
                        a.get("claimable").and_then(Value::as_bool).unwrap_or(false),
                        a.get("amount").map(|v| v.to_string()).unwrap_or_default(),
                        a.get("unit").and_then(Value::as_str).unwrap_or(""),
                    ),
                );
            }
        }
    }
    let message = out.get("message").and_then(Value::as_str).unwrap_or("");
    let status = out.get("status").and_then(Value::as_str).unwrap_or("?");
    if status == "failed" {
        // 手动签到时失败要给出非零退出码，否则 cron/systemd 会把失败当成功。
        ctx.log(LogLevel::Error, message.to_string());
        return Err(AppError::Runtime(message.to_string()));
    }
    ctx.log(LogLevel::Info, message.to_string());
    let mut out = out;
    out["elapsed_ms"] = json!(start.elapsed().as_millis() as u64);
    ctx.done(out.clone(), start.elapsed().as_millis() as u64);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Activity;

    #[test]
    fn daily_pick_prefers_real_checkin_over_signup_gift() {
        let gift = json!({
            "campaignId": 9, "type": "USER_LOGIN", "title": "Skill创新赛报名礼",
            "status": "CLAIMED", "claimable": false, "benefitAmount": 500
        });
        let daily = json!({
            "campaignId": 1, "type": "USER_LOGIN", "title": "每日签到领1000积分",
            "status": "ELIGIBLE", "claimable": true, "benefitAmount": 1000
        });
        let acts = vec![
            Activity::from_value(&gift).unwrap(),
            Activity::from_value(&daily).unwrap(),
        ];
        let picked = pick_daily(&acts);
        assert_eq!(picked.len(), 1, "报名礼不应混进每日签到候选");
        assert_eq!(picked[0].title, "每日签到领1000积分");
        assert!(picked[0].claimable);
    }

    #[test]
    fn claim_codes_classify() {
        let (s, m, c) = classify_claim(&json!({"code": 0}), 1000.0, "积分");
        assert_eq!(s, "success");
        assert!(m.contains("1000"));
        assert_eq!(c, 1000.0);

        let (s, _, c) = classify_claim(
            &json!({"code": 40001, "message": "今日已领取"}),
            1000.0,
            "积分",
        );
        assert_eq!(s, "already");
        assert_eq!(c, 0.0);

        let (s, m, _) = classify_claim(
            &json!({"code": 4006, "message": "名额已领完"}),
            1000.0,
            "积分",
        );
        assert_eq!(s, "failed");
        assert!(m.contains("4006"));
    }

    #[test]
    fn state_round_trip_and_history_limit() {
        let dir = std::env::temp_dir().join(format!("lcodearts-checkin-{}", util::uuid_v4()));
        let path = dir.join("checkin.json");
        std::env::set_var("LCODEARTS_CHECKIN_STATE", &path);
        let mut state = CheckinState::default();
        for i in 0..(HISTORY_LIMIT + 5) {
            record(&mut state, "success", &format!("run {i}"), 1000.0);
        }
        let loaded = load_state();
        assert_eq!(loaded.history.len(), HISTORY_LIMIT);
        assert_eq!(loaded.last_status, "success");
        std::env::remove_var("LCODEARTS_CHECKIN_STATE");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn already_today_requires_success_or_already() {
        let mut s = CheckinState {
            last_date: util::beijing_day(),
            last_status: "success".into(),
            ..Default::default()
        };
        assert!(already_today(&s));
        s.last_status = "failed".into();
        assert!(!already_today(&s));
        s.last_date = "1999-01-01".into();
        s.last_status = "success".into();
        assert!(!already_today(&s));
    }
}
