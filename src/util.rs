//! 通用工具：路径、时间格式化、ID 生成。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 数据目录：`%LOCALAPPDATA%\DeskKeeper`
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("DeskKeeper")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn db_path() -> PathBuf {
    data_dir().join("clipboard.db")
}

pub fn thumbs_dir() -> PathBuf {
    data_dir().join("thumbs")
}

pub fn boxes_dir() -> PathBuf {
    data_dir().join("boxes")
}

pub fn state_path() -> PathBuf {
    data_dir().join("session.state")
}

/// 用户桌面目录。
pub fn user_desktop_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("USERPROFILE") {
        let d = PathBuf::from(p).join("Desktop");
        if d.is_dir() {
            return d;
        }
    }
    PathBuf::from(".")
}

/// 公共桌面目录（所有用户可见）。
pub fn public_desktop_dir() -> PathBuf {
    std::env::var_os("PUBLIC").map_or_else(
        || PathBuf::from("C:\\Users\\Public\\Desktop"),
        |p| PathBuf::from(p).join("Desktop"),
    )
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 无依赖的日期换算（Howard Hinnant civil_from_days 算法）。
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y } as i32, m, d)
}

/// 将秒级时间戳格式化为 `HH:MM:SS`（同一天）或 `MM-DD HH:MM`。
pub fn format_time(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (_, month, day) = civil_from_days(days);
    let today = (now_secs() / 86_400) as i64;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    if days == today {
        format!("{h:02}:{mi:02}:{s:02}")
    } else if days == today - 1 {
        format!("昨天 {h:02}:{mi:02}")
    } else {
        format!("{month:02}-{day:02} {h:02}:{mi:02}")
    }
}

/// 完整日期时间，用于设置页/兼容显示。
pub fn format_datetime(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 人类可读的文件大小（列表视图右侧显示）。
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 生成简短唯一 id（时间戳 + 计数器 + 随机散列），无需 uuid 依赖。
pub fn gen_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut x = nanos ^ (u128::from(n) << 64);
    // 简易混洗，避免相邻 id 相似
    x ^= x >> 33;
    x = x.wrapping_mul(0x_ff51_afd7_ed55_8ccd);
    x ^= x >> 29;
    format!("{prefix}{:x}", x as u64)
}

/// 按字符数截断，保留尾部省略号。
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// 文件名安全化（用于新建盒子目录）。
pub fn sanitize_dir_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        "box".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_formatting_is_stable() {
        // 2024-01-01 00:00:00 UTC
        assert_eq!(format_datetime(1_704_067_200), "2024-01-01 00:00:00");
        // 2024-02-29（闰年）
        assert_eq!(format_datetime(1_709_164_800), "2024-02-29 00:00:00");
    }

    #[test]
    fn size_formatting_uses_binary_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn truncation_counts_chars() {
        assert_eq!(truncate_chars("你好世界", 2), "你好…");
        assert_eq!(truncate_chars("abc", 5), "abc");
    }

    #[test]
    fn sanitize_replaces_forbidden_chars() {
        assert_eq!(sanitize_dir_name("a/b:c*"), "a_b_c_");
        assert_eq!(sanitize_dir_name("   "), "box");
    }

    #[test]
    fn ids_are_unique() {
        let a = gen_id("b");
        let b = gen_id("b");
        assert_ne!(a, b);
        assert!(a.starts_with('b'));
    }
}
