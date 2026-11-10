//! SQLite 存储线程。去重、限额清理、记录查询。

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};

use rusqlite::{params, Connection, OptionalExtension};

use crate::config::ClipFilter;
use crate::events::{BgEvent, EventSink};

/// 一条剪贴板历史记录。
#[derive(Clone, Debug, Default)]
pub struct ClipRecord {
    pub id: i64,
    pub kind: String,
    pub created_at: u64,
    pub pinned: bool,
    pub text: String,
    pub html: Option<String>,
    pub thumb_path: Option<PathBuf>,
    pub image_path: Option<PathBuf>,
    pub file_paths: Vec<PathBuf>,
    pub byte_len: u64,
    pub source: Option<String>,
}

impl ClipRecord {
    /// 列表中的单行预览文本。
    pub fn preview(&self) -> String {
        match self.kind.as_str() {
            "image" => format!("图片（{} KB）", self.byte_len / 1024),
            "files" => {
                let names: Vec<String> = self
                    .file_paths
                    .iter()
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .take(3)
                    .collect();
                let more = self.file_paths.len().saturating_sub(names.len());
                if more > 0 {
                    format!("{} 等 {} 项", names.join("、"), self.file_paths.len())
                } else {
                    names.join("、")
                }
            }
            _ => crate::util::truncate_chars(&self.text.replace(['\r', '\n'], " "), 160),
        }
    }

    pub fn kind_label(&self) -> &'static str {
        match self.kind.as_str() {
            "image" => "图片",
            "files" => "文件",
            _ => "文本",
        }
    }
}

/// 剪贴板盒的显示过滤：类型筛选 + 关键词（预览文本或来源，大小写不敏感）。
pub fn record_matches(rec: &ClipRecord, filter: ClipFilter, needle: &str) -> bool {
    let kind_ok = match filter {
        ClipFilter::All => true,
        ClipFilter::Text => rec.kind != "image" && rec.kind != "files",
        ClipFilter::Image => rec.kind == "image",
        ClipFilter::Files => rec.kind == "files",
    };
    if !kind_ok {
        return false;
    }
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    rec.preview().to_lowercase().contains(&needle)
        || rec
            .source
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains(&needle)
}

/// 待入库的新记录。
#[derive(Clone, Debug, Default)]
pub struct NewClip {
    pub kind: String,
    pub hash: String,
    pub created_at: u64,
    pub text: String,
    pub html: Option<String>,
    /// 原始图片 PNG 字节（仅 image 类型）
    pub png: Option<Vec<u8>>,
    /// 缩略图 PNG 字节（仅 image 类型）
    pub thumb_png: Option<Vec<u8>>,
    pub file_paths: Vec<PathBuf>,
    pub byte_len: u64,
    pub source: Option<String>,
}

pub enum StoreCmd {
    Insert(Box<NewClip>),
    /// 剪贴板盒的基线查询：全部类型、无搜索，盒内再做筛选。
    List {
        limit: usize,
    },
    SetPinned {
        id: i64,
        pinned: bool,
    },
    Delete(i64),
    Clear,
    SetLimits {
        max_items: usize,
        max_bytes: u64,
    },
    Shutdown,
}

/// 存储线程句柄（可克隆，多线程共享同一个通道）。
#[derive(Clone)]
pub struct Store {
    tx: Sender<StoreCmd>,
}

impl Store {
    pub fn spawn(sink: EventSink) -> Self {
        let (tx, rx) = mpsc::channel();
        let db_path = crate::util::db_path();
        let thumbs = crate::util::thumbs_dir();
        std::thread::Builder::new()
            .name("clip-store".into())
            .spawn(move || run_store(rx, sink, db_path, thumbs))
            .ok();
        Self { tx }
    }

    pub fn send(&self, cmd: StoreCmd) -> bool {
        self.tx.send(cmd).is_ok()
    }
}

fn open_db(path: &std::path::Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS clips (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             kind TEXT NOT NULL,
             hash TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             pinned INTEGER NOT NULL DEFAULT 0,
             text TEXT NOT NULL DEFAULT '',
             html TEXT,
             thumb_path TEXT,
             image_path TEXT,
             file_paths TEXT,
             byte_len INTEGER NOT NULL DEFAULT 0,
             source TEXT
         );
         CREATE UNIQUE INDEX IF NOT EXISTS idx_clips_hash ON clips(hash);
         CREATE INDEX IF NOT EXISTS idx_clips_created ON clips(created_at DESC);",
    )?;
    Ok(conn)
}

