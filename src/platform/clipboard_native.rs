//! 原生剪贴板读写：文本 / HTML / DIB 图片 / CF_HDROP 文件列表。

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, POINT};
use windows::Win32::System::DataExchange::{
    EmptyClipboard, GetClipboardData, GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{DragQueryFileW, DROPFILES, HDROP};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

/// 一次剪贴板读取的结果。
#[derive(Clone, Debug)]
pub enum ClipContent {
    Text {
        text: String,
        html: Option<String>,
    },
    Image {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Files(Vec<PathBuf>),
}

impl ClipContent {
    pub fn kind(&self) -> &'static str {
        match self {
            ClipContent::Text { .. } => "text",
            ClipContent::Image { .. } => "image",
            ClipContent::Files(_) => "files",
        }
    }
}

fn with_clipboard_open<T>(f: impl FnOnce() -> T) -> Option<T> {
    for attempt in 0..6 {
        // SAFETY: 打开剪贴板，失败时短暂重试（其他程序可能正占用）。
        unsafe {
            if OpenClipboard(None).is_ok() {
                let out = f();
                let _ = windows::Win32::System::DataExchange::CloseClipboard();
                return Some(out);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20 + attempt * 10));
    }
    log::warn!("剪贴板被占用，读取失败");
    None
}

fn html_format() -> u32 {
    // SAFETY: 注册剪贴板格式，仅返回 id。
    unsafe { RegisterClipboardFormatW(w!("HTML Format")) }
}

fn read_text() -> Option<String> {
    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32) }.is_err() {
        return None;
    }
    // SAFETY: 格式可用，GetClipboardData 返回有效句柄。
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }.ok()?;
    // SAFETY: 剪贴板内存已锁定，读取直到 NUL。
    unsafe {
        let hglobal = HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            return None;
        }
        let mut len = 0usize;
        let data = ptr as *const u16;
        while len < 1_048_576 && *data.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(data, len);
        let text = String::from_utf16_lossy(slice);
        let _ = GlobalUnlock(hglobal);
        Some(text)
    }
}

fn read_html() -> Option<String> {
    let fmt = html_format();
    if fmt == 0 {
        return None;
    }
    // SAFETY: 读取已注册格式。
    let handle = unsafe { GetClipboardData(fmt) }.ok()?;
    // SAFETY: 剪贴板内存已锁定，按字节读取到 NUL。
    unsafe {
        let hglobal = HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            return None;
        }
        let size = GlobalSize(hglobal) as usize;
        let bytes = std::slice::from_raw_parts(ptr as *const u8, size);
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        let text = String::from_utf8_lossy(&bytes[..end]).to_string();
        let _ = GlobalUnlock(hglobal);
        Some(text)
    }
}

fn read_files() -> Option<Vec<PathBuf>> {
    // SAFETY: 格式可用性查询。
    if unsafe { IsClipboardFormatAvailable(CF_HDROP.0 as u32) }.is_err() {
        return None;
    }
    // SAFETY: 读取 CF_HDROP。
    let handle = unsafe { GetClipboardData(CF_HDROP.0 as u32) }.ok()?;
    let hdrop = HDROP(handle.0);
    // SAFETY: HDROP 有效。
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    if count == 0 || count > 10_000 {
        return None;
    }
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mut buf = vec![0u16; 32_768];
        // SAFETY: 缓冲区有效，长度正确。
        let n = unsafe { DragQueryFileW(hdrop, i, Some(&mut buf)) };
        if n == 0 {
            continue;
        }
        let path = PathBuf::from(OsString::from_wide(&buf[..n as usize]));
        if path.as_os_str().is_empty() {
            continue;
        }
        out.push(path);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn read_dib() -> Option<(u32, u32, Vec<u8>)> {
    // SAFETY: 格式可用性查询。
    if unsafe { IsClipboardFormatAvailable(CF_DIB.0 as u32) }.is_err() {
        return None;
    }
    // SAFETY: 读取 CF_DIB。
    let handle = unsafe { GetClipboardData(CF_DIB.0 as u32) }.ok()?;
    // SAFETY: 剪贴板内存已锁定，按字节读取。
    unsafe {
        let hglobal = HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            return None;
        }
        let size = GlobalSize(hglobal) as usize;
        if size < 40 {
            let _ = GlobalUnlock(hglobal);
            return None;
        }
        let bytes = std::slice::from_raw_parts(ptr as *const u8, size);
        let out = parse_dib(bytes);
        let _ = GlobalUnlock(hglobal);
        out
    }
}

