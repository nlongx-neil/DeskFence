//! 文件移动/复制的安全策略与撤销栈。
//! 策略：同盘优先 `rename`；`rename` 因跨卷失败时退化为"复制 → 校验 → 删除源"。

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum FileOpError {
    Io(io::Error),
    NotFound(PathBuf),
    VerifyFailed(PathBuf),
}

impl fmt::Display for FileOpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileOpError::Io(e) => write!(f, "文件操作失败：{e}"),
            FileOpError::NotFound(p) => write!(f, "找不到文件：{}", p.display()),
            FileOpError::VerifyFailed(p) => write!(f, "复制校验失败：{}", p.display()),
        }
    }
}

impl std::error::Error for FileOpError {}

impl From<io::Error> for FileOpError {
    fn from(e: io::Error) -> Self {
        FileOpError::Io(e)
    }
}

/// 在目标目录内生成不冲突的文件名（`name (2).ext`）。
pub fn unique_target(dir: &Path, name: &OsStr) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let name = name.to_string_lossy();
    let (stem, ext) = match name.rfind('.') {
        Some(idx) if idx > 0 => (&name[..idx], &name[idx..]),
        _ => (&name[..], ""),
    };
    for n in 2..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // 极端情况下退化为时间戳
    dir.join(format!(
        "{stem} ({}){ext}",
        crate::util::now_secs() % 100_000
    ))
}

fn is_cross_device(e: &io::Error) -> bool {
    // ERROR_NOT_SAME_DEVICE = 17
    e.raw_os_error() == Some(17)
}

/// 把 `src` 移入 `dest_dir`，返回最终路径。源已在目标目录时直接返回。
pub fn move_into(src: &Path, dest_dir: &Path) -> Result<PathBuf, FileOpError> {
    move_into_impl(src, dest_dir, false)
}

/// 显式拷贝模式（用于测试跨卷退化路径）。
pub fn move_into_impl(
    src: &Path,
    dest_dir: &Path,
    force_copy: bool,
) -> Result<PathBuf, FileOpError> {
    if !src.exists() {
        return Err(FileOpError::NotFound(src.to_path_buf()));
    }
    if let Some(parent) = src.parent() {
        if parent == dest_dir {
            return Ok(src.to_path_buf());
        }
    }
    std::fs::create_dir_all(dest_dir)?;
    let name = src
        .file_name()
        .ok_or_else(|| FileOpError::NotFound(src.to_path_buf()))?;
    let target = unique_target(dest_dir, name);

    if !force_copy {
        match std::fs::rename(src, &target) {
            Ok(()) => {
                log::info!("移动 {} -> {}", src.display(), target.display());
                return Ok(target);
            }
            Err(e) if is_cross_device(&e) => {
                log::info!("跨卷移动，退化为复制+删除: {} ({e})", src.display());
            }
            Err(e) => return Err(FileOpError::Io(e)),
        }
    }

    copy_path(src, &target)?;
    if !verify_copy(src, &target) {
        let _ = remove_path(&target);
        return Err(FileOpError::VerifyFailed(target));
    }
    remove_path(src)?;
    log::info!("复制移动完成 {} -> {}", src.display(), target.display());
    Ok(target)
}

/// 递归复制（文件或目录）。
pub fn copy_path(src: &Path, dst: &Path) -> Result<(), FileOpError> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let child_dst = dst.join(entry.file_name());
            copy_path(&entry.path(), &child_dst)?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// 校验复制结果（比较文件数量与字节数）。
