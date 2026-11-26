//! # lilyco-webchat-core
//!
//! 面向所有「网页 AI → Lilyco 四端 (CLI / TUI / Web / MCP) + OpenAI 兼容网关」的通用底座
//! （供 `lilyco-chatgpt`、`lilyco-deepseek` 等域工具复用，不再每个网站重复踩坑）。
//!
//! 模块划分：
//! - [`cdp`]：纯 Rust CDP WebSocket 客户端、端口探测、安全关标签页（绝不误杀最后一个 page）、
//!   坐标点击、按键、长文本 DOM 快速写入与 ProseMirror 空白漂移容忍校验；
//! - [`guard`]：`Semaphore(1)` 串行门闩 + RAII 排队计数、常量时间 Bearer 鉴权、滑动 1 小时限流；
//! - [`session`]：多轮 `messages[]` 转录、会话锚点绑定与增量续聊、超长历史折叠、
//!   CJK/ASCII token 估算、生图意图识别与 UTF-8 安全 `Diff` 增量器；
//! - [`util`]：标准 base64 编码、图片魔数 MIME 嗅探、字符边界安全截断。

pub mod cdp;
pub mod guard;
pub mod session;
pub mod util;
