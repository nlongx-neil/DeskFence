//! 旧盒子目录 → 虚拟路径引用的一次性迁移。
//!
//! 旧版把桌面文件真实搬进 `%LOCALAPPDATA%\DeskKeeper\boxes\<盒子 id>`。改为虚拟引用后，
//! 这些文件需要安全搬回桌面并把盒子成员改成桌面真实路径。迁移分两步：
//! `plan` 只读扫描并输出逐文件 SHA256（dry-run），`apply` 才真正搬运。
//! 任何一步失败都必须保留源文件；绝不覆盖同名目标（冲突时用 ` (2)` 后缀）。

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::{AppConfig, BoxConfig};
use crate::model;

/// 一个待搬回的顶层条目（文件或目录）。
#[derive(Clone, Debug)]
pub struct PlannedMove {
    pub box_id: String,
    pub source: PathBuf,
    pub target: PathBuf,
    /// 目标已存在，已改用 ` (2)` 等后缀
    pub conflict: bool,
    pub files: usize,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Default)]
pub struct MigrationPlan {
    pub entries: Vec<PlannedMove>,
    pub warnings: Vec<String>,
}

impl MigrationPlan {
    pub fn total_files(&self) -> usize {
        self.entries.iter().map(|e| e.files).sum()
    }
}

/// 单项搬运结果。
#[derive(Clone, Debug)]
pub struct MoveOutcome {
    pub source: PathBuf,
    pub target: Option<PathBuf>,
    pub ok: bool,
    pub sha_before: String,
    pub sha_after: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct MigrationReport {
    pub outcomes: Vec<MoveOutcome>,
    pub removed_dirs: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub updated_members: usize,
}

impl MigrationReport {
    pub fn succeeded(&self) -> usize {
        self.outcomes.iter().filter(|o| o.ok).count()
    }

    pub fn failed(&self) -> usize {
        self.outcomes.iter().filter(|o| !o.ok).count()
    }
}

/// 启动时：成员为空的旧盒子从遗留目录接管顶层条目为路径引用（不移动文件）。
///
/// 返回接管的引用数量。旧配置因此仍能正常显示盒子内容，等待迁移把文件搬回桌面。
pub fn seed_legacy_members(cfg: &mut AppConfig, boxes_dir: &Path) -> usize {
    let mut total = 0;
    for b in &mut cfg.boxes {
        if !b.members.is_empty() {
            continue;
        }
        let Ok(rd) = fs::read_dir(boxes_dir.join(&b.id)) else {
            continue;
        };
        let mut paths: Vec<String> = rd
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| e.path().to_string_lossy().to_string())
            .collect();
        paths.sort();
        if !paths.is_empty() {
            total += paths.len();
            b.members = paths;
        }
    }
    total
}

/// 只读规划：扫描每个盒子的遗留目录，生成搬回桌面的清单（含冲突改名与源 SHA256）。
pub fn plan(boxes_dir: &Path, desktop: &Path, boxes: &[BoxConfig]) -> MigrationPlan {
    let mut plan = MigrationPlan::default();
    for b in boxes {
        let dir = boxes_dir.join(&b.id);
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        let mut sources: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        sources.sort();
        for source in sources {
            let Some(name) = source.file_name() else {
                continue;
            };
            if name.to_string_lossy().eq_ignore_ascii_case("desktop.ini") {
                plan.warnings
                    .push(format!("跳过系统文件：{}", source.display()));
                continue;
            }
            let base = desktop.join(name);
            let conflict = base.exists();
            let target = if conflict {
                crate::fileops::unique_target(desktop, name)
            } else {
                base
            };
            match hash_path(&source) {
                Ok((files, bytes, sha256)) => plan.entries.push(PlannedMove {
                    box_id: b.id.clone(),
                    source,
                    target,
                    conflict,
                    files,
                    bytes,
                    sha256,
                }),
                Err(e) => plan.warnings.push(format!(
                    "跳过 {}：无法读取或计算哈希（{e}）",
                    source.display()
                )),
            }
        }
    }
    plan
}

