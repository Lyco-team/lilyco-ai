# lilyco-codearts (`lcodearts`)

**把华为云码道（CodeArts Agent）的模型、积分与每日签到接进 Lilyco 生态。**

一个 `#[derive(App)]` 结构体一条命令，所以同一套能力自动具备 **CLI / TUI / Web / MCP** 四个面；
再加一个 `serve` 把账号开成 **OpenAI 兼容**端口，dsh / opencode 这类客户端可以直接把它当模型提供端用。

---

## What：它能做什么

| 命令 | 作用 | 安全级 |
| --- | --- | --- |
| `lcodearts login` | 浏览器授权华为云，换临时 AK/SK/STS 并保存（0600） | T1 |
| `lcodearts auth` | 当前凭据是谁、从哪来、什么时候过期（不打印 SK） | T0 |
| `lcodearts auth-import` | 导入华为云长期 AK/SK（stdin / CSV / JSON，0600） | T1 |
| `lcodearts quota` | 积分套餐：总额 / 已用 / 剩余 / 过期日 | T0 |
| `lcodearts models` | 可用模型清单与积分倍率 | T0 |
| `lcodearts checkin` | 领取今天的运营活动奖励（通常 +1000 积分） | T1 |
| `lcodearts ask --prompt "…"` | 直接问模型，流式打印回答 | T1 |
| `lcodearts serve` | OpenAI 兼容 `/v1` + 后台自动签到 | T1 |

- **签到**：`GET /v1/ops/delivery?channel=IDE` 找到 `type=USER_LOGIN`，再 `POST /v1/ops/claim`
  （带 `idempotentKey`，服务端幂等）。只领服务端明确 `claimable=true` 的活动；
  邀请 / 学生认证必须由真实事件触发，本工具**绝不伪造触发**。
- **自动签到**：`lcodearts checkin --repeat` 常驻，或交给 `lcodearts serve` 的后台线程。
  默认北京时间 8:00 后检查、每 300 秒一次；每次以服务端 `claimable` 为准，已领取的不重复，
  并会领取所有标题含「每日 / 签到」的活动（实测同一账号有 1000 分与 100 分两项）。
- **模型网关**：统一向上游发流式（`glm-5.3` 只支持 streaming），非流式客户端在本地聚合；
  `model-id` / `model-name` 请求头与请求体 `model` 严格一致。

## Why：为什么不直接调 CodeArts

CodeArts 上游不是标准 OpenAI：

- 每个请求都要 **SDK-HMAC-SHA256** 签名，且 canonical URI 要补尾斜杠、CanonicalHeaders
  末尾多一个换行（与官方文档不同，不照抄会一直 `APIG.0301`）；
- 模型/积分/签到分属不同网关路径，身份头、`x-ot-session-id`、STS 头缺一个就不通；
- 临时凭据会过期，模型 `glm-5.3` 还只支持流式。

这些细节全部封装在 `sign.rs` / `api.rs` 里；上层命令只关心「查什么、领什么、问什么」。

## How：快速开始

```bash
# 0) 构建
cargo build -p lilyco-codearts

# 1a) 临时授权（会打开浏览器；无桌面环境用 --no-browser 打印链接）
lcodearts login

# 1b) 或者导入长期 AK/SK（推荐；在华为云 IAM → 我的凭证 → 访问密钥 创建并下载 CSV）
lcodearts auth-import --file ~/Downloads/credentials.csv
# 也可以手工粘贴两行：printf '%s\n%s\n' '<AK>' '<SK>' | lcodearts auth-import

lcodearts auth

# 2) 看看额度和模型
lcodearts quota
lcodearts models

# 3) 手动签到（先 dry-run 确认活动）
lcodearts checkin --dry-run
lcodearts checkin

# 4) 一次提问（花积分）
lcodearts ask --prompt "用一句话解释 borrow checker" --model glm-5.3

# 5) OpenAI 兼容端 + 后台自动签到
lcodearts serve            # 默认 127.0.0.1:8793
```

四端与其它 Lilyco 应用相同：

```bash
lcodearts --tui
lcodearts --gui
lcodearts --mcp
lcodearts --schema
```

### 接进 dsh / opencode

`serve` 启动后，在 provider 列表里加一条（**不要覆盖已有 provider**）：

```jsonc
"provider": {
  "codearts": {
    "npm": "@ai-sdk/openai-compatible",
    "name": "CodeArts (lcodearts)",
    "options": {
      "baseURL": "http://127.0.0.1:8793/v1",
      "apiKey": "可选：LCODEARTS_SERVE_API_KEY"
    },
    "models": {
      "glm-5.3": { "name": "GLM-5.3" },
      "deepseek-v4.1-flash": { "name": "DeepSeek-V4.1-Flash" }
    }
  }
}
```

