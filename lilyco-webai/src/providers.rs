//! 四个网页/桌面 AI 站点的登录参数。
//!
//! 这里刻意不依赖任何 Python/Go 反向服务：登录页、域名、登录 cookie、点击
//! 「登录/扫码」和「刷新二维码」的 JS 全部是站点适配数据，真正执行的是
//! `lilyco-webchat-core::cdp` 的纯 Rust CDP 客户端。

use lilyco::prelude::AppError;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Doubao,
    Qianwen,
    Kimi,
    Zcode,
}

impl Provider {
    pub const ALL: [Provider; 4] = [
        Provider::Doubao,
        Provider::Qianwen,
        Provider::Kimi,
        Provider::Zcode,
    ];

    pub fn parse(raw: &str) -> Result<Self, AppError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "doubao" | "db" | "豆包" => Ok(Provider::Doubao),
            "qianwen" | "qwen" | "tongyi" | "千问" | "通义" => Ok(Provider::Qianwen),
            "kimi" | "moonshot" | "月之暗面" => Ok(Provider::Kimi),
            "zcode" | "z-ai" | "zai" | "智谱" => Ok(Provider::Zcode),
            other => Err(AppError::InvalidInput(format!(
                "不认识的 provider `{other}` —— 支持 doubao / qianwen / kimi / zcode"
            ))),
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            Provider::Doubao => "doubao",
            Provider::Qianwen => "qianwen",
            Provider::Kimi => "kimi",
            Provider::Zcode => "zcode",
        }
    }

    pub fn display(&self) -> &'static str {
        match self {
            Provider::Doubao => "豆包",
            Provider::Qianwen => "千问",
            Provider::Kimi => "Kimi",
            Provider::Zcode => "ZCode",
        }
    }

    pub fn login_url(&self) -> &'static str {
        match self {
            Provider::Doubao => "https://www.doubao.com/chat/",
            Provider::Qianwen => "https://www.qianwen.com/",
            Provider::Kimi => "https://www.kimi.com/",
            // ZCode 的登录入口是 Z.ai 的 OAuth；桌面客户端也能用 --login-url 覆盖
            Provider::Zcode => "https://chat.z.ai/",
        }
    }

    pub fn domain_filter(&self) -> &'static str {
        match self {
            Provider::Doubao => "doubao.com",
            Provider::Qianwen => "qianwen.com",
            Provider::Kimi => "kimi.com",
            Provider::Zcode => "z.ai",
        }
    }

    pub fn origin(&self) -> &'static str {
        match self {
            Provider::Doubao => "https://www.doubao.com",
            Provider::Qianwen => "https://www.qianwen.com",
            Provider::Kimi => "https://www.kimi.com",
            Provider::Zcode => "https://chat.z.ai",
        }
    }

    /// 登录后才会出现的 cookie / localStorage 键（小写包含匹配）。
    /// 这些是「账号已登录」的判断依据；二维码页面本身不会带它们。
    pub fn login_markers(&self) -> &'static [&'static str] {
        match self {
            Provider::Doubao => &["sessionid", "sid_tt", "sid_guard"],
            Provider::Qianwen => &["login_aliyunid_ticket", "login_aliyunid"],
            Provider::Kimi => &["kimi-auth", "access_token", "authorization"],
            Provider::Zcode => &["zcode", "access_token", "access-token"],
        }
    }

    pub fn activate_login_js(&self) -> &'static str {
        r#"(() => {
  const visible = e => e && e.offsetParent !== null;
  const text = e => ((e.innerText || '') + ' ' + (e.getAttribute('aria-label') || '') + ' ' + (e.getAttribute('title') || '')).trim();
  const click = (re) => {
    const hits = Array.from(document.querySelectorAll('button,[role="button"],a,div,span'))
      .filter(visible)
      .filter(e => {
        const t = text(e);
        return t.length > 0 && t.length < 40 && re.test(t);
      });
    if (hits[0]) { hits[0].click(); return true; }
    return false;
  };
  let opened = click(/^(登录|登錄|立即登录|Log in|Sign in)$/i);
  // 豆包/千问/Kimi 的登录弹窗默认可能是手机号登录；
  // 600ms 后再点「打开 App 扫码登录 / 扫码登录 / 扫一扫」切到二维码。
  setTimeout(() => { click(/扫码登录|打开.*扫码|扫一扫|QR code/i); }, 600);
  return opened;
})()"#
    }

    pub fn refresh_qr_js(&self) -> &'static str {
        r#"(() => {
  const visible = e => e && e.offsetParent !== null;
  const text = e => ((e.innerText || '') + ' ' + (e.getAttribute('aria-label') || '')).trim();
  const hits = Array.from(document.querySelectorAll('button,[role="button"],a,div,span'))
    .filter(visible)
    .filter(e => {
      const t = text(e);
      return t.length > 0 && t.length < 20 && /刷新|重新获取|二维码失效|二维码过期|Refresh/i.test(t);
    });
  if (hits[0]) { hits[0].click(); return true; }
  return false;
})()"#
    }

    /// 从导出的 cookies/localStorage 判断账号是否已登录。
    pub fn logged_in(&self, bundle: &Value) -> bool {
        let markers = self.login_markers();
        if let Some(cookies) = bundle.get("cookies").and_then(Value::as_array) {
            for c in cookies {
                let name = c
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if markers.iter().any(|m| name.contains(m)) {
                    return true;
                }
            }
        }
        if let Some(ls) = bundle.get("local_storage").and_then(Value::as_object) {
            for k in ls.keys() {
                let k = k.to_ascii_lowercase();
                if markers.iter().any(|m| k.contains(m)) {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve() {
        assert_eq!(Provider::parse("豆包").unwrap(), Provider::Doubao);
        assert_eq!(Provider::parse("qwen").unwrap(), Provider::Qianwen);
        assert_eq!(Provider::parse("Kimi").unwrap(), Provider::Kimi);
        assert_eq!(Provider::parse("z-ai").unwrap(), Provider::Zcode);
        assert!(Provider::parse("unknown").is_err());
    }

    #[test]
    fn ids_and_domains_are_unique() {
        let mut ids = std::collections::HashSet::new();
        let mut domains = std::collections::HashSet::new();
        for p in Provider::ALL {
            assert!(ids.insert(p.id()));
            assert!(domains.insert(p.domain_filter()));
            assert!(!p.login_url().is_empty());
            assert!(!p.login_markers().is_empty());
        }
    }

    #[test]
    fn logged_in_detects_cookie_and_local_storage() {
        let p = Provider::Kimi;
        let cookie = serde_json::json!({
            "cookies": [{"name": "kimi-auth"}],
            "local_storage": {}
        });
        assert!(p.logged_in(&cookie));
        let ls = serde_json::json!({
            "cookies": [],
            "local_storage": {"access_token": "x"}
        });
        assert!(p.logged_in(&ls));
        assert!(!p.logged_in(&serde_json::json!({"cookies": [], "local_storage": {}})));
    }
}
