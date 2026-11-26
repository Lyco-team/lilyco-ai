//! 通用小工具：base64 编码、图片魔数嗅探与 UTF-8 安全截断。

/// 标准 base64（带 `=` 填充），用于图片内联进 JSON / MCP 内容块。
pub fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = ((chunk[0] as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 按魔数判断图片类型：返回 `(mime, 扩展名)`。
pub fn sniff_image_mime(bytes: &[u8]) -> (&'static str, &'static str) {
    if bytes.starts_with(b"\x89PNG") {
        ("image/png", "png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        ("image/jpeg", "jpg")
    } else if bytes.starts_with(b"RIFF") && bytes.len() > 11 && &bytes[8..12] == b"WEBP" {
        ("image/webp", "webp")
    } else if bytes.starts_with(b"GIF8") {
        ("image/gif", "gif")
    } else {
        ("application/octet-stream", "bin")
    }
}

/// 按字符数截断（不破坏多字节 UTF-8 边界）
pub fn clip(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_and_mime_sniff_work() {
        assert_eq!(base64_encode(b"\x89PNG"), "iVBORw==");
        assert_eq!(sniff_image_mime(b"\x89PNG\r\n").0, "image/png");
        assert_eq!(sniff_image_mime(b"\xff\xd8\xff").1, "jpg");
        assert_eq!(sniff_image_mime(b"RIFF1234WEBP").0, "image/webp");
        assert_eq!(sniff_image_mime(b"GIF89a").0, "image/gif");
        assert_eq!(clip("你好世界", 2), "你好…");
    }
}
