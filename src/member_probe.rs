//! 盒子成员文件的存在性监听。
//!
//! 目标：文件被删除（回收站/彻底删除/外部程序/资源管理器）后，尽快清理盒子引用，
//! 同时区分"盘符卸载/网络不可达"这种暂时读不到的情况，避免误删引用。
//!
//! 开销控制（几百项也不拖慢 UI）：
//! - 不做全量高频轮询。成员集合变化时只在后台**探测新增路径**一次；
//! - 为成员父目录（上限 16 个）起 `ReadDirectoryChangesW` 监听，目录变化后只复测
//!   该目录下的成员，通常 300ms 内上报；
//! - 兜底滚动探测：每 30s 只探测 32 项（300 项约 5 分钟一轮），只做 `metadata`；
//! - 所有探测都在独立线程，UI 线程只收到一条结果事件。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::events::{BgEvent, EventSink};
use crate::model::path_key;
use crate::platform::path_state::{self, PathState};

/// 兜底滚动探测间隔。
const ROLL_INTERVAL: Duration = Duration::from_secs(30);
/// 每次滚动探测的条目数。
const ROLL_BATCH: usize = 32;
/// 目录监听上限；超出的父目录只靠滚动探测。
const MAX_WATCH_DIRS: usize = 16;
/// 删除二次确认延迟（避免编辑器"保存时短暂重建"之类的瞬态）。
const CONFIRM_DELAY: Duration = Duration::from_millis(400);

enum ProbeCmd {
    /// 成员集合变化。
    SetPaths(Vec<PathBuf>),
    /// 立即确认这些路径（系统菜单删除/剪切后用）。
    Confirm(Vec<PathBuf>),
    /// 某个被监听目录发生变化。
    DirChanged(PathBuf),
}

/// 后台成员监听句柄（Clone 便宜；`stop` 只在退出时调用）。
#[derive(Clone)]
pub struct ProbeHandle {
    tx: Sender<ProbeCmd>,
    stop: Arc<AtomicBool>,
}

impl ProbeHandle {
    pub fn set_paths(&self, paths: Vec<PathBuf>) {
        let _ = self.tx.send(ProbeCmd::SetPaths(paths));
    }