pub fn verify_copy(src: &Path, dst: &Path) -> bool {
    fn stats(p: &Path) -> Option<(usize, u64)> {
        let meta = std::fs::symlink_metadata(p).ok()?;
        if meta.is_dir() {
            let mut count = 0;
            let mut bytes = 0;
            for entry in std::fs::read_dir(p).ok()? {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => return None,
                };
                let (c, b) = stats(&entry.path())?;
                count += c;
                bytes += b;
            }
            Some((count, bytes))
        } else {
            Some((1, meta.len()))
        }
    }
    match (stats(src), stats(dst)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// 递归删除（不经过回收站，仅供内部清理/撤销失败时回滚）。
pub fn remove_path(path: &Path) -> Result<(), FileOpError> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// 一次成功移动的撤销记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRecord {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// 有上限的撤销栈（最近的操作在末尾）。
#[derive(Debug)]
pub struct UndoStack {
    records: Vec<MoveRecord>,
    capacity: usize,
}

impl UndoStack {
    pub fn new(capacity: usize) -> Self {
        Self {
            records: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&mut self, record: MoveRecord) {
        self.records.push(record);
        if self.records.len() > self.capacity {
            self.records.remove(0);
        }
    }

    pub fn pop(&mut self) -> Option<MoveRecord> {
        self.records.pop()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// 执行撤销（把文件移回原位）。失败时记录会被丢弃并返回错误。
    pub fn undo_last(&mut self) -> Option<Result<PathBuf, FileOpError>> {
        let rec = self.pop()?;
        Some(undo_record(&rec))
    }
}

pub fn undo_record(rec: &MoveRecord) -> Result<PathBuf, FileOpError> {
    if !rec.to.exists() {
        return Err(FileOpError::NotFound(rec.to.clone()));
    }
    let parent = rec
        .from
        .parent()
        .ok_or_else(|| FileOpError::NotFound(rec.from.clone()))?;
    let name = rec
        .from
        .file_name()
        .ok_or_else(|| FileOpError::NotFound(rec.from.clone()))?;
    let target = unique_target(parent, name);
    move_into_impl(&rec.to, parent, false).map(|_| target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("deskkeeper_fo_{tag}_{}", crate::util::now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unique_target_avoids_collision() {
        let dir = temp_dir("unique");
        std::fs::write(dir.join("a.txt"), b"1").unwrap();
        let t = unique_target(&dir, OsStr::new("a.txt"));
        assert_eq!(t.file_name().unwrap(), "a (2).txt");
        std::fs::write(&t, b"2").unwrap();
        let t2 = unique_target(&dir, OsStr::new("a.txt"));
        assert_eq!(t2.file_name().unwrap(), "a (3).txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_into_same_dir_is_noop() {
        let dir = temp_dir("same");
        let file = dir.join("x.txt");
        std::fs::write(&file, b"hi").unwrap();
        let out = move_into(&file, &dir).unwrap();
        assert_eq!(out, file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_file_between_dirs() {
        let src = temp_dir("src");
        let dst = temp_dir("dst");
        let file = src.join("报告.txt");
        std::fs::write(&file, b"data").unwrap();
        let moved = move_into(&file, &dst).unwrap();
        assert!(moved.exists());
        assert!(!file.exists());
        assert_eq!(std::fs::read(&moved).unwrap(), b"data");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn copy_fallback_moves_directory() {
        let src = temp_dir("csrc");
        let dst = temp_dir("cdst");
        let sub = src.join("folder");
        std::fs::create_dir_all(sub.join("nested")).unwrap();
        std::fs::write(sub.join("a.txt"), b"aaa").unwrap();
        std::fs::write(sub.join("nested").join("b.txt"), b"bbbb").unwrap();
        let moved = move_into_impl(&sub, &dst, true).unwrap();
        assert!(moved.join("a.txt").exists());
        assert!(moved.join("nested").join("b.txt").exists());
        assert!(!sub.exists());
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn undo_restores_original_location() {
        let src = temp_dir("usrc");
        let dst = temp_dir("udst");
        let file = src.join("note.txt");
        std::fs::write(&file, b"n").unwrap();
        let moved = move_into(&file, &dst).unwrap();
        let rec = MoveRecord {
            from: file.clone(),
            to: moved.clone(),
        };
        let mut stack = UndoStack::new(4);
        stack.push(rec);
        stack.undo_last().unwrap().unwrap();
        assert!(file.exists());
        assert!(!moved.exists());
        assert!(stack.is_empty());
        // 撤销后放入同名新文件，恢复时自动改名
        std::fs::write(&file, b"new").unwrap();
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn undo_stack_drops_oldest() {
        let mut stack = UndoStack::new(2);
        for i in 0..3 {
            stack.push(MoveRecord {
                from: PathBuf::from(format!("a{i}")),
                to: PathBuf::from(format!("b{i}")),
            });
        }
        assert_eq!(stack.len(), 2);
        let top = stack.pop().unwrap();
        assert_eq!(top.from, PathBuf::from("a2"));
    }
}
