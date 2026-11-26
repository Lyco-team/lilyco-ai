//! CodeArts 凭据：华为云 AK / SK / STS。
//!
//! 加载顺序（先命中先用）：
//! 1. 环境变量 `LCODEARTS_AK` + `LCODEARTS_SK`（CI / 临时借用）；
//! 2. 兼容 CodeArts CLI 的 `CODEARTS_CLI_AK` + `CODEARTS_CLI_SK`；
//! 3. 自己的仓库 `~/.local/share/lcodearts/auth.json`（0600，`lcodearts login` 写入）。
//!
//! STS（security token）是临时凭据的一部分，只有浏览器登录换来的凭据才带；
//! 长期 AK/SK 没有它也能用，但权限和有效期由用户在华为云 IAM 侧决定。

use lilyco::prelude::AppError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    #[serde(alias = "accessKey", alias = "access_key_id")]
    pub access_key: String,
    #[serde(alias = "secretKey", alias = "secret_access_key")]
    pub secret_key: String,
    #[serde(
        default,
        alias = "securitytoken",
        alias = "securityToken",
        alias = "security_token",
        skip_serializing_if = "Option::is_none"
    )]
    pub security_token: Option<String>,
    #[serde(
        default,
        alias = "expiresAt",
        alias = "expiration",
        skip_serializing_if = "Option::is_none"
    )]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_id: Option<String>,
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_at: Option<String>,
}

impl Credential {
    pub fn masked(&self) -> String {
        crate::util::mask(&self.access_key)
    }

    /// 本地记录的有效期已过（只做友好预检查；最终以接口返回为准）
    pub fn expired(&self) -> bool {
        let Some(raw) = self.expires_at.as_deref() else {
            return false;
        };
        let Some(exp) = parse_instant(raw) else {
            return false;
        };
        exp <= crate::util::now_secs()
    }

    pub fn source_label(&self) -> &str {
        if self.source.is_empty() {
            "unknown"
        } else {
            &self.source
        }
    }
}

/// 从环境变量或本地仓库加载凭据。
pub fn load() -> Result<Credential, AppError> {
    if let Some(c) = from_env() {
        return Ok(c);
    }
    let path = store_path().ok_or_else(|| {
        AppError::InvalidInput(
            "拿不到 HOME / XDG_DATA_HOME，无法确定凭据文件位置（可用 LCODEARTS_STORE 指定）"
                .to_string(),
        )
    })?;
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::InvalidInput(format!(
                "尚未配置 CodeArts 凭据（{} 不存在）\n先运行 `lcodearts login` 在浏览器里授权一次，\
                 或设置 LCODEARTS_AK / LCODEARTS_SK 环境变量",
                path.display()
            ))
        } else {
            AppError::Runtime(format!("读取凭据失败 {}: {e}", path.display()))
        }
    })?;
    let mut cred: Credential = serde_json::from_str(&raw).map_err(|e| {
        AppError::Runtime(format!("凭据文件不是合法 JSON（{}）: {e}", path.display()))
    })?;
    if cred.source.is_empty() {
        cred.source = format!("file:{}", path.display());
    }
    if cred.access_key.is_empty() || cred.secret_key.is_empty() {
        return Err(AppError::InvalidInput(format!(
            "凭据文件缺少 access_key / secret_key：{}",
            path.display()
        )));
    }
    Ok(cred)
}

fn from_env() -> Option<Credential> {
    let ak = std::env::var("LCODEARTS_AK")
        .ok()
        .or_else(|| std::env::var("CODEARTS_CLI_AK").ok())
        .filter(|s| !s.trim().is_empty())?;
    let sk = std::env::var("LCODEARTS_SK")
        .ok()
        .or_else(|| std::env::var("CODEARTS_CLI_SK").ok())
        .filter(|s| !s.trim().is_empty())?;
    let st = std::env::var("LCODEARTS_SECURITY_TOKEN")
        .ok()
        .or_else(|| std::env::var("CODEARTS_CLI_SECURITY_TOKEN").ok())
        .filter(|s| !s.trim().is_empty());
    Some(Credential {
        access_key: ak.trim().to_string(),
        secret_key: sk.trim().to_string(),
        security_token: st.map(|s| s.trim().to_string()),
        expires_at: None,
        user_name: None,
        domain_id: None,
        source: "env:LCODEARTS_AK/CODEARTS_CLI_AK".to_string(),
        saved_at: None,
    })
}

