//! `lcodearts` —— 华为云码道（CodeArts Agent）账号后端。
//!
//! 把 CodeArts 的模型、积分与每日签到做成一套本地工具：
//!
//! ```bash
//! lcodearts login              # 浏览器授权一次，换临时 AK/SK（0600 落盘）
//! lcodearts auth               # 看看凭据是谁、什么时候过期
//! lcodearts quota              # 积分套餐与余量
//! lcodearts models             # 可用模型与积分倍率
//! lcodearts checkin            # 领今天的积分（--dry-run 先看活动）
//! lcodearts checkin --repeat   # 常驻，自动签到（也可交给 serve）
//! lcodearts ask --prompt "hi"  # 直接问模型（花积分）
//! lcodearts serve              # OpenAI 兼容端 + 后台自动签到
//! lcodearts --tui / --gui / --mcp / --schema
//! ```
//!
//! 凭据优先级：`LCODEARTS_AK/SK`（或 `CODEARTS_CLI_AK/SK`）→
//! `~/.local/share/lcodearts/auth.json`。签到/聊天等写操作都在 T1，
//! 只读命令（auth / quota / models）是 T0。

mod api;
mod ask;
mod checkin;
mod commands;
mod cred;
mod login;
#[cfg(feature = "server")]
mod serve;
mod sign;
mod util;

use lilyco::prelude::*;
use std::sync::Arc;

pub fn build_registry_with_policy(policy: Arc<dyn SafetyPolicy>) -> Registry {
    let mut reg = Registry::new().with_policy(policy);
    #[allow(unused_mut)]
    let mut cmds: Vec<RegisteredCommand> = vec![
        RegisteredCommand::from_app::<commands::Auth>(),
        RegisteredCommand::from_app::<commands::Quota>(),
        RegisteredCommand::from_app::<commands::Models>(),
        RegisteredCommand::from_app::<commands::Login>(),
        RegisteredCommand::from_app::<commands::AuthImport>(),
        RegisteredCommand::from_app::<checkin::CheckinCmd>(),
        RegisteredCommand::from_app::<ask::Ask>(),
    ];
    #[cfg(feature = "server")]
    cmds.push(RegisteredCommand::from_app::<serve::Serve>());

    for c in cmds {
        let name = c.schema.name.clone();
        reg.register(c)
            .unwrap_or_else(|e| panic!("注册命令 `{name}` 失败: {e}"));
    }
    reg
}

pub fn policy_for(backend: lilyco::Backend) -> Arc<dyn SafetyPolicy> {
    match backend {
        // MCP 这类自动化面只放行只读命令；签到、登录、聊天都会改远端/本地状态
        lilyco::Backend::Mcp => Arc::new(DenyElevated),
        #[allow(unreachable_patterns)]
        _ => Arc::new(Interactive),
    }
}

fn main() {
    let backend = lilyco::detect_registry_backend();
    let reg = build_registry_with_policy(policy_for(backend));
    lilyco::run_registry_with("lcodearts", reg, backend);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        let mut v: Vec<String> = build_registry_with_policy(Arc::new(Unrestricted))
            .visible()
            .map(|c| c.schema.name.clone())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn registry_contains_all_commands() {
        let mut expected = vec![
            "ask".to_string(),
            "auth".to_string(),
            "auth-import".to_string(),
            "checkin".to_string(),
            "login".to_string(),
            "models".to_string(),
            "quota".to_string(),
        ];
        if cfg!(feature = "server") {
            expected.push("serve".to_string());
        }
        expected.sort();
        assert_eq!(names(), expected);
    }

    #[test]
    fn read_commands_are_t0_and_write_commands_are_t1() {
        let reg = build_registry_with_policy(Arc::new(Unrestricted));
        for name in ["auth", "quota", "models"] {
            let c = reg.get(name).unwrap_or_else(|| panic!("缺少 `{name}`"));
            assert_eq!(c.schema.safety, SafetyTier::ReadOnly, "{name}");
        }
        for name in ["login", "auth-import", "checkin", "ask", "serve"] {
            if let Some(c) = reg.get(name) {
                assert_eq!(c.schema.safety, SafetyTier::Confirm, "{name}");
            }
        }
    }
}
