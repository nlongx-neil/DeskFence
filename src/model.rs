//! 数据模型与纯布局计算（不依赖任何系统 API，便于单元测试）。

use crate::config::{DesktopSlot, FileFilter, SortMode};
use std::path::{Path, PathBuf};

/// 程序合成的系统项（不对应任何真实文件，也不写盘）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemItem {
    /// 「此电脑」：双击打开资源管理器「此电脑」。
    Computer,
    /// 「回收站」：`full` 表示回收站非空（决定使用系统「满/空」两种真实图标）。
    RecycleBin { full: bool },
}

/// 系统项「此电脑」使用的 Shell 命名空间路径（不指向真实磁盘位置）。
pub const COMPUTER_CLSID_PATH: &str = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";

/// 系统项「回收站」使用的 Shell 命名空间路径（不指向真实磁盘位置）。
pub const RECYCLE_BIN_CLSID_PATH: &str = "::{645FF040-5081-101B-9F08-00AA002F954E}";

/// 桌面上或盒子中的一个真实文件/文件夹。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemMeta {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// 修改时间（Unix 秒），0 表示未知
    pub modified: u64,
    /// 非空表示这是程序合成的系统项：不参与重命名、拖拽、自动归类与成员登记。
    pub system: Option<SystemItem>,
}

impl ItemMeta {
    /// 合成「此电脑」条目（不写盘；路径为 Shell 命名空间 CLSID）。
    pub fn computer() -> Self {
        Self {
            path: PathBuf::from(COMPUTER_CLSID_PATH),
            name: "此电脑".to_string(),
            is_dir: true,
            size: 0,
            modified: 0,
            system: Some(SystemItem::Computer),
        }
    }

    /// 合成「回收站」条目（不写盘；路径为 Shell 命名空间 CLSID）。
    ///
    /// `full` 决定图标使用系统「回收站满 / 空」哪一档，内容变化时由 UI 轮询后重建。
    pub fn recycle_bin(full: bool) -> Self {
        Self {
            path: PathBuf::from(RECYCLE_BIN_CLSID_PATH),
            name: "回收站".to_string(),
            is_dir: true,
            size: 0,
            modified: 0,
            system: Some(SystemItem::RecycleBin { full }),
        }
    }

    /// 是否为程序合成的系统项。
    pub fn is_system(&self) -> bool {
        self.system.is_some()
    }

    /// 路径是否是「此电脑」的 Shell 命名空间路径。
    pub fn is_computer_path(path: &Path) -> bool {
        path_key(path) == path_key(Path::new(COMPUTER_CLSID_PATH))
    }

    /// 路径是否是「回收站」的 Shell 命名空间路径。
    pub fn is_recycle_bin_path(path: &Path) -> bool {
        path_key(path) == path_key(Path::new(RECYCLE_BIN_CLSID_PATH))
    }

    /// 路径是否是任一程序合成系统项的 Shell 命名空间路径。
    pub fn is_system_path(path: &Path) -> bool {
        Self::is_computer_path(path) || Self::is_recycle_bin_path(path)
    }

    /// 读取单个路径的元数据；隐藏项与读取失败返回 None。
    pub fn from_path(path: PathBuf) -> Option<Self> {
        let name = path.file_name()?.to_string_lossy().to_string();
        if name.starts_with('.') {
            return None;
        }
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if is_hidden(&path, &meta) {
            return None;
        }
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Some(Self {
            path,
            name,
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified,
            system: None,
        })
    }

    pub fn extension(&self) -> String {
        self.path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    }

    /// 显示名称：只用于绘制标签，真实文件名请继续用 [`ItemMeta::name`]。
    pub fn display_name(&self) -> &str {
        display_name(&self.name, self.is_dir)
    }
}

/// 路径引用的比较键：Windows 路径大小写不敏感，统一转小写。
pub fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// 解析盒子成员引用：返回可显示的条目与失效引用（保留原始字符串用于提示）。
pub fn resolve_members(members: &[String]) -> (Vec<ItemMeta>, Vec<String>) {
    let mut items = Vec::new();
    let mut invalid = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in members {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let path = PathBuf::from(raw);
        if !seen.insert(path_key(&path)) {
            continue;
        }
        match ItemMeta::from_path(path) {
            Some(meta) => items.push(meta),
            None => invalid.push(raw.to_string()),
        }
    }
    (items, invalid)
}

/// 从桌面列表中剔除已归属盒子的条目（虚拟引用下文件仍在桌面，但桌面不再重复显示）。
pub fn retain_non_members(items: &mut Vec<ItemMeta>, members: &std::collections::HashSet<String>) {
    if members.is_empty() {
        return;
    }
    items.retain(|item| !members.contains(&path_key(&item.path)));
}

/// 显示名称的纯函数：去掉快捷方式后缀（`.lnk` / `.url`），大小写不敏感。
///
/// - 只影响显示；打开、拖拽、重命名仍使用真实文件名与路径。
/// - 文件夹一律原样返回（即使名字以 `.lnk` 结尾也不截断）。
/// - `a.b.c.lnk` → `a.b.c`；名字本身只有后缀（如 `.lnk`）时保持原样，避免显示为空。
pub fn display_name(name: &str, is_dir: bool) -> &str {
    if is_dir {
        return name;
    }
    for ext in [".lnk", ".url"] {
        if name.len() <= ext.len() {
            continue;
        }
        // 用 get 而非直接切片：文件名可能包含多字节字符，避免越界/非字符边界 panic。
        if let Some(tail) = name.get(name.len() - ext.len()..) {
            if tail.eq_ignore_ascii_case(ext) {
                return &name[..name.len() - ext.len()];
            }
        }
    }
    name
}

#[cfg(windows)]
fn is_hidden(path: &std::path::Path, meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    let attrs = meta.file_attributes();
    if attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0 {
        return true;
    }
    // 文件名以 `desktop.ini` 结尾等系统文件
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.eq_ignore_ascii_case("desktop.ini"))
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_hidden(_path: &std::path::Path, _meta: &std::fs::Metadata) -> bool {
    false
}

/// 文件大类数量；下标即配置 `category_targets` 的索引。
pub const CATEGORY_COUNT: usize = 10;

/// 文件大类标签（决定文件盒子内的固定分组顺序）。
pub const CATEGORY_LABELS: [&str; CATEGORY_COUNT] = [
    "文档",
    "表格",
    "演示",
    "PDF",
    "图片",
    "音视频",
    "压缩包",
    "网页",
    "程序",
    "其他",
];

/// 扩展名所属大类下标，见 [`CATEGORY_LABELS`]。
///
/// 固定大类顺序（而不是按组内数量排序）是为了让图标位置稳定：
/// 新增一个文件不会因为组数量变化把其它组整体挪位。
pub fn category_of(ext: &str) -> usize {
    match ext.to_ascii_lowercase().as_str() {
        "doc" | "docx" | "docm" | "dot" | "dotx" | "rtf" | "odt" | "txt" | "md" | "markdown"
        | "wps" | "tex" => 0,
        "xls" | "xlsx" | "xlsm" | "xlsb" | "csv" | "ods" | "et" => 1,
        "ppt" | "pptx" | "pptm" | "pps" | "ppsx" | "odp" | "dps" | "key" => 2,
        "pdf" => 3,
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "svg" | "tif" | "tiff"
        | "psd" | "raw" | "heic" | "avif" => 4,
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "mp4" | "mkv" | "avi" | "mov"
        | "wmv" | "flv" | "webm" | "ts" | "rmvb" | "m4v" | "mpg" | "mpeg" => 5,
        "zip" | "rar" | "7z" | "gz" | "tar" | "bz2" | "xz" | "iso" | "cab" => 6,
        "url" | "website" | "webloc" | "htm" | "html" | "mht" => 7,
        "exe" | "lnk" | "bat" | "cmd" | "ps1" | "msi" | "apk" | "appx" | "dll" => 8,
        _ => 9,
    }
}

/// tab 筛选是否命中：只按扩展名大类判断，不改文件位置、不影响排序。
pub fn file_filter_matches(filter: FileFilter, item: &ItemMeta) -> bool {
    if item.is_dir {
        // 文件夹没有扩展名，归入「其他」；「全部」自然包含。
        return matches!(filter, FileFilter::All | FileFilter::Other);
    }
    let category = category_of(&item.extension());
    match filter {
        FileFilter::All => true,
        FileFilter::Docs => category == 0,
        FileFilter::Sheets => category == 1,
        FileFilter::Slides => category == 2,
        FileFilter::Pdf => category == 3,
        FileFilter::Image => category == 4,
        FileFilter::Archive => category == 6,
        FileFilter::Other => matches!(category, 5 | 7 | 8 | 9),
    }
}

/// 按 tab 过滤后的真实下标序列；保持原顺序（「全部」即原顺序，已由 A 的类型分组保证连续）。
pub fn filtered_indices(items: &[ItemMeta], filter: FileFilter) -> Vec<usize> {
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| file_filter_matches(filter, item))
        .map(|(index, _)| index)
        .collect()
}

/// 扩展名族：把同一族的变体合并成同一段（doc/docx 一段、xls/xlsx 一段……）。
///
/// 入参应为小写扩展名（[`ItemMeta::extension`] 已归一化）；只合并真正同源的变体，
/// 不同图片/音视频格式各自成段，保证「同扩展名连续」。
pub fn extension_family(ext: &str) -> &str {
    match ext {
        "docx" | "docm" | "dot" | "dotx" => "doc",
        "xlsx" | "xlsm" | "xlsb" => "xls",
        "pptx" | "pptm" | "pps" | "ppsx" => "ppt",
        "jpeg" => "jpg",
        "tiff" => "tif",
        "m4v" => "mp4",
        "htm" | "mht" => "html",
        "markdown" => "md",
        "webloc" => "url",
        other => other,
    }
}

