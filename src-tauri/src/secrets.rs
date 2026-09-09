//! 本机凭据存储：
//! - 短密钥（飞书 App Secret）→ Windows 凭据管理器；
//! - 长 JWT 令牌（飞书 access/refresh token）→ DPAPI 按用户加密后存本地文件。
//!   凭据管理器 Blob 上限 2560 字节，装不下飞书的长令牌，故分流。
//! 绝不保存闲鱼凭据，任何文件中不出现明文令牌。

use std::path::Path;

use keyring::Entry;

const SERVICE: &str = "xianyu-delivery-assistant";
const TOKEN_FILE: &str = "feishu_tokens.bin";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Secret {
    FeishuAppSecret,
}

impl Secret {
    fn account(self) -> &'static str {
        match self {
            Secret::FeishuAppSecret => "feishu-app-secret",
        }
    }

    fn entry(self) -> Result<Entry, String> {
        Entry::new(SERVICE, self.account()).map_err(|e| format!("凭据条目创建失败: {e}"))
    }

    pub fn save(self, value: &str) -> Result<(), String> {
        let entry = self.entry()?;
        entry.set_password(value).map_err(|e| format!("凭据写入失败: {e}"))
    }

    pub fn load(self) -> Result<Option<String>, String> {
        match self.entry()?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("凭据读取失败: {e}")),
        }
    }

    pub fn clear(self) -> Result<(), String> {
        match self.entry()?.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("凭据删除失败: {e}")),
        }
    }
}

// ---------- DPAPI 令牌存储 ----------

/// 保存令牌 JSON（DPAPI CurrentUser 加密，只有本 Windows 用户可解）。
pub fn save_token_bundle(dir: &Path, json: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
    let blob = dpapi_protect(json.as_bytes())?;
    std::fs::write(dir.join(TOKEN_FILE), blob).map_err(|e| format!("令牌文件写入失败: {e}"))
}

/// 读取令牌 JSON；不存在时返回 None。
pub fn load_token_bundle(dir: &Path) -> Result<Option<String>, String> {
    let blob = match std::fs::read(dir.join(TOKEN_FILE)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("令牌文件读取失败: {e}")),
    };
    let plain = dpapi_unprotect(&blob)?;
    String::from_utf8(plain).map(Some).map_err(|_| "令牌文件内容损坏".into())
}

pub fn clear_token_bundle(dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(dir.join(TOKEN_FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("令牌文件删除失败: {e}")),
    }
}

/// 断开飞书授权：清除全部飞书相关凭据。
pub fn clear_all_feishu(dir: &Path) -> Result<(), String> {
    Secret::FeishuAppSecret.clear()?;
    clear_token_bundle(dir)?;
    clear_legacy_entries();
    Ok(())
}

/// 清理旧版本写入凭据管理器的令牌条目（现已迁移到 DPAPI 文件）。
/// 尽力而为：条目不存在或已无权限时静默跳过。
fn clear_legacy_entries() {
    for account in ["feishu-user-access-token", "feishu-user-refresh-token"] {
        if let Ok(entry) = Entry::new(SERVICE, account) {
            let _ = entry.delete_credential();
        }
    }
}

// ---------- DPAPI 封装 ----------

const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

fn dpapi_protect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        );
        if ok == 0 {
            return Err(format!("DPAPI 加密失败 (GetLastError={})", windows_sys::Win32::Foundation::GetLastError()));
        }
        let vec = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        windows_sys::Win32::Foundation::LocalFree(out.pbData.cast());
        Ok(vec)
    }
}

fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        );
        if ok == 0 {
            return Err(format!("DPAPI 解密失败 (GetLastError={})", windows_sys::Win32::Foundation::GetLastError()));
        }
        let vec = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        windows_sys::Win32::Foundation::LocalFree(out.pbData.cast());
        Ok(vec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyring_roundtrip() {
        let s = Secret::FeishuAppSecret;
        s.clear().ok();
        s.save("test-value-123").unwrap();
        assert_eq!(s.load().unwrap().as_deref(), Some("test-value-123"));
        s.clear().unwrap();
        assert_eq!(s.load().unwrap(), None);
    }

    #[test]
    fn dpapi_roundtrip() {
        let long_json = format!("{{\"access_token\":\"{}\",\"expires_at_epoch\":123}}", "x".repeat(4000));
        let dir = std::env::temp_dir().join(format!("xda-test-{}", std::process::id()));
        save_token_bundle(&dir, &long_json).unwrap();
        assert_eq!(load_token_bundle(&dir).unwrap().as_deref(), Some(long_json.as_str()));
        clear_token_bundle(&dir).unwrap();
        assert_eq!(load_token_bundle(&dir).unwrap(), None);
        std::fs::remove_dir(&dir).ok();
    }
}
