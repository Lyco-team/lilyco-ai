//! CodeArts 上游 API：签名请求、模型清单、积分统计、运营活动、聊天 SSE。
//!
//! 所有业务接口都打 `snap-access.cn-north-4.myhuaweicloud.com`；主域名连不上时
//! 回退 `.cn` 备用域名。错误翻译照抄 CodeArts2API 的对照表：
//! `APIG.0301/0303` = 签名/凭据失效，`APIG.0602` / `STS5.1804/1806` = STS 过期，
//! `TM.00001041` = 并发会话超限。

use lilyco::prelude::AppError;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

use crate::cred::Credential;
use crate::{sign, util};

pub const SNAP_HOST: &str = "https://snap-access.cn-north-4.myhuaweicloud.com";
pub const SNAP_HOST_CN: &str = "https://snap-access.cn-north-4.myhuaweicloud.cn";
pub const CHAT_PATH: &str = "/api/v2/chat/completions";
pub const MODELS_PATH: &str = "/v1/model/builtin";
pub const STATS_PATH: &str = "/snap-manager/v1/statistics/plugin";
pub const OPS_DELIVERY_PATH: &str = "/v1/ops/delivery";
pub const OPS_CLAIM_PATH: &str = "/v1/ops/claim";

const PLUGIN_NAME: &str = "codemate_vscode";
const PLUGIN_VERSION: &str = "26.9.902";

pub struct Api {
    cred: Credential,
    agent: ureq::Agent,
    /// 上游按 `x-ot-session-id` 统计并发会话（每账号上限 3 个），必须按凭据复用
    session_id: String,
}