/// 文件盒子内的排序：固定大类顺序 → 扩展名族连续 → 族内名称自然排序。
pub fn sort_items_grouped(items: &mut [ItemMeta]) {
    items.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| category_of(&a.extension()).cmp(&category_of(&b.extension())))
            .then_with(|| {
                let (ea, eb) = (a.extension(), b.extension());
                extension_family(&ea).cmp(extension_family(&eb))
            })
            .then_with(|| natural_cmp(&a.name, &b.name))
    });
}

/// 排序：文件夹优先，再按指定规则。
pub fn sort_items(items: &mut [ItemMeta], mode: SortMode) {
    items.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| match mode {
                SortMode::Name => natural_cmp(&a.name, &b.name),
                SortMode::Type => a
                    .extension()
                    .cmp(&b.extension())
                    .then_with(|| natural_cmp(&a.name, &b.name)),
                SortMode::Modified => b.modified.cmp(&a.modified),
                SortMode::Size => b.size.cmp(&a.size),
            })
            .then_with(|| natural_cmp(&a.name, &b.name))
    });
}

/// 按用户手动顺序重排（顺序表元素为 [`path_key`]）。
///
/// 已登记的条目严格按顺序表排布；未登记的新条目保持传入时的相对顺序，追加到末尾。
/// 调用方通常先 [`sort_items`] 再调用本函数，因此新条目仍按当前排序规则出现。
pub fn apply_manual_order(items: &mut [ItemMeta], order: &[String]) {
    if order.is_empty() || items.len() < 2 {
        return;
    }
    let rank: std::collections::HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(i, key)| (key.as_str(), i))
        .collect();
    // 稳定排序：未登记项取 usize::MAX，彼此相对顺序保持不变。
    items.sort_by_cached_key(|it| {
        rank.get(path_key(&it.path).as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });
}

/// 把当前桌面顺序导出为手动顺序表（[`path_key`] 形式），供拖放后持久化。
pub fn manual_order_of(items: &[ItemMeta]) -> Vec<String> {
    items.iter().map(|it| path_key(&it.path)).collect()
}

/// 名称比较：忽略大小写，并对"数字段"做自然排序（文件 10 排在 2 之后）。
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let mut na = String::new();
                    let mut nb = String::new();
                    while let Some(c) = ai.peek().copied() {
                        if !c.is_ascii_digit() {
                            break;
                        }
                        na.push(c);
                        ai.next();
                    }
                    while let Some(c) = bi.peek().copied() {
                        if !c.is_ascii_digit() {
                            break;
                        }
                        nb.push(c);
                        bi.next();
                    }
                    let va: u64 = na.parse().unwrap_or(0);
                    let vb: u64 = nb.parse().unwrap_or(0);
                    let ord = va.cmp(&vb).then_with(|| na.len().cmp(&nb.len()));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    let la = ca.to_lowercase().next().unwrap_or(ca);
                    let lb = cb.to_lowercase().next().unwrap_or(cb);
                    let ord = la.cmp(&lb);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
}

/// 网格布局参数（全部为逻辑像素）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GridLayout {
    pub cols: usize,
    pub rows: usize,
    pub cell_w: f32,
    pub cell_h: f32,
    pub icon: f32,
    pub label_h: f32,
    pub padding: f32,
}

impl GridLayout {
    /// 计算第 index 个格子的左上角（相对区域原点）。
    pub fn cell_pos(&self, index: usize) -> (f32, f32) {
        if self.cols == 0 {
            return (self.padding, self.padding);
        }
        let col = index % self.cols;
        let row = index / self.cols;
        (
            self.padding + col as f32 * self.cell_w,
            self.padding + row as f32 * self.cell_h,
        )
    }

    pub fn cell_rect(&self, index: usize, origin: (f32, f32)) -> (f32, f32, f32, f32) {
        let (x, y) = self.cell_pos(index);
        (
            origin.0 + x,
            origin.1 + y,
            self.cell_w.max(1.0),
            self.cell_h.max(1.0),
        )
    }

    pub fn total_rows_for(&self, count: usize) -> usize {
        if self.cols == 0 || count == 0 {
            0
        } else {
            count.div_ceil(self.cols)
        }
    }
}

/// 计算网格布局；区域过小时仍保证至少 1 列。
pub fn grid_layout(
    area_w: f32,
    area_h: f32,
    icon: f32,
    label_h: f32,
    gap: f32,
    padding: f32,
    count: usize,
) -> GridLayout {
    let icon = icon.max(1.0);
    let label_h = label_h.max(0.0);
    let gap = gap.max(0.0);
    let cell_w = icon + gap;
    let cell_h = icon + label_h + gap;
    let usable_w = (area_w - padding * 2.0 + gap).max(cell_w);
    let cols = ((usable_w / cell_w).floor() as usize).max(1);
    let rows = if count == 0 { 0 } else { count.div_ceil(cols) };
    let _ = area_h; // 行数由滚动/裁剪处理，此处仅用于布局宽度
    GridLayout {
        cols,
        rows,
        cell_w,
        cell_h,
        icon,
        label_h,
        padding,
    }
}

/// 图标名称字号占图标边长的比例（逻辑点）。
pub const LABEL_FONT_RATIO: f32 = 0.27;
/// 字号下限：极小图标时保证可读，但相对图标会偏大。
pub const LABEL_FONT_MIN: f32 = 8.5;
/// 字号上限：最大图标档（96）仍保持约 27%，同时避免更大图标时文字喧宾夺主。
pub const LABEL_FONT_MAX: f32 = 26.0;
/// 文字行高相对字号的倍数。
pub const LABEL_LINE_RATIO: f32 = 1.22;
/// 名称最多换行行数，超出部分截断并加省略号。
pub const LABEL_MAX_LINES: usize = 2;

/// 按图标逻辑边长换算名称字号：比例 27%，并夹在上下限之间。
pub fn label_font_for_icon(icon: f32) -> f32 {
    (icon * LABEL_FONT_RATIO).clamp(LABEL_FONT_MIN, LABEL_FONT_MAX)
}

/// 名称区域高度：按最多两行预留，并在文字上方留出与图标之间的间隙（逻辑点）。
///
/// `gap` 由配置「图标与文字间距」按屏 DPI 换算后传入，文字区高度随之增长，
/// 因此调大间距只会把格子撑高，不会让文字压到下一行。
pub fn label_block_height(font: f32, gap: f32) -> f32 {
    font * LABEL_LINE_RATIO * LABEL_MAX_LINES as f32 + gap.max(0.0)
}

/// 按**真实字形宽度**把名称切成最多 `LABEL_MAX_LINES` 行，放不下时末行加省略号。
///
/// `measure` 由调用方提供：生产路径用 egui 字体实际排版宽度测量，
/// 单元测试可用近似字宽（中文按 1em、ASCII 按 0.55em）验证同一套换行逻辑。
pub fn label_lines_measured<F>(name: &str, max_w: f32, measure: F) -> Vec<String>
where
    F: Fn(&str) -> f32,
{
    if name.is_empty() || max_w <= 0.0 {
        return Vec::new();
    }
    if measure(name) <= max_w {
        return vec![name.to_string()];
    }
    let chars: Vec<char> = name.chars().collect();
    let split = fit_prefix(&chars, max_w, &measure);
    if split >= chars.len() {
        return vec![name.to_string()];
    }
    let rest = &chars[split..];
    let rest_string: String = rest.iter().collect();
    if measure(&rest_string) <= max_w {
        return vec![chars[..split].iter().collect(), rest_string];
    }
    let keep = fit_prefix_with_ellipsis(rest, max_w, &measure);
    let mut second: String = rest[..keep].iter().collect();
    second.push('…');
    vec![chars[..split].iter().collect(), second]
}

/// 最长的、实际宽度不超过 `max_w` 的前缀字符数（至少 1，单字超宽时交给裁剪兜底）。
fn fit_prefix<F>(chars: &[char], max_w: f32, measure: &F) -> usize
where
    F: Fn(&str) -> f32,
{
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let text: String = chars[..mid].iter().collect();
        if measure(&text) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo.max(1).min(chars.len())
}

/// 第二行带省略号时，能放下的最长前缀字符数。
fn fit_prefix_with_ellipsis<F>(chars: &[char], max_w: f32, measure: &F) -> usize
where
    F: Fn(&str) -> f32,
{
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let mut text: String = chars[..mid].iter().collect();
        text.push('…');
        if measure(&text) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// 一块显示器的布局输入（物理像素工作区 + 各自 DPI 缩放）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonitorArea {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub scale: f32,
}

/// 桌面图标格子在屏幕上的逻辑位置（egui 点），以及该屏的缩放系数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DesktopCell {
    /// 所属显示器在输入数组中的下标
    pub monitor: usize,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// 该屏上图标绘制边长（点）
    pub icon: f32,
    /// UI 缩放系数 = 该屏 DPI 缩放 / 窗口 pixels_per_point
    pub k: f32,
    /// 网格列（自左向右）
    pub col: usize,
    /// 网格行（自上而下）
    pub row: usize,
}

