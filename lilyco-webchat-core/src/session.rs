//! 多轮会话管理与 OpenAI 兼容层通用逻辑：
//! 会话锚点绑定、增量续聊、超长折叠、token 估算、生图意图识别、累计转增量 `Diff`。

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use serde_json::{json, Value};

/// 默认单条新开/滚动会话的字符上限
pub const DEFAULT_MAX_CHAT_CHARS: usize = 120_000;
/// 默认最多缓存多少条客户端会话绑定
pub const DEFAULT_MAX_BOUND_SESSIONS: usize = 64;

/// 一条已绑定的网页会话
#[derive(Clone, Debug)]
pub struct BoundSession {
    pub conversation_id: String,
    pub sent_turns: usize,
    pub branch_len: usize,
}

/// 线程安全的会话绑定表（`锚点 hash → BoundSession`）
pub struct SessionStore {
    max_entries: usize,
    map: Mutex<HashMap<u64, BoundSession>>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BOUND_SESSIONS)
    }
}

impl SessionStore {
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: u64) -> Option<BoundSession> {
        self.map.lock().ok()?.get(&key).cloned()
    }

    pub fn insert(&self, key: u64, bound: BoundSession) {
        if let Ok(mut m) = self.map.lock() {
            if m.len() >= self.max_entries && !m.contains_key(&key) {
                m.clear();
            }
            m.insert(key, bound);
        }
    }

    pub fn remove(&self, key: u64) {
        if let Ok(mut m) = self.map.lock() {
            m.remove(&key);
        }
    }
}

/// 累计全文 → 新增量（UTF-8 字符边界安全）
#[derive(Default)]
pub struct Diff {
    sent: usize,
}

impl Diff {
    pub fn next(&mut self, text: &str) -> Option<String> {
        if text.len() <= self.sent {
            return None;
        }
        let mut from = self.sent;
        while from < text.len() && !text.is_char_boundary(from) {
            from += 1;
        }
        let delta = text[from..].to_string();
        self.sent = text.len();
        if delta.is_empty() {
            None
        } else {
            Some(delta)
        }
    }
}

/// OpenAI 的 `content` 可能是纯字符串，也可能是 `[{type:"text",text:"..."}]` 分段数组
pub fn content_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|p| match p.get("text").and_then(Value::as_str) {
                Some(t) => t.to_string(),
                None => p.as_str().unwrap_or("").to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub fn message_text(m: &Value) -> String {
    let mut text = content_text(m.get("content"));
    if text.trim().is_empty() {
        if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
            let parts = calls
                .iter()
                .map(|c| {
                    let f = c.get("function");
                    let name = f.and_then(|x| x.get("name")).and_then(Value::as_str).unwrap_or("tool");
                    let args = f.and_then(|x| x.get("arguments")).and_then(Value::as_str).unwrap_or("{}");
                    format!("{name}({args})")
                })
                .collect::<Vec<_>>();
            if !parts.is_empty() { text = format!("[tool_calls] {}", parts.join(", ")); }
        }
    }
    text
}