fn run_store(rx: mpsc::Receiver<StoreCmd>, sink: EventSink, db_path: PathBuf, thumbs: PathBuf) {
    let _ = std::fs::create_dir_all(&thumbs);
    let mut conn = match open_db(&db_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("打开剪贴板数据库失败: {e}");
            sink.emit(BgEvent::Status(format!("剪贴板数据库不可用：{e}")));
            return;
        }
    };
    let mut max_items = 200usize;
    let mut max_bytes = 512u64 * 1024 * 1024;

    for cmd in rx {
        match cmd {
            StoreCmd::Insert(clip) => {
                match insert_clip(&mut conn, &clip, &thumbs, max_items, max_bytes) {
                    Ok(()) => {
                        sink.emit(BgEvent::HistoryChanged);
                    }
                    Err(e) => {
                        log::warn!("写入剪贴板记录失败: {e}");
                        sink.emit(BgEvent::Status(format!("写入剪贴板记录失败：{e}")));
                    }
                }
            }
            StoreCmd::List { limit } => match list_clips(&conn, "", &ClipFilter::All, limit) {
                Ok((records, _total)) => {
                    sink.emit(BgEvent::History { records });
                }
                Err(e) => {
                    log::warn!("查询剪贴板历史失败: {e}");
                    sink.emit(BgEvent::History {
                        records: Vec::new(),
                    });
                }
            },
            StoreCmd::SetPinned { id, pinned } => {
                if let Err(e) = conn.execute(
                    "UPDATE clips SET pinned = ?1 WHERE id = ?2",
                    params![i32::from(pinned), id],
                ) {
                    log::warn!("置顶操作失败: {e}");
                } else {
                    sink.emit(BgEvent::HistoryChanged);
                }
            }
            StoreCmd::Delete(id) => {
                match row_image_paths(&conn, id) {
                    Ok(paths) => {
                        for p in paths {
                            let _ = std::fs::remove_file(p);
                        }
                    }
                    Err(e) => log::warn!("读取待删除记录失败: {e}"),
                }
                if let Err(e) = conn.execute("DELETE FROM clips WHERE id = ?1", params![id]) {
                    log::warn!("删除记录失败: {e}");
                } else {
                    sink.emit(BgEvent::HistoryChanged);
                }
            }
            StoreCmd::Clear => {
                if let Ok(mut stmt) = conn.prepare("SELECT image_path, thumb_path FROM clips") {
                    if let Ok(rows) = stmt.query_map([], |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    }) {
                        for row in rows.flatten() {
                            for p in [row.0, row.1].into_iter().flatten() {
                                let _ = std::fs::remove_file(p);
                            }
                        }
                    }
                }
                if let Err(e) = conn.execute("DELETE FROM clips", []) {
                    log::warn!("清空历史失败: {e}");
                } else {
                    log::info!("剪贴板历史已清空");
                    sink.emit(BgEvent::HistoryChanged);
                }
            }
            StoreCmd::SetLimits {
                max_items: mi,
                max_bytes: mb,
            } => {
                max_items = mi.max(1);
                max_bytes = mb;
                prune(&mut conn, max_items, max_bytes);
                sink.emit(BgEvent::HistoryChanged);
            }
            StoreCmd::Shutdown => break,
        }
    }
}