/// 执行规划：逐项搬回并校验 SHA256，然后更新成员引用、删除已空的遗留目录。
pub fn apply(plan: &MigrationPlan, cfg: &mut AppConfig, _desktop: &Path) -> MigrationReport {
    let mut report = MigrationReport {
        warnings: plan.warnings.clone(),
        ..Default::default()
    };
    for entry in &plan.entries {
        if !entry.source.exists() {
            report.outcomes.push(MoveOutcome {
                source: entry.source.clone(),
                target: None,
                ok: false,
                sha_before: entry.sha256.clone(),
                sha_after: None,
                error: Some("源文件已不存在，跳过（不删除任何内容）".into()),
            });
            continue;
        }
        let sha_before = match hash_path(&entry.source) {
            Ok((_, _, sha)) => sha,
            Err(e) => {
                report.outcomes.push(MoveOutcome {
                    source: entry.source.clone(),
                    target: None,
                    ok: false,
                    sha_before: entry.sha256.clone(),
                    sha_after: None,
                    error: Some(format!("搬运前读取失败，保持源文件不动：{e}")),
                });
                continue;
            }
        };
        if sha_before != entry.sha256 {
            report.warnings.push(format!(
                "{} 内容在 dry-run 后发生变化，已按最新内容重新校验",
                entry.source.display()
            ));
        }
        if let Some(parent) = entry.target.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                report.outcomes.push(MoveOutcome {
                    source: entry.source.clone(),
                    target: None,
                    ok: false,
                    sha_before,
                    sha_after: None,
                    error: Some(format!("创建目标目录失败，保留源文件：{e}")),
                });
                continue;
            }
        }
        match move_verified(&entry.source, &entry.target, &sha_before) {
            Ok(final_path) => {
                let sha_after = hash_path(&final_path).ok().map(|(_, _, sha)| sha);
                let changed = update_members(cfg, &entry.box_id, &entry.source, &final_path);
                if changed {
                    report.updated_members += 1;
                }
                report.outcomes.push(MoveOutcome {
                    source: entry.source.clone(),
                    target: Some(final_path),
                    ok: true,
                    sha_before,
                    sha_after,
                    error: None,
                });
            }
            Err(err) => report.outcomes.push(MoveOutcome {
                source: entry.source.clone(),
                target: None,
                ok: false,
                sha_before,
                sha_after: None,
                error: Some(err),
            }),
        }
    }

    // 只删除确认已空的遗留盒目录；仍有内容则保留并告警。
    let ids: HashSet<&str> = plan.entries.iter().map(|e| e.box_id.as_str()).collect();
    for id in ids {
        let Some(dir) = boxes_dir_root(plan, id) else {
            continue;
        };
        if !dir.is_dir() {
            continue;
        }
        match fs::read_dir(&dir) {
            Ok(mut rd) => {
                if rd.next().is_none() {
                    match fs::remove_dir(&dir) {
                        Ok(()) => report.removed_dirs.push(dir),
                        Err(e) => report
                            .warnings
                            .push(format!("删除空目录失败 {}：{e}", dir.display())),
                    }
                } else {
                    report
                        .warnings
                        .push(format!("遗留目录仍有内容，已保留：{}", dir.display()));
                }
            }
            Err(e) => report
                .warnings
                .push(format!("读取遗留目录失败 {}：{e}", dir.display())),
        }
    }
    report
}

/// 从规划条目推断盒子根目录：条目源路径的父目录即 `boxes\<id>`。
fn boxes_dir_root(plan: &MigrationPlan, id: &str) -> Option<PathBuf> {
    plan.entries
        .iter()
        .find(|e| e.box_id == id)
        .and_then(|e| e.source.parent())
        .map(Path::to_path_buf)
}