    pub fn confirm_now(&self, paths: Vec<PathBuf>) {
        if !paths.is_empty() {
            let _ = self.tx.send(ProbeCmd::Confirm(paths));
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// 启动成员监听线程。
pub fn spawn(sink: EventSink) -> ProbeHandle {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let handle = ProbeHandle {
        tx: tx.clone(),
        stop: stop.clone(),
    };
    let spawned = std::thread::Builder::new()
        .name("member-probe".into())
        .spawn(move || probe_loop(sink, rx, tx, stop));
    if let Err(e) = spawned {
        log::error!("启动成员探测线程失败: {e}");
    }
    handle
}

fn probe_loop(
    sink: EventSink,
    rx: Receiver<ProbeCmd>,
    tx: Sender<ProbeCmd>,
    stop: Arc<AtomicBool>,
) {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut keys: HashSet<String> = HashSet::new();
    let mut watched: Vec<(PathBuf, Arc<AtomicBool>)> = Vec::new();
    let mut cursor = 0usize;
    let mut last_roll = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ProbeCmd::SetPaths(new_paths)) => {
                let mut seen: HashSet<String> = HashSet::new();
                let mut deduped: Vec<PathBuf> = Vec::new();
                for path in new_paths {
                    if seen.insert(path_key(&path)) {
                        deduped.push(path);
                    }
                }
                // 只探测新增路径：启动时（成员首次下发）等价于一次全量首扫，
                // 平时加/移成员只探测新增的少量路径。
                let added: Vec<PathBuf> = deduped
                    .iter()
                    .filter(|p| !keys.contains(&path_key(p)))
                    .cloned()
                    .collect();
                paths = deduped;
                keys = seen;
                cursor = 0;
                reconcile_watchers(&paths, &mut watched, &tx, &stop);
                if !added.is_empty() {
                    probe_batch(&added, &sink);
                }
                last_roll = Instant::now();
            }
            Ok(ProbeCmd::Confirm(list)) => {
                let existing: Vec<PathBuf> = list
                    .into_iter()
                    .filter(|p| keys.contains(&path_key(p)))
                    .collect();
                if !existing.is_empty() {
                    probe_batch(&existing, &sink);
                }
            }
            Ok(ProbeCmd::DirChanged(dir)) => {
                let batch: Vec<PathBuf> = paths
                    .iter()
                    .filter(|p| p.parent() == Some(dir.as_path()))
                    .cloned()
                    .collect();
                if !batch.is_empty() {
                    probe_batch(&batch, &sink);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if paths.is_empty() {
                    continue;
                }
                if last_roll.elapsed() >= ROLL_INTERVAL {
                    let take = paths.len().min(ROLL_BATCH);
                    let mut batch = Vec::with_capacity(take);
                    for i in 0..take {
                        batch.push(paths[(cursor + i) % paths.len()].clone());
                    }
                    cursor = (cursor + take) % paths.len();
                    last_roll = Instant::now();
                    probe_batch(&batch, &sink);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    for (_, flag) in &watched {
        flag.store(true, Ordering::Relaxed);
    }
    log::debug!("成员探测线程退出");
}

/// 探测一批路径并上报；只有"卷可用且路径不存在"才算删除，暂时不可达只统计不清理。
fn probe_batch(batch: &[PathBuf], sink: &EventSink) {
    let started = Instant::now();
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut unreachable: Vec<String> = Vec::new();
    for path in batch {
        match path_state::probe(path) {
            PathState::Deleted => missing.push(path.clone()),
            PathState::Unreachable => unreachable.push(path_key(path)),
            PathState::Alive | PathState::Unknown => {}
        }
    }
    if !missing.is_empty() {
        // 二次确认：仍不存在且卷可用才真正清理，避免瞬态误删。
        std::thread::sleep(CONFIRM_DELAY);
        missing.retain(|p| path_state::probe(p) == PathState::Deleted);
    }
    let deleted: Vec<String> = missing.iter().map(|p| path_key(p)).collect();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    log::info!(
        "[perf] 成员探测 {} 项：{:.1} ms（删除 {}，不可达 {}）",
        batch.len(),
        elapsed_ms,
        deleted.len(),
        unreachable.len()
    );
    if !deleted.is_empty() || !unreachable.is_empty() {
        sink.emit(BgEvent::MembersProbed {
            deleted,
            unreachable,
            checked: batch.len(),
            elapsed_ms,
        });
    }
}

/// 同步成员父目录监听线程集合（上限 `MAX_WATCH_DIRS`，按成员数优先）。
fn reconcile_watchers(
    paths: &[PathBuf],
    watched: &mut Vec<(PathBuf, Arc<AtomicBool>)>,
    tx: &Sender<ProbeCmd>,
    master_stop: &Arc<AtomicBool>,
) {
    let mut counts: HashMap<PathBuf, usize> = HashMap::new();
    for path in paths {
        if let Some(parent) = path.parent() {
            if parent.is_dir() {
                *counts.entry(parent.to_path_buf()).or_default() += 1;
            }
        }
    }
    let mut wanted: Vec<PathBuf> = counts.keys().cloned().collect();
    wanted.sort_by(|a, b| {
        counts[b]
            .cmp(&counts[a])
            .then_with(|| a.as_os_str().cmp(b.as_os_str()))
    });
    let dropped = wanted.len().saturating_sub(MAX_WATCH_DIRS);
    wanted.truncate(MAX_WATCH_DIRS);
    if dropped > 0 {
        log::info!("成员目录监听上限 {MAX_WATCH_DIRS}，{dropped} 个目录改由滚动探测覆盖");
    }

    watched.retain(|(dir, flag)| {
        if wanted.contains(dir) {
            true
        } else {
            flag.store(true, Ordering::Relaxed);
            false
        }
    });

    for dir in wanted {
        if watched.iter().any(|(d, _)| d == &dir) {
            continue;
        }
        let local_stop = Arc::new(AtomicBool::new(false));
        let dir_clone = dir.clone();
        let tx_clone = tx.clone();
        let master = master_stop.clone();
        let local = local_stop.clone();
        let spawned = std::thread::Builder::new()
            .name(format!(
                "probe-watch-{}",
                dir.file_name().unwrap_or_default().to_string_lossy()
            ))
            .spawn(move || dir_watch_loop(dir_clone, tx_clone, master, local));
        if let Err(e) = spawned {
            log::error!("启动成员目录监听失败 {}: {e}", dir.display());
            continue;
        }
        watched.push((dir, local_stop));
    }
}

fn dir_watch_loop(
    dir: PathBuf,
    tx: Sender<ProbeCmd>,
    master_stop: Arc<AtomicBool>,
    local_stop: Arc<AtomicBool>,
) {
    while !master_stop.load(Ordering::Relaxed) && !local_stop.load(Ordering::Relaxed) {
        let Some(handle) = open_dir(&dir) else {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        };
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            if master_stop.load(Ordering::Relaxed) || local_stop.load(Ordering::Relaxed) {
                break;
            }
            let mut returned: u32 = 0;
            // SAFETY: 目录句柄由 open_dir 打开；buffer 为有效可变缓冲区；同步模式。
            let result = unsafe {
                ReadDirectoryChangesW(
                    handle,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    false,
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_DIR_NAME
                        | FILE_NOTIFY_CHANGE_SIZE
                        | FILE_NOTIFY_CHANGE_LAST_WRITE,
                    Some(&mut returned),
                    None,
                    None,
                )
            };
            if result.is_err() {
                log::debug!(
                    "成员目录监听中断 {}: {}",
                    dir.display(),
                    windows::core::Error::from_thread()
                );
                break;
            }
            if returned == 0 {
                continue;
            }
            // 防抖：批量删除/粘贴会连续产生通知，合并成一次复测。
            std::thread::sleep(Duration::from_millis(300));
            if tx.send(ProbeCmd::DirChanged(dir.clone())).is_err() {
                break;
            }
        }
        // SAFETY: 句柄由本线程 open_dir 创建并拥有。
        unsafe {
            let _ = CloseHandle(handle);
        }
    }
}

fn open_dir(dir: &Path) -> Option<HANDLE> {
    let path = HSTRING::from(dir.as_os_str());
    // SAFETY: 打开目录句柄用于监听；路径在调用期间有效。
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
            log::debug!("打开成员目录失败 {}: {e}", dir.display());
            None
        }
    }
}