fn insert_clip(
    conn: &mut Connection,
    clip: &NewClip,
    thumbs: &std::path::Path,
    max_items: usize,
    max_bytes: u64,
) -> rusqlite::Result<()> {
    let existing: Option<(i64, i32)> = conn
        .query_row(
            "SELECT id, pinned FROM clips WHERE hash = ?1",
            params![clip.hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let (image_path, thumb_path) = if let Some(png) = clip.png.as_ref() {
        let _ = std::fs::create_dir_all(thumbs);
        let image = thumbs.join(format!("{}.png", clip.hash));
        if std::fs::write(&image, png).is_err() {
            log::warn!("写入图片文件失败: {}", image.display());
        }
        let thumb = clip.thumb_png.as_ref().map(|t| {
            let p = thumbs.join(format!("{}.thumb.png", clip.hash));
            if std::fs::write(&p, t).is_err() {
                log::warn!("写入缩略图失败: {}", p.display());
            }
            p
        });
        (Some(image), thumb)
    } else {
        (None, None)
    };

    let files: String = clip
        .file_paths
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("\n");

    if let Some((id, _pinned)) = existing {
        conn.execute(
            "UPDATE clips SET created_at = ?1, text = ?2, html = ?3, image_path = ?4,
                 thumb_path = ?5, file_paths = ?6, byte_len = ?7, source = ?8 WHERE id = ?9",
            params![
                clip.created_at as i64,
                clip.text,
                clip.html,
                image_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                thumb_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                files,
                clip.byte_len as i64,
                clip.source,
                id
            ],
        )?;
        log::debug!("更新重复剪贴板记录 #{id}");
    } else {
        conn.execute(
            "INSERT INTO clips (kind, hash, created_at, pinned, text, html, image_path, thumb_path,
                 file_paths, byte_len, source)
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                clip.kind,
                clip.hash,
                clip.created_at as i64,
                clip.text,
                clip.html,
                image_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                thumb_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                files,
                clip.byte_len as i64,
                clip.source
            ],
        )?;
    }
    // 只有可能超出条数/容量上限时才做清理扫描：避免每次插入都跑 OFFSET 全表扫描。
    let byte_cap = max_bytes.min(i64::MAX as u64) as i64;
    let (count, total): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(byte_len), 0) FROM clips WHERE pinned = 0",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if count > max_items as i64 || total > byte_cap {
        prune(conn, max_items, max_bytes);
    }
    Ok(())
}

fn prune(conn: &mut Connection, max_items: usize, max_bytes: u64) {
    let overflow: Vec<(i64, Option<String>, Option<String>)> = {
        let mut stmt = match conn.prepare(
            "SELECT id, image_path, thumb_path FROM clips WHERE pinned = 0
             ORDER BY created_at DESC LIMIT -1 OFFSET ?1",
        ) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("清理超额记录失败: {e}");
                return;
            }
        };
        let rows = stmt.query_map(params![max_items as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        });
        match rows {
            Ok(rows) => rows.flatten().collect(),
            Err(e) => {
                log::warn!("清理超额记录失败: {e}");
                return;
            }
        }
    };
    for (id, image, thumb) in overflow {
        if let Err(e) = conn.execute("DELETE FROM clips WHERE id = ?1", params![id]) {
            log::warn!("删除超额记录失败: {e}");
            continue;
        }
        for p in [image, thumb].into_iter().flatten() {
            let _ = std::fs::remove_file(p);
        }
    }

    // 按总容量继续清理最旧的非置顶记录（u64::MAX 表示不限，需防止 i64 回绕）
    let byte_cap = max_bytes.min(i64::MAX as u64) as i64;
    let mut total: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(byte_len), 0) FROM clips WHERE pinned = 0",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    while total > byte_cap {
        let oldest: Option<(i64, i64, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT id, byte_len, image_path, thumb_path FROM clips WHERE pinned = 0
                 ORDER BY created_at ASC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .unwrap_or(None);
        let Some((id, len, image, thumb)) = oldest else {
            break;
        };
        if let Err(e) = conn.execute("DELETE FROM clips WHERE id = ?1", params![id]) {
            log::warn!("按容量清理失败: {e}");
            break;
        }
        for p in [image, thumb].into_iter().flatten() {
            let _ = std::fs::remove_file(p);
        }
        total -= len;
    }
}