模型 id 以 `lcodearts models` 的输出为准；积分倍率也由同一接口返回。

### 常驻自动签到（systemd 用户服务）

```bash
cp deploy/lcodearts-checkin.service ~/.config/systemd/user/
# 如需长期 AK/SK，编辑 Environment 行或加 EnvironmentFile
systemctl --user daemon-reload
systemctl --user enable --now lcodearts-checkin.service
journalctl --user -u lcodearts-checkin -f
```

不装 systemd 也可以直接跑：

```bash
lcodearts checkin --repeat --after-hour 8 --interval 300
```

## 凭据

优先级从高到低：

1. 环境变量 `LCODEARTS_AK` / `LCODEARTS_SK`（可选 `LCODEARTS_SECURITY_TOKEN`）；
2. 兼容 CodeArts CLI 的 `CODEARTS_CLI_AK` / `CODEARTS_CLI_SK`；
3. `~/.local/share/lcodearts/auth.json`（`login` 写入，权限 0600）。

`login` 换到的是**临时的** AK/SK/STS，过期后要重新授权；如果希望无人值守长期签到，
在华为云 IAM「我的凭证 → 访问密钥」创建长期 AK/SK，再 `lcodearts auth-import` 导入
（支持 IAM 下载的 CSV、两行文本或 JSON）。长期凭据不受 STS 过期影响，导入后 serve 会
在每次请求时重新读取，**无需重启**。

## 自动签到的规则

- 按**北京时间**算「今天」，避免服务器时区不同导致跨天漏签；
- 北京时间 `--checkin-after-hour`（默认 8）之后才尝试；
- 每次向服务端确认 `claimable`，领取接口本身幂等；标题含「每日 / 签到」的活动会全部领取；
- 只领 `claimable=true` 的活动，报名礼 / 邀请 / 学生认证不会被误领；
- 结果写入 `~/.local/share/lcodearts/checkin.json`（历史最多 60 条），便于排查到账。

## 环境变量

| 变量 | 作用 |
| --- | --- |
| `LCODEARTS_AK` / `LCODEARTS_SK` / `LCODEARTS_SECURITY_TOKEN` | 直接提供凭据 |
| `CODEARTS_CLI_AK` / `CODEARTS_CLI_SK` / `CODEARTS_CLI_SECURITY_TOKEN` | 兼容 CodeArts CLI |
| `LCODEARTS_STORE` | 覆盖凭据文件路径 |
| `LCODEARTS_CHECKIN_STATE` | 覆盖签到状态文件路径 |
| `LCODEARTS_SERVE_API_KEY` | `serve` 的 Bearer key（等价 `--api-key`） |

## 安全

- 凭据文件落盘 0600，`auth` 只显示打码后的 AK，任何命令都不打印 SK；
- `serve` 默认只绑 `127.0.0.1`；如果改成 `0.0.0.0`，请务必设置 `--api-key`；
- `login` / `checkin` / `ask` / `serve` 都是 T1，MCP 自动化面默认拒绝；
- 签到只领 `claimable=true` 的活动，不伪造邀请/学生认证事件。

## 实现参考与验证

- 上游协议：`2951461586/CodeArts2API`（`docs/UPSTREAM.md`、`signer_test.go`）
- 浏览器登录与签到：`AIcivilization/dsh-model`（`codearts-login.js` / `codearts.js`）
- 四端结构：本仓库的 `lilyco-chatgpt`

本地单测 33 项：签名金标准四组向量、canonical URI/query、活动状态与多签到选择、
领取响应分类、SSE 聚合（文本 / 思维链 / tool_calls）、凭据 0600、签到状态、
长期 AK/SK 的两行 / CSV / JSON 导入解析。

真实上游验证（2026-11-26，账号 lilyco42）：

- `GET /v1/models` 200，返回 12 个模型（GLM-5.2 / GLM-5.3 / OpenPangu / DeepSeek-V4.1 等）；
- `quota` 读到体验版 16500 总积分、约 2932 剩余（2026-12-17 重置）；
- `checkin` 领取「Skill创新赛每日签到礼 +1000」与「每日签到领100 +100」，共 +1100；
- 根路径 `/` 说明页 200（不再 404），`/v1/chat/completions` 非流式实测返回标准
  `chat.completion`（glm-5.3-flash，含 `reasoning_content`）；
- 修复了一个真实数据才暴露的问题：USER_LOGIN 下还有「报名礼」，且存在多个每日签到，
  旧逻辑只取第一个会漏领；现在按标题过滤并全部领取。

真实上游验证需要先 `lcodearts login` 授权一次；在拿到凭据前，网络路径未被端到端执行。

## 许可

MIT OR Apache-2.0