/// 把桌面图标按显示器依次排布：先填满第一块屏的列，再溢出到下一块屏。
///
/// 各屏使用自己的 DPI 缩放（图标、间距、标签高度按逻辑 DIP 换算），
/// 因此混合 DPI 下不会把 150% 屏的图标尺寸套到 100% 屏上；
/// 名称字号与标签高度按该屏的图标逻辑尺寸联动换算。
#[allow(clippy::too_many_arguments)]
pub fn desktop_cells(
    monitors: &[MonitorArea],
    count: usize,
    pp: f32,
    icon_dip: f32,
    gap_dip: f32,
    label_gap_dip: f32,
    show_labels: bool,
    padding_dip: f32,
) -> Vec<DesktopCell> {
    let mut out = Vec::with_capacity(count);
    if monitors.is_empty() || count == 0 {
        return out;
    }
    let specs = grid_specs(
        monitors,
        pp,
        icon_dip,
        gap_dip,
        label_gap_dip,
        show_labels,
        padding_dip,
    );
    let mut remaining = count;
    let mut last: Option<&GridSpec> = None;
    for g in &specs {
        if remaining == 0 {
            break;
        }
        let capacity = g.rows * g.cols;
        let take = capacity.min(remaining);
        for i in 0..take {
            out.push(g.cell(i / g.rows, i % g.rows));
        }
        remaining -= take;
        last = Some(g);
    }
    // 所有屏都放满后仍有剩余：继续在最后一块屏右侧延伸（超出部分由窗口裁剪）。
    if let Some(g) = last {
        let mut i = out.len();
        while i < count {
            out.push(g.cell(i / g.rows + 1, i % g.rows));
            i += 1;
        }
    }
    out
}

/// 返回每屏完整棋盘（`rows * cols` 个格子），供「固定棋盘 + 占位」使用。
///
/// 与 [`desktop_cells`] 共用 [`grid_specs`]；`min_count` 超过总容量时沿用同样的
/// 「最后一块屏向右延伸」溢出规则，保证两种排布的格子几何完全一致。
#[allow(clippy::too_many_arguments)]
pub fn desktop_board_cells(
    monitors: &[MonitorArea],
    pp: f32,
    icon_dip: f32,
    gap_dip: f32,
    label_gap_dip: f32,
    show_labels: bool,
    padding_dip: f32,
    min_count: usize,
) -> Vec<DesktopCell> {
    if monitors.is_empty() {
        return Vec::new();
    }
    let specs = grid_specs(
        monitors,
        pp,
        icon_dip,
        gap_dip,
        label_gap_dip,
        show_labels,
        padding_dip,
    );
    let mut out = Vec::new();
    for g in &specs {
        for i in 0..g.rows * g.cols {
            out.push(g.cell(i / g.rows, i % g.rows));
        }
    }
    if let Some(g) = specs.last() {
        let mut i = out.len();
        while i < min_count {
            out.push(g.cell(i / g.rows + 1, i % g.rows));
            i += 1;
        }
    }
    out
}

/// 棋盘布局参数（每屏一份），`desktop_cells` 与 `desktop_board_cells` 共用。
struct GridSpec {
    monitor: usize,
    x: f32,
    y: f32,
    pad: f32,
    cell_w: f32,
    cell_h: f32,
    icon: f32,
    k: f32,
    rows: usize,
    cols: usize,
}

impl GridSpec {
    fn cell(&self, col: usize, row: usize) -> DesktopCell {
        DesktopCell {
            monitor: self.monitor,
            x: self.x + self.pad + col as f32 * self.cell_w,
            y: self.y + self.pad + row as f32 * self.cell_h,
            w: self.cell_w,
            h: self.cell_h,
            icon: self.icon,
            k: self.k,
            col,
            row,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn grid_specs(
    monitors: &[MonitorArea],
    pp: f32,
    icon_dip: f32,
    gap_dip: f32,
    label_gap_dip: f32,
    show_labels: bool,
    padding_dip: f32,
) -> Vec<GridSpec> {
    let pp = pp.max(1.0);
    monitors
        .iter()
        .enumerate()
        .map(|(mi, m)| {
            let k = m.scale.max(1.0) / pp;
            let icon = icon_dip * k;
            let gap = gap_dip * k;
            let label = if show_labels {
                label_block_height(label_font_for_icon(icon), label_gap_dip * k)
            } else {
                0.0
            };
            let pad = padding_dip * k;
            let cell_w = (icon + gap).max(1.0);
            let cell_h = (icon + label + gap).max(1.0);
            let area_w = m.w / pp;
            let area_h = m.h / pp;
            let rows = (((area_h - pad * 2.0 + gap) / cell_h).floor() as usize).max(1);
            let cols = (((area_w - pad * 2.0 + gap) / cell_w).floor() as usize).max(1);
            GridSpec {
                monitor: mi,
                x: m.x / pp,
                y: m.y / pp,
                pad,
                cell_w,
                cell_h,
                icon,
                k,
                rows,
                cols,
            }
        })
        .collect()
}

/// 把桌面项分配到棋盘格子：旧槽位优先，未占位项按「列优先」取第一个空格。
///
/// 列优先 = 同一列内自上而下排，排满当前列再向右开新列（与 [`desktop_cells`]
/// 的默认紧凑排列方向一致，图标整体靠屏幕左侧而不是横向铺满）。
/// 旧槽位先夹取到该屏当前最大列/行（图标尺寸或分辨率变化后可能越界），
/// 同一格被多项命中时按 `items` 顺序先到先得，后到者视为未占位。
/// 返回 `(item→cell 下标, 归一化槽位)`；归一化槽位只保留当前存在的项，
/// 删除/移入盒子的项会自动释放格子。
pub fn assign_board_slots(
    items: &[ItemMeta],
    cells: &[DesktopCell],
    slots: &[DesktopSlot],
) -> (Vec<Option<usize>>, Vec<DesktopSlot>) {
    let mut slot_map: std::collections::HashMap<&str, &DesktopSlot> =
        std::collections::HashMap::new();
    for s in slots {
        slot_map.entry(s.key.as_str()).or_insert(s);
    }
    let mut bounds: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::new();
    for c in cells {
        let e = bounds.entry(c.monitor).or_insert((c.col, c.row));
        e.0 = e.0.max(c.col);
        e.1 = e.1.max(c.row);
    }
    // 系统项（此电脑 → 回收站）排在最前，保证它们默认占住最左上角两格，
    // 普通图标（包括新出现/新移出的项）不会抢到这两格。
    let mut order: Vec<usize> = Vec::with_capacity(items.len());
    order.extend((0..items.len()).filter(|&i| items[i].is_system()));
    order.extend((0..items.len()).filter(|&i| !items[i].is_system()));
    order.sort_by_key(|&i| {
        if items[i].is_system() {
            if ItemMeta::is_computer_path(&items[i].path) {
                0usize
            } else {
                1usize
            }
        } else {
            2usize
        }
    });
    let mut used = vec![false; cells.len()];
    let mut assignment: Vec<Option<usize>> = vec![None; items.len()];
    for &i in &order {
        let item = &items[i];
        let key = path_key(&item.path);
        let Some(s) = slot_map.get(key.as_str()) else {
            continue;
        };
        let Some(&(max_col, max_row)) = bounds.get(&s.monitor) else {
            continue;
        };
        let col = s.col.min(max_col);
        let row = s.row.min(max_row);
        if let Some(ci) = cells
            .iter()
            .position(|c| c.monitor == s.monitor && c.col == col && c.row == row)
        {
            if !used[ci] {
                used[ci] = true;
                assignment[i] = Some(ci);
            }
        }
    }
    // 列优先扫描空格：不覆盖已占位项，空洞保持不变；同一列排满后才向右开新列。
    let mut column_major: Vec<usize> = (0..cells.len()).collect();
    column_major.sort_by_key(|&ci| (cells[ci].monitor, cells[ci].col, cells[ci].row));
    let mut free = column_major.into_iter();
    for &i in &order {
        if assignment[i].is_some() {
            continue;
        }
        let Some(ci) = free.find(|&ci| !used[ci]) else {
            break;
        };
        used[ci] = true;
        assignment[i] = Some(ci);
    }
    let normalized = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let ci = assignment[i]?;
            let c = cells[ci];
            Some(DesktopSlot {
                key: path_key(&item.path),
                monitor: c.monitor,
                col: c.col,
                row: c.row,
            })
        })
        .collect();
    (assignment, normalized)
}

/// 把系统项稳定排到最前：此电脑 → 回收站 → 其余项（保持原有相对顺序）。
///
/// 紧凑排列按 `items` 下标依次占格，因此系统项排在最前就能固定占住
/// 左上角最前两格；「恢复自动排列」与棋盘快照都复用这一顺序。
pub fn system_first(items: &mut Vec<ItemMeta>) {
    let mut sys = Vec::new();
    let mut rest = Vec::new();
    for item in items.drain(..) {
        if item.is_system() {
            sys.push(item);
        } else {
            rest.push(item);
        }
    }
    sys.sort_by_key(|i| {
        if ItemMeta::is_computer_path(&i.path) {
            0u8
        } else {
            1u8
        }
    });
    sys.append(&mut rest);
    *items = sys;
}

/// 交换两个路径键的格子坐标；返回是否找到两项。
pub fn swap_desktop_slots(slots: &mut [DesktopSlot], a: &str, b: &str) -> bool {
    let Some(ai) = slots.iter().position(|s| s.key == a) else {
        return false;
    };
    let Some(bi) = slots.iter().position(|s| s.key == b) else {
        return false;
    };
    if ai == bi {
        return true;
    }
    let (monitor, col, row) = (slots[ai].monitor, slots[ai].col, slots[ai].row);
    slots[ai].monitor = slots[bi].monitor;
    slots[ai].col = slots[bi].col;
    slots[ai].row = slots[bi].row;
    slots[bi].monitor = monitor;
    slots[bi].col = col;
    slots[bi].row = row;
    true
}

/// 桌面式"先行后列"排布：图标自上而下填满一列，再换到下一列。
///
/// `rows` 为每列可容纳的行数（至少 1）。返回第 `index` 个图标相对区域原点的左上角。
pub fn column_grid_pos(
    index: usize,
    rows: usize,
    cell_w: f32,
    cell_h: f32,
    padding: f32,
) -> (f32, f32) {
    let rows = rows.max(1);
    let col = index / rows;
    let row = index % rows;
    (padding + col as f32 * cell_w, padding + row as f32 * cell_h)
}

/// 根据滚动偏移（像素）裁剪出可见的行范围。
pub fn visible_row_range(
    layout: &GridLayout,
    scroll: f32,
    view_h: f32,
    count: usize,
) -> (usize, usize) {
    if layout.cell_h <= 0.0 || count == 0 {
        return (0, 0);
    }
    let first = (scroll / layout.cell_h).floor().max(0.0) as usize;
    let visible = ((view_h / layout.cell_h).ceil() as usize).max(1);
    let last = (first + visible + 1).min(layout.total_rows_for(count));
    (first.min(last), last)
}

/// 方向键导航方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavDir {
    Left,
    Right,
    Up,
    Down,
}