/// 校验搬运后 SHA256；失败时还原/删除副本，源文件绝不丢失。
fn move_verified(source: &Path, target: &Path, expected: &str) -> Result<PathBuf, String> {
    match fs::rename(source, target) {
        Ok(()) => match hash_path(target) {
            Ok((_, _, sha)) if sha == expected => Ok(target.to_path_buf()),
            _ => {
                let _ = fs::rename(target, source);
                Err("搬回后 SHA256 不一致，已尝试还原源文件".into())
            }
        },
        Err(e) if e.raw_os_error() == Some(17) => {
            crate::fileops::copy_path(source, target)
                .map_err(|e| format!("跨盘复制失败，源文件保留：{e}"))?;
            let same = hash_path(target)
                .map(|(_, _, sha)| sha == expected)
                .unwrap_or(false);
            if !same {
                let _ = crate::fileops::remove_path(target);
                return Err("跨盘复制后 SHA256 不一致，已删除副本并保留源文件".into());
            }
            if let Err(e) = crate::fileops::remove_path(source) {
                return Err(format!("复制校验通过但删除源失败（两份均保留）：{e}"));
            }
            Ok(target.to_path_buf())
        }
        Err(e) => Err(format!("同盘 rename 失败，源文件保留：{e}")),
    }
}

/// 把盒子成员从旧路径改为新路径；同步手动覆盖记录。返回是否有改动。
fn update_members(cfg: &mut AppConfig, box_id: &str, source: &Path, target: &Path) -> bool {
    let src_key = model::path_key(source);
    let dst = target.to_string_lossy().to_string();
    let dst_key = model::path_key(target);
    let mut changed = false;
    for b in &mut cfg.boxes {
        if b.id != box_id {
            continue;
        }
        let before = b.members.len();
        b.members
            .retain(|m| model::path_key(Path::new(m)) != src_key);
        if !b
            .members
            .iter()
            .any(|m| model::path_key(Path::new(m)) == dst_key)
        {
            b.members.push(dst.clone());
        }
        changed |= b.members.len() != before;
    }
    for m in &mut cfg.desktop.manual_overrides {
        if model::path_key(Path::new(m)) == src_key {
            *m = dst.clone();
            changed = true;
        }
    }
    changed
}

fn hash_path(path: &Path) -> std::io::Result<(usize, u64, String)> {
    let mut hasher = Sha256::new();
    let mut files = 0usize;
    let mut bytes = 0u64;
    hash_into(path, Path::new(""), &mut hasher, &mut files, &mut bytes)?;
    Ok((files, bytes, format!("{:x}", hasher.finalize())))
}

fn hash_into(
    path: &Path,
    rel: &Path,
    hasher: &mut Sha256,
    files: &mut usize,
    bytes: &mut u64,
) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        hasher.update(b"D");
        hasher.update(rel.to_string_lossy().as_bytes());
        let mut entries: Vec<fs::DirEntry> = fs::read_dir(path)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            hash_into(
                &entry.path(),
                &rel.join(entry.file_name()),
                hasher,
                files,
                bytes,
            )?;
        }
    } else {
        hasher.update(b"F");
        hasher.update(rel.to_string_lossy().as_bytes());
        *files += 1;
        *bytes += meta.len();
        let mut file = fs::File::open(path)?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }
    Ok(())
}

/// dry-run 报告：数量、逐项 SHA256、冲突改名与告警；明确说明不移动文件。
pub fn format_plan(plan: &MigrationPlan, desktop: &Path) -> String {
    let mut out = String::new();
    out.push_str("== 盒子虚拟引用迁移（dry-run，只读，不移动文件） ==\n");
    out.push_str(&format!("桌面目录：{}\n", desktop.display()));
    out.push_str(&format!(
        "待搬回：{} 项（{} 个文件，{} 字节）\n",
        plan.entries.len(),
        plan.total_files(),
        plan.entries.iter().map(|e| e.bytes).sum::<u64>()
    ));
    for (i, e) in plan.entries.iter().enumerate() {
        out.push_str(&format!(
            "[{}] {}：{} -> {}{}\n    文件 {} 个，{} 字节，SHA256 {}\n",
            i + 1,
            e.box_id,
            e.source.display(),
            e.target.display(),
            if e.conflict {
                "（同名冲突，已改名）"
            } else {
                ""
            },
            e.files,
            e.bytes,
            e.sha256
        ));
    }
    for w in &plan.warnings {
        out.push_str(&format!("告警：{w}\n"));
    }
    if plan.entries.is_empty() {
        out.push_str("没有需要搬回的文件。\n");
    }
    out.push_str("真实执行命令：deskkeeper --migrate-boxes（执行前请退出 DeskKeeper）\n");
    out
}

