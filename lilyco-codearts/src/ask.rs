//! `lcodearts ask` —— 直接向上游模型提一个问题（T1，消耗积分）。
//!
//! 上游统一走流式（`glm-5.3` 只支持 stream=true），这里逐行读 SSE、把增量打印出来，
//! 最后返回完整文本。非流式的调用方请用 `serve` 的 OpenAI 兼容端点。

use lilyco::prelude::*;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::time::Instant;

use crate::{api, cred};

#[derive(App)]
#[app(
    name = "ask",
    run = "run_ask",
    safety = "t1",
    about = "Send one prompt to a CodeArts model and stream the answer back. The request goes to POST /api/v2/chat/completions with stream=true (some models such as glm-5.3 only support streaming) and spends credits according to the model's ratio. --model empty picks the first enabled built-in model. Safety T1: consumes credits."
)]
pub struct Ask {
    #[arg(about = "Prompt text", default = "")]
    prompt: String,
    #[arg(
        about = "Model id, e.g. glm-5.3 / deepseek-v4.1-flash (empty means first enabled model)",
        default = ""
    )]
    model: String,
    #[arg(
        about = "Max output tokens (0 means let the upstream decide)",
        default = 0
    )]
    max_tokens: u32,
}

pub fn run_ask(app: &Ask, ctx: &Context) -> Result<Value, AppError> {
    let start = Instant::now();
    if app.prompt.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "prompt 不能为空：lcodearts ask --prompt \"你好\"".to_string(),
        ));
    }
    let c = cred::load()?;
    if c.expired() {
        return Err(AppError::InvalidInput(
            "本地记录的 CodeArts 凭据已过期：运行 `lcodearts login` 重新授权".to_string(),
        ));
    }
    let api = api::Api::new(c);
    let model = if app.model.trim().is_empty() {
        first_enabled_model(&api)?
    } else {
        app.model.trim().to_string()
    };

    let mut body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": app.prompt }],
        "stream": true,
    });
    if app.max_tokens > 0 {
        body["max_tokens"] = json!(app.max_tokens);
    }

    let res = api.chat_response(&body)?;
    let mut reader = BufReader::new(res.into_body().into_reader());
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut chunks = 0u32;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| AppError::Runtime(format!("读 SSE 失败: {e}")))?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(':') {
            continue;
        }
        let Some(payload) = trimmed.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if let Some(delta) = v
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("delta"))
        {
            if let Some(s) = delta.get("content").and_then(Value::as_str) {
                if !s.is_empty() {
                    text.push_str(s);
                    ctx.log(LogLevel::Info, s.to_string());
                }
            }
            if let Some(s) = delta.get("reasoning_content").and_then(Value::as_str) {
                reasoning.push_str(s);
            }
        }
        chunks += 1;
    }

    if text.is_empty() {
        return Err(AppError::Runtime(
            "上游没有返回任何正文（可能是模型拒答或积分不足）".to_string(),
        ));
    }
    let out = json!({
        "ok": true,
        "model": model,
        "text": text,
        "reasoning": reasoning,
        "chunks": chunks,
        "elapsed_ms": start.elapsed().as_millis() as u64,
    });
    ctx.done(out.clone(), start.elapsed().as_millis() as u64);
    Ok(out)
}

fn first_enabled_model(api: &api::Api) -> Result<String, AppError> {
    let models = api.models()?;
    models
        .iter()
        .find(|m| m.get("enable").and_then(Value::as_bool).unwrap_or(false))
        .or_else(|| models.first())
        .and_then(|m| m.get("model_id").and_then(Value::as_str))
        .map(str::to_string)
        .ok_or_else(|| AppError::Runtime("账号没有可用模型".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_model_prefers_enabled() {
        let models = vec![
            json!({"model_id": "a", "enable": false}),
            json!({"model_id": "b", "enable": true}),
        ];
        let found = models
            .iter()
            .find(|m| m.get("enable").and_then(Value::as_bool).unwrap_or(false))
            .and_then(|m| m.get("model_id").and_then(Value::as_str));
        assert_eq!(found, Some("b"));
    }
}
