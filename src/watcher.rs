//! 目录监听：`ReadDirectoryChangesW` 后台线程 + 目录扫描。
//!
//! 设计：每个被监听目录一个常驻后台线程（阻塞式 `ReadDirectoryChangesW`），
//! 一批通知到达后先做 120ms 防抖，再在**后台线程**中重新扫描受影响的目录，
//! 通过 [`EventSink`] 把 [`BgEvent::DirContents`] 发回 UI 线程。
//! UI 线程永远不做目录遍历。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::config::SortMode;
use crate::events::{BgEvent, EventSink};
use crate::model::{sort_items, ItemMeta};

/// 扫描一个目录（同步阻塞，呼叫方应保证在后台线程）。
pub fn scan_dir(dir: &Path) -> Vec<ItemMeta> {
    let mut items = Vec::new();
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                if let Some(meta) = ItemMeta::from_path(entry.path()) {
                    items.push(meta);
                }
            }
        }
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::debug!("扫描目录失败 {}: {e}", dir.display());
            }
        }
    }
    sort_items(&mut items, SortMode::Name);
    items
}

/// 扫描桌面（用户桌面 + 公共桌面），按文件名去重。
pub fn scan_desktop() -> Vec<ItemMeta> {
    let mut items = scan_dir(&crate::util::user_desktop_dir());
    let mut seen: HashSet<String> = items.iter().map(|i| i.name.to_lowercase()).collect();
    for item in scan_dir(&crate::util::public_desktop_dir()) {
        if seen.insert(item.name.to_lowercase()) {
            items.push(item);
        }
    }
    sort_items(&mut items, SortMode::Name);
    items
}

/// 一个监听目标。
#[derive(Clone, Debug)]
pub struct WatchSpec {
    pub dir: PathBuf,
    pub recursive: bool,
}

/// 监听线程控制句柄。
#[derive(Clone, Default)]
pub struct WatcherHandle {
    stop: Arc<AtomicBool>,
}

impl WatcherHandle {
    /// 请求所有监听线程退出（阻塞中的 `ReadDirectoryChangesW` 无法立即打断，
    /// 线程会在下一次目录变动或进程退出时结束；这里只置标志位，不做 join）。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// 启动监听线程组；目录不存在时跳过并记录日志。
pub fn spawn(sink: EventSink, specs: Vec<WatchSpec>) -> WatcherHandle {
    let handle = WatcherHandle::default();
    for spec in specs {
        if !spec.dir.is_dir() {
            log::debug!("跳过不存在的监听目录: {}", spec.dir.display());
            continue;
        }
        let dir = spec.dir.clone();
        let sink = sink.clone();
        let stop = handle.stop.clone();
        let spawned = std::thread::Builder::new()
            .name(format!(
                "watch-{}",
                dir.file_name().unwrap_or_default().to_string_lossy()
            ))
            .spawn(move || watch_loop(dir, spec.recursive, sink, stop));
        if let Err(e) = spawned {
            log::error!("启动目录监听线程失败: {e}");
        }
    }
    handle
}

fn watch_loop(dir: PathBuf, recursive: bool, sink: EventSink, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let opened = open_dir(&dir);
        let Some(handle) = opened else {
            // 目录暂时不可用（例如盒子目录被外部删除），稍后重试
            std::thread::sleep(Duration::from_millis(500));
            continue;
        };
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let mut returned: u32 = 0;
            // SAFETY: handle 由 open_dir 打开的目录句柄；buffer 为 64KB 有效可变缓冲区，
            // returned 为栈上输出参数，overlapped/完成例程传 None 走同步模式。
            let res = unsafe {
                ReadDirectoryChangesW(
                    handle,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    recursive,
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_DIR_NAME
                        | FILE_NOTIFY_CHANGE_SIZE
                        | FILE_NOTIFY_CHANGE_LAST_WRITE,
                    Some(&mut returned),
                    None,
                    None,
                )
            };
            if res.is_err() {
                log::debug!(
                    "目录监听中断 {}: {}",
                    dir.display(),
                    windows::core::Error::from_thread()
                );
                break;
            }
            if returned == 0 {
                continue;
            }
            let affected = affected_dirs(&dir, &buffer[..returned as usize], recursive);
            if affected.is_empty() {
                continue;
            }
            // 防抖：批量写入（复制多个文件）会产生大量通知，合并成一次扫描
            std::thread::sleep(Duration::from_millis(120));
            for affected_dir in affected {
                let items = scan_dir(&affected_dir);
                sink.emit(BgEvent::DirContents {
                    dir: affected_dir,
                    items,
                });
            }
        }
        // SAFETY: 句柄由 open_dir 创建，本线程拥有所有权。
        unsafe {
            let _ = CloseHandle(handle);
        }
    }
    log::debug!("目录监听线程退出: {}", dir.display());
}