/// 解析 CF_DIB（BITMAPINFOHEADER + 像素）为 RGBA。
pub fn parse_dib(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if bytes.len() < 40 {
        return None;
    }
    let u32_at = |off: usize| -> u32 {
        u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap_or([0; 4]))
    };
    let i32_at = |off: usize| -> i32 {
        i32::from_le_bytes(bytes[off..off + 4].try_into().unwrap_or([0; 4]))
    };
    let u16_at = |off: usize| -> u16 {
        u16::from_le_bytes(bytes[off..off + 2].try_into().unwrap_or([0; 2]))
    };

    let header_size = u32_at(0) as usize;
    let width = i32_at(4);
    let height = i32_at(8);
    let bit_count = u16_at(14);
    let compression = u32_at(16);
    let clr_used = u32_at(32) as usize;
    if width <= 0 || height == 0 || header_size < 40 || header_size > bytes.len() {
        return None;
    }
    let w = width as usize;
    let h = height.unsigned_abs() as usize;
    if w > 20_000 || h > 20_000 {
        return None;
    }
    // 仅支持 BI_RGB(0) 与 BI_BITFIELDS(3) 的 24/32 位
    if compression != 0 && compression != 3 {
        return None;
    }
    let mask_bytes = if compression == 3 { 12 } else { 0 };
    let palette_entries = if bit_count <= 8 {
        if clr_used == 0 {
            1usize << bit_count
        } else {
            clr_used
        }
    } else {
        clr_used
    };
    let data_offset = header_size + mask_bytes + palette_entries * 4;
    let stride = (w * bit_count as usize).div_ceil(32) * 4;
    if data_offset + stride * h > bytes.len() {
        return None;
    }
    let top_down = height < 0;
    let mut rgba = vec![0u8; w * h * 4];
    match bit_count {
        32 => {
            let mut alphas = 0u32;
            for row in 0..h {
                let src_row = if top_down { row } else { h - 1 - row };
                let src = data_offset + src_row * stride;
                for x in 0..w {
                    let p = src + x * 4;
                    let b = bytes[p];
                    let g = bytes[p + 1];
                    let r = bytes[p + 2];
                    let a = bytes[p + 3];
                    alphas += a as u32;
                    let d = (row * w + x) * 4;
                    rgba[d] = r;
                    rgba[d + 1] = g;
                    rgba[d + 2] = b;
                    rgba[d + 3] = a;
                }
            }
            if alphas == 0 {
                for px in rgba.as_chunks_mut::<4>().0 {
                    px[3] = 255;
                }
            }
        }
        24 => {
            for row in 0..h {
                let src_row = if top_down { row } else { h - 1 - row };
                let src = data_offset + src_row * stride;
                for x in 0..w {
                    let p = src + x * 3;
                    let d = (row * w + x) * 4;
                    rgba[d] = bytes[p + 2];
                    rgba[d + 1] = bytes[p + 1];
                    rgba[d + 2] = bytes[p];
                    rgba[d + 3] = 255;
                }
            }
        }
        _ => return None,
    }
    Some((w as u32, h as u32, rgba))
}

/// 读取当前剪贴板内容（按 文件 > 文本/HTML > 图片 的优先级）。
pub fn read_clipboard() -> Option<ClipContent> {
    with_clipboard_open(|| {
        if let Some(files) = read_files() {
            return Some(ClipContent::Files(files));
        }
        if let Some(text) = read_text() {
            if !text.is_empty() {
                return Some(ClipContent::Text {
                    text,
                    html: read_html(),
                });
            }
        }
        if let Some((w, h, rgba)) = read_dib() {
            return Some(ClipContent::Image {
                width: w,
                height: h,
                rgba,
            });
        }
        None
    })?
}

fn alloc_and_set(format: u32, bytes: &[u8]) -> bool {
    // SAFETY: 分配可移动全局内存并复制数据，成功后所有权交给剪贴板。
    unsafe {
        let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) else {
            return false;
        };
        let ptr = GlobalLock(mem);
        if ptr.is_null() {
            let _ = windows::Win32::Foundation::GlobalFree(Some(mem));
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        let _ = GlobalUnlock(mem);
        match SetClipboardData(format, Some(HANDLE(mem.0))) {
            Ok(_) => true,
            Err(e) => {
                log::warn!("设置剪贴板格式失败: {e}");
                let _ = windows::Win32::Foundation::GlobalFree(Some(mem));
                false
            }
        }
    }
}

