# lilyco-webai (`lwebai`)

**豆包 / 千问 / Kimi / ZCode 的纯 Rust 登录与多账号管理。**

不依赖 doubao2api、zcode2api、kimi-proxy，也不依赖任何 Python / Go 运行时。
复用 `lilyco-webchat-core` 的 CDP 客户端（tungstenite），直接驱动真实登录页：

- 自动打开登录页，点击「登录 / 扫码登录」；
- 自动截取二维码到 PNG，每 20 秒刷新截图（站点自身刷新二维码时也会跟上）；
- 登录成功后自动导出 `cookies + localStorage`，写入 `0600` 账号文件；
- 多账号：`--account` 命名，`--fresh` 先清站点旧会话再登录另一个账号；
- `accounts` / `use` / `remove` / `status` 管理账号池。

## What

| 命令 | 作用 | 安全级 |
| --- | --- | --- |
| `lwebai login --provider doubao --account lilyco` | 打开豆包登录页，自动截图/刷新二维码，登录后保存账号 | T1 |
| `lwebai login --provider kimi --account alt --fresh` | 清掉 Kimi 旧会话，登录第二个账号 | T1 |
| `lwebai accounts` | 列出已保存账号（cookies 数、localStorage 键、保存时间） | T0 |
| `lwebai use --provider doubao --account lilyco` | 设置默认账号 | T1 |
| `lwebai remove --provider doubao --account old` | 删除账号 | T1 |
| `lwebai status` | CDP 可达性与各 provider 账号数 | T0 |

## Why

之前的方案把 doubao2api / zcode2api / kimi-proxy 当后端，等于把登录和协议交给
Python/Go 第三方服务，二维码也不会自动刷新。这里改成和 `ldeepseek` / `lchatgpt`
完全相同的路线：

- **Rust + CDP**：`lilyco-webchat-core::cdp` 是纯 Rust WebSocket 客户端；
- **真实登录页**：用浏览器打开站点自己的登录页/桌面客户端页面，不逆向私有 API；
- **自动二维码**：`Page.captureScreenshot` 截当前视口，定时覆盖 PNG，图片查看器可随时扫最新二维码；
- **多账号**：登录成功才导出 cookies + localStorage；每个账号一份 0600 文件；
- **桌面版**：`--cdp` 指向桌面 Electron 客户端的 remote-debugging 端口，
  `--login-url` 指向客户端页面即可；不伪造设备/额度。

## How

```bash
# 需要先有一个 CDP Chromium（ego-browser 默认 127.0.0.1:9229）
lwebai status

# 豆包：自动打开登录页、截二维码；扫码后自动保存
lwebai login --provider doubao --account lilyco

# 追加第二个账号：清掉该站点旧会话再登录
lwebai login --provider doubao --account work --fresh

# 千问 / Kimi / ZCode
lwebai login --provider qianwen --account qianwen-main
lwebai login --provider kimi --account kimi-main
lwebai login --provider zcode --account zcode-main

# 管理
lwebai accounts
lwebai use --provider doubao --account lilyco
lwebai status
```

账号仓库：

```text
~/.local/share/lwebai/
  accounts/doubao/lilyco.json     # 0600，cookies + localStorage
  qr/doubao-lilyco.png            # 每次刷新覆盖的登录截图
  current.json                    # 默认 provider/account
```

`LWEBAI_HOME` 可覆盖仓库位置；`LWEBAI_CDP` 或 `--cdp` 可指定 CDP。

## 已验证

2026-11-26 真机（ego-browser / CDP 9229）：

- `lwebai login --provider doubao --account lilyco --fresh`
  - 清掉 32 条旧 cookies；
  - 打开豆包登录页并截图二维码（`zbarimg` 检测到 QR）；
  - 18 秒内检测到登录，保存 28 条 cookies + 28 个 localStorage 键；
  - 页面变为 `?from_login=1`，登录按钮消失，确认是真登录；
- `accounts` / `use` / `status` 均验证通过；
- 账号文件权限 `0600`，默认账号写入 `current.json`。

## 下一步

- 千问 / Kimi / ZCode 的登录页选择器逐家真机校验（当前共用「登录 → 扫码登录」通用脚本 + provider 域名/标记）；
- `ask` / `serve`：复用账号仓库与 CDP，按 provider 发送消息/收回复，再聚合 OpenAI 兼容端；
- ZCode 额度/每日领取属于桌面客户端侧，后续用客户端 CDP 页面实现，不伪造客户端。

## 许可

MIT OR Apache-2.0