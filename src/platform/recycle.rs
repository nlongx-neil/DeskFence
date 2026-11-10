//! 回收站删除（`SHFileOperationW` + `FOF_ALLOWUNDO`）。
//!
//! 调用本身可能阻塞（几百个大文件、被杀毒软件扫描等），因此公开的
//! [`spawn_recycle`] 在独立线程执行，结果通过 [`BgEvent::FileRecycled`] 回 UI。

use std::path::PathBuf;

use windows::core::PCWSTR;
use windows::Win32::UI::Shell::{
    SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE,
    SHFILEOPSTRUCTW,
};

/// 去掉 `\\?\` / `\\?\UNC\` 前缀：旧版 SHFileOperation 不接受 verbatim 路径。
fn normalize_for_shell(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

/// 构造 `pFrom`：每个路径以 NUL 结尾，列表再以一个 NUL 收尾（双 NUL 结束）。
pub fn build_double_null(paths: &[PathBuf]) -> Vec<u16> {
    let mut buf = Vec::new();
    for path in paths {
        let normalized = normalize_for_shell(&path.to_string_lossy());
        buf.extend(normalized.encode_utf16());
        buf.push(0);
    }
    buf.push(0);
    buf
}

/// 同步删除到回收站（必须在后台线程调用）。
pub fn recycle_paths(paths: &[PathBuf]) -> Result<(), String> {
    let absolute: Vec<PathBuf> = paths.iter().filter(|p| p.is_absolute()).cloned().collect();
    if absolute.is_empty() {
        return Err("没有可操作的绝对路径".to_string());
    }
    let from = build_double_null(&absolute);
    let flags = (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_SILENT.0 | FOF_NOERRORUI.0) as u16;
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: flags,
        ..Default::default()
    };
    // SAFETY: `op` 与 `from` 在调用期间均有效；`pFrom` 指向双 NUL 结尾的 UTF-16 缓冲。
    let result = unsafe { SHFileOperationW(&mut op) };
    if result != 0 {
        return Err(format!("SHFileOperationW 失败，返回码 {result}"));
    }
    if op.fAnyOperationsAborted.as_bool() {
        return Err("操作被中止".to_string());
    }
    Ok(())
}

/// 在后台线程执行回收站删除，完成后发回 UI。
pub fn spawn_recycle(paths: Vec<PathBuf>, sink: crate::events::EventSink) {
    let spawned = std::thread::Builder::new()
        .name("recycle-batch".into())
        .spawn(move || {
            let result = recycle_paths(&paths);
            let (ok, error) = match &result {
                Ok(()) => (true, None),
                Err(e) => (false, Some(e.clone())),
            };
            if let Err(e) = &result {
                log::warn!("回收站删除失败（{} 项）: {e}", paths.len());
            } else {
                log::info!("已送入回收站 {} 项", paths.len());
            }
            sink.emit(crate::events::BgEvent::FileRecycled { paths, ok, error });
        });
    if let Err(e) = spawned {
        log::error!("启动回收站线程失败: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_null_terminates_list() {
        let paths = vec![PathBuf::from(r"C:\a\b.txt"), PathBuf::from(r"C:\c\d.txt")];
        let buf = build_double_null(&paths);
        assert_eq!(buf.len(), "C:\\a\\b.txt".len() + "C:\\c\\d.txt".len() + 3);
        assert_eq!(&buf[buf.len() - 1..], &[0][..]);
        // 每个路径之后一个 NUL，最后再一个 NUL
        let nul_count = buf.iter().filter(|c| **c == 0).count();
        assert_eq!(nul_count, 3);
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(normalize_for_shell(r"\\?\C:\x\y.txt"), r"C:\x\y.txt");
        assert_eq!(
            normalize_for_shell(r"\\?\UNC\server\share\y.txt"),
            r"\\server\share\y.txt"
        );
        assert_eq!(normalize_for_shell(r"C:\plain.txt"), r"C:\plain.txt");
    }

    #[test]
    fn rejects_relative_paths() {
        assert!(recycle_paths(&[PathBuf::from("relative.txt")]).is_err());
        assert!(recycle_paths(&[]).is_err());
    }
}