/// 拆分 `system/developer` 与非 system 轮次
pub fn split_chat_messages(messages: &[Value]) -> (Vec<String>, Vec<(&'static str, String)>) {
    let mut sys: Vec<String> = Vec::new();
    let mut turns: Vec<(&'static str, String)> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let text = message_text(m);
        if text.trim().is_empty() {
            continue;
        }
        match role {
            "system" | "developer" => sys.push(text),
            "assistant" => turns.push(("Assistant", text)),
            "tool" | "function" => turns.push(("Tool", text)),
            _ => turns.push(("User", text)),
        }
    }
    (sys, turns)
}

/// 把 OpenAI 的 `messages[]` 压成一段带角色标签的转录文本
pub fn transcript(messages: &[Value]) -> String {
    let (sys, turns) = split_chat_messages(messages);
    let mut blocks: Vec<String> = Vec::new();
    if !sys.is_empty() {
        blocks.push(sys.join("\n\n"));
    }
    if blocks.is_empty() && turns.len() == 1 && turns[0].0 == "User" {
        return turns[0].1.clone();
    }
    for (role, text) in turns {
        blocks.push(format!("{role}: {text}"));
    }
    blocks.join("\n\n")
}

/// 会话锚点：取 system 前 200 字符（不含尾部动态时间戳）+ 第一条 User 消息
pub fn session_anchor(sys: &[String], turns: &[(&'static str, String)]) -> Option<u64> {
    let first_user = turns.iter().find(|(r, _)| *r == "User").map(|(_, t)| t)?;
    let mut h = DefaultHasher::new();
    if let Some(s) = sys.first() {
        let head: String = s.chars().take(200).collect();
        head.hash(&mut h);
    }
    first_user.hash(&mut h);
    Some(h.finish())
}

/// 把增量轮次压成发往已有会话的一段文本
pub fn format_delta_turns(turns: &[(&'static str, String)]) -> String {
    if turns.len() == 1 && turns[0].0 == "User" {
        return turns[0].1.clone();
    }
    turns
        .iter()
        .map(|(r, t)| format!("{r}: {t}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 超长兜底：超过 `max_chars` 时保留头部与尾部，折叠中间过老历史
pub fn cap_transcript(prompt: &str, max_chars: usize) -> String {
    let total = prompt.chars().count();
    if total <= max_chars {
        return prompt.to_string();
    }
    let head_keep = (max_chars * 3 / 10).min(36_000);
    let tail_keep = max_chars.saturating_sub(head_keep + 200);
    let head: String = prompt.chars().take(head_keep).collect();
    let tail: String = prompt
        .chars()
        .skip(total.saturating_sub(tail_keep))
        .collect();
    let omitted = total.saturating_sub(head_keep + tail_keep);
    format!(
        "{head}\n\n[... 已折叠中间约 {omitted} 字符的较早历史，以防超出网页上下文上限 ...]\n\n{tail}"
    )
}

/// 按字符数估算 token（CJK ≈ 1.5 token/字，ASCII ≈ 0.25 token/字）
pub fn estimate_tokens(text: &str) -> u64 {
    if text.trim().is_empty() {
        return 0;
    }
    let mut cjk = 0u64;
    let mut other = 0u64;
    for c in text.chars() {
        if ('\u{4E00}'..='\u{9FFF}').contains(&c)
            || ('\u{3400}'..='\u{4DBF}').contains(&c)
            || ('\u{3000}'..='\u{303F}').contains(&c)
            || ('\u{FF00}'..='\u{FFEF}').contains(&c)
        {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    ((cjk * 3).div_ceil(2) + other.div_ceil(4)).max(1)
}

pub fn estimated_usage(prompt: &str, completion: &str) -> Value {
    let p = estimate_tokens(prompt);
    let c = estimate_tokens(completion);
    json!({
        "prompt_tokens": p,
        "completion_tokens": c,
        "total_tokens": p + c,
    })
}

/// 判断用户消息是否属于「直接请求生图」（同时排除排错、修 bug、标题生成）
pub fn is_image_gen_request(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 800 {
        return false;
    }
    let lower = trimmed.to_lowercase();
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if lower.starts_with("create a concise title")
        || lower.starts_with("/goal")
        || compact.contains("无法生成图片")
        || compact.contains("生成图片失败")
        || compact.contains("修复")
        || compact.contains("报错")
        || compact.contains("为什么")
        || compact.contains("代码")
        || lower.contains("bug")
    {
        return false;
    }
    const ZH_PREFIXES: &[&str] = &[
        "画一", "画个", "画幅", "帮我画", "请画", "给我画", "画图", "作图", "生图", "生成一",
        "生成图", "帮我生成", "请生成", "给我生成", "来一张", "来一只",
    ];
    if ZH_PREFIXES.iter().any(|p| compact.starts_with(p)) {
        return true;
    }
    const ZH_KEYWORDS: &[&str] = &[
        "画一张", "画一只", "画一个", "画一幅", "生成图片", "生成一张", "生成一只", "生成照片",
        "生成插画", "生成头像", "生成壁纸", "生成海报",
    ];
    if ZH_KEYWORDS.iter().any(|k| compact.contains(k)) {
        return true;
    }
    const EN_PREFIXES: &[&str] = &[
        "draw ",
        "paint ",
        "illustrate ",
        "generate an image",
        "generate a picture",
        "generate a photo",
        "create an image",
        "create a picture",
        "make an image",
    ];
    EN_PREFIXES.iter().any(|p| lower.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_handles_multibyte_utf8() {
        let mut d = Diff::default();
        assert_eq!(d.next("你好"), Some("你好".into()));
        assert_eq!(d.next("你好世界"), Some("世界".into()));
        assert_eq!(d.next("你好世界"), None);
    }

    #[test]
    fn anchor_is_stable_across_dynamic_tail() {
        let pfx = "Agent system identity. ".repeat(15);
        let (s1, t1) = split_chat_messages(&[
            json!({"role": "system", "content": format!("{pfx} 10:00")}),
            json!({"role": "user", "content": "hello"}),
        ]);
        let (s2, t2) = split_chat_messages(&[
            json!({"role": "system", "content": format!("{pfx} 10:05")}),
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
            json!({"role": "user", "content": "next"}),
        ]);
        assert_eq!(session_anchor(&s1, &t1), session_anchor(&s2, &t2));
        assert_eq!(format_delta_turns(&t2[2..]), "next");
    }

    #[test]
    fn image_gen_detector_works() {
        assert!(is_image_gen_request("画一只戴墨镜的橘猫"));
        assert!(is_image_gen_request("生成 一张 小狗图片"));
        assert!(!is_image_gen_request("修复无法生成图片的问题"));
    }
}