/// 方向键在网格/列表中的目标显示下标。
///
/// `cols == 1` 视为列表：上下左右都按 ±1 移动；`cols > 1` 视为网格：
/// 上下按整行跳（最后一行的短缺列向同列最后一个条目收缩），左右在行内移动，
/// 到边界时原地不动（返回 `current`）。
pub fn nav_target(dir: NavDir, cols: usize, len: usize, current: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let current = current.min(len - 1);
    let cols = cols.max(1);
    if cols == 1 {
        return match dir {
            NavDir::Left | NavDir::Up => current.saturating_sub(1),
            NavDir::Right | NavDir::Down => (current + 1).min(len - 1),
        };
    }
    let row = current / cols;
    let col = current % cols;
    match dir {
        NavDir::Left => {
            if col == 0 {
                current
            } else {
                current - 1
            }
        }
        NavDir::Right => {
            if col + 1 >= cols || current + 1 >= len {
                current
            } else {
                current + 1
            }
        }
        NavDir::Up => {
            if row == 0 {
                current
            } else {
                let target = (row - 1) * cols + col;
                target.min(len - 1)
            }
        }
        NavDir::Down => {
            let target = (row + 1) * cols + col;
            if target < len {
                target
            } else if row + 1 < len.div_ceil(cols) {
                // 最后一行缺列：落到同列可用范围的最后一个条目
                len - 1
            } else {
                current
            }
        }
    }
}

/// 框选：返回与选择矩形相交的全部条目下标（列表视图传入整行矩形、网格传入整格矩形）。
///
/// 只要格子与框选矩形有任何非零面积交集即命中，因此"框住半个图标"也会选中。
/// 纯水平/垂直拖动得到的零宽或零高矩形按 1px 细线处理，否则竖着划一列会什么都选不中。
pub fn marquee_hits(cells: &[(usize, RectF)], sel: RectF) -> Vec<usize> {
    let line = (sel.0, sel.1, sel.2.max(1.0), sel.3.max(1.0));
    cells
        .iter()
        .filter(|(_, cell)| intersect_rect(*cell, line).is_some())
        .map(|(index, _)| *index)
        .collect()
}

/// Shift 区间选择的两端（含端点），返回 `(lo, hi)`。
pub fn range_endpoints(anchor: usize, target: usize) -> (usize, usize) {
    (anchor.min(target), anchor.max(target))
}

/// 矩形 `(x, y, w, h)`；逻辑点或物理像素均可，由调用方保证同一坐标空间。
pub type RectF = (f32, f32, f32, f32);

/// 点是否落在矩形内（含边界）。零面积矩形不命中。
pub fn point_in_rect(px: f32, py: f32, r: RectF) -> bool {
    r.2 > 0.0 && r.3 > 0.0 && px >= r.0 && px <= r.0 + r.2 && py >= r.1 && py <= r.1 + r.3
}

/// 两个矩形求交；完全在外或交集为零面积时返回 `None`。
///
/// 盒子里"半格滑出内容区"的图标必须走这个函数裁剪命中区域：只有交集部分可交互。
pub fn intersect_rect(a: RectF, b: RectF) -> Option<RectF> {
    let x0 = a.0.max(b.0);
    let y0 = a.1.max(b.1);
    let x1 = (a.0 + a.2).min(b.0 + b.2);
    let y1 = (a.1 + a.3).min(b.1 + b.3);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((x0, y0, x1 - x0, y1 - y0))
}

/// 把以 `size` 为边长的图标左上角夹到内容区内，保证整个图标矩形不出界。
///
/// 内容区比图标还小时退化为内容区左上角（此时图标只能部分可见，由 clip 兜底）。
pub fn clamp_icon_pos(px: f32, py: f32, size: f32, content: RectF) -> (f32, f32) {
    let max_x = (content.0 + content.2 - size).max(content.0);
    let max_y = (content.1 + content.3 - size).max(content.1);
    (px.clamp(content.0, max_x), py.clamp(content.1, max_y))
}

/// 一条磁吸对齐提示线（坐标为物理像素）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapGuide {
    /// true = 竖直提示线（对齐 x），false = 水平提示线（对齐 y）。
    pub vertical: bool,
    /// 提示线所在的 x 或 y。
    pub at: f32,
    /// 提示线起点（另一个轴）。
    pub start: f32,
    /// 提示线终点（另一个轴）。
    pub end: f32,
}

/// 磁吸结果：吸附后的左上角坐标 + 需要绘制的提示线。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SnapOutcome {
    pub x: f32,
    pub y: f32,
    pub guides: Vec<SnapGuide>,
}

/// 矩形磁吸计算：靠近工作区边缘或其它矩形边缘时对齐。
///
/// - `x/y/w/h` 为待拖动矩形（物理像素）。
/// - `work` 为所在显示器工作区 `(x, y, w, h)`；`others` 为其它盒子矩形。
/// - `threshold` 为磁吸阈值（物理像素，<=0 表示不吸附）。
///
/// 吸附候选：工作区左/右/上/下边；其它盒子的左/右边对齐、右/左边相邻、
/// 上/下边对齐、下/上边相邻。取每个轴最近且在阈值内的候选。
pub fn snap_rect(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    work: (f32, f32, f32, f32),
    others: &[(f32, f32, f32, f32)],
    threshold: f32,
) -> SnapOutcome {
    let mut out = SnapOutcome {
        x,
        y,
        guides: Vec::new(),
    };
    if threshold <= 0.0 || w <= 0.0 || h <= 0.0 {
        return out;
    }
    let (wx, wy, ww, wh) = work;

    // ---- 竖直方向（吸附 x） ----
    let mut best_x: Option<(f32, f32, SnapGuide)> = None; // (距离, 新 x, 提示线)
    let mut consider_x = |delta: f32, guide: SnapGuide| {
        let d = delta.abs();
        if d <= threshold {
            let better = match &best_x {
                Some((bd, _, _)) => d < *bd,
                None => true,
            };
            if better {
                best_x = Some((d, x + delta, guide));
            }
        }
    };
    consider_x(
        wx - x,
        SnapGuide {
            vertical: true,
            at: wx,
            start: y,
            end: y + h,
        },
    );
    consider_x(
        (wx + ww) - (x + w),
        SnapGuide {
            vertical: true,
            at: wx + ww,
            start: y,
            end: y + h,
        },
    );
    for &(ox, oy, ow, oh) in others {
        // 对齐左右边
        consider_x(
            ox - x,
            SnapGuide {
                vertical: true,
                at: ox,
                start: y.min(oy),
                end: (y + h).max(oy + oh),
            },
        );
        consider_x(
            (ox + ow) - (x + w),
            SnapGuide {
                vertical: true,
                at: ox + ow,
                start: y.min(oy),
                end: (y + h).max(oy + oh),
            },
        );
        // 相邻拼接：我们的右边贴对方的左边 / 我们的左边贴对方的右边
        consider_x(
            ox - (x + w),
            SnapGuide {
                vertical: true,
                at: ox,
                start: y.min(oy),
                end: (y + h).max(oy + oh),
            },
        );
        consider_x(
            (ox + ow) - x,
            SnapGuide {
                vertical: true,
                at: ox + ow,
                start: y.min(oy),
                end: (y + h).max(oy + oh),
            },
        );
    }

    // ---- 水平方向（吸附 y） ----
    let mut best_y: Option<(f32, f32, SnapGuide)> = None;
    let mut consider_y = |delta: f32, guide: SnapGuide| {
        let d = delta.abs();
        if d <= threshold {
            let better = match &best_y {
                Some((bd, _, _)) => d < *bd,
                None => true,
            };
            if better {
                best_y = Some((d, y + delta, guide));
            }
        }
    };
    consider_y(
        wy - y,
        SnapGuide {
            vertical: false,
            at: wy,
            start: x,
            end: x + w,
        },
    );
    consider_y(
        (wy + wh) - (y + h),
        SnapGuide {
            vertical: false,
            at: wy + wh,
            start: x,
            end: x + w,
        },
    );
    for &(ox, oy, ow, oh) in others {
        consider_y(
            oy - y,
            SnapGuide {
                vertical: false,
                at: oy,
                start: x.min(ox),
                end: (x + w).max(ox + ow),
            },
        );
        consider_y(
            (oy + oh) - (y + h),
            SnapGuide {
                vertical: false,
                at: oy + oh,
                start: x.min(ox),
                end: (x + w).max(ox + ow),
            },
        );
        consider_y(
            oy - (y + h),
            SnapGuide {
                vertical: false,
                at: oy,
                start: x.min(ox),
                end: (x + w).max(ox + ow),
            },
        );
        consider_y(
            (oy + oh) - y,
            SnapGuide {
                vertical: false,
                at: oy + oh,
                start: x.min(ox),
                end: (x + w).max(ox + ow),
            },
        );
    }

    // 应用并夹到工作区内（盒子比工作区大时贴住左上）。
    let mut nx = best_x.as_ref().map(|(_, v, _)| *v).unwrap_or(x);
    let mut ny = best_y.as_ref().map(|(_, v, _)| *v).unwrap_or(y);
    nx = nx.clamp(wx, (wx + ww - w).max(wx));
    ny = ny.clamp(wy, (wy + wh - h).max(wy));
    // 吸附后若已被夹回边界，用实际位置修正提示线。
    if let Some((_, _, g)) = &best_x {
        out.guides.push(*g);
    }
    if let Some((_, _, g)) = &best_y {
        out.guides.push(*g);
    }
    out.x = nx;
    out.y = ny;
    out
}

