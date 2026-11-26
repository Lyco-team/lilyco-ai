//! 多账号仓库：每个账号一份 cookies + localStorage，0600。
//!
//! ```text
//! ~/.local/share/lwebai/
//!   accounts/doubao/lilyco.json
//!   accounts/kimi/alt.json
//!   qr/doubao-lilyco.png
//!   current.json            # 默认 provider/account
//! ```

use crate::providers::Provider;
use lilyco::prelude::AppError;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn home() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LWEBAI_HOME") {
        let p = PathBuf::from(p);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(x);
        if !p.as_os_str().is_empty() {
            return Some(p.join("lwebai"));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share").join("lwebai"))
}

pub fn accounts_root() -> Option<PathBuf> {
    home().map(|h| h.join("accounts"))
}

pub fn qr_root() -> Option<PathBuf> {
    home().map(|h| h.join("qr"))
}

pub fn validate_name(name: &str) -> Result<&str, AppError> {
    let n = name.trim();
    if n.is_empty() {
        return Err(AppError::InvalidInput("account 不能为空".to_string()));
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(AppError::InvalidInput(
            "account 只能用字母数字、_、-、.（不能带路径分隔符）".to_string(),
        ));
    }
    Ok(n)
}

pub fn account_path(provider: Provider, account: &str) -> Result<PathBuf, AppError> {
    let root = accounts_root().ok_or_else(|| {
        AppError::InvalidInput("拿不到 HOME / XDG_DATA_HOME（可用 LWEBAI_HOME 指定）".to_string())
    })?;
    Ok(root
        .join(provider.id())
        .join(format!("{}.json", validate_name(account)?)))
}

pub fn qr_path(provider: Provider, account: &str) -> Result<PathBuf, AppError> {
    let root = qr_root().ok_or_else(|| {
        AppError::InvalidInput("拿不到 HOME / XDG_DATA_HOME（可用 LWEBAI_HOME 指定）".to_string())
    })?;
    Ok(root.join(format!("{}-{}.png", provider.id(), validate_name(account)?)))
}

fn current_path() -> Option<PathBuf> {
    home().map(|h| h.join("current.json"))
}

pub fn save_bundle(provider: Provider, account: &str, bundle: &Value) -> Result<PathBuf, AppError> {
    let path = account_path(provider, account)?;
    save_bundle_at(provider, account, &path, bundle)?;
    Ok(path)
}

pub fn save_bundle_at(
    provider: Provider,
    account: &str,
    path: &Path,
    bundle: &Value,
) -> Result<(), AppError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| AppError::Runtime(format!("建目录失败 {}: {e}", dir.display())))?;
    }
    let mut v = bundle.clone();
    v["provider"] = json!(provider.id());
    v["account"] = json!(validate_name(account)?);
    v["saved_at"] = json!(now_secs());
    let body = serde_json::to_string_pretty(&v)
        .map_err(|e| AppError::Runtime(format!("序列化账号失败: {e}")))?;
    std::fs::write(path, body)
        .map_err(|e| AppError::Runtime(format!("写账号失败 {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn load(provider: Provider, account: &str) -> Result<Value, AppError> {
    let path = account_path(provider, account)?;
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::InvalidInput(format!(
                "账号不存在：{}。先运行 `lwebai login --provider {} --account {}`",
                path.display(),
                provider.id(),
                account
            ))
        } else {
            AppError::Runtime(format!("读取账号失败 {}: {e}", path.display()))
        }
    })?;
    serde_json::from_str(&raw)
        .map_err(|e| AppError::Runtime(format!("账号文件不是合法 JSON（{}）: {e}", path.display())))
}

pub fn remove(provider: Provider, account: &str) -> Result<PathBuf, AppError> {
    let path = account_path(provider, account)?;
    if !path.is_file() {
        return Err(AppError::InvalidInput(format!(
            "账号不存在：{}",
            path.display()
        )));
    }
    std::fs::remove_file(&path)
        .map_err(|e| AppError::Runtime(format!("删除账号失败 {}: {e}", path.display())))?;
    if let Some((p, a)) = current() {
        if p == provider.id() && a == validate_name(account)? {
            let _ = std::fs::remove_file(current_path().unwrap_or_default());
        }
    }
    Ok(path)
}

pub fn set_current(provider: Provider, account: &str) -> Result<PathBuf, AppError> {
    let path = current_path().ok_or_else(|| {
        AppError::InvalidInput("拿不到 HOME / XDG_DATA_HOME（可用 LWEBAI_HOME 指定）".to_string())
    })?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = serde_json::to_string_pretty(&json!({
        "provider": provider.id(),
        "account": validate_name(account)?,
    }))
    .map_err(|e| AppError::Runtime(format!("序列化 current 失败: {e}")))?;
    std::fs::write(&path, body)
        .map_err(|e| AppError::Runtime(format!("写 current 失败 {}: {e}", path.display())))?;
    Ok(path)
}

pub fn current() -> Option<(String, String)> {
    let path = current_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let p = v.get("provider")?.as_str()?.to_string();
    let a = v.get("account")?.as_str()?.to_string();
    Some((p, a))
}

/// 列出账号；provider 为空表示全部。返回给 JSON 的扁平数组。
pub fn list(provider: Option<Provider>) -> Result<Vec<Value>, AppError> {
    let root = accounts_root().ok_or_else(|| {
        AppError::InvalidInput("拿不到 HOME / XDG_DATA_HOME（可用 LWEBAI_HOME 指定）".to_string())
    })?;
    let providers: Vec<Provider> = match provider {
        Some(p) => vec![p],
        None => Provider::ALL.to_vec(),
    };
    let mut out = Vec::new();
    for p in providers {
        let dir = root.join(p.id());
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            out.push(json!({
                "provider": p.id(),
                "display": p.display(),
                "account": stem,
                "saved_at": v.get("saved_at").cloned().unwrap_or(Value::Null),
                "cookie_count": v.get("cookie_count").cloned().unwrap_or(Value::Null),
                "local_storage_keys": v.get("local_storage").and_then(Value::as_object).map(|m| m.len()).unwrap_or(0),
                "origin": v.get("origin").cloned().unwrap_or(Value::Null),
                "path": path.display().to_string(),
            }));
        }
    }
    out.sort_by(|a, b| {
        let ka = format!(
            "{}/{}",
            a["provider"].as_str().unwrap_or(""),
            a["account"].as_str().unwrap_or("")
        );
        let kb = format!(
            "{}/{}",
            b["provider"].as_str().unwrap_or(""),
            b["account"].as_str().unwrap_or("")
        );
        ka.cmp(&kb)
    });
    Ok(out)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_reject_path_traversal() {
        assert!(validate_name("../x").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("lilyco-01_alt").is_ok());
    }

    #[test]
    fn save_read_remove_roundtrip() {
        let dir = std::env::temp_dir().join(format!("lwebai-{}", std::process::id()));
        let path = dir.join("doubao").join("a.json");
        let bundle = json!({"cookies": [], "local_storage": {}, "cookie_count": 0});
        save_bundle_at(Provider::Doubao, "a", &path, &bundle).unwrap();
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["provider"], "doubao");
        assert_eq!(raw["account"], "a");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
