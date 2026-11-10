//! 剪贴板监听服务：采样 → 过滤 → 去重哈希 → 入库；以及回填粘贴。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::config::ClipboardConfig;
use crate::events::EventSink;
use crate::platform::clipboard_native::{self, ClipContent};
use crate::platform::input::WindowRef;
use crate::{
    clipboard::store::{ClipRecord, NewClip, Store, StoreCmd},
    util,
};

/// 自己写剪贴板后在这段时间内忽略监听（避免回填造成重复记录）。
const SELF_WRITE_SUPPRESS_MS: u64 = 1500;

static LAST_SELF_WRITE: AtomicU64 = AtomicU64::new(0);

pub struct ClipService {
    signal: std::sync::mpsc::Sender<()>,
    pub paused: Arc<AtomicBool>,
    pub cfg: Arc<RwLock<ClipboardConfig>>,
}

impl ClipService {
    pub fn spawn(
        sink: EventSink,
        store: Store,
        cfg: Arc<RwLock<ClipboardConfig>>,
        paused: Arc<AtomicBool>,
    ) -> Self {
        let (signal, rx) = std::sync::mpsc::channel::<()>();
        let cfg_thread = cfg.clone();
        let paused_thread = paused.clone();
        std::thread::Builder::new()
            .name("clip-capture".into())
            .spawn(move || run_capture(rx, sink, store, cfg_thread, paused_thread))
            .ok();
        Self {
            signal,
            paused,
            cfg,
        }
    }

    /// 收到 WM_CLIPBOARDUPDATE 后调用，投递采样请求。
    pub fn notify(&self) {
        let _ = self.signal.send(());
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }
}

fn run_capture(
    rx: std::sync::mpsc::Receiver<()>,
    _sink: EventSink,
    store: Store,
    cfg: Arc<RwLock<ClipboardConfig>>,
    paused: Arc<AtomicBool>,
) {
    let mut last_hash: Option<String> = None;
    for _ in rx {
        if paused.load(Ordering::Relaxed) {
            continue;
        }
        let cfg = match cfg.read() {
            Ok(c) => c.clone(),
            Err(_) => continue,
        };
        let now_ms = crate::util::now_secs() * 1000;
        if now_ms.saturating_sub(LAST_SELF_WRITE.load(Ordering::Relaxed)) < SELF_WRITE_SUPPRESS_MS {
            log::debug!("忽略自身写入触发的剪贴板更新");
            continue;
        }
        let Some(content) = clipboard_native::read_clipboard() else {
            continue;
        };
        if !capture_allowed(&content, &cfg) {
            log::debug!("剪贴板内容被隐私/长度规则过滤");
            continue;
        }
        let source = clipboard_native::owner_process_name();
        if let Some(proc_name) = source.as_ref() {
            if is_ignored_process(proc_name, &cfg.ignored_processes) {
                log::info!("忽略来自隐私应用 {proc_name} 的剪贴板内容");
                continue;
            }
        }
        if let Some(name) = source.as_ref() {
            if cfg.ignore_password_windows && looks_like_password_manager(name) {
                log::info!("忽略密码类应用 {name} 的剪贴板内容");
                continue;
            }
        }

        let new_clip = match build_clip(content, &cfg, source) {
            Some(c) => c,
            None => continue,
        };
        if last_hash.as_deref() == Some(new_clip.hash.as_str()) {
            continue;
        }
        last_hash = Some(new_clip.hash.clone());
        if !store.send(StoreCmd::Insert(Box::new(new_clip))) {
            log::warn!("剪贴板存储线程已退出");
            break;
        }
    }
}

fn looks_like_password_manager(process: &str) -> bool {
    const HINTS: [&str; 8] = [
        "keepass",
        "1password",
        "bitwarden",
        "lastpass",
        "enpass",
        "dashlane",
        "keeper",
        "roboform",
    ];
    HINTS.iter().any(|h| process.contains(h))
}

fn is_ignored_process(process: &str, list: &[String]) -> bool {
    let process = process.trim().to_lowercase();
    let process = process.strip_suffix(".exe").unwrap_or(&process);
    list.iter().any(|p| {
        let p = p.trim().to_lowercase();
        let p = p.strip_suffix(".exe").unwrap_or(&p);
        // 精确匹配进程名，避免 "code.exe" 误命中 "vscode.exe"。
        !p.is_empty() && process == p
    })
}

fn capture_allowed(content: &ClipContent, cfg: &ClipboardConfig) -> bool {
    match content {
        ClipContent::Text { text, .. } => {
            let len = text.chars().count();
            if len > cfg.max_text_len {
                log::debug!("跳过超长文本（{len} 字符 > {}）", cfg.max_text_len);
                return false;
            }
            !text.trim().is_empty()
        }
        ClipContent::Image { .. } => cfg.capture_images,
        ClipContent::Files(paths) => cfg.capture_files && !paths.is_empty(),
    }
}

