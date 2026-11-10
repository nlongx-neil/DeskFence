//! 成员路径的存在性判定：区分「已删除」与「暂时不可达」。
//!
//! 盒子只保存路径引用，所以文件可能被外部程序删除，也可能只是因为盘符卸载、
//! 网络共享断开而暂时读不到。若把后者当成删除并清理引用，用户重新插上 U 盘/
//! 连上网络后就会丢失盒子归属。
//!
//! 判据：先 `symlink_metadata`；只有"路径不存在"类错误才继续判定卷是否可用。
//! 卷根可访问 ⇒ 路径确实不存在（已删除）；卷根不可访问 ⇒ 暂时不可达（保留）。
//! 探测本身可能阻塞（死掉的 SMB 共享），因此只能在后台线程调用。

use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathState {
    /// 路径可读（或至少可 stat）。
    Alive,
    /// 卷可用，但路径不存在：已删除/已移走。
    Deleted,
    /// 卷当前不可访问（盘符卸载、网络共享断开、光盘无盘）：不要清理引用。
    Unreachable,
    /// 无法判断（相对路径、设备命名空间、权限不足等）：保留引用。
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeKind {
    /// 盘符卷（`C:\`）。
    Disk,
    /// UNC 共享（`\\server\share`）。
    Unc,
}

/// 解析路径所属卷根：`C:\` 或 `\\server\share`。
pub fn volume_root(path: &Path) -> Option<(PathBuf, VolumeKind)> {
    if let Some(Component::Prefix(prefix)) = path.components().next() {
        return prefix_root(prefix.kind());
    }
    None
}

fn prefix_root(kind: std::path::Prefix<'_>) -> Option<(PathBuf, VolumeKind)> {
    use std::path::Prefix;
    match kind {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            let letter = char::from(letter);
            Some((PathBuf::from(format!("{letter}:\\")), VolumeKind::Disk))
        }
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            let root = PathBuf::from(format!(
                "\\\\{}\\{}",
                server.to_string_lossy(),
                share.to_string_lossy()
            ));
            Some((root, VolumeKind::Unc))
        }
        _ => None,
    }
}

/// 纯分类：`volume_available` 表示卷根当前可访问。
pub fn classify_missing(kind: VolumeKind, volume_available: bool) -> PathState {
    let _ = kind;
    if volume_available {
        PathState::Deleted
    } else {
        PathState::Unreachable
    }
}

/// 探测一个成员路径（可能阻塞，必须在后台线程调用）。
pub fn probe(path: &Path) -> PathState {
    match std::fs::symlink_metadata(path) {
        Ok(_) => PathState::Alive,
        Err(e) if e.kind() == io::ErrorKind::NotFound => match volume_root(path) {
            Some((root, kind)) => {
                let available = std::fs::symlink_metadata(&root).is_ok();
                classify_missing(kind, available)
            }
            None => PathState::Unknown,
        },
        // 权限不足、文件名非法、死共享超时等：一律保留引用。
        Err(_) => PathState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_deleted_only_when_volume_available() {
        assert_eq!(classify_missing(VolumeKind::Disk, true), PathState::Deleted);
        assert_eq!(
            classify_missing(VolumeKind::Disk, false),
            PathState::Unreachable
        );
        assert_eq!(classify_missing(VolumeKind::Unc, true), PathState::Deleted);
        assert_eq!(
            classify_missing(VolumeKind::Unc, false),
            PathState::Unreachable
        );
    }

    #[test]
    fn volume_root_parses_disk_and_unc() {
        let (root, kind) = volume_root(Path::new("C:\\Users\\Neil\\Desktop\\a.txt")).unwrap();
        assert_eq!(root, PathBuf::from("C:\\"));
        assert_eq!(kind, VolumeKind::Disk);

        let (root, kind) = volume_root(Path::new("\\\\server\\share\\dir\\b.txt")).unwrap();
        assert_eq!(root, PathBuf::from("\\\\server\\share"));
        assert_eq!(kind, VolumeKind::Unc);

        // 相对路径没有卷前缀
        assert!(volume_root(Path::new("relative\\x.txt")).is_none());
    }

    #[test]
    fn probe_missing_local_path_is_deleted() {
        let dir = std::env::temp_dir().join(format!("deskkeeper_ps_{}", crate::util::now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        let alive = dir.join("alive.txt");
        std::fs::write(&alive, b"x").unwrap();
        assert_eq!(probe(&alive), PathState::Alive);
        let gone = dir.join("gone.txt");
        // 卷根可访问 ⇒ 判定为已删除
        assert_eq!(probe(&gone), PathState::Deleted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_unmounted_drive_is_unreachable() {
        // Z: 盘在本机通常不存在；若恰好存在则跳过该断言，避免环境相关失败。
        if std::fs::symlink_metadata("Z:\\").is_err() {
            assert_eq!(
                probe(Path::new("Z:\\halfway\\file.txt")),
                PathState::Unreachable
            );
        }
    }
}