fn wide_bytes(s: &str) -> Vec<u8> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    // SAFETY: u16 切片按字节读取，长度正确。
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), v.len() * 2).to_vec() }
}

/// 构造标准 CF_HTML 载荷（带字节偏移头）。
pub fn build_cf_html(fragment: &str) -> String {
    // 偏移字段宽度固定为 10，因此用占位值算出的头部长度即为最终长度
    let header_len = format!(
        "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
        0, 0, 0, 0
    )
    .len();
    let prefix = "<html><body><!--StartFragment-->";
    let suffix = "<!--EndFragment--></body></html>";
    let start_html = header_len;
    let start_fragment = start_html + prefix.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + suffix.len();
    let header = format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n"
    );
    format!("{header}{prefix}{fragment}{suffix}")
}

/// 写入纯文本（可选同时写入 HTML 富文本）。
pub fn write_text(text: &str, html: Option<&str>) -> bool {
    let Some(()) = with_clipboard_open(|| {
        // SAFETY: 清空并写入；所有权转移由系统保证。
        unsafe {
            if EmptyClipboard().is_err() {
                return;
            }
        }
        let ok = alloc_and_set(CF_UNICODETEXT.0 as u32, &wide_bytes(text));
        if let Some(html) = html {
            let fmt = html_format();
            if fmt != 0 {
                let payload = build_cf_html(html);
                alloc_and_set(fmt, &wide_bytes(&payload));
            }
        }
        if !ok {
            log::warn!("写入剪贴板文本失败");
        }
    }) else {
        return false;
    };
    true
}

/// 把 RGBA 图像写入剪贴板（CF_DIB，32 位 BGRA，bottom-up）。
pub fn write_image(width: u32, height: u32, rgba: &[u8]) -> bool {
    if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
        return false;
    }
    let w = width as usize;
    let h = height as usize;
    let stride = w * 4;
    let mut dib = vec![0u8; 40 + stride * h];
    #[inline]
    fn put(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    put(&mut dib, 0, 40);
    put(&mut dib, 4, width);
    put(&mut dib, 8, height);
    dib[12..14].copy_from_slice(&1u16.to_le_bytes()); // planes
    dib[14..16].copy_from_slice(&32u16.to_le_bytes()); // bpp
    put(&mut dib, 16, 0); // BI_RGB
    put(&mut dib, 20, (stride * h) as u32);
    for row in 0..h {
        let src_row = h - 1 - row; // bottom-up
        for x in 0..w {
            let s = (src_row * w + x) * 4;
            let d = 40 + row * stride + x * 4;
            dib[d] = rgba[s + 2];
            dib[d + 1] = rgba[s + 1];
            dib[d + 2] = rgba[s];
            dib[d + 3] = rgba[s + 3];
        }
    }
    let Some(()) = with_clipboard_open(|| {
        // SAFETY: 清空并写入。
        unsafe {
            if EmptyClipboard().is_err() {
                return;
            }
        }
        if !alloc_and_set(CF_DIB.0 as u32, &dib) {
            log::warn!("写入剪贴板图片失败");
        }
    }) else {
        return false;
    };
    true
}

/// 写入文件列表（CF_HDROP）。
pub fn write_files(paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }
    let mut list: Vec<u16> = Vec::new();
    for p in paths {
        list.extend(p.as_os_str().to_string_lossy().encode_utf16());
        list.push(0);
    }
    list.push(0);
    let header = std::mem::size_of::<DROPFILES>();
    let mut buf = vec![0u8; header + list.len() * 2];
    // SAFETY: DROPFILES 为简单 POD 结构，写入缓冲区前 header 字节。
    unsafe {
        let df = DROPFILES {
            pFiles: header as u32,
            pt: POINT { x: 0, y: 0 },
            fNC: Default::default(),
            fWide: windows::core::BOOL(1),
        };
        std::ptr::copy_nonoverlapping(
            &df as *const DROPFILES as *const u8,
            buf.as_mut_ptr(),
            header,
        );
    }
    // SAFETY: u16 列表按字节写入尾部。
    unsafe {
        std::ptr::copy_nonoverlapping(
            list.as_ptr().cast::<u8>(),
            buf.as_mut_ptr().add(header),
            list.len() * 2,
        );
    }
    let Some(()) = with_clipboard_open(|| {
        // SAFETY: 清空并写入。
        unsafe {
            if EmptyClipboard().is_err() {
                return;
            }
        }
        if !alloc_and_set(CF_HDROP.0 as u32, &buf) {
            log::warn!("写入剪贴板文件列表失败");
        }
    }) else {
        return false;
    };
    true
}