impl Api {
    pub fn new(cred: Credential) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .build()
            .new_agent();
        let seed = format!(
            "{}|{}",
            cred.domain_id.as_deref().unwrap_or(""),
            cred.access_key
        );
        let digest = sign::sha256_hex(seed.as_bytes());
        let session_id = format!("ses_{}", &digest[..24]);
        Self {
            cred,
            agent,
            session_id,
        }
    }

    fn base_headers(&self, model: Option<&str>, prompt_center: bool) -> BTreeMap<String, String> {
        let trace_id = util::random_hex(16);
        let span_id = util::random_hex(8);
        let mut h = BTreeMap::new();
        h.insert("content-type".into(), "application/json".into());
        h.insert("app-id".into(), "CodeAgent3.0".into());
        h.insert("x-ot-function".into(), "agent-general".into());
        h.insert("x-ot-trace-id".into(), trace_id.clone());
        h.insert("x-ot-span-id".into(), span_id);
        h.insert(
            "x-snap-traceid".into(),
            format!("{trace_id}_{}", util::random_hex(8)),
        );
        h.insert("x-ot-client-type".into(), "IDE".into());
        h.insert("x-ot-client-version".into(), PLUGIN_VERSION.into());
        h.insert("plugin-name".into(), PLUGIN_NAME.into());
        h.insert("plugin-version".into(), PLUGIN_VERSION.into());
        h.insert("ide-name".into(), "VSCode".into());
        h.insert("ide-version".into(), PLUGIN_VERSION.into());
        h.insert(
            "created-time".into(),
            util::iso8601_from_epoch_secs(util::now_secs()),
        );
        h.insert(
            "user-msg-id".into(),
            format!("msg_{}", util::random_hex(12)),
        );
        h.insert("x-ot-session-id".into(), self.session_id.clone());
        h.insert("x-ot-parent-session-id".into(), String::new());
        h.insert("user-session-id".into(), self.session_id.clone());
        h.insert("is_confidential".into(), "false".into());
        h.insert("X-Language".into(), "zh-cn".into());
        if prompt_center {
            h.insert("Agent-Type".into(), "PromptCenter".into());
        }
        if let Some(m) = model {
            // 上游会校验 model-id 与请求体里的 model 完全一致（不一致报 406）
            h.insert("model-id".into(), m.to_string());
            h.insert("model-name".into(), m.to_string());
        }
        h
    }

    /// 发一次签名请求，返回 (状态码, 响应体)。
    fn send(
        &self,
        method: &str,
        host: &str,
        path: &str,
        body: Option<&[u8]>,
        model: Option<&str>,
        prompt_center: bool,
    ) -> Result<(u16, String), AppError> {
        let headers = self.base_headers(model, prompt_center);
        let mut last_err = None;
        for candidate in host_candidates(host) {
            let full = format!("{candidate}{path}");
            let signed = sign::sign_request(
                &self.cred.access_key,
                &self.cred.secret_key,
                self.cred.security_token.as_deref(),
                method,
                &full,
                body.unwrap_or(&[]),
                &headers,
            )
            .map_err(AppError::Runtime)?;

            let result = match method {
                "GET" => {
                    let mut rb = self.agent.get(&full);
                    for (k, v) in &signed {
                        if k != "host" {
                            rb = rb.header(k.as_str(), v.as_str());
                        }
                    }
                    rb.call()
                }
                "POST" => {
                    let mut rb = self.agent.post(&full);
                    for (k, v) in &signed {
                        if k != "host" {
                            rb = rb.header(k.as_str(), v.as_str());
                        }
                    }
                    rb.send(body.unwrap_or(&[]))
                }
                other => {
                    return Err(AppError::InvalidInput(format!(
                        "不支持的 HTTP 方法: {other}"
                    )))
                }
            };

            match result {
                Ok(res) => {
                    let status = res.status().as_u16();
                    let text = res
                        .into_body()
                        .read_to_string()
                        .map_err(|e| AppError::Runtime(format!("读响应失败: {e}")))?;
                    return Ok((status, text));
                }
                Err(e) => {
                    last_err = Some(format!("{candidate}: {e}"));
                }
            }
        }
        Err(AppError::Runtime(format!(
            "连不上 CodeArts 上游：{}",
            last_err.unwrap_or_else(|| "未知网络错误".into())
        )))
    }

    fn send_json(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        model: Option<&str>,
        prompt_center: bool,
    ) -> Result<Value, AppError> {
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|e| AppError::Runtime(format!("序列化请求体失败: {e}")))?;
        let (status, text) = self.send(
            method,
            SNAP_HOST,
            path,
            bytes.as_deref(),
            model,
            prompt_center,
        )?;
        if !(200..300).contains(&status) {
            return Err(upstream_error(status, &text));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| {
            AppError::Runtime(format!(
                "上游返回的不是 JSON（HTTP {status}）: {e}；原文：{}",
                util::clip(&text, 200)
            ))
        })?;
        if let Some(code) = v.get("error_code").and_then(Value::as_str) {
            if !code.is_empty() && code != "0000" {
                return Err(translate_code(code, &text));
            }
        }
        Ok(v)
    }

    /// `GET /v1/model/builtin`
    pub fn models(&self) -> Result<Vec<Value>, AppError> {
        let v = self.send_json("GET", MODELS_PATH, None, None, true)?;
        let list = v
            .get("builtinModels")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| AppError::Runtime("模型清单缺少 builtinModels 数组".to_string()))?;
        Ok(list)
    }

    /// `GET /snap-manager/v1/statistics/plugin`
    pub fn credits(&self) -> Result<Value, AppError> {
        self.send_json("GET", STATS_PATH, None, None, false)
    }

    /// `GET /v1/ops/delivery?channel=IDE`
    pub fn activities(&self) -> Result<Vec<Activity>, AppError> {
        let v = self.send_json(
            "GET",
            &format!("{OPS_DELIVERY_PATH}?channel=IDE"),
            None,
            None,
            true,
        )?;
        let items = v
            .get("data")
            .and_then(|d| d.get("items"))
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| AppError::Runtime("活动列表缺少 data.items".to_string()))?;
        Ok(items.iter().filter_map(Activity::from_value).collect())
    }

    /// `POST /v1/ops/claim`
    pub fn claim(&self, campaign_id: &Value) -> Result<Value, AppError> {
        let body = json!({
            "campaignId": campaign_id,
            "channel": "IDE",
            "idempotentKey": util::uuid_v4(),
        });
        self.send_json("POST", OPS_CLAIM_PATH, Some(&body), None, true)
    }

    /// 发起聊天请求。调用方拿到的响应体是要自己逐行读的 SSE。
    pub fn chat_response(
        &self,
        body: &Value,
    ) -> Result<ureq::http::Response<ureq::Body>, AppError> {
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::InvalidInput("请求体缺少 model".to_string()))?;
        let headers = self.base_headers(Some(model), false);
        let payload = serde_json::to_vec(body)
            .map_err(|e| AppError::Runtime(format!("序列化请求体失败: {e}")))?;
        // 聊天单独用流式 agent：60 秒的全局超时会在长回答中途把 SSE 掐断；
        // 这里只限制建连与单次读，不用整体预算（照 lchatgpt）。
        let stream_agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_recv_body(Some(Duration::from_secs(180)))
            .http_status_as_error(false)
            .build()
            .new_agent();
        let mut last_err = None;
        for candidate in host_candidates(SNAP_HOST) {
            let full = format!("{candidate}{CHAT_PATH}");
            let signed = sign::sign_request(
                &self.cred.access_key,
                &self.cred.secret_key,
                self.cred.security_token.as_deref(),
                "POST",
                &full,
                &payload,
                &headers,
            )
            .map_err(AppError::Runtime)?;
            let mut rb = stream_agent.post(&full);
            for (k, v) in &signed {
                if k != "host" {
                    rb = rb.header(k.as_str(), v.as_str());
                }
            }
            match rb.send(&payload) {
                Ok(res) => {
                    if (200..300).contains(&res.status().as_u16()) {
                        return Ok(res);
                    }
                    let status = res.status().as_u16();
                    let text = res
                        .into_body()
                        .read_to_string()
                        .map_err(|e| AppError::Runtime(format!("读响应失败: {e}")))?;
                    return Err(upstream_error(status, &text));
                }
                Err(e) => last_err = Some(format!("{candidate}: {e}")),
            }
        }
        Err(AppError::Runtime(format!(
            "连不上 CodeArts 聊天上游：{}",
            last_err.unwrap_or_else(|| "未知网络错误".into())
        )))
    }
}

