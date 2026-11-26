//! 公共小工具：时间、随机数、十六进制、打码。
//!
//! 刻意不引 chrono / time / uuid —— 需要的能力只有「UTC 时间格式化」「北京时间
//! 日期」「随机 hex / UUIDv4」，手写足够，也避免为一个签到工具拉一堆传递依赖。

use ring::rand::{SecureRandom, SystemRandom};

/// 当前 epoch 秒
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// epoch 秒 → `YYYY-MM-DDTHH:MM:SSZ`（UTC）
pub fn iso8601_from_epoch_secs(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 华为云 `X-Sdk-Date`：`YYYYMMDDTHHMMSSZ`
pub fn sdk_date_now() -> String {
    let secs = now_secs();
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 按钉死的北京时间（UTC+8）算「今天」，与 CodeArts 客户端的签到日界一致。
pub fn beijing_day() -> String {
    let secs = now_secs() + 8 * 3600;
    let days = secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// 北京时间现在是当天第几分钟（用于「几点以后才签到」的判断）。
pub fn beijing_minute_of_day() -> u32 {
    let rem = (now_secs() + 8 * 3600).rem_euclid(86_400);
    (rem / 60) as u32
}

/// Howard Hinnant civil-from-days（公历，无闰秒）
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 测试用：固定 `X-Sdk-Date`
#[cfg(test)]
pub fn sdk_date_from_epoch(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 安全随机字节
fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    let rng = SystemRandom::new();
    // SystemRandom 失败意味着系统熵源不可用；此时宁可返回全零（签名/幂等键仍能工作），
    // 也不要让整个命令 panic。真正需要密码学安全的地方只有凭据文件本身。
    if rng.fill(&mut buf).is_err() {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (now_secs() as usize).wrapping_add(i).wrapping_mul(31) as u8;
        }
    }
    buf
}

pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// `n` 字节随机数 → `2n` 位小写 hex
pub fn random_hex(n: usize) -> String {
    hex(&random_bytes(n))
}

/// UUIDv4（用于 OAuth ticket_id 与签到 idempotentKey）
pub fn uuid_v4() -> String {
    let mut b = random_bytes(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// AK 打码：只留前 4 后 4，避免日志/JSON 里出现完整凭据
pub fn mask(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
}

/// 按字符（不是字节）截断，避免把中文切坏
pub fn clip(s: &str, budget: usize) -> String {
    if s.chars().count() <= budget {
        s.to_string()
    } else {
        s.chars().take(budget).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_formats_match_known_instants() {
        assert_eq!(iso8601_from_epoch_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            iso8601_from_epoch_secs(1_795_608_075),
            "2026-11-25T12:01:15Z"
        );
        assert_eq!(sdk_date_from_epoch(1_795_608_075), "20261125T120115Z");
    }

    #[test]
    fn uuid_is_version_4_and_unique() {
        let a = uuid_v4();
        let b = uuid_v4();
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert_ne!(a, b);
    }

    #[test]
    fn mask_keeps_only_edges() {
        assert_eq!(mask("HSTAAAAAAAAAAAAAAAAAAA"), "HSTA****AAAA");
        assert_eq!(mask("short"), "*****");
    }

    #[test]
    fn clip_counts_characters() {
        assert_eq!(clip("中文中文", 2), "中文");
        assert_eq!(clip("中文", 8), "中文");
    }
}
