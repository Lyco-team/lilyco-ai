//! `lwebai` —— 豆包 / 千问 / Kimi / ZCode 的纯 Rust CDP 登录与多账号管理。
//!
//! 不依赖 doubao2api / zcode2api / kimi-proxy / Python / Go：
//! 直接复用 `lilyco-webchat-core`（tungstenite CDP）驱动真实登录页，
//! 自动截取并刷新二维码，登录后保存 cookies + localStorage，支持多账号。
//!
//! ```bash
//! lwebai login --provider doubao --account lilyco
//! lwebai login --provider kimi --account alt --fresh
//! lwebai accounts
//! lwebai use --provider doubao --account lilyco
//! lwebai status
//! ```
//!
//! 后续 `ask` / `serve` 会复用同一套账号仓库和 CDP 驱动，不再引入外部代理。

mod accounts;
mod commands;
mod login;
mod providers;

use lilyco::prelude::*;
use std::sync::Arc;

pub fn build_registry_with_policy(policy: Arc<dyn SafetyPolicy>) -> Registry {
    let mut reg = Registry::new().with_policy(policy);
    let cmds = vec![
        RegisteredCommand::from_app::<commands::Login>(),
        RegisteredCommand::from_app::<commands::Accounts>(),
        RegisteredCommand::from_app::<commands::Use>(),
        RegisteredCommand::from_app::<commands::Remove>(),
        RegisteredCommand::from_app::<commands::Status>(),
    ];
    for c in cmds {
        let name = c.schema.name.clone();
        reg.register(c)
            .unwrap_or_else(|e| panic!("注册命令 `{name}` 失败: {e}"));
    }
    reg
}

pub fn policy_for(backend: lilyco::Backend) -> Arc<dyn SafetyPolicy> {
    match backend {
        lilyco::Backend::Mcp => Arc::new(DenyElevated),
        #[allow(unreachable_patterns)]
        _ => Arc::new(Interactive),
    }
}

fn main() {
    let backend = lilyco::detect_registry_backend();
    let reg = build_registry_with_policy(policy_for(backend));
    lilyco::run_registry_with("lwebai", reg, backend);
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
    fn registry_has_account_commands() {
        assert_eq!(
            names(),
            vec![
                "accounts".to_string(),
                "login".to_string(),
                "remove".to_string(),
                "status".to_string(),
                "use".to_string()
            ]
        );
    }

    #[test]
    fn writes_are_t1_reads_are_t0() {
        let reg = build_registry_with_policy(Arc::new(Unrestricted));
        for name in ["login", "use", "remove"] {
            assert_eq!(reg.get(name).unwrap().schema.safety, SafetyTier::Confirm);
        }
        for name in ["accounts", "status"] {
            assert_eq!(reg.get(name).unwrap().schema.safety, SafetyTier::ReadOnly);
        }
    }
}
