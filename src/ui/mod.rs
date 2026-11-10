//! 界面层：主题、图标纹理缓存、设置页。

pub mod settings;
pub mod theme;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crate::platform::icons::{rgba_to_color_image, IconResponse, IconTask};

/// 图标/缩略图纹理缓存：避免在绘制循环里重复创建纹理，也避免重复请求。
#[derive(Default)]
pub struct TextureCache {
    textures: HashMap<String, egui::TextureHandle>,
    pending: HashSet<String>,
    failed: HashSet<String>,
    /// 每个文件已缓存过哪些图标的物理尺寸，用于尺寸切换期间找同路径的旧纹理临时占位。
    icon_sizes: HashMap<(String, String), Vec<u32>>,
    /// 缩略图最近使用时间戳（LRU 淘汰，键格式 `thumb:<size>:<mtime>:<path>`）。
    thumb_use: HashMap<String, u64>,
    thumb_clock: u64,
}

/// 缩略图缓存上限（按条目数，超出后淘汰最久未使用的纹理）。
const THUMB_CACHE_LIMIT: usize = 512;

impl TextureCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn texture_id(&self, key: &str) -> Option<egui::TextureId> {
        self.textures.get(key).map(|t| t.id())
    }

    pub fn insert_response(&mut self, ctx: &egui::Context, resp: &IconResponse) {
        self.pending.remove(&resp.key);
        if resp.rgba.is_empty() {
            if self.failed.len() < 4096 {
                self.failed.insert(resp.key.clone());
            }
            return;
        }
        let Some(image) = rgba_to_color_image(resp.width, resp.height, &resp.rgba) else {
            self.failed.insert(resp.key.clone());
            return;
        };
        if self.textures.len() > 2048 {
            // 极端情况下限制缓存规模，避免长期运行后显存无限增长
            self.textures.clear();
            self.icon_sizes.clear();
            self.thumb_use.clear();
        }
        let handle = ctx.load_texture(&resp.key, image, egui::TextureOptions::LINEAR);
        log::debug!(
            "图标纹理上传: {}（缓存共 {} 项）",
            resp.key,
            self.textures.len() + 1
        );
        if let Some((kind, path, size)) = parse_icon_key(&resp.key) {
            let sizes = self
                .icon_sizes
                .entry((kind.to_owned(), path.to_owned()))
                .or_default();
            if !sizes.contains(&size) {
                sizes.push(size);
            }
        }
        self.textures.insert(resp.key.clone(), handle);
        if resp.key.starts_with("thumb:") {
            self.touch_thumb(&resp.key);
            self.evict_thumbs();
        }
    }

    /// 记录缩略图本次使用，供 LRU 淘汰判断（可见项每帧都会刷新）。
    fn touch_thumb(&mut self, key: &str) {
        self.thumb_clock += 1;
        let tick = self.thumb_clock;
        self.thumb_use.insert(key.to_owned(), tick);
    }

    /// 超出上限时淘汰最久未使用的缩略图纹理；正在显示的行每帧都会 touch，不会被误淘汰。
    fn evict_thumbs(&mut self) {
        while self.thumb_use.len() > THUMB_CACHE_LIMIT {
            let Some(victim) = self
                .thumb_use
                .iter()
                .min_by_key(|(_, tick)| *tick)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.thumb_use.remove(&victim);
            self.textures.remove(&victim);
        }
    }

    /// 确保 Shell 图标已请求；返回可用的纹理 id。
    ///
    /// 目标尺寸的纹理未就绪时，先返回同一路径其它尺寸的旧纹理让界面立即有图（
    /// 绘制端会临时拉伸），后台再解码新尺寸并替换，避免滑杆拖动时图标闪烁/消失。
    pub fn ensure_shell_icon(
        &mut self,
        tx: &Sender<IconTask>,
        key: String,
        path: PathBuf,
        is_dir: bool,
        size: u32,
        system: Option<crate::model::SystemItem>,
    ) -> Option<egui::TextureId> {
        if let Some(id) = self.texture_id(&key) {
            return Some(id);
        }
        if !self.pending.contains(&key) && !self.failed.contains(&key) {
            let task = IconTask::Shell {
                key: key.clone(),
                path,
                is_dir,
                size,
                system,
            };
            if tx.send(task).is_ok() {
                self.pending.insert(key.clone());
            }
        }
        self.fallback_icon_id(&key)
    }

    /// 在同一路径已缓存的尺寸里找最接近目标的纹理作为占位。
    fn fallback_icon_id(&self, key: &str) -> Option<egui::TextureId> {
        let (kind, path, size) = parse_icon_key(key)?;
        let sizes = self.icon_sizes.get(&(kind.to_owned(), path.to_owned()))?;
        let best = sizes.iter().min_by_key(|s| s.abs_diff(size))?;
        self.textures
            .get(&format!("icon:{kind}:{best}:{path}"))
            .map(|t| t.id())
    }

    /// 图标尺寸变化后剔除不在期望集合中的缓存条目，避免滑杆每移动一步/切换显示器
    /// 都累积一整套纹理。键格式为 `icon:<d|f>:<size>:<path>`，路径中的冒号不参与分段。
    pub fn retain_icon_sizes(&mut self, sizes: &[u32]) {
        let stale = |key: &String| parse_icon_key(key).is_some_and(|(_, _, v)| !sizes.contains(&v));
        self.textures.retain(|k, _| !stale(k));
        self.pending.retain(|k| !stale(k));
        self.failed.retain(|k| !stale(k));
        self.rebuild_icon_sizes();
    }

    /// 删除某个路径的全部图标/缩略图缓存（重命名或修改后旧纹理必须失效）。
    pub fn forget_path(&mut self, path: &std::path::Path) {
        let target = path.to_string_lossy();
        let stale = |key: &String| {
            parse_icon_key(key).is_some_and(|(_, cached, _)| cached == target.as_ref())
                || parse_thumb_key(key).is_some_and(|cached| cached == target.as_ref())
        };
        self.textures.retain(|k, _| !stale(k));
        self.pending.retain(|k| !stale(k));
        self.failed.retain(|k| !stale(k));
        self.thumb_use.retain(|k, _| !stale(k));
        self.icon_sizes
            .retain(|(_, cached), _| cached != target.as_ref());
    }

    fn rebuild_icon_sizes(&mut self) {
        self.icon_sizes.clear();
        for key in self.textures.keys() {
            if let Some((kind, path, size)) = parse_icon_key(key) {
                let sizes = self
                    .icon_sizes
                    .entry((kind.to_owned(), path.to_owned()))
                    .or_default();
                if !sizes.contains(&size) {
                    sizes.push(size);
                }
            }
        }
    }

    /// 确保图片缩略图已请求。
    pub fn ensure_thumbnail(
        &mut self,
        tx: &Sender<IconTask>,
        key: String,
        path: PathBuf,
        max_size: u32,
    ) -> Option<egui::TextureId> {
        if let Some(id) = self.texture_id(&key) {
            return Some(id);
        }
        if self.pending.contains(&key) || self.failed.contains(&key) {
            return None;
        }
        let task = IconTask::Thumbnail {
            key: key.clone(),
            path,
            max_size,
        };
        if tx.send(task).is_ok() {
            self.pending.insert(key);
        }
        None
    }

    /// 确保系统缩略图（IShellItemImageFactory，支持图片/PDF/Office 等）已请求。
    /// 命中缓存时同时返回纹理 id 与原始像素尺寸，便于按宽高比内接绘制。
    pub fn ensure_shell_thumb(
        &mut self,
        tx: &Sender<IconTask>,
        key: String,
        path: PathBuf,
        size: u32,
    ) -> Option<(egui::TextureId, egui::Vec2)> {
        if let Some(tex) = self.textures.get(&key) {
            let id = tex.id();
            let natural = tex.size_vec2();
            self.touch_thumb(&key);
            return Some((id, natural));
        }
        if self.pending.contains(&key) || self.failed.contains(&key) {
            return None;
        }
        let task = IconTask::ShellThumb {
            key: key.clone(),
            path,
            size,
        };
        if tx.send(task).is_ok() {
            self.pending.insert(key);
        }
        None
    }
}