fn open_dir(dir: &Path) -> Option<HANDLE> {
    let path = HSTRING::from(dir.as_os_str());
    // SAFETY: 打开目录句柄用于监听；路径在调用期间有效，共享模式允许其他程序写入。
    let handle = unsafe {
        CreateFileW(
            &path,
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    };
    match handle {
        Ok(h) => Some(h),
        Err(e) => {
            log::debug!("打开监听目录失败 {}: {e}", dir.display());
            None
        }
    }
}

/// 解析 `FILE_NOTIFY_INFORMATION` 链，返回需要重扫的目录集合。
fn affected_dirs(root: &Path, buffer: &[u8], recursive: bool) -> Vec<PathBuf> {
    let mut out: HashSet<PathBuf> = HashSet::new();
    let mut offset = 0usize;
    // FILE_NOTIFY_INFORMATION 固定头部 12 字节（NextEntryOffset/Action/FileNameLength），
    // FileName 紧随其后。缓冲区来自 Vec<u8>，不保证 4 字节对齐，因此统一用未对齐读取。
    while offset + 12 <= buffer.len() {
        // SAFETY: offset + 12 已检查在缓冲区内，使用 read_unaligned 避免对齐假设。
        let (next, name_len) = unsafe {
            let next = std::ptr::read_unaligned(buffer.as_ptr().add(offset) as *const u32);
            let name_len = std::ptr::read_unaligned(buffer.as_ptr().add(offset + 8) as *const u32);
            (next, name_len as usize / 2)
        };
        let name_end = offset + 12 + name_len * 2;
        if name_len == 0 || name_end > buffer.len() {
            break;
        }
        // SAFETY: FileName 的 u16 长度已检查不越界；使用未对齐读取复制到独立切片。
        let name_slice = unsafe {
            std::slice::from_raw_parts(buffer.as_ptr().add(offset + 12) as *const u16, name_len)
        };
        let name = String::from_utf16_lossy(name_slice);
        let changed = root.join(name);
        let parent = changed
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());
        if recursive {
            // 递归模式：任意层级变动都只重扫其直接父目录
            out.insert(parent);
        } else {
            out.insert(root.to_path_buf());
        }
        if next == 0 {
            break;
        }
        offset += next as usize;
    }
    out.into_iter().collect()
}

/// 立即扫描全部目标并投递结果（启动时用于填充 UI，避免启动期阻塞）。
pub fn initial_scan(sink: EventSink, specs: Vec<WatchSpec>) {
    std::thread::Builder::new()
        .name("watch-initial".into())
        .spawn(move || {
            for spec in specs {
                if !spec.dir.is_dir() {
                    continue;
                }
                let items = scan_dir(&spec.dir);
                sink.emit(BgEvent::DirContents {
                    dir: spec.dir,
                    items,
                });
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Storage::FileSystem::{FILE_ACTION_ADDED, FILE_ACTION_REMOVED};

    fn build_buffer(entries: &[(&str, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, (name, action)) in entries.iter().enumerate() {
            let start = out.len();
            let name_len = name.encode_utf16().count();
            out.extend_from_slice(&0u32.to_le_bytes()); // NextEntryOffset 占位
            out.extend_from_slice(&action.to_le_bytes());
            out.extend_from_slice(&(name_len as u32 * 2).to_le_bytes());
            let wide: Vec<u16> = name.encode_utf16().collect();
            for ch in wide {
                out.extend_from_slice(&ch.to_le_bytes());
            }
            if i + 1 < entries.len() {
                let next = (out.len() - start) as u32;
                out[start..start + 4].copy_from_slice(&next.to_le_bytes());
            }
        }
        out
    }

    #[test]
    fn parse_notify_information_chain() {
        let buf = build_buffer(&[
            ("a.txt", FILE_ACTION_ADDED.0),
            ("b.txt", FILE_ACTION_REMOVED.0),
        ]);
        let root = PathBuf::from("C:\\desktop");
        let dirs = affected_dirs(&root, &buf, false);
        assert_eq!(dirs, vec![root.clone()]);
        let dirs_rec = affected_dirs(&root, &buf, true);
        assert_eq!(dirs_rec.len(), 1);
        assert_eq!(dirs_rec[0], root);
    }

    #[test]
    fn parse_recursive_subdir_changes() {
        let buf = build_buffer(&[("box\\sub\\file.txt", FILE_ACTION_ADDED.0)]);
        let root = PathBuf::from("C:\\boxes");
        let dirs = affected_dirs(&root, &buf, true);
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].ends_with("box\\sub"));
    }

    #[test]
    fn scan_skips_missing_dir_without_panic() {
        let items = scan_dir(Path::new("Z:\\definitely_missing_dir_dk"));
        assert!(items.is_empty());
    }
}