/// 凭据仓库路径：`LCODEARTS_STORE` → `$XDG_DATA_HOME/lcodearts/auth.json` →
/// `~/.local/share/lcodearts/auth.json`
pub fn store_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LCODEARTS_STORE") {
        let p = PathBuf::from(p);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(x);
        if !p.as_os_str().is_empty() {
            return Some(p.join("lcodearts").join("auth.json"));
        }
    }
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".local")
            .join("share")
            .join("lcodearts")
            .join("auth.json")
    })
}

/// 写凭据：父目录自动建，落盘后改 0600。
pub fn save(cred: &Credential) -> Result<PathBuf, AppError> {
    let path = store_path().ok_or_else(|| {
        AppError::InvalidInput(
            "拿不到 HOME / XDG_DATA_HOME（可用 LCODEARTS_STORE 指定）".to_string(),
        )
    })?;
    save_at(&path, cred)?;
    Ok(path)
}

pub fn save_at(path: &std::path::Path, cred: &Credential) -> Result<(), AppError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| AppError::Runtime(format!("建目录失败 {}: {e}", dir.display())))?;
    }
    let mut cred = cred.clone();
    cred.saved_at = Some(crate::util::iso8601_from_epoch_secs(crate::util::now_secs()));
    let body = serde_json::to_string_pretty(&cred)
        .map_err(|e| AppError::Runtime(format!("序列化凭据失败: {e}")))?;
    std::fs::write(path, body)
        .map_err(|e| AppError::Runtime(format!("写凭据失败 {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 解析 ISO-8601 / epoch（秒或毫秒）。识别不了就返回 None，由调用方决定是否忽略。
fn parse_instant(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        let n: i64 = s.parse().ok()?;
        return Some(if n > 10_000_000_000 { n / 1000 } else { n });
    }
    // 只精确到「日期 + 时分秒」；时区偏移按 UTC 处理（仅用于过期提示，不用于签名）
    let head = s.get(0..19)?;
    let mut it = head.split(|c| c == '-' || c == 'T' || c == ':');
    let y: i64 = it.next()?.parse().ok()?;
    let mo: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    let h: i64 = it.next()?.parse().ok()?;
    let mi: i64 = it.next()?.parse().ok()?;
    let se: i64 = it.next()?.parse().ok()?;
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_lock() -> MutexGuard<'static, ()> {
        static CELL: OnceLock<Mutex<()>> = OnceLock::new();
        CELL.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn parse_iso_and_epoch() {
        assert!(parse_instant("2026-11-25T12:00:00Z").is_some());
        assert_eq!(parse_instant("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_instant("1795608075000"), Some(1_795_608_075));
        assert_eq!(parse_instant("1795608075"), Some(1_795_608_075));
        assert_eq!(parse_instant("not-a-time"), None);
    }

    #[test]
    fn json_accepts_codearts_session_shape() {
        let v = serde_json::json!({
            "accessKey": "AK",
            "secretKey": "SK",
            "securitytoken": "STS",
            "expires_at": "2026-12-01T00:00:00Z"
        });
        let c: Credential =
            serde_json::from_value(v.clone()).expect("应能解析 accessKey/secretKey/securitytoken");
        assert_eq!(c.access_key, "AK");
        assert_eq!(c.secret_key, "SK");
        assert_eq!(c.security_token.as_deref(), Some("STS"));
        assert_eq!(c.expires_at.as_deref(), Some("2026-12-01T00:00:00Z"));
        assert!(!c.expired());
    }

    #[test]
    fn store_permissions_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lcodearts-store-{}", crate::util::uuid_v4()));
        let path = dir.join("auth.json");
        let cred = Credential {
            access_key: "AK".into(),
            secret_key: "SK".into(),
            security_token: None,
            expires_at: None,
            user_name: None,
            domain_id: None,
            source: "test".into(),
            saved_at: None,
        };
        save_at(&path, &cred).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn env_credentials_win_and_round_trip() {
        let _g = env_lock();
        std::env::set_var("LCODEARTS_AK", "AK-ENV");
        std::env::set_var("LCODEARTS_SK", "SK-ENV");
        std::env::remove_var("CODEARTS_CLI_AK");
        let c = load().unwrap();
        assert_eq!(c.access_key, "AK-ENV");
        assert!(c.source.contains("env"));
        std::env::remove_var("LCODEARTS_AK");
        std::env::remove_var("LCODEARTS_SK");
    }
}