/// 解析图标缓存键 `icon:<d|f>:<size>:<path>`（路径中的冒号不参与分段）。
fn parse_icon_key(key: &str) -> Option<(&str, &str, u32)> {
    let rest = key.strip_prefix("icon:")?;
    let (kind, rest) = rest.split_once(':')?;
    let (size, path) = rest.split_once(':')?;
    Some((kind, path, size.parse().ok()?))
}

/// 解析缩略图缓存键 `thumb:<size>:<mtime>:<path>`，返回路径部分。
fn parse_thumb_key(key: &str) -> Option<&str> {
    let rest = key.strip_prefix("thumb:")?;
    let (_, rest) = rest.split_once(':')?;
    let (_, path) = rest.split_once(':')?;
    Some(path)
}

/// 中文字体安装（失败时保留内置字体，界面仍可运行但中文可能显示为方框）。
pub fn install_fonts(ctx: &egui::Context) {
    let candidates = [
        "C:\\Windows\\Fonts\\Deng.ttf",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\msyh.ttf",
        "C:\\Windows\\Fonts\\simsun.ttc",
    ];
    let mut fonts = egui::FontDefinitions::default();
    let mut installed = None;
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "zh".to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            installed = Some(path);
            break;
        }
    }
    if installed.is_some() {
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .insert(0, "zh".to_owned());
        }
    } else {
        log::warn!("未找到可用的中文字体，界面中文可能显示异常");
    }
    ctx.set_fonts(fonts);
}