fn build_clip(
    content: ClipContent,
    cfg: &ClipboardConfig,
    source: Option<String>,
) -> Option<NewClip> {
    match content {
        ClipContent::Text { text, html } => {
            let kind = "text";
            let hash = content_hash(kind, &text);
            let byte_len = text.len() as u64;
            if byte_len > cfg.max_bytes {
                return None;
            }
            Some(NewClip {
                kind: kind.to_string(),
                hash,
                created_at: util::now_secs(),
                text,
                html,
                byte_len,
                source,
                ..Default::default()
            })
        }
        ClipContent::Image {
            width,
            height,
            rgba,
        } => {
            let img = image::RgbaImage::from_raw(width, height, rgba)?;
            let mut png = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .ok()?;
            let thumb_img = image::imageops::thumbnail(&img, 160, 160);
            let mut thumb = Vec::new();
            thumb_img
                .write_to(
                    &mut std::io::Cursor::new(&mut thumb),
                    image::ImageFormat::Png,
                )
                .ok()?;
            // 哈希必须基于真实像素内容，否则尺寸相同的不同图片会被误判为重复。
            let hash = format!("img-{}", content_hash_bytes("image", &png));
            Some(NewClip {
                kind: "image".to_string(),
                hash,
                created_at: util::now_secs(),
                text: String::new(),
                html: None,
                png: Some(png.clone()),
                thumb_png: Some(thumb),
                byte_len: png.len() as u64,
                source,
                ..Default::default()
            })
        }
        ClipContent::Files(paths) => {
            if paths.is_empty() {
                return None;
            }
            let joined = paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            let hash = content_hash("files", &joined);
            let byte_len: u64 = paths
                .iter()
                .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
                .sum();
            Some(NewClip {
                kind: "files".to_string(),
                hash,
                created_at: util::now_secs(),
                text: joined.clone(),
                html: None,
                file_paths: paths,
                byte_len,
                source,
                ..Default::default()
            })
        }
    }
}

/// 内容哈希：类型 + 内容（图片使用像素尺寸+PNG 长度）→ 取 32 位十六进制。
fn content_hash(kind: &str, content: &str) -> String {
    content_hash_bytes(kind, content.as_bytes())
}

/// 字节级内容哈希（图片等二进制内容使用）。
fn content_hash_bytes(kind: &str, content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(content);
    let digest = hasher.finalize();
    format!("{:x}", digest)[..32].to_string()
}

pub fn mark_self_write() {
    LAST_SELF_WRITE.store(util::now_secs() * 1000, Ordering::Relaxed);
}

impl Clone for ClipService {
    fn clone(&self) -> Self {
        Self {
            signal: self.signal.clone(),
            paused: self.paused.clone(),
            cfg: self.cfg.clone(),
        }
    }
}

/// 把记录写回剪贴板；`auto_paste` 为 true 时模拟 Ctrl+V 粘贴到之前的窗口。
pub fn paste_record(
    record: &ClipRecord,
    auto_paste: bool,
    previous: WindowRef,
) -> anyhow::Result<()> {
    mark_self_write();
    let ok = match record.kind.as_str() {
        "image" => {
            let path = record
                .image_path
                .clone()
                .or_else(|| record.thumb_path.clone())
                .ok_or_else(|| anyhow::anyhow!("图片记录缺少文件"))?;
            let img = image::open(&path)?.to_rgba8();
            clipboard_native::write_image(img.width(), img.height(), img.as_raw())
        }
        "files" => clipboard_native::write_files(&record.file_paths),
        _ => clipboard_native::write_text(&record.text, record.html.as_deref()),
    };
    if !ok {
        return Err(anyhow::anyhow!("写入剪贴板失败"));
    }
    if auto_paste {
        // 调用方在双击时采集目标前台窗口；这里先恢复焦点，再模拟 Ctrl+V。
        previous.restore();
        std::thread::sleep(Duration::from_millis(80));
        crate::platform::input::send_ctrl_v();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_distinct() {
        let a = content_hash("text", "hello");
        let b = content_hash("text", "hello");
        let c = content_hash("text", "hello!");
        let d = content_hash("files", "hello");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn ignored_process_matching() {
        let list = vec!["KeePass.exe".to_string(), "1password.exe".to_string()];
        assert!(is_ignored_process("keepass.exe", &list));
        assert!(is_ignored_process("1password.exe", &list));
        assert!(!is_ignored_process("chrome.exe", &list));
        assert!(!is_ignored_process("x", &[]));
    }

    #[test]
    fn password_manager_detection() {
        assert!(looks_like_password_manager("keepassxc.exe"));
        assert!(looks_like_password_manager("bitwarden.exe"));
        assert!(!looks_like_password_manager("notepad.exe"));
    }

    #[test]
    fn long_text_is_rejected() {
        let cfg = ClipboardConfig {
            max_text_len: 20,
            ..Default::default()
        };
        let content = ClipContent::Text {
            text: "一二三四五六七八九十一二三四五六七八九十一".into(),
            html: None,
        };
        assert!(!capture_allowed(&content, &cfg));
        let short = ClipContent::Text {
            text: "你好".into(),
            html: None,
        };
        assert!(capture_allowed(&short, &cfg));
    }

    #[test]
    fn empty_text_is_rejected() {
        let cfg = ClipboardConfig::default();
        let content = ClipContent::Text {
            text: "   \n".into(),
            html: None,
        };
        assert!(!capture_allowed(&content, &cfg));
    }
}
