# lilyco-ai

Lilyco AI & agent domain suite: aic, chatgpt, bitnet, mpkg memory packages & web2cli

本仓库为 [lilyco-42/lilyco](https://github.com/lilyco-42/lilyco) 生态的独立主题仓（Hub-and-Spoke 架构），从合并完 PR #75 / #44 / #43 后的最新 `main` 拆分而来，以便外部开发者低门槛贡献、独立编译与按主题发版。

## 包含模块

- `lilyco-aic`
- `lilyco-bitnet`
- `lilyco-chatgpt`
- `lilyco-codearts`（华为云码道：模型清单 / OpenAI 兼容网关 / 积分查询 / 每日签到）
- `lilyco-deepseek`
- `lilyco-mpkg`
- `lilyco-web2cli`
- `lilyco-webchat-core`（网页 AI 通用底座：CDP 驱动 / 串行队列 / Bearer 鉴权 / 限流 / 增量续聊 / token 估算）

## 快速开始

```bash
cargo check --workspace
cargo test --workspace
```
