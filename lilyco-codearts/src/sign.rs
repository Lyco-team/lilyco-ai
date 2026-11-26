//! 华为云 SDK-HMAC-SHA256 签名。
//!
//! 逐字对齐 `@huaweicloud/huaweicloud-sdk-core` 的 JS 实现。它与官方文档有两处
//! 不同、又必须一起复刻的坑（抄自 2951461586/CodeArts2API 的 Go 实现）：
//!
//! 1. `CanonicalURI` 每段单独编码后**强制补尾斜杠**；
//! 2. `CanonicalHeaders` 每行自带 `\n`，拼进 `CanonicalRequest` 时又补一个 `\n`，
//!    于是 `SignedHeaders` 前多一个空行。
//!
//! 改这段逻辑之前先跑 `signature_vectors` —— 那四组期望值是用 Python 独立按 JS
//! signer 语义算出来的，不是本实现的输出。

use ring::{digest, hmac};
use std::collections::BTreeMap;

pub const SIGNING_ALGORITHM: &str = "SDK-HMAC-SHA256";

/// 计算一次请求的全部签名头（含 `host`，调用方发送前要把 `host` 去掉）。
///
/// `headers` 是其他要参与签名的头；`x-sdk-date` 缺省时自动补当前 UTC 时间。
/// 返回的键全部小写，值已 trim。
pub fn sign_request(
    ak: &str,
    sk: &str,
    security_token: Option<&str>,
    method: &str,
    raw_url: &str,
    body: &[u8],
    headers: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let url = url::Url::parse(raw_url).map_err(|e| format!("URL 解析失败: {e}"))?;

    let mut signed: BTreeMap<String, String> = headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.trim().to_string()))
        .collect();
    signed.insert("host".to_string(), host_of(&url));
    signed
        .entry("x-sdk-date".to_string())
        .or_insert_with(crate::util::sdk_date_now);
    if let Some(st) = security_token.filter(|s| !s.is_empty()) {
        signed.insert("x-security-token".to_string(), st.to_string());
    }

    // 只有 PUT/PATCH/POST 的 body 参与签名，其余方法按空 body
    let payload: &[u8] = match method.to_ascii_uppercase().as_str() {
        "PUT" | "PATCH" | "POST" => body,
        _ => &[],
    };
    let payload_hash = sha256_hex(payload);

    let names: Vec<&str> = signed.keys().map(String::as_str).collect();
    let signed_headers = names.join(";");

    let mut canonical_headers = String::new();
    for (k, v) in &signed {
        canonical_headers.push_str(k);
        canonical_headers.push(':');
        canonical_headers.push_str(v.trim());
        canonical_headers.push('\n');
    }

    // 注意 ch 已以 \n 结尾，这里第一处 \n 会再补一个空行（上游 JS signer 的行为）
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.to_ascii_uppercase(),
        canonical_uri(&url),
        canonical_query(&url),
        canonical_headers,
        signed_headers,
        payload_hash
    );

    let string_to_sign = format!(
        "{}\n{}\n{}",
        SIGNING_ALGORITHM,
        signed["x-sdk-date"],
        sha256_hex(canonical_request.as_bytes())
    );

    let key = hmac::Key::new(hmac::HMAC_SHA256, sk.as_bytes());
    let signature = hex_lower(hmac::sign(&key, string_to_sign.as_bytes()).as_ref());
    signed.insert(
        "Authorization".to_string(),
        format!(
            "{SIGNING_ALGORITHM} Access={ak}, SignedHeaders={signed_headers}, Signature={signature}"
        ),
    );
    Ok(signed)
}

fn host_of(url: &url::Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    }
}

/// JS signer 的路径编码：逐段解码后按 JS 规则重新编码，再强制补尾斜杠。
fn canonical_uri(url: &url::Url) -> String {
    let path = url.path();
    let mut uri = path
        .split('/')
        .map(|seg| js_encode(&percent_decode(seg)))
        .collect::<Vec<_>>()
        .join("/");
    if uri.is_empty() {
        uri.push('/');
    }
    if !uri.ends_with('/') {
        uri.push('/');
    }
    uri
}