/// 真实执行后的报告：逐项前后 SHA256 对比与失败原因。
pub fn format_report(report: &MigrationReport) -> String {
    let mut out = String::new();
    out.push_str("== 迁移执行结果 ==\n");
    out.push_str(&format!(
        "成功 {} 项，失败 {} 项，更新成员引用 {} 处\n",
        report.succeeded(),
        report.failed(),
        report.updated_members
    ));
    for o in &report.outcomes {
        let status = if o.ok { "OK" } else { "FAILED" };
        out.push_str(&format!(
            "[{}] {} -> {}\n    前 SHA256 {}\n    后 SHA256 {}\n",
            status,
            o.source.display(),
            o.target
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "（未移动）".into()),
            o.sha_before,
            o.sha_after.as_deref().unwrap_or("（未生成）")
        ));
        if let Some(e) = &o.error {
            out.push_str(&format!("    错误：{e}\n"));
        }
    }
    for d in &report.removed_dirs {
        out.push_str(&format!("已删除空目录：{}\n", d.display()));
    }
    for w in &report.warnings {
        out.push_str(&format!("告警：{w}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BoxConfig;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deskkeeper_mig_{tag}_{}_{}",
            crate::util::now_secs(),
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cfg_with_box(id: &str, members: Vec<String>) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.boxes.push(BoxConfig {
            id: id.into(),
            members,
            ..Default::default()
        });
        cfg
    }

    #[test]
    fn plan_uses_suffix_on_conflict() {
        let boxes = temp_dir("boxes");
        let desktop = temp_dir("desk");
        let box_dir = boxes.join("box-a");
        std::fs::create_dir_all(&box_dir).unwrap();
        std::fs::write(box_dir.join("报告.txt"), b"new").unwrap();
        std::fs::write(desktop.join("报告.txt"), b"old").unwrap();
        let cfg = cfg_with_box("box-a", vec![]);
        let plan = plan(&boxes, &desktop, &cfg.boxes);
        assert_eq!(plan.entries.len(), 1);
        assert!(plan.entries[0].conflict);
        assert_eq!(plan.entries[0].target.file_name().unwrap(), "报告 (2).txt");
        // dry-run 不改动任何文件。
        assert!(box_dir.join("报告.txt").exists());
        assert_eq!(std::fs::read(desktop.join("报告.txt")).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&boxes);
        let _ = std::fs::remove_dir_all(&desktop);
    }

    #[test]
    fn seed_legacy_members_collects_top_level_entries() {
        let boxes = temp_dir("seed");
        let box_dir = boxes.join("box-a");
        std::fs::create_dir_all(box_dir.join("子目录")).unwrap();
        std::fs::write(box_dir.join("a.txt"), b"a").unwrap();
        let mut cfg = cfg_with_box("box-a", vec![]);
        assert_eq!(seed_legacy_members(&mut cfg, &boxes), 2);
        assert_eq!(cfg.boxes[0].members.len(), 2);
        let _ = std::fs::remove_dir_all(&boxes);
    }

    #[test]
    fn apply_moves_back_updates_members_and_keeps_hash() {
        let boxes = temp_dir("applyboxes");
        let desktop = temp_dir("applydesk");
        let box_dir = boxes.join("box-a");
        std::fs::create_dir_all(&box_dir).unwrap();
        let source = box_dir.join("a.txt");
        std::fs::write(&source, b"payload").unwrap();
        let mut cfg = cfg_with_box("box-a", vec![source.to_string_lossy().to_string()]);
        let plan = plan(&boxes, &desktop, &cfg.boxes);
        let before = plan.entries[0].sha256.clone();
        let report = apply(&plan, &mut cfg, &desktop);
        assert_eq!(report.succeeded(), 1);
        assert_eq!(report.failed(), 0);
        let target = desktop.join("a.txt");
        assert!(target.exists());
        assert!(!source.exists());
        let (_, _, after) = hash_path(&target).unwrap();
        assert_eq!(before, after);
        assert_eq!(cfg.boxes[0].members.len(), 1);
        assert_eq!(
            model::path_key(Path::new(&cfg.boxes[0].members[0])),
            model::path_key(&target)
        );
        let _ = std::fs::remove_dir_all(&boxes);
        let _ = std::fs::remove_dir_all(&desktop);
    }
}