fn row_image_paths(conn: &Connection, id: i64) -> rusqlite::Result<Vec<PathBuf>> {
    let (image, thumb): (Option<String>, Option<String>) = conn.query_row(
        "SELECT image_path, thumb_path FROM clips WHERE id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok([image, thumb]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .collect())
}

fn list_clips(
    conn: &Connection,
    search: &str,
    filter: &ClipFilter,
    limit: usize,
) -> rusqlite::Result<(Vec<ClipRecord>, usize)> {
    let mut where_sql = String::from("WHERE 1=1");
    let kind = match filter {
        ClipFilter::All => None,
        ClipFilter::Text => Some("text"),
        ClipFilter::Image => Some("image"),
        ClipFilter::Files => Some("files"),
    };
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(kind) = kind {
        where_sql.push_str(" AND kind = ?");
        params_vec.push(Box::new(kind.to_string()));
    }
    if !search.trim().is_empty() {
        where_sql.push_str(" AND (text LIKE ? OR file_paths LIKE ?)");
        let pattern = format!("%{}%", search.trim());
        params_vec.push(Box::new(pattern.clone()));
        params_vec.push(Box::new(pattern));
    }
    let total: usize = conn.query_row(
        &format!("SELECT COUNT(*) FROM clips {where_sql}"),
        rusqlite::params_from_iter(params_vec.iter().map(|p| p.as_ref())),
        |row| row.get::<_, i64>(0),
    )? as usize;

    let sql = format!(
        "SELECT id, kind, created_at, pinned, text, html, thumb_path, image_path, file_paths,
                byte_len, source FROM clips {where_sql}
         ORDER BY pinned DESC, created_at DESC LIMIT ?"
    );
    let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = params_vec;
    all_params.push(Box::new(limit as i64));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(all_params.iter().map(|p| p.as_ref())),
        |row| {
            let file_paths: Option<String> = row.get(8)?;
            Ok(ClipRecord {
                id: row.get(0)?,
                kind: row.get(1)?,
                created_at: row.get::<_, i64>(2)? as u64,
                pinned: row.get::<_, i32>(3)? != 0,
                text: row.get(4)?,
                html: row.get(5)?,
                thumb_path: row.get::<_, Option<String>>(6)?.map(PathBuf::from),
                image_path: row.get::<_, Option<String>>(7)?.map(PathBuf::from),
                file_paths: file_paths
                    .map(|s| s.lines().map(PathBuf::from).collect())
                    .unwrap_or_default(),
                byte_len: row.get::<_, i64>(9)? as u64,
                source: row.get(10)?,
            })
        },
    )?;
    let records = rows.flatten().collect();
    Ok((records, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE clips (
                id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, hash TEXT NOT NULL,
                created_at INTEGER NOT NULL, pinned INTEGER NOT NULL DEFAULT 0,
                text TEXT NOT NULL DEFAULT '', html TEXT, thumb_path TEXT, image_path TEXT,
                file_paths TEXT, byte_len INTEGER NOT NULL DEFAULT 0, source TEXT);
             CREATE UNIQUE INDEX idx_clips_hash ON clips(hash);",
        )
        .unwrap();
        conn
    }

    fn new_clip(hash: &str, text: &str, at: u64) -> NewClip {
        NewClip {
            kind: "text".into(),
            hash: hash.into(),
            created_at: at,
            text: text.into(),
            byte_len: text.len() as u64,
            ..Default::default()
        }
    }

    #[test]
    fn dedupe_keeps_single_row_and_updates_time() {
        let mut conn = test_db();
        let thumbs = std::env::temp_dir().join("dk_thumbs_test");
        insert_clip(
            &mut conn,
            &new_clip("h1", "相同内容", 100),
            &thumbs,
            50,
            u64::MAX,
        )
        .unwrap();
        insert_clip(
            &mut conn,
            &new_clip("h1", "相同内容", 200),
            &thumbs,
            50,
            u64::MAX,
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM clips", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let created: i64 = conn
            .query_row("SELECT created_at FROM clips", [], |r| r.get(0))
            .unwrap();
        assert_eq!(created, 200);
    }

    #[test]
    fn prune_enforces_max_items() {
        let mut conn = test_db();
        let thumbs = std::env::temp_dir().join("dk_thumbs_test2");
        for i in 0..6 {
            insert_clip(
                &mut conn,
                &new_clip(&format!("h{i}"), &format!("内容{i}"), i as u64 * 10),
                &thumbs,
                3,
                u64::MAX,
            )
            .unwrap();
        }
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM clips", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
        let newest: i64 = conn
            .query_row("SELECT MAX(created_at) FROM clips", [], |r| r.get(0))
            .unwrap();
        assert_eq!(newest, 50);
    }

    #[test]
    fn prune_skips_pinned() {
        let mut conn = test_db();
        let thumbs = std::env::temp_dir().join("dk_thumbs_test3");
        insert_clip(
            &mut conn,
            &new_clip("old", "旧内容", 1),
            &thumbs,
            10,
            u64::MAX,
        )
        .unwrap();
        conn.execute("UPDATE clips SET pinned = 1", []).unwrap();
        for i in 0..4 {
            insert_clip(
                &mut conn,
                &new_clip(&format!("n{i}"), "新内容", 10 + i as u64),
                &thumbs,
                2,
                u64::MAX,
            )
            .unwrap();
        }
        let pinned: i64 = conn
            .query_row("SELECT COUNT(*) FROM clips WHERE pinned = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(pinned, 1);
    }

    #[test]
    fn search_filters_by_kind_and_text() {
        let mut conn = test_db();
        let thumbs = std::env::temp_dir().join("dk_thumbs_test4");
        insert_clip(
            &mut conn,
            &new_clip("a", "hello world", 1),
            &thumbs,
            50,
            u64::MAX,
        )
        .unwrap();
        let mut image = new_clip("b", "", 2);
        image.kind = "image".into();
        insert_clip(&mut conn, &image, &thumbs, 50, u64::MAX).unwrap();
        let (records, total) = list_clips(&conn, "hello", &ClipFilter::All, 20).unwrap();
        assert_eq!(total, 1);
        assert_eq!(records.len(), 1);
        let (imgs, total_img) = list_clips(&conn, "", &ClipFilter::Image, 20).unwrap();
        assert_eq!(total_img, 1);
        assert_eq!(imgs[0].kind, "image");
    }

    #[test]
    fn bench_1000_records_insert_and_list() {
        let mut conn = test_db();
        let thumbs = std::env::temp_dir().join("dk_thumbs_bench");
        let started = std::time::Instant::now();
        for i in 0..1000u64 {
            let mut clip = new_clip(&format!("bench-{i}"), &format!("条目 {i} 一些文本"), i);
            if i % 10 == 0 {
                clip.kind = "image".into();
                clip.byte_len = 64 * 1024;
            }
            insert_clip(&mut conn, &clip, &thumbs, 2000, 256 * 1024 * 1024).unwrap();
        }
        let insert_ms = started.elapsed().as_secs_f64() * 1000.0;

        let started = std::time::Instant::now();
        let (records, total) = list_clips(&conn, "", &ClipFilter::All, 500).unwrap();
        let list_ms = started.elapsed().as_secs_f64() * 1000.0;

        let started = std::time::Instant::now();
        let (hits, _) = list_clips(&conn, "条目 999", &ClipFilter::All, 500).unwrap();
        let search_ms = started.elapsed().as_secs_f64() * 1000.0;

        eprintln!(
            "BENCH 1000 条：插入 {insert_ms:.1} ms，列表(500) {list_ms:.2} ms，搜索 {search_ms:.2} ms，命中 {}",
            hits.len()
        );
        assert_eq!(records.len(), 500);
        assert_eq!(total, 1000);
    }

    #[test]
    fn record_matches_search_and_filter() {
        let text = ClipRecord {
            kind: "text".into(),
            text: "Hello World".into(),
            source: Some("notepad.exe".into()),
            ..Default::default()
        };
        assert!(record_matches(&text, ClipFilter::All, ""));
        assert!(record_matches(&text, ClipFilter::All, "  "));
        assert!(record_matches(&text, ClipFilter::Text, "hello"));
        assert!(record_matches(&text, ClipFilter::All, "NOTEPAD"));
        assert!(!record_matches(&text, ClipFilter::All, "missing"));

        let file = ClipRecord {
            kind: "files".into(),
            file_paths: vec![PathBuf::from(r"C:\Users\Neil\Desktop\报告.pdf")],
            source: Some("explorer.exe".into()),
            ..Default::default()
        };
        assert!(record_matches(&file, ClipFilter::Files, "报告"));
        assert!(record_matches(&file, ClipFilter::All, "explorer"));
        assert!(!record_matches(&file, ClipFilter::Image, ""));
        assert!(!record_matches(&file, ClipFilter::Text, ""));
    }
}