/// 清空剪贴板。
pub fn clear() -> bool {
    with_clipboard_open(|| {
        // SAFETY: 仅清空。
        unsafe {
            let _ = EmptyClipboard();
        }
    })
    .is_some()
}

/// 当前剪贴板内容持有者的进程名（小写，如 `chrome.exe`）。
pub fn owner_process_name() -> Option<String> {
    // SAFETY: 只读查询。
    let owner: HWND = unsafe { GetClipboardOwner() }.unwrap_or_default();
    if owner.is_invalid() {
        return None;
    }
    let mut pid = 0u32;
    // SAFETY: 输出 pid 为有效栈内存。
    unsafe {
        GetWindowThreadProcessId(owner, Some(&mut pid));
    }
    if pid == 0 {
        return None;
    }
    // SAFETY: 打开进程用于查询镜像名。
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: 输出缓冲区有效。
    let res = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };
    // SAFETY: 关闭已打开句柄。
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    if res.is_err() || len == 0 {
        return None;
    }
    let path = OsString::from_wide(&buf[..len as usize]);
    PathBuf::from(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
}

/// 剪贴板当前是否有内容。
pub fn has_content() -> bool {
    // SAFETY: 只读查询。
    unsafe {
        IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok()
            || IsClipboardFormatAvailable(CF_DIB.0 as u32).is_ok()
            || IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok()
    }
}

/// 仅用于调试：判断某个路径字符串是否可作为 HSTRING。
#[allow(dead_code)]
fn _debug_hstring(s: &str) -> HSTRING {
    HSTRING::from(s)
}

#[allow(dead_code)]
fn _debug_pcwstr(p: PCWSTR) -> usize {
    p.as_ptr() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_html_offsets_are_consistent() {
        let payload = build_cf_html("<b>hi</b>");
        assert!(payload.starts_with("Version:0.9\r\n"));
        assert!(payload.contains("StartFragment:"));
        let idx = payload.find("<!--StartFragment-->").unwrap() + "<!--StartFragment-->".len();
        let start = payload
            .split("StartFragment:")
            .nth(1)
            .unwrap()
            .split("\r\n")
            .next()
            .unwrap()
            .trim()
            .parse::<usize>()
            .unwrap();
        assert_eq!(start, idx);
    }

    #[test]
    fn parse_32bit_dib_roundtrip() {
        // 2x2 32bpp top-down DIB，含 alpha
        let w = 2u32;
        let h = 2u32;
        let stride = (w * 4) as usize;
        let mut dib = vec![0u8; 40 + stride * h as usize];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&(w as i32).to_le_bytes());
        dib[8..12].copy_from_slice(&(h as i32).to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[16..20].copy_from_slice(&0u32.to_le_bytes());
        dib[20..24].copy_from_slice(&((stride * 2) as u32).to_le_bytes());
        for i in 0..(w * h) as usize {
            let off = 40 + i * 4;
            dib[off] = 10; // B
            dib[off + 1] = 20; // G
            dib[off + 2] = 30; // R
            dib[off + 3] = 200; // A
        }
        let (pw, ph, rgba) = parse_dib(&dib).unwrap();
        assert_eq!((pw, ph), (2, 2));
        assert_eq!(&rgba[0..4], &[30, 20, 10, 200]);
    }

    #[test]
    fn parse_24bit_bottom_up() {
        let w = 1u32;
        let h = 2u32;
        let stride = 4usize;
        let mut dib = vec![0u8; 40 + stride * h as usize];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&(w as i32).to_le_bytes());
        dib[8..12].copy_from_slice(&(h as i32).to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&24u16.to_le_bytes());
        dib[16..20].copy_from_slice(&0u32.to_le_bytes());
        dib[20..24].copy_from_slice(&((stride * 2) as u32).to_le_bytes());
        // 第 0 行（内存中最后一行，对应图像顶部）
        dib[40 + 4] = 1;
        dib[40 + 5] = 2;
        dib[40 + 6] = 3;
        let (pw, ph, rgba) = parse_dib(&dib).unwrap();
        assert_eq!((pw, ph), (1, 2));
        assert_eq!(&rgba[0..3], &[3, 2, 1]);
        assert_eq!(rgba[3], 255);
    }
}