/// 运营活动。只保留每日签到需要的字段；原始 JSON 保留给 claim 用。
#[derive(Debug, Clone)]
pub struct Activity {
    pub campaign_id: Value,
    pub kind: String,
    pub title: String,
    pub amount: f64,
    pub unit: String,
    pub status: String,
    pub claimable: bool,
    pub trigger_mode: String,
}

impl Activity {
    pub fn from_value(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        Some(Self {
            campaign_id: obj.get("campaignId").cloned().unwrap_or(Value::Null),
            kind: obj
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            title: obj
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            amount: obj
                .get("benefitAmount")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            unit: obj
                .get("benefitUnit")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            status: obj
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            claimable: obj
                .get("claimable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            trigger_mode: obj
                .get("extra")
                .and_then(|e| e.get("triggerMode"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    }

    /// 每日登录活动（服务端用 `USER_LOGIN` 标识；标题兜底认「签到」）
    pub fn is_daily_login(&self) -> bool {
        self.kind == "USER_LOGIN" || self.title.contains("签到")
    }

    pub fn already_claimed(&self) -> bool {
        !self.claimable && matches!(self.status.as_str(), "CLAIMED" | "CONFIRMED" | "CONSUMED")
    }

    pub fn unit_label(&self) -> &str {
        if self.unit == "CREDIT" {
            "积分"
        } else {
            self.unit.as_str().trim()
        }
    }
}

/// 积分快照：把 `statistics/plugin` 的 metrics 摊平成界面好用的字段。
#[derive(Debug, Clone, Default)]
pub struct CreditSnapshot {
    pub package_name: String,
    pub status: String,
    pub is_credit_package: bool,
    pub is_token_package: bool,
    pub total: Option<f64>,
    pub used: Option<f64>,
    pub remain: Option<f64>,
    pub expiring: Option<f64>,
    pub percent: Option<f64>,
    pub end_date: String,
}

impl CreditSnapshot {
    pub fn from_value(v: &Value) -> Self {
        // 有的上游把业务字段包在 `data` 里（signedRequest 会 unwrap），有的直接平铺；
        // 两种形状都认，避免「明明有积分却显示没有套餐」。
        let root = v.get("data").filter(|d| d.is_object()).unwrap_or(v);
        let mut s = CreditSnapshot::default();
        if let Some(pkg) = root.get("package") {
            s.package_name = pkg
                .get("package_name_cn")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            s.status = pkg
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            s.is_credit_package = truthy(pkg.get("is_credit_package"));
            s.is_token_package = truthy(pkg.get("is_token_package"));
            s.end_date = pkg
                .get("end_date")
                .or_else(|| root.get("end_date"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        if let Some(metrics) = root.get("metrics").and_then(Value::as_array) {
            let pick = metrics
                .iter()
                .find(|m| m.get("name").and_then(Value::as_str) == Some("usageTotalPackageCredit"));
            let m = pick.unwrap_or_else(|| {
                metrics
                    .iter()
                    .find(|m| m.get("package_credit_remain").is_some())
                    .unwrap_or(&Value::Null)
            });
            s.total = num(m.get("package_credit_amount"));
            s.used = num(m.get("package_credit_used"));
            s.remain = num(m.get("package_credit_remain"));
            s.expiring = num(m.get("package_credit_expiring_amount"));
            s.percent = num(m.get("value"));
        }
        s
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn host_candidates(host: &str) -> Vec<&'static str> {
    if host.starts_with(SNAP_HOST) {
        vec![SNAP_HOST, SNAP_HOST_CN]
    } else {
        vec![SNAP_HOST]
    }
}

/// HTTP 非 2xx → 尽量从 JSON 里提炼 `error_code/error_msg`，再翻译成中文。
pub fn upstream_error(status: u16, body: &str) -> AppError {
    let v: Option<Value> = serde_json::from_str(body).ok();
    let code = v
        .as_ref()
        .and_then(|x| {
            x.get("error_code")
                .or_else(|| x.get("errorCode"))
                .or_else(|| x.get("code"))
        })
        .map(|c| match c {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    if !code.is_empty() {
        return translate_code(&code, body);
    }
    let msg = v
        .as_ref()
        .and_then(|x| {
            x.get("error_msg")
                .or_else(|| x.get("message"))
                .or_else(|| x.get("msg"))
        })
        .and_then(Value::as_str)
        .unwrap_or("");
    AppError::Runtime(format!(
        "CodeArts 上游 HTTP {status}：{}",
        if msg.is_empty() {
            util::clip(body, 300)
        } else {
            msg.to_string()
        }
    ))
}

/// 业务错误码 → 用户能行动的中文提示。
pub fn translate_code(code: &str, body: &str) -> AppError {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error_msg")
                .or_else(|| v.get("message"))
                .or_else(|| v.get("msg"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| util::clip(body, 200));
    match code {
        c if c.starts_with("APIG.0301") || c.starts_with("APIG.0303") => {
            AppError::InvalidInput(format!(
                "凭据签名校验失败（{c}）：检查 AK/SK 是否匹配，或重新运行 `lcodearts login`。上游：{msg}"
            ))
        }
        c if c.starts_with("APIG.0602") || c.starts_with("STS5.1804") || c.starts_with("STS5.1806") => {
            AppError::InvalidInput(format!(
                "华为云临时凭据（STS）已过期（{c}）：运行 `lcodearts login` 重新授权一次。上游：{msg}"
            ))
        }
        "TM.00001041" => AppError::Runtime(
            "CodeArts 并发会话数已达上限（每账号 3 个），等几秒再试".to_string(),
        ),
        "40001" => AppError::Runtime(format!("今天的权益还没刷新或已领过：{msg}")),
        "4006" => AppError::Runtime(format!("活动名额已领完（4006）：{msg}")),
        "4000" => AppError::Runtime(format!(
            "福利 Token 通道已下线（4000 invalid channel），不影响每日积分活动：{msg}"
        )),
        _ => AppError::Runtime(format!("CodeArts 返回 [{code}]：{msg}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_parsing_and_daily_detection() {
        let v = json!({
            "campaignId": 1,
            "type": "USER_LOGIN",
            "title": "每日签到领1000积分",
            "benefitAmount": 1000,
            "benefitUnit": "CREDIT",
            "status": "ELIGIBLE",
            "claimable": true,
            "extra": { "triggerMode": "MANUAL_CLAIM" }
        });
        let a = Activity::from_value(&v).unwrap();
        assert!(a.is_daily_login());
        assert!(a.claimable);
        assert!(!a.already_claimed());
        assert_eq!(a.unit_label(), "积分");
        assert_eq!(a.amount, 1000.0);
    }

    #[test]
    fn already_claimed_states() {
        for status in ["CLAIMED", "CONFIRMED", "CONSUMED"] {
            let a = Activity::from_value(&json!({
                "campaignId": 1, "type": "USER_LOGIN", "status": status, "claimable": false
            }))
            .unwrap();
            assert!(a.already_claimed(), "{status}");
        }
    }

    #[test]
    fn credit_snapshot_accepts_wrapped_data() {
        let v = json!({
            "code": 0,
            "data": {
                "package": { "package_name_cn": "包", "is_credit_package": true },
                "metrics": [{ "name": "usageTotalPackageCredit", "package_credit_remain": 7 }]
            }
        });
        let s = CreditSnapshot::from_value(&v);
        assert_eq!(s.remain, Some(7.0));
        assert_eq!(s.package_name, "包");
    }

    #[test]
    fn credit_snapshot_picks_total_metric() {
        let v = json!({
            "package": {
                "package_name_cn": "CodeArts Doer 体验包",
                "status": "normal",
                "is_credit_package": "true",
                "is_token_package": "false"
            },
            "metrics": [
                { "name": "usageTokenChatMessages", "value": "0" },
                { "name": "usageTotalPackageCredit", "package_credit_amount": 5000,
                  "package_credit_used": 1200, "package_credit_remain": 3800,
                  "package_credit_expiring_amount": 0, "value": "76" }
            ]
        });
        let s = CreditSnapshot::from_value(&v);
        assert!(s.is_credit_package);
        assert_eq!(s.total, Some(5000.0));
        assert_eq!(s.remain, Some(3800.0));
        assert_eq!(s.percent, Some(76.0));
    }

    #[test]
    fn error_codes_map_to_actionable_messages() {
        let e = translate_code("APIG.0301", r#"{"error_msg":"bad signature"}"#);
        assert!(format!("{e}").contains("login"));
        let e = translate_code("STS5.1804", r#"{"error_msg":"expired"}"#);
        assert!(format!("{e}").contains("过期"));
    }
}