/// 查询串：解码后按 key/value 排序，再逐对编码。
fn canonical_query(url: &url::Url) -> String {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", js_encode(k), js_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// 只保留 `A-Za-z0-9-._~`，其余 UTF-8 字节 → `%XX`（大写）
fn js_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex_lower(digest::digest(&digest::SHA256, data).as_ref())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const VEC_AK: &str = "HSTAAAAAAAAAAAAAAAAAAA";
    const VEC_SK: &str = "sk-secret-0123456789abcdef";
    const VEC_STS: &str = "sts-token-abc123";

    fn map(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// 期望值来自 CodeArts2API 的 signer_test.go（用 Python 按 JS signer 独立算的）
    #[test]
    fn signature_vectors() {
        let cases = vec![
            (
                "POST with body, STS and padded header value",
                VEC_AK,
                VEC_SK,
                VEC_STS,
                "POST",
                "https://snap-access.cn-north-4.myhuaweicloud.com/api/v2/chat/completions",
                r#"{"model":"glm-5.3-flash","stream":false}"#,
                map(&[
                    ("Content-Type", "application/json"),
                    ("X-Sdk-Date", "20260101T000000Z"),
                    ("maas_type", "benefit"),
                    ("model-id", "glm-5.3-flash"),
                    ("X-Ot-Function", "  agent-general  "),
                ]),
                "SDK-HMAC-SHA256 Access=HSTAAAAAAAAAAAAAAAAAAA, SignedHeaders=content-type;host;maas_type;model-id;x-ot-function;x-sdk-date;x-security-token, Signature=d74d70f38c5d7e69b73c728740a2c7397a4191737d93f207d1a6f5fce49e99c4",
            ),
            (
                "GET with query, body must not be signed",
                "AKID",
                "SECRET",
                "",
                "GET",
                "https://opengw.developer.huaweicloud.com/api/v1/user/tokens/balance?b=2&a=1&a=0",
                r#"{"ignored":true}"#,
                map(&[("X-Sdk-Date", "20260203T040506Z")]),
                "SDK-HMAC-SHA256 Access=AKID, SignedHeaders=host;x-sdk-date, Signature=0acd5e903f49fd4e91d71aee6188d446d6f76863c5d8f13a4d428c0af017af45",
            ),
            (
                "path and query need JS urlencode",
                "AKID",
                "SECRET",
                "TOK",
                "POST",
                "https://snap-access.cn-north-4.myhuaweicloud.com/api/v2/models/模型 名称/a$b+c?q=a b&z=~-._",
                "{}",
                map(&[
                    ("X-Sdk-Date", "20260304T050607Z"),
                    ("x-ot-session-id", "ses_deadbeefdeadbeef"),
                ]),
                "SDK-HMAC-SHA256 Access=AKID, SignedHeaders=host;x-ot-session-id;x-sdk-date;x-security-token, Signature=177aeae3dc48dc4b2dec46b1754ee2e1ea3af8d3f76044c33fe74f74df5f7224",
            ),
            (
                "PUT with empty body",
                "AKID",
                "SECRET",
                "",
                "PUT",
                "https://snap-access.cn-north-4.myhuaweicloud.com/v1/ops/claim",
                "",
                map(&[("X-Sdk-Date", "20260405T060708Z")]),
                "SDK-HMAC-SHA256 Access=AKID, SignedHeaders=host;x-sdk-date, Signature=1ee2960c0fad87a352395c32d2e4cb4f1fc2755cd92d09822dea0994700f3f79",
            ),
        ];

        for (name, ak, sk, sts, method, url, body, headers, want) in cases {
            let got = sign_request(
                ak,
                sk,
                if sts.is_empty() { None } else { Some(sts) },
                method,
                url,
                body.as_bytes(),
                &headers,
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(got["Authorization"], want, "{name}");
            assert!(got.contains_key("host"), "{name}: 缺少 host");
            if sts.is_empty() {
                assert!(
                    !got.contains_key("x-security-token"),
                    "{name}: 空 STS 不应注入头"
                );
            } else {
                assert_eq!(got["x-security-token"], sts, "{name}");
            }
        }
    }

    #[test]
    fn canonical_uri_matches_js_rules() {
        let cases = [
            (
                "https://h/api/v2/chat/completions",
                "/api/v2/chat/completions/",
            ),
            ("https://h/", "/"),
            ("https://h", "/"),
            ("https://h/a/b/", "/a/b/"),
            ("https://h/a$b+c", "/a%24b%2Bc/"),
            (
                "https://h/模型 名称",
                "/%E6%A8%A1%E5%9E%8B%20%E5%90%8D%E7%A7%B0/",
            ),
        ];
        for (raw, want) in cases {
            let u = url::Url::parse(raw).unwrap();
            assert_eq!(canonical_uri(&u), want, "{raw}");
        }
    }

    #[test]
    fn canonical_query_sorts_keys_and_values() {
        let cases = [
            ("https://h/p", ""),
            ("https://h/p?b=2&a=1&a=0", "a=0&a=1&b=2"),
            ("https://h/p?q=a b&z=~-._", "q=a%20b&z=~-._"),
        ];
        for (raw, want) in cases {
            let u = url::Url::parse(raw).unwrap();
            assert_eq!(canonical_query(&u), want, "{raw}");
        }
    }

    #[test]
    fn input_headers_are_not_mutated() {
        let headers = map(&[
            ("Content-Type", "application/json"),
            ("X-Sdk-Date", "20260101T000000Z"),
        ]);
        let before = headers.clone();
        let _ = sign_request(
            "AK",
            "SK",
            Some("STS"),
            "POST",
            "https://h/p",
            b"{}",
            &headers,
        );
        assert_eq!(headers, before);
    }
}