/// 新建盒子的右侧停靠位置（物理像素）。
///
/// 从工作区最右列开始，纵向自上而下寻找与 `others` 不重叠的空位；
/// 当前列高度放不下时整体左移一列（`w + gap`），复用磁吸前先得到干净的落点。
pub fn docked_box_pos(
    work: (f32, f32, f32, f32),
    w: f32,
    h: f32,
    margin: f32,
    gap: f32,
    others: &[(f32, f32, f32, f32)],
) -> (f32, f32) {
    let (wx, wy, ww, wh) = work;
    if w <= 0.0 || h <= 0.0 {
        return (wx + margin, wy + margin);
    }
    let margin = margin.max(0.0);
    let gap = gap.max(0.0);
    let min_x = wx + margin;
    let dock_x = (wx + ww - w - margin).max(min_x);
    let mut x = dock_x;
    loop {
        let mut y = wy + margin;
        loop {
            // 取与当前落点相交的盒子中最近的底边；没有相交则说明空位可用。
            let next_bottom = others
                .iter()
                .filter(|&&(ox, oy, ow, oh)| {
                    // 严格相交：贴边不算重叠
                    x < ox + ow && ox < x + w && y < oy + oh && oy < y + h
                })
                .map(|&(_, oy, _, oh)| oy + oh + gap)
                .fold(f32::INFINITY, f32::min);
            if !next_bottom.is_finite() {
                return (x, y);
            }
            if next_bottom + h + margin > wy + wh + 0.5 {
                break;
            }
            y = next_bottom;
        }
        if x - (w + gap) < min_x {
            // 整屏都没有空位时退回最右列顶部，由调用方再做边界钳制。
            return (dock_x, wy + margin);
        }
        x -= w + gap;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, is_dir: bool, size: u64, modified: u64) -> ItemMeta {
        ItemMeta {
            path: PathBuf::from(name),
            name: name.to_string(),
            is_dir,
            size,
            modified,
            system: None,
        }
    }

    #[test]
    fn path_key_is_case_insensitive_for_windows_paths() {
        assert_eq!(
            path_key(Path::new(r"C:\Users\Neil\Desktop\Report.TXT")),
            path_key(Path::new(r"c:\users\neil\desktop\report.txt"))
        );
    }

    #[test]
    fn manual_order_reorders_known_items_and_appends_new_ones() {
        let mut items = vec![
            item("a", false, 0, 0),
            item("b", false, 0, 0),
            item("c", false, 0, 0),
        ];
        let order = vec!["c".to_string(), "a".to_string()];
        apply_manual_order(&mut items, &order);
        assert_eq!(
            items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            vec!["c", "a", "b"],
            "已登记项按手动顺序，未登记项 b 保持相对顺序追加到末尾"
        );
        assert_eq!(
            manual_order_of(&items),
            vec!["c".to_string(), "a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn manual_order_uses_case_insensitive_path_keys() {
        let a = r"C:\Users\X\A.TXT";
        let b = r"C:\Users\X\B.TXT";
        let mut items = vec![item(a, false, 0, 0), item(b, false, 0, 0)];
        let order = vec![
            path_key(Path::new(r"c:\users\x\b.txt")),
            path_key(Path::new(r"c:\users\x\a.txt")),
        ];
        apply_manual_order(&mut items, &order);
        assert_eq!(items[0].name, b);
        assert_eq!(items[1].name, a);
    }

    #[test]
    fn resolve_members_keeps_invalid_refs_and_dedupes() {
        let dir = std::env::temp_dir().join(format!(
            "deskkeeper_members_{}_{}",
            crate::util::now_secs(),
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("a.txt");
        std::fs::write(&real, b"a").unwrap();
        let real_raw = real.to_string_lossy().to_string();
        let missing_raw = dir.join("missing.txt").to_string_lossy().to_string();
        let members = vec![
            real_raw.clone(),
            real_raw.to_uppercase(),
            missing_raw.clone(),
            "   ".into(),
        ];
        let (items, invalid) = resolve_members(&members);
        assert_eq!(items.len(), 1);
        assert_eq!(invalid, vec![missing_raw]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn desktop_items_already_in_box_are_excluded() {
        let mut items = vec![
            item(r"C:\Users\Neil\Desktop\a.txt", false, 1, 0),
            item(r"C:\Users\Neil\Desktop\b.txt", false, 1, 0),
            item(r"C:\Users\Neil\Desktop\文件夹", true, 0, 0),
        ];
        let mut members = std::collections::HashSet::new();
        members.insert(path_key(Path::new(r"c:\users\neil\desktop\A.TXT")));
        members.insert(path_key(Path::new(r"C:\Users\Neil\Desktop\文件夹")));
        retain_non_members(&mut items, &members);
        assert_eq!(items.len(), 1);
        assert!(items[0].path.ends_with("b.txt"));
    }

    #[test]
    fn file_filter_maps_extensions_to_tabs() {
        let cases = [
            ("报告.docx", FileFilter::Docs),
            ("数据.xlsx", FileFilter::Sheets),
            ("讲稿.pptx", FileFilter::Slides),
            ("说明.pdf", FileFilter::Pdf),
            ("照片.PNG", FileFilter::Image),
            ("打包.zip", FileFilter::Archive),
            ("音乐.mp3", FileFilter::Other),
            ("快捷方式.lnk", FileFilter::Other),
            ("无扩展名", FileFilter::Other),
        ];
        for (name, expected) in cases {
            let it = item(name, false, 0, 0);
            assert!(
                file_filter_matches(expected, &it),
                "{name} 应命中 {}",
                expected.label()
            );
        }
        let dir = item("新建文件夹", true, 0, 0);
        assert!(file_filter_matches(FileFilter::All, &dir));
        assert!(file_filter_matches(FileFilter::Other, &dir));
        assert!(!file_filter_matches(FileFilter::Pdf, &dir));
    }

    #[test]
    fn file_filter_keeps_grouped_order_and_counts() {
        let mut items = vec![
            item("b.pdf", false, 0, 0),
            item("a.docx", false, 0, 0),
            item("新建文件夹", true, 0, 0),
            item("c.xlsx", false, 0, 0),
            item("d.zip", false, 0, 0),
            item("a.pdf", false, 0, 0),
        ];
        sort_items_grouped(&mut items);
        // PDF tab：数量正确，且保持类型分组内的自然顺序（a.pdf 在 b.pdf 前）。
        let pdf = filtered_indices(&items, FileFilter::Pdf);
        assert_eq!(pdf.len(), 2);
        assert_eq!(items[pdf[0]].name, "a.pdf");
        assert_eq!(items[pdf[1]].name, "b.pdf");
        // 全部 tab：顺序与真实顺序完全一致（A 的类型连续排序不被筛选改变）。
        let all = filtered_indices(&items, FileFilter::All);
        assert_eq!(all, (0..items.len()).collect::<Vec<_>>());
        assert_eq!(filtered_indices(&items, FileFilter::Docs).len(), 1);
        assert_eq!(filtered_indices(&items, FileFilter::Sheets).len(), 1);
        assert_eq!(filtered_indices(&items, FileFilter::Archive).len(), 1);
        // 文件夹与音视频/网页/程序同属「其他」。
        assert_eq!(filtered_indices(&items, FileFilter::Other).len(), 1);
        assert!(filtered_indices(&items, FileFilter::Slides).is_empty());
    }

    #[test]
    fn natural_sort_orders_numbers() {
        let mut items = vec![
            item("文件10.txt", false, 0, 0),
            item("文件2.txt", false, 0, 0),
            item("alpha", false, 0, 0),
        ];
        sort_items(&mut items, SortMode::Name);
        let names: Vec<_> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "文件2.txt", "文件10.txt"]);
    }

    #[test]
    fn directories_come_first() {
        let mut items = vec![item("a.txt", false, 5, 10), item("zdir", true, 0, 1)];
        sort_items(&mut items, SortMode::Size);
        assert!(items[0].is_dir);
    }

    #[test]
    fn sort_by_size_and_modified() {
        let mut items = vec![
            item("a", false, 10, 100),
            item("b", false, 30, 50),
            item("c", false, 20, 200),
        ];
        sort_items(&mut items, SortMode::Size);
        assert_eq!(items[0].name, "b");
        sort_items(&mut items, SortMode::Modified);
        assert_eq!(items[0].name, "c");
    }

    #[test]
    fn grid_layout_columns() {
        // 宽 400，padding 12，gap 16，icon 48 => cell 64；(400-24+16)/64 = 6.125 → 6 列
        let g = grid_layout(400.0, 300.0, 48.0, 20.0, 16.0, 12.0, 13);
        assert_eq!(g.cols, 6);
        assert_eq!(g.rows, 3);
        assert_eq!(g.total_rows_for(13), 3);
        let (x, y) = g.cell_pos(7);
        assert_eq!(x, 12.0 + 64.0);
        assert_eq!(y, 12.0 + 84.0);
    }

    #[test]
    fn grid_layout_minimums() {
        let g = grid_layout(10.0, 10.0, 48.0, 0.0, 16.0, 8.0, 1);
        assert_eq!(g.cols, 1);
        assert_eq!(g.rows, 1);
        let empty = grid_layout(400.0, 300.0, 48.0, 20.0, 16.0, 12.0, 0);
        assert_eq!(empty.rows, 0);
    }

    #[test]
    fn column_grid_fills_top_to_bottom() {
        let (x, y) = column_grid_pos(0, 4, 80.0, 90.0, 10.0);
        assert_eq!((x, y), (10.0, 10.0));
        let (x2, y2) = column_grid_pos(3, 4, 80.0, 90.0, 10.0);
        assert_eq!((x2, y2), (10.0, 10.0 + 3.0 * 90.0));
        let (x3, y3) = column_grid_pos(4, 4, 80.0, 90.0, 10.0);
        assert_eq!((x3, y3), (10.0 + 80.0, 10.0));
        // rows = 0 时按 1 处理，避免除零
        let (x4, _) = column_grid_pos(2, 0, 80.0, 90.0, 10.0);
        assert_eq!(x4, 10.0 + 2.0 * 80.0);
    }

    #[test]
    fn docked_boxes_stack_on_right_edge_without_overlap() {
        let work = (0.0, 0.0, 2560.0, 1392.0);
        let (w, h, margin, gap) = (600.0, 400.0, 24.0, 16.0);
        let (x1, y1) = docked_box_pos(work, w, h, margin, gap, &[]);
        assert_eq!((x1, y1), (1936.0, 24.0));
        let others = vec![(x1, y1, w, h)];
        let (x2, y2) = docked_box_pos(work, w, h, margin, gap, &others);
        assert_eq!((x2, y2), (x1, y1 + h + gap));
        let others = vec![(x1, y1, w, h), (x2, y2, w, h)];
        let (x3, y3) = docked_box_pos(work, w, h, margin, gap, &others);
        assert_eq!((x3, y3), (x1, y2 + h + gap));
        // 第三块之后右侧该列已满，第四块应左移一列并回到顶部。
        let others = vec![(x1, y1, w, h), (x2, y2, w, h), (x3, y3, w, h)];
        let (x4, y4) = docked_box_pos(work, w, h, margin, gap, &others);
        assert_eq!((x4, y4), (x1 - w - gap, 24.0));
    }

    #[test]
    fn visible_rows_respect_scroll() {
        let g = grid_layout(400.0, 300.0, 48.0, 20.0, 16.0, 12.0, 100);
        assert_eq!(g.cols, 6);
        let (first, last) = visible_row_range(&g, 0.0, 300.0, 100);
        assert_eq!(first, 0);
        assert!(last >= 4);
        let (first2, _) = visible_row_range(&g, g.cell_h * 3.0, 300.0, 100);
        assert_eq!(first2, 3);
    }

    #[test]
    fn desktop_cells_follow_each_monitor_scale() {
        // 150% 主屏 3840×2088 + 100% 副屏 1920×1080，窗口 pp 取主屏 1.5
        let mons = vec![
            MonitorArea {
                x: 0.0,
                y: 0.0,
                w: 3840.0,
                h: 2088.0,
                scale: 1.5,
            },
            MonitorArea {
                x: 3840.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0,
                scale: 1.0,
            },
        ];
        let cells = desktop_cells(&mons, 1200, 1.5, 32.0, 18.0, 2.0, true, 18.0);
        assert_eq!(cells.len(), 1200);
        // 主屏：icon=32pt，名称字号 8.64pt，两行标签区约 21.08pt + 间距 2pt，
        // cell_h≈73.08pt，rows=18，cols=50 => 900 个
        assert_eq!(cells[0].monitor, 0);
        assert!((cells[0].icon - 32.0).abs() < 0.01);
        assert_eq!(cells[899].monitor, 0);
        assert_eq!(cells[900].monitor, 1);
        // 副屏 100%：icon = 32 × (1.0/1.5) ≈ 21.33pt，k≈0.667
        assert!((cells[900].icon - 21.333).abs() < 0.01);
        assert!((cells[900].k - 0.6667).abs() < 0.001);
        // 副屏从自己的工作区左上角起排，而不是接着主屏坐标
        assert!((cells[900].x - (3840.0 / 1.5 + 18.0 * 0.6667)).abs() < 0.01);
    }

    #[test]
    fn desktop_board_cells_share_geometry_with_compact_layout() {
        let mons = vec![
            MonitorArea {
                x: 0.0,
                y: 0.0,
                w: 1024.0,
                h: 768.0,
                scale: 1.0,
            },
            MonitorArea {
                x: 1024.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0,
                scale: 1.25,
            },
        ];
        let args = (1.0, 32.0, 16.0, 2.0, true, 18.0);
        let compact = desktop_cells(&mons, 40, args.0, args.1, args.2, args.3, args.4, args.5);
        let board = desktop_board_cells(&mons, args.0, args.1, args.2, args.3, args.4, args.5, 40);
        assert_eq!(compact, board[..compact.len()]);
        assert!(board.len() > compact.len(), "棋盘应包含全部空格");
    }

    #[test]
    fn board_slots_keep_holes_and_use_first_column_major_free_cell() {
        let cells = cols_rows(2, 3);
        let items = vec![
            item("a", false, 0, 0),
            item("b", false, 0, 0),
            item("c", false, 0, 0),
        ];
        let slots = vec![slot("a", 0, 0, 1), slot("b", 0, 0, 2)];
        let (map, normalized) = assign_board_slots(&items, &cells, &slots);
        assert_eq!(map[0], Some(1), "a 保持原格");
        assert_eq!(map[1], Some(2), "b 保持原格");
        assert_eq!(map[2], Some(0), "c 取列优先第一个空格（col0,row0）");
        assert_eq!(normalized.len(), 3);

        // 只有 a、b 时，空出来的格子保持为空洞。
        let (map, _) = assign_board_slots(&items[..2], &cells, &slots);
        assert_eq!(map, vec![Some(1), Some(2)]);
        assert!(!map.contains(&Some(0)));

        // 列优先扫描：col0 未排满前不会横向铺到 col1。
        let slots2 = vec![slot("a", 0, 0, 0), slot("b", 0, 0, 1)];
        let (map2, _) = assign_board_slots(&items, &cells, &slots2);
        assert_eq!(map2[2], Some(2), "c 落 col0,row2，而不是 col1,row0");
    }

    #[test]
    fn system_items_take_first_two_cells_before_ordinary_items() {
        let cells = cols_rows(2, 3);
        // 普通项故意排在系统项之前，且系统项没有历史槽位。
        let mut items = vec![
            item("a", false, 0, 0),
            ItemMeta::computer(),
            ItemMeta::recycle_bin(false),
        ];
        let (map, _) = assign_board_slots(&items, &cells, &[]);
        assert_eq!(map[1], Some(0), "此电脑占 col0,row0");
        assert_eq!(map[2], Some(1), "回收站占 col0,row1");
        assert_eq!(map[0], Some(2), "普通项接在系统项之后（col0,row2）");

        system_first(&mut items);
        assert_eq!(items[0].name, "此电脑");
        assert_eq!(items[1].name, "回收站");
        assert_eq!(items[2].name, "a", "普通项相对顺序不变");
    }

    #[test]
    fn board_slots_clamp_when_grid_shrinks_without_losing_items() {
        let cells = cols_rows(2, 2);
        let items = vec![item("a", false, 0, 0), item("b", false, 0, 0)];
        let slots = vec![slot("a", 0, 0, 3), slot("b", 0, 0, 5)];
        let (map, normalized) = assign_board_slots(&items, &cells, &slots);
        assert_eq!(map[0], Some(1), "row 夹取到最大行 1（列优先下标 1）");
        assert_eq!(map[1], Some(0), "冲突项回填列优先第一个空格");
        assert_eq!(normalized.len(), 2, "不丢项");
        let keys: Vec<&str> = normalized.iter().map(|s| s.key.as_str()).collect();
        assert!(keys.contains(&"a") && keys.contains(&"b"));
    }

    #[test]
    fn board_item_cell_mapping_is_unique_and_matches_slot_coordinates() {
        // 预览锚点必须按 item→cell 的真实下标取格子；该测试保证映射唯一且槽位坐标一致。
        let cells = cols_rows(3, 3);
        let items = vec![
            ItemMeta::computer(),
            ItemMeta::recycle_bin(false),
            item("a", false, 0, 0),
            item("b", false, 0, 0),
            item("c", false, 0, 0),
        ];
        let slots = vec![slot("c", 0, 2, 0), slot("a", 0, 0, 2)];
        let (map, normalized) = assign_board_slots(&items, &cells, &slots);
        assert!(map.iter().all(|m| m.is_some()), "每项都必须落到一个格子");
        let mut seen = std::collections::HashSet::new();
        for &ci in map.iter().flatten() {
            assert!(seen.insert(ci), "同一格不能分给两个项");
        }
        for s in &normalized {
            let i = items
                .iter()
                .position(|it| path_key(&it.path) == s.key)
                .unwrap();
            let c = cells[map[i].unwrap()];
            assert_eq!(
                (c.monitor, c.col, c.row),
                (s.monitor, s.col, s.row),
                "槽位坐标必须与 item_cell 指向的格子一致（真实下标，不是显示序）"
            );
        }
        assert_eq!(cells[map[2].unwrap()].row, 2, "a 保持自己的 row=2 槽位");
        assert_eq!(cells[map[4].unwrap()].col, 2, "c 保持自己的 col=2 槽位");
    }

    #[test]
    fn swap_desktop_slots_exchanges_two_items() {
        let mut slots = vec![slot("a", 0, 0, 0), slot("b", 0, 1, 1)];
        assert!(swap_desktop_slots(&mut slots, "a", "b"));
        assert_eq!((slots[0].col, slots[0].row), (1, 1));
        assert_eq!((slots[1].col, slots[1].row), (0, 0));
        assert!(!swap_desktop_slots(&mut slots, "a", "missing"));
    }

    fn board_cell(monitor: usize, col: usize, row: usize) -> DesktopCell {
        DesktopCell {
            monitor,
            x: col as f32 * 10.0,
            y: row as f32 * 10.0,
            w: 10.0,
            h: 10.0,
            icon: 8.0,
            k: 1.0,
            col,
            row,
        }
    }

    /// 与 `desktop_cells` 一致的列优先顺序。
    fn cols_rows(cols: usize, rows: usize) -> Vec<DesktopCell> {
        let mut cells = Vec::new();
        for col in 0..cols {
            for row in 0..rows {
                cells.push(board_cell(0, col, row));
            }
        }
        cells
    }

    fn slot(key: &str, monitor: usize, col: usize, row: usize) -> DesktopSlot {
        DesktopSlot {
            key: key.to_string(),
            monitor,
            col,
            row,
        }
    }

    #[test]
    fn label_font_scales_with_icon_and_clamps() {
        // 24pt 图标落到下限，48/96pt 按 27% 比例
        assert_eq!(label_font_for_icon(16.0), LABEL_FONT_MIN);
        assert!((label_font_for_icon(32.0) - 8.64).abs() < 0.01);
        assert!((label_font_for_icon(64.0) - 17.28).abs() < 0.01);
        let f96 = label_font_for_icon(96.0);
        assert!((f96 - 25.92).abs() < 0.01);
        // 最大图标档仍需落在 25%–30% 的目标比例内
        assert!((f96 / 96.0) >= 0.25 && (f96 / 96.0) <= 0.30);
        // 标签区高度随字号线性增长
        assert!(label_block_height(17.28, 2.0) > label_block_height(8.64, 2.0) * 1.8);
    }

    /// 近似字宽：中文/全角按 1em，ASCII 按 0.55em（贴近 egui 默认比例字体）。
    fn approx_measure(font_size: f32) -> impl Fn(&str) -> f32 {
        move |s: &str| {
            s.chars()
                .map(|c| {
                    if c as u32 >= 0x2E80 {
                        font_size
                    } else {
                        font_size * 0.55
                    }
                })
                .sum()
        }
    }

    /// 断言每行的实际测量宽度都不超过单元格宽度。
    fn assert_lines_fit(lines: &[String], max_w: f32, measure: &impl Fn(&str) -> f32) {
        assert!(lines.len() <= LABEL_MAX_LINES, "最多两行");
        for line in lines {
            assert!(
                measure(line) <= max_w + 0.01,
                "行宽 {} 超过单元格 {}：{:?}",
                measure(line),
                max_w,
                line
            );
        }
    }

    #[test]
    fn label_lines_wrap_to_two_and_ellipsize() {
        let m = approx_measure(17.0);
        let short = label_lines_measured("1.pdf", 120.0, &m);
        assert_eq!(short, vec!["1.pdf".to_string()]);
        let wrapped = label_lines_measured("一个很长的桌面文件名abc.pdf", 60.0, &m);
        assert_eq!(wrapped.len(), 2);
        assert!(wrapped[1].ends_with('…'));
        assert_lines_fit(&wrapped, 60.0, &m);
    }

    #[test]
    fn label_lines_fit_cell_width_for_all_scripts() {
        let font = 24.0;
        let m = approx_measure(font);
        let cases = [
            "短.txt",
            "English filename.txt",
            "中英混合Mixed长名称2026.docx",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "这是一串非常非常长的中文名称没有空格直接连在一起用于验证换行与省略",
            "A",
        ];
        for max_w in [40.0f32, 60.0, 96.0, 160.0] {
            for name in cases {
                let lines = label_lines_measured(name, max_w, &m);
                assert_lines_fit(&lines, max_w, &m);
                assert!(!lines.is_empty(), "名称 {name:?} 不应为空");
                // 第一行不允许带省略号（省略号只出现在末行）
                if lines.len() == 1 && name.chars().count() > 0 && m(name) > max_w {
                    panic!("超宽名称 {name:?} 却只输出一行且未省略");
                }
            }
        }
    }

    #[test]
    fn label_lines_never_exceed_two_lines() {
        let m = approx_measure(18.0);
        let long = "超长名称".repeat(60);
        let lines = label_lines_measured(&long, 50.0, &m);
        assert_eq!(lines.len(), 2);
        assert_lines_fit(&lines, 50.0, &m);
    }

    #[test]
    fn grid_reserves_two_line_label_height() {
        let icon = 96.0;
        let font = label_font_for_icon(icon);
        let label_h = label_block_height(font, 2.0);
        let grid = grid_layout(1000.0, 800.0, icon, label_h, 16.0, 10.0, 30);
        let two_lines = font * LABEL_LINE_RATIO * 2.0;
        assert!(grid.cell_h >= icon + two_lines, "格子高度需容纳两行标签");
        // 图标与文字间距完整保留在格子内，文字不会压到下一行。
        assert!(grid.cell_h - icon - two_lines >= 2.0);
    }

    #[test]
    fn label_gap_grows_label_block_without_overlap() {
        let font = label_font_for_icon(96.0);
        let two_lines = font * LABEL_LINE_RATIO * 2.0;
        for gap in [0.0_f32, 2.0, 8.0, 16.0] {
            let block = label_block_height(font, gap);
            // 文字区高度不随间距变化，间距只增加预留高度。
            assert!((block - two_lines - gap).abs() < 0.01);
            let grid = grid_layout(800.0, 600.0, 96.0, block, 16.0, 8.0, 10);
            assert!((grid.cell_h - 96.0 - block - 16.0).abs() < 0.01);
        }
    }

    #[test]
    fn display_name_strips_only_shortcut_suffixes() {
        // 常规快捷方式：去掉后缀，保留其余的所有点号段
        assert_eq!(display_name("报告.lnk", false), "报告");
        assert_eq!(display_name("a.b.c.lnk", false), "a.b.c");
        assert_eq!(display_name("网页快捷方式.url", false), "网页快捷方式");
        // 大写/混合大小写
        assert_eq!(display_name("FOO.LNK", false), "FOO");
        assert_eq!(display_name("Bar.Url", false), "Bar");
        // 文件夹一律不动（即使名字以 .lnk 结尾）
        assert_eq!(display_name("weird.lnk", true), "weird.lnk");
        // 非快捷方式后缀保持原样
        assert_eq!(display_name("readme.txt", false), "readme.txt");
        assert_eq!(display_name("photo.lnk.bak", false), "photo.lnk.bak");
        // 只有后缀时保持原样，避免显示为空
        assert_eq!(display_name(".lnk", false), ".lnk");
        assert_eq!(display_name(".url", false), ".url");
        // 多字节文件名：不能因为按字节截取而 panic 或越界
        assert_eq!(display_name("中文名称.lnk", false), "中文名称");
        assert_eq!(display_name("中文名称", false), "中文名称");
        assert_eq!(display_name("a中b.LNK", false), "a中b");
    }

    #[test]
    fn item_display_name_matches_pure_function() {
        let it = ItemMeta {
            path: PathBuf::from("C:/Users/x/Desktop/项目.计划.lnk"),
            name: "项目.计划.lnk".to_string(),
            is_dir: false,
            size: 0,
            modified: 0,
            system: None,
        };
        assert_eq!(it.display_name(), "项目.计划");
        // 真实文件名不受影响
        assert_eq!(it.name, "项目.计划.lnk");
        assert_eq!(
            it.path.file_name().unwrap().to_string_lossy(),
            "项目.计划.lnk"
        );
    }

    #[test]
    fn computer_item_is_marked_system_and_kept_out_of_members() {
        let pc = ItemMeta::computer();
        assert!(pc.is_system());
        assert_eq!(pc.name, "此电脑");
        assert!(ItemMeta::is_computer_path(&pc.path));
        assert!(ItemMeta::is_system_path(&pc.path));
        let mut items = vec![pc.clone()];
        let members: std::collections::HashSet<String> = [path_key(&pc.path)].into_iter().collect();
        // 即使有人误把该路径写进成员集合，桌面排除也要剔除它（不会被吞进盒子）。
        retain_non_members(&mut items, &members);
        assert!(items.is_empty());
    }

    #[test]
    fn recycle_bin_item_tracks_state_and_namespace() {
        for full in [false, true] {
            let bin = ItemMeta::recycle_bin(full);
            assert!(bin.is_system());
            assert_eq!(bin.name, "回收站");
            assert!(bin.is_dir);
            assert!(ItemMeta::is_recycle_bin_path(&bin.path));
            assert!(ItemMeta::is_system_path(&bin.path));
            assert_eq!(bin.system, Some(SystemItem::RecycleBin { full }));
            // 状态不同 => 系统项不相等，UI 可据此切换到「满/空」图标。
            assert_ne!(
                bin,
                ItemMeta::recycle_bin(!full),
                "回收站空/非空状态应可区分"
            );
        }
        // 与「此电脑」互不误判。
        assert!(!ItemMeta::is_recycle_bin_path(Path::new(
            COMPUTER_CLSID_PATH
        )));
        assert!(!ItemMeta::is_computer_path(Path::new(
            RECYCLE_BIN_CLSID_PATH
        )));
    }

    #[test]
    fn snap_rect_snaps_edges_and_adjacency() {
        let work = (0.0, 0.0, 1000.0, 800.0);
        // 靠近工作区左边 6px → 吸附到 x=0
        let r = snap_rect(6.0, 100.0, 200.0, 100.0, work, &[], 12.0);
        assert_eq!(r.x, 0.0);
        assert_eq!(r.y, 100.0);
        assert!(r.guides.iter().any(|g| g.vertical && g.at == 0.0));
        // 右边贴合工作区右边
        let r = snap_rect(795.0, 100.0, 200.0, 100.0, work, &[], 12.0);
        assert_eq!(r.x, 800.0);
        // 与其它盒子相邻拼接（我们的右边贴对方左边）
        let others = [(500.0, 0.0, 200.0, 200.0)];
        let r = snap_rect(294.0, 0.0, 200.0, 100.0, work, &others, 12.0);
        assert_eq!(r.x, 300.0);
        assert!(r.guides.iter().any(|g| g.vertical && g.at == 500.0));
        // 与其它盒子上边对齐
        let r = snap_rect(100.0, 195.0, 200.0, 100.0, work, &others, 12.0);
        assert_eq!(r.y, 200.0);
        // 超出阈值不吸附
        let r = snap_rect(40.0, 40.0, 200.0, 100.0, work, &[], 12.0);
        assert_eq!((r.x, r.y), (40.0, 40.0));
        assert!(r.guides.is_empty());
        // 阈值 0 关闭吸附
        let r = snap_rect(6.0, 6.0, 200.0, 100.0, work, &[], 0.0);
        assert_eq!((r.x, r.y), (6.0, 6.0));
    }

    #[test]
    fn box_hit_test_accepts_inside_and_rejects_outside() {
        // 盒子内容区 100..400 x 200..500
        let content: RectF = (100.0, 200.0, 300.0, 300.0);
        // 中心
        assert!(point_in_rect(250.0, 350.0, content));
        // 四条边及四角（含边界）
        assert!(point_in_rect(100.0, 200.0, content));
        assert!(point_in_rect(400.0, 200.0, content));
        assert!(point_in_rect(100.0, 500.0, content));
        assert!(point_in_rect(400.0, 500.0, content));
        assert!(point_in_rect(400.0, 350.0, content));
        assert!(point_in_rect(250.0, 500.0, content));
        // 边外 1px
        assert!(!point_in_rect(99.0, 350.0, content));
        assert!(!point_in_rect(401.0, 350.0, content));
        assert!(!point_in_rect(250.0, 199.0, content));
        assert!(!point_in_rect(250.0, 501.0, content));
        // 零面积
        assert!(!point_in_rect(0.0, 0.0, (0.0, 0.0, 0.0, 10.0)));
    }

    #[test]
    fn box_hit_rect_is_clipped_to_content() {
        let content: RectF = (100.0, 200.0, 300.0, 300.0);
        // 半格滑出内容区：只有交集部分可交互
        let half = intersect_rect((80.0, 210.0, 100.0, 80.0), content).unwrap();
        assert_eq!(half, (100.0, 210.0, 80.0, 80.0));
        // 完全在内容区
        let inner = intersect_rect((150.0, 250.0, 60.0, 60.0), content).unwrap();
        assert_eq!(inner, (150.0, 250.0, 60.0, 60.0));
        // 完全在盒子外：不产生命中区域
        assert_eq!(intersect_rect((0.0, 0.0, 80.0, 80.0), content), None);
        assert_eq!(intersect_rect((420.0, 250.0, 60.0, 60.0), content), None);
        // 仅贴边（零面积）同样不命中
        assert_eq!(intersect_rect((400.0, 250.0, 60.0, 60.0), content), None);
    }

    #[test]
    fn drag_position_clamps_inside_box_content() {
        let content: RectF = (100.0, 200.0, 300.0, 300.0);
        let size = 64.0;
        // 四个方向越界都被夹回，且整块图标留在内容区
        for (px, py) in [
            (0.0, 0.0),
            (1000.0, 200.0),
            (100.0, 1000.0),
            (1000.0, 1000.0),
            (-30.0, 460.0),
        ] {
            let (x, y) = clamp_icon_pos(px, py, size, content);
            assert!(point_in_rect(x, y, content));
            assert!(point_in_rect(x + size, y + size, content));
            assert_eq!(
                intersect_rect((x, y, size, size), content).unwrap(),
                (x, y, size, size)
            );
        }
        // 正常范围内不改变
        assert_eq!(clamp_icon_pos(150.0, 250.0, size, content), (150.0, 250.0));
        // 右下极限
        assert_eq!(clamp_icon_pos(999.0, 999.0, size, content), (336.0, 436.0));
        // 内容区比图标还小时退化到左上角，交给 clip 兜底
        let tiny: RectF = (10.0, 20.0, 40.0, 30.0);
        assert_eq!(clamp_icon_pos(0.0, 0.0, 64.0, tiny), (10.0, 20.0));
    }

    #[test]
    fn grouped_sort_keeps_extension_families_together() {
        let mut items = vec![
            item("报告10.pdf", false, 0, 0),
            item("图片.png", false, 0, 0),
            item("表格.xlsx", false, 0, 0),
            item("报告2.pdf", false, 0, 0),
            item("文档.docx", false, 0, 0),
            item("文档.doc", false, 0, 0),
            item("表格.xls", false, 0, 0),
            item("网页.url", false, 0, 0),
            item("歌曲.mp3", false, 0, 0),
            item("压缩包.zip", false, 0, 0),
            item("程序.exe", false, 0, 0),
            item("未知.bin", false, 0, 0),
            item("目录", true, 0, 0),
        ];
        sort_items_grouped(&mut items);
        let names: Vec<_> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "目录",
                "文档.doc",
                "文档.docx",
                "表格.xls",
                "表格.xlsx",
                "报告2.pdf",
                "报告10.pdf",
                "图片.png",
                "歌曲.mp3",
                "压缩包.zip",
                "网页.url",
                "程序.exe",
                "未知.bin",
            ]
        );
    }

    #[test]
    fn category_and_family_map_variants() {
        assert_eq!(category_of("docx"), category_of("doc"));
        assert_eq!(category_of("JPEG"), category_of("png"));
        assert_eq!(category_of("url"), category_of("html"));
        assert_eq!(extension_family("docx"), "doc");
        assert_eq!(extension_family("jpeg"), "jpg");
        assert_eq!(extension_family("mp4"), "mp4");
        assert_eq!(category_of("exe"), 8);
        assert_eq!(category_of("unknown"), 9);
    }

    #[test]
    fn nav_target_grid_rows_and_edges() {
        // 4 列 9 项：0..8，最后一行只有 1 项
        assert_eq!(nav_target(NavDir::Right, 4, 9, 0), 1);
        assert_eq!(nav_target(NavDir::Right, 4, 9, 3), 3); // 行尾不动
        assert_eq!(nav_target(NavDir::Left, 4, 9, 4), 4); // 行首不动
        assert_eq!(nav_target(NavDir::Down, 4, 9, 0), 4);
        assert_eq!(nav_target(NavDir::Down, 4, 9, 8), 8); // 无下一行
        assert_eq!(nav_target(NavDir::Up, 4, 9, 4), 0);
        assert_eq!(nav_target(NavDir::Up, 4, 9, 0), 0);
        // 从第三行第 2 列向下：下一行该列不存在 → 落到最后一项
        assert_eq!(nav_target(NavDir::Down, 4, 9, 6), 8);
    }

    #[test]
    fn nav_target_list_moves_one_step() {
        assert_eq!(nav_target(NavDir::Down, 1, 5, 0), 1);
        assert_eq!(nav_target(NavDir::Up, 1, 5, 0), 0);
        assert_eq!(nav_target(NavDir::Down, 1, 5, 4), 4);
        assert_eq!(nav_target(NavDir::Right, 1, 5, 2), 3);
        assert_eq!(nav_target(NavDir::Left, 1, 5, 0), 0);
        assert_eq!(nav_target(NavDir::Down, 1, 0, 0), 0);
    }

    #[test]
    fn marquee_hits_intersect_cells() {
        let cells = vec![
            (0, (10.0, 10.0, 100.0, 80.0)),
            (1, (110.0, 10.0, 100.0, 80.0)),
            (2, (10.0, 90.0, 100.0, 80.0)),
        ];
        // 框住第一格右半与第二格左半
        assert_eq!(marquee_hits(&cells, (60.0, 20.0, 60.0, 30.0)), vec![0, 1]);
        // 只轻触第二行第一格（与第一行恰好无重叠面积）
        assert_eq!(marquee_hits(&cells, (90.0, 90.0, 30.0, 10.0)), vec![2]);
        // 纯竖直拖动（零宽）按 1px 细线命中第一列两格
        assert_eq!(marquee_hits(&cells, (50.0, 10.0, 0.0, 120.0)), vec![0, 2]);
        // 完全在外
        assert!(marquee_hits(&cells, (300.0, 300.0, 20.0, 20.0)).is_empty());
    }

    #[test]
    fn range_endpoints_normalize_order() {
        assert_eq!(range_endpoints(2, 7), (2, 7));
        assert_eq!(range_endpoints(7, 2), (2, 7));
        assert_eq!(range_endpoints(4, 4), (4, 4));
    }
}
