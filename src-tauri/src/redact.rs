//! 脱敏与摘要工具：审计记录中不出现完整发货内容、链接或提取码。

use rand::RngCore;
use sha2::{Digest, Sha256};

/// 对发货内容做不可逆摘要（盐存于本地配置，逐台安装不同）。
pub fn content_hash(salt: &str, content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b"\x00");
    hasher.update(content.as_bytes());
    let out = hasher.finalize();
    hex(&out)
}

/// 生成新的随机盐（hex）。
pub fn new_salt() -> String {
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    hex(&buf)
}

/// 生成一次性批准令牌（hex）。
pub fn new_token() -> String {
    let mut buf = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut buf);
    hex(&buf)
}

/// 生成 OAuth state（hex）。
pub fn new_state() -> String {
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    hex(&buf)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 审计用文本脱敏：链接、提取码/密码、邮箱、长数字串替换为占位符。
pub fn redact_text(text: &str) -> String {
    let out = redact_urls(text);
    let out = redact_after_marker(&out, "提取码", 8);
    let out = redact_after_marker(&out, "密码", 8);
    let out = redact_emails(&out);
    let out = redact_long_digits(&out);
    if out.chars().count() > 200 {
        out.chars().take(200).collect::<String>() + "…[截断]"
    } else {
        out
    }
}

/// 把 http(s)://… 替换为占位符（ASCII 大小写无关，字节位置与原文对齐）。
fn redact_urls(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut result = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let tail = &bytes[i..];
        let is_https = tail.len() >= 8 && tail[..8].eq_ignore_ascii_case(b"https://");
        let is_http = !is_https && tail.len() >= 7 && tail[..7].eq_ignore_ascii_case(b"http://");
        if is_https || is_http {
            let scheme_len = if is_https { 8 } else { 7 };
            let mut j = (i + scheme_len).min(bytes.len());
            while j < bytes.len() && !bytes[j].is_ascii_whitespace() && bytes[j] != b'"' && bytes[j] != b'\'' {
                j += 1;
            }
            result.push_str("[链接已脱敏]");
            i = j;
        } else {
            let ch_len = utf8_len(bytes[i]).min(bytes.len() - i);
            result.push_str(&text[i..i + ch_len]);
            i += ch_len;
        }
    }
    result
}

fn utf8_len(first_byte: u8) -> usize {
    match first_byte {
        b if b < 0x80 => 1,
        b if b >> 5 == 0b110 => 2,
        b if b >> 4 == 0b1110 => 3,
        _ => 4,
    }
}

/// 标记词后的 1..=max 个非空白字符替换为占位符（如提取码）。
fn redact_after_marker(text: &str, marker: &str, max: usize) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(marker) {
        result.push_str(&rest[..pos]);
        result.push_str(marker);
        result.push_str("[已脱敏]");
        let after = &rest[pos + marker.len()..];
        // 跳过紧跟的冒号与空白
        let mut skip = 0usize;
        for ch in after.chars() {
            if ch == '：' || ch == ':' || ch.is_whitespace() {
                skip += ch.len_utf8();
            } else {
                break;
            }
        }
        let after = &after[skip..];
        let mut consumed = 0usize;
        let mut taken = 0usize;
        for ch in after.chars() {
            if ch.is_whitespace() || taken >= max.max(1) {
                break;
            }
            consumed += ch.len_utf8();
            taken += 1;
        }
        rest = &after[consumed..];
    }
    result.push_str(rest);
    result
}

fn redact_emails(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for token in split_tokens(text) {
        if token.contains('@') && token.len() > 3 {
            result.push_str("[邮箱已脱敏]");
        } else {
            result.push_str(token);
        }
    }
    result
}

fn split_tokens(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = 0usize;
    for (i, ch) in text.char_indices() {
        if ch.is_whitespace() {
            if i > start {
                tokens.push(&text[start..i]);
            }
            tokens.push(&text[i..i + ch.len_utf8()]);
            start = i + ch.len_utf8();
        }
    }
    if start < text.len() {
        tokens.push(&text[start..]);
    }
    tokens
}

fn redact_long_digits(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let mut run: Vec<char> = Vec::new();
    for ch in chars {
        if ch.is_ascii_digit() {
            run.push(ch);
        } else {
            flush_digit_run(&mut run, &mut result);
            result.push(ch);
        }
    }
    flush_digit_run(&mut run, &mut result);
    result
}

fn flush_digit_run(run: &mut Vec<char>, result: &mut String) {
    if run.len() >= 7 {
        result.push_str(&format!("[数字已脱敏:{}位]", run.len()));
    } else {
        result.extend(run.iter());
    }
    run.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_deterministic_and_salted() {
        let content = "链接 https://pan.baidu.com/s/xyz 提取码: ab12cd";
        assert_eq!(content_hash("s1", content), content_hash("s1", content));
        assert_ne!(content_hash("s1", content), content_hash("s2", content));
    }

    #[test]
    fn redact_removes_url_code_and_phone() {
        let out = redact_text("网盘 HTTPS://Pan.Baidu.Com/s/abc123 提取码：ab12cd 联系 13812345678 邮箱 a@b.com");
        assert!(!out.contains("baidu"), "url leaked: {out}");
        assert!(!out.contains("ab12cd"), "code leaked: {out}");
        assert!(!out.contains("13812345678"), "phone leaked: {out}");
        assert!(!out.contains("a@b.com"), "email leaked: {out}");
        assert!(out.contains("网盘") && out.contains("联系"), "中文正文丢失: {out}");
    }

    #[test]
    fn redact_keeps_normal_chinese_text() {
        let out = redact_text("您购买的课程已发货，请注意查收");
        assert_eq!(out, "您购买的课程已发货，请注意查收");
    }
}
