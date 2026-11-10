//! Shell 集成：打开文件、查看属性、删除到回收站、显示资源管理器。

use std::path::{Path, PathBuf};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::UI::Shell::{
    SHFileOperationW, SHQueryRecycleBinW, ShellExecuteExW, ShellExecuteW, SEE_MASK_INVOKEIDLIST,
    SHELLEXECUTEINFOW, SHFILEOPSTRUCTW, SHQUERYRBINFO,
};
use windows::Win32::UI::WindowsAndMessaging::{SW_SHOW, SW_SHOWNORMAL};

/// 用默认程序打开路径。
pub fn open_path(path: &Path) -> anyhow::Result<()> {
    // 「此电脑」/「回收站」是合成系统项，路径为 Shell 命名空间 CLSID，
    // 交给 ShellExecute 打开，绝不落到文件系统路径。
    if crate::model::ItemMeta::is_computer_path(path) {
        return open_computer();
    }
    if crate::model::ItemMeta::is_recycle_bin_path(path) {
        return open_recycle_bin();
    }
    let hpath = HSTRING::from(path.as_os_str());
    // SAFETY: 参数为有效 HSTRING / 字面量，SW_SHOWNORMAL 合法。
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &hpath,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.is_invalid() {
        Err(anyhow::anyhow!("无法打开：{}", path.display()))
    } else {
        Ok(())
    }
}

/// 打开资源管理器「此电脑」（合成系统项，不涉及任何磁盘路径）。
pub fn open_computer() -> anyhow::Result<()> {
    // 直接把命名空间 CLSID 交给 ShellExecute，由资源管理器解析为「此电脑」；
    // 若传 explorer.exe + "shell:::{...}" 参数，本机会落到用户文档目录而不是「此电脑」。
    let target = HSTRING::from(crate::model::COMPUTER_CLSID_PATH);
    // SAFETY: 参数为有效 HSTRING / 字面量。
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &target,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.is_invalid() {
        Err(anyhow::anyhow!("无法打开「此电脑」"))
    } else {
        Ok(())
    }
}

/// 打开回收站（合成系统项，不涉及任何磁盘路径）。
pub fn open_recycle_bin() -> anyhow::Result<()> {
    let target = HSTRING::from(crate::model::RECYCLE_BIN_CLSID_PATH);
    // SAFETY: 参数为有效 HSTRING / 字面量。
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &target,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.is_invalid() {
        Err(anyhow::anyhow!("无法打开「回收站」"))
    } else {
        Ok(())
    }
}

/// 启动回收站状态监听线程：每 2 秒做一次 `SHQueryRecycleBinW`（单次 shell32 调用），
/// 只在空/非空状态变化时通过事件唤醒 UI，不产生周期性重绘，也不占用 UI 线程。
pub fn spawn_recycle_bin_watcher(sink: crate::events::EventSink, initial_full: bool) {
    std::thread::Builder::new()
        .name("recycle-bin-watch".into())
        .spawn(move || {
            let mut last = initial_full;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                if let Some(full) = recycle_bin_has_items() {
                    if full != last {
                        last = full;
                        sink.emit(crate::events::BgEvent::RecycleBinState(full));
                    }
                }
            }
        })
        .ok();
}

/// 查询回收站是否非空（`SHQueryRecycleBinW` 传空根路径统计全部驱动器）。
///
/// 返回 `None` 表示查询失败；调用方应保持上一次图标状态，避免误切换成空桶图标。
pub fn recycle_bin_has_items() -> Option<bool> {
    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: info 为有效栈上结构；空 PCWSTR 表示查询所有驱动器。
    match unsafe { SHQueryRecycleBinW(PCWSTR::null(), &mut info) } {
        Ok(()) => Some(info.i64NumItems > 0),
        Err(e) => {
            log::warn!("查询回收站状态失败: {e}");
            None
        }
    }
}

/// 在新窗口中打开目录。
pub fn open_folder(path: &Path) -> anyhow::Result<()> {
    let hpath = HSTRING::from(path.as_os_str());
    // SAFETY: 参数有效。
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &hpath,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.is_invalid() {
        Err(anyhow::anyhow!("无法打开目录：{}", path.display()))
    } else {
        Ok(())
    }
}

/// 在新窗口中打开目录；文件则用资源管理器定位（文件本身没有"新窗口"语义）。
pub fn open_in_new_window(path: &Path) -> anyhow::Result<()> {
    if path.is_file() {
        return reveal_in_explorer(path);
    }
    let hpath = HSTRING::from(path.as_os_str());
    // SAFETY: 参数有效。
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("opennew"),
            &hpath,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if !result.is_invalid() {
        return Ok(());
    }
    // 某些系统壳不认 opennew 动词，退回 explorer.exe /n。
    let arg = format!("/n,\"{}\"", path.display());
    let harg = HSTRING::from(arg);
    // SAFETY: 参数有效。
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("explorer.exe"),
            &harg,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.is_invalid() {
        Err(anyhow::anyhow!("无法在新窗口打开：{}", path.display()))
    } else {
        Ok(())
    }
}

/// 打开系统"属性"对话框。
pub fn show_properties(path: &Path) -> anyhow::Result<()> {
    let hpath = HSTRING::from(path.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST,
        lpVerb: w!("properties"),
        lpFile: PCWSTR::from_raw(hpath.as_ptr()),
        nShow: SW_SHOW.0,
        ..Default::default()
    };
    // SAFETY: info 为有效栈上结构，lpFile 指向的 HSTRING 在调用期间有效。
    let ok = unsafe { ShellExecuteExW(&mut info) };
    ok.map_err(|e| anyhow::anyhow!("打开属性失败: {e}"))
}

/// 删除到回收站（可撤销的系统删除）。
pub fn trash_paths(paths: &[PathBuf]) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        from.extend(p.as_os_str().to_string_lossy().encode_utf16());
        from.push(0);
    }
    from.push(0); // 双 NUL 结尾

    let mut op = SHFILEOPSTRUCTW {
        hwnd: windows::Win32::Foundation::HWND::default(),
        wFunc: windows::Win32::UI::Shell::FO_DELETE,
        pFrom: PCWSTR::from_raw(from.as_ptr()),
        fFlags: (windows::Win32::UI::Shell::FOF_ALLOWUNDO
            | windows::Win32::UI::Shell::FOF_NOCONFIRMATION
            | windows::Win32::UI::Shell::FOF_SILENT)
            .0 as u16,
        ..Default::default()
    };
    // SAFETY: from 缓冲区在调用期间有效，op 为有效结构。
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 {
        Err(anyhow::anyhow!("删除到回收站失败，错误码 {code}"))
    } else if op.fAnyOperationsAborted.as_bool() {
        Err(anyhow::anyhow!("删除操作被中止"))
    } else {
        log::info!("已删除 {} 项到回收站", paths.len());
        Ok(())
    }
}

/// 在资源管理器中选中文件。
pub fn reveal_in_explorer(path: &Path) -> anyhow::Result<()> {
    let arg = format!("/select,\"{}\"", path.display());
    let harg = HSTRING::from(arg);
    // SAFETY: 参数有效。
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("explorer.exe"),
            &harg,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.is_invalid() {
        Err(anyhow::anyhow!("无法在资源管理器中显示"))
    } else {
        Ok(())
    }
}
