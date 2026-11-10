//! 开机自启：HKCU Run 注册表项。

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("DeskKeeper");

fn open_key(access: REG_SAM_FLAGS) -> Option<HKEY> {
    let mut key = HKEY::default();
    // SAFETY: 打开注册表键，输出结构为有效栈内存。
    let res = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, Some(0), access, &mut key) };
    if res == ERROR_SUCCESS {
        Some(key)
    } else {
        None
    }
}

pub fn is_enabled() -> bool {
    let Some(key) = open_key(KEY_READ) else {
        return false;
    };
    let mut buf = vec![0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: 输出缓冲区长度与 size 一致。
    let res = unsafe {
        RegGetValueW(
            key,
            PCWSTR::null(),
            VALUE_NAME,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    // SAFETY: key 由 open_key 打开。
    unsafe {
        let _ = RegCloseKey(key);
    }
    res == ERROR_SUCCESS && size > 2
}

pub fn set_enabled(enabled: bool) -> anyhow::Result<()> {
    let key = open_key(KEY_SET_VALUE).ok_or_else(|| anyhow::anyhow!("无法打开注册表 Run 项"))?;
    let result = if enabled {
        let exe = std::env::current_exe()?;
        let value = format!("\"{}\"", exe.display());
        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: 将 u16 切片按字节读取，长度按 2 倍计算，缓冲区有效。
        let bytes =
            unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
        // SAFETY: key 有效，数据缓冲区有效且长度正确。
        unsafe { RegSetValueExW(key, VALUE_NAME, Some(0), REG_SZ, Some(bytes)) }
    } else {
        // SAFETY: 删除 Run 值；不存在时忽略错误。
        unsafe { RegDeleteValueW(key, VALUE_NAME) }
    };
    // SAFETY: key 由 open_key 打开。
    unsafe {
        let _ = RegCloseKey(key);
    }
    if result == ERROR_SUCCESS || result == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "写入自启注册表失败: {}",
            windows::core::Error::from_hresult(result.to_hresult())
        ))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn autostart_roundtrip_is_queryable() {
        // 只做查询，不修改用户注册表
        let _ = super::is_enabled();
    }
}