/// 把裁剪后的文本画进给定宽度（先按字符估算截断，避免文本溢出格子）。
pub fn fit_text(name: &str, cell_w: f32, font_size: f32) -> String {
    let approx_char_w = (font_size * 0.62).max(1.0);
    let max_chars = ((cell_w / approx_char_w).floor() as usize).max(4);
    crate::util::truncate_chars(name, max_chars)
}

/// 统一的圆角矩形填充 + 假阴影（多层半透明描边模拟，成本极低）。
pub fn rounded_card(
    painter: &egui::Painter,
    rect: egui::Rect,
    radius: u8,
    fill: egui::Color32,
    shadow: bool,
) {
    if shadow {
        for i in 0..3u8 {
            let expand = 2.0 + i as f32 * 3.0;
            painter.rect_filled(
                rect.expand(expand),
                egui::CornerRadius::same(radius.saturating_add(i + 2)),
                egui::Color32::from_black_alpha(14 - i * 4),
            );
        }
    }
    painter.rect_filled(rect, egui::CornerRadius::same(radius), fill);
}

/// 取色盘（盒子配色用）。
pub const BOX_PRESETS: [[u8; 3]; 8] = [
    [58, 78, 120],
    [46, 96, 84],
    [112, 66, 88],
    [96, 84, 46],
    [52, 52, 60],
    [36, 88, 118],
    [92, 56, 110],
    [70, 70, 78],
];

#[cfg(test)]
mod tests {
    use super::parse_icon_key;

    #[test]
    fn parse_icon_key_keeps_windows_path_intact() {
        let (kind, path, size) = parse_icon_key(r"icon:d:48:C:\Users\Neil\Desktop\项目").unwrap();
        assert_eq!(kind, "d");
        assert_eq!(size, 48);
        assert_eq!(path, r"C:\Users\Neil\Desktop\项目");
        assert_eq!(parse_icon_key(r"thumb:64:C:\a.png"), None);
    }
}
