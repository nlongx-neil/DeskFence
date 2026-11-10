//! 配置数据模型、TOML 读写与节流保存线程。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    Auto,
    Light,
    Dark,
}

impl ThemeMode {
    pub const ALL: [ThemeMode; 3] = [ThemeMode::Auto, ThemeMode::Light, ThemeMode::Dark];
    pub fn label(self) -> &'static str {
        match self {
            ThemeMode::Auto => "跟随系统",
            ThemeMode::Light => "浅色",
            ThemeMode::Dark => "深色",
        }
    }
}

/// 盒子类型：普通 / 自动归类文件 / 自动归类目录 / 剪贴板 / 待办。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BoxKind {
    #[default]
    Normal,
    /// 自动归类盒子：普通文件（按扩展名族分组连续排列）
    Files,
    /// 自动归类盒子：文件夹
    Dirs,
    /// 剪贴板历史（数据来自剪贴板 store，不落盘为目录）
    Clipboard,
    /// 待办清单（数据保存在配置中）
    Todo,
}

impl BoxKind {
    pub fn is_group(&self) -> bool {
        matches!(self, BoxKind::Files | BoxKind::Dirs)
    }

    /// 是否使用磁盘目录承载内容（剪贴板/待办为虚拟盒子）。
    pub fn uses_dir(&self) -> bool {
        matches!(self, BoxKind::Normal | BoxKind::Files | BoxKind::Dirs)
    }
}

/// 单个文件大类在自动归类时的去向。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GroupTarget {
    /// 文件夹 → 目录盒子，文件 → 文件盒子
    #[default]
    Auto,
    Files,
    Dirs,
    /// 不自动归类
    Off,
}

impl GroupTarget {
    pub const ALL: [GroupTarget; 4] = [
        GroupTarget::Auto,
        GroupTarget::Files,
        GroupTarget::Dirs,
        GroupTarget::Off,
    ];

    pub fn label(self) -> &'static str {
        match self {
            GroupTarget::Auto => "自动",
            GroupTarget::Files => "文件盒子",
            GroupTarget::Dirs => "目录盒子",
            GroupTarget::Off => "不归类",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortMode {
    #[default]
    Name,
    Type,
    Modified,
    Size,
}

impl SortMode {
    pub const ALL: [SortMode; 4] = [
        SortMode::Name,
        SortMode::Type,
        SortMode::Modified,
        SortMode::Size,
    ];
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Name => "名称",
            SortMode::Type => "类型",
            SortMode::Modified => "修改时间",
            SortMode::Size => "大小",
        }
    }
}

/// 拖到已被占用的格子时的处理方式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OccupiedDropMode {
    /// 两项互换格子（默认）
    #[default]
    Swap,
    /// 在落点新建盒子并收纳两项
    NewBox,
}

impl OccupiedDropMode {
    pub fn label(self) -> &'static str {
        match self {
            OccupiedDropMode::Swap => "交换位置",
            OccupiedDropMode::NewBox => "生成新盒子",
        }
    }
}

/// 桌面图标的棋盘占位：路径键 → 固定占用的格子（显示器序号 + 列/行）。
///
/// 存格子坐标而不是屏幕像素：绘制时由桌面网格按 `monitor + col + row` 换算，
/// DPI/分辨率变化不需要改数据。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopSlot {
    /// 图标路径稳定键（[`crate::model::path_key`] 形式）
    pub key: String,
    /// 显示器序号（`DeskApp::monitors` 的枚举下标）
    pub monitor: usize,
    /// 网格列（自左向右）
    pub col: usize,
    /// 网格行（自上而下）
    pub row: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopConfig {
    /// 图标像素尺寸（物理像素）
    pub icon_size: u32,
    /// 网格间距（物理像素）
    pub spacing: u32,
    /// 图标与名称之间的间距（逻辑点，按屏 DPI 缩放）
    pub label_gap: f32,
    pub show_labels: bool,
    pub hide_system_icons: bool,
    pub animations: bool,
    /// 盒子背景不透明度（0-255 的 alpha，仅影响背景填充）
    pub box_opacity: u8,
    /// 盒子拖动磁吸开关
    pub snap_enabled: bool,
    /// 盒子磁吸阈值（物理像素）
    pub snap_threshold: u32,
    /// 桌面散落图标的默认排序
    pub sort: SortMode,
    /// 新建盒子的默认尺寸（逻辑点）与配色
    pub box_defaults: BoxDefaults,
    /// 新建盒子的默认内容视图
    pub default_box_view: BoxView,
    /// 新出现的桌面图标自动归类到文件/目录盒子
    pub auto_group: bool,
    /// 首次启动是否已创建过默认分类盒子（用户删除后不再重建）
    pub group_boxes_created: bool,
    /// 各文件大类的去向，下标见 [`crate::model::category_of`]；缺省视为「自动」
    pub category_targets: Vec<GroupTarget>,
    /// 用户手动移出盒子后记录路径，自动归类不再拉回
    pub manual_overrides: Vec<String>,
    /// 桌面图标的用户手动顺序（[`crate::model::path_key`] 形式）。
    ///
    /// **已废弃，仅作一次性迁移输入**：启用棋盘摆放时，程序按此顺序生成
    /// [`Self::slots`] 快照后即清空该字段。之后位置唯一来源是 [`Self::slots`]，
    /// 不再叠加任何排序压缩，避免两套顺序来源互相覆盖。
    pub manual_order: Vec<String>,
    /// 是否启用棋盘占位摆放（拖到任意格固定位置）。
    ///
    /// `false` 且 [`Self::slots`] 为空时按 [`Self::sort`] 紧凑排列；
    /// `true` 时位置唯一来源是 [`Self::slots`]。
    pub board_placement: bool,
    /// 每个图标固定占用的格子（[`DesktopSlot`] 形式）。
    pub slots: Vec<DesktopSlot>,
    /// 拖到已占格时的处理方式。
    pub drop_on_occupied: OccupiedDropMode,
}

/// 新建盒子的默认外观。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BoxDefaults {
    /// 逻辑点（乘以 DPI 缩放后为物理像素）
    pub w: f32,
    pub h: f32,
    pub color: [u8; 3],
    pub alpha: u8,
}

impl Default for BoxDefaults {
    fn default() -> Self {
        Self {
            w: 420.0,
            h: 320.0,
            color: [58, 78, 120],
            alpha: 16,
        }
    }
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            icon_size: 48,
            spacing: 18,
            label_gap: 2.0,
            show_labels: true,
            hide_system_icons: true,
            animations: true,
            box_opacity: 16,
            snap_enabled: true,
            snap_threshold: 12,
            sort: SortMode::Name,
            box_defaults: BoxDefaults::default(),
            default_box_view: BoxView::default(),
            auto_group: true,
            group_boxes_created: false,
            category_targets: vec![GroupTarget::Auto; crate::model::CATEGORY_COUNT],
            manual_overrides: Vec::new(),
            manual_order: Vec::new(),
            board_placement: false,
            slots: Vec::new(),
            drop_on_occupied: OccupiedDropMode::default(),
        }
    }
}

/// 待办的重复规则。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RepeatRule {
    /// 只提醒一次
    #[default]
    Once,
    /// 每天同一本地时刻
    Daily,
    /// 工作日（周一至周五）同一本地时刻
    Weekdays,
    /// 固定间隔（每 [`TodoItem::interval_minutes`] 分钟）
    Interval,
}

impl RepeatRule {
    pub const ALL: [RepeatRule; 4] = [
        RepeatRule::Once,
        RepeatRule::Daily,
        RepeatRule::Weekdays,
        RepeatRule::Interval,
    ];

    pub fn label(self) -> &'static str {
        match self {
            RepeatRule::Once => "一次",
            RepeatRule::Daily => "每天",
            RepeatRule::Weekdays => "工作日",
            RepeatRule::Interval => "周期",
        }
    }

    pub fn is_repeating(self) -> bool {
        !matches!(self, RepeatRule::Once)
    }
}

/// 固定间隔的相位基准（仅在 `repeat = interval` 时生效）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IntervalBase {
    /// 按到期时刻推进：固定相位，不随完成时间漂移。
    #[default]
    DueTime,
    /// 按完成时刻推进：每次完成后再起算下一个周期。
    CompletionTime,
}

/// 固定间隔的最小粒度（分钟）：低于该值视为噪音，写回时收敛到该值。
pub const MIN_INTERVAL_MIN: u32 = 5;

/// 固定间隔的预设项（分钟）：30 分钟 / 1 小时 / 2 小时 / 4 小时。
pub const INTERVAL_PRESETS_MIN: [u32; 4] = [30, 60, 120, 240];

/// 「只设日期」时的默认提醒时刻（本地分钟，09:00）。
pub const DEFAULT_DAILY_REMINDER_MINUTE: u16 = 9 * 60;

/// 待办项（保存在配置中，不落盘为文件）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TodoItem {
    pub id: String,
    pub text: String,
    pub done: bool,
    /// 当前有效到期点的 UTC epoch 秒；`None` = 无到期时间。
    pub due_at: Option<i64>,
    /// 只设日期：`due_at` 存当天 00:00 本地时间，提醒时刻取全局「每日提醒时刻」。
    pub all_day: bool,
    /// 本地日期（自 1970-01-01 起的天数），日历类重复的锚点。
    pub local_date: Option<i32>,
    /// 本地 0-1439 分钟，日历类重复的锚点。
    pub local_minute: u16,
    /// 重复规则。
    pub repeat: RepeatRule,
    /// 固定间隔的周期长度（分钟，下限 [`MIN_INTERVAL_MIN`]；0 表示未设置）。
    pub interval_min: u32,
    /// 固定间隔的相位基准。
    pub interval_base: IntervalBase,
    /// 提前提醒的分钟数。
    pub lead_min: u16,
    /// 稍后提醒的抑制窗口截止（UTC epoch 秒），不改 `due_at`。
    pub snooze_until: Option<i64>,
    /// 已提醒到的到期点（UTC epoch 秒），用于幂等去重。
    pub last_fired: Option<i64>,
}

impl TodoItem {
    /// 实际生效的周期长度（分钟）；未设置或低于下限时收敛到下限。
    pub fn interval_minutes(&self) -> u32 {
        self.interval_min.max(MIN_INTERVAL_MIN)
    }
}

/// 待办盒排序方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoSort {
    /// 手动顺序（新增追加到末尾）
    #[default]
    Manual,
    /// 按名称自然排序
    Name,
    /// 未完成在前，已完成在后（同组保持原顺序）
    Pending,
}

impl TodoSort {
    pub const ALL: [TodoSort; 3] = [TodoSort::Manual, TodoSort::Name, TodoSort::Pending];
    pub fn label(self) -> &'static str {
        match self {
            TodoSort::Manual => "手动",
            TodoSort::Name => "名称",
            TodoSort::Pending => "未完成优先",
        }
    }
}

/// 盒子内容视图模式（每个盒子独立保存）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoxView {
    /// 大图标网格（原有行为）
    #[default]
    Icon,
    /// 单行一项：小图标 + 名称 + 大小/修改时间
    List,
    /// 缩略图网格：图片/PDF/Office 用系统缩略图，失败回退图标
    Thumb,
}

impl BoxView {
    pub const ALL: [BoxView; 3] = [BoxView::Icon, BoxView::List, BoxView::Thumb];

    pub fn label(self) -> &'static str {
        match self {
            BoxView::Icon => "图标",
            BoxView::List => "列表",
            BoxView::Thumb => "缩略图",
        }
    }
}

/// 盒子布局（坐标均为虚拟屏幕物理像素）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BoxConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: BoxKind,
    /// 剪贴板盒的类型筛选
    #[serde(default)]
    pub clip_filter: ClipFilter,
    /// 文件盒/普通盒的类型 tab 筛选（每盒独立保存）
    #[serde(default)]
    pub file_filter: FileFilter,
    /// 待办盒内容
    #[serde(default)]
    pub todos: Vec<TodoItem>,
    /// 待办盒排序方式
    #[serde(default)]
    pub todo_sort: TodoSort,
    /// 盒子内容视图（图标/列表/缩略图），每盒独立持久化
    #[serde(default)]
    pub view: BoxView,
    /// 盒子成员：源文件的绝对路径（虚拟引用，不移动/复制文件）。
    #[serde(default)]
    pub members: Vec<String>,
    /// 用户在盒子内拖动形成的手动顺序（路径 key）；为空时完全按 `sort` 排序。
    #[serde(default)]
    pub manual_order: Vec<String>,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: [u8; 3],
    pub alpha: u8,
    pub collapsed: bool,
    pub locked: bool,
    /// 已关闭：从桌面隐藏，但成员/待办/几何/样式全部保留，可从设置页重新打开。
    #[serde(default)]
    pub closed: bool,
    pub sort: SortMode,
}

impl Default for BoxConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: "新盒子".to_string(),
            kind: BoxKind::Normal,
            clip_filter: ClipFilter::All,
            file_filter: FileFilter::All,
            todos: Vec::new(),
            todo_sort: TodoSort::Manual,
            view: BoxView::default(),
            members: Vec::new(),
            manual_order: Vec::new(),
            x: 0.0,
            y: 0.0,
            w: 420.0,
            h: 320.0,
            color: [58, 78, 120],
            alpha: 16,
            collapsed: false,
            locked: false,
            closed: false,
            sort: SortMode::Name,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipFilter {
    #[default]
    All,
    Text,
    Image,
    Files,
}

/// 文件盒类型 tab：只做「显示筛选」，不改文件位置，也不参与排序。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileFilter {
    #[default]
    All,
    Docs,
    Sheets,
    Slides,
    Pdf,
    Image,
    Archive,
    /// 音视频、网页、程序、无扩展名与文件夹
    Other,
}

impl FileFilter {
    pub const TABS: [FileFilter; 8] = [
        FileFilter::All,
        FileFilter::Docs,
        FileFilter::Sheets,
        FileFilter::Slides,
        FileFilter::Pdf,
        FileFilter::Image,
        FileFilter::Archive,
        FileFilter::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FileFilter::All => "全部",
            FileFilter::Docs => "文档",
            FileFilter::Sheets => "表格",
            FileFilter::Slides => "演示",
            FileFilter::Pdf => "PDF",
            FileFilter::Image => "图片",
            FileFilter::Archive => "压缩包",
            FileFilter::Other => "其他",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    pub enabled: bool,
    pub max_items: usize,
    /// 图片缩略图总容量上限（字节）
    pub max_bytes: u64,
    /// 超过该长度的文本不记录（字符数）
    pub max_text_len: usize,
    pub ignore_password_windows: bool,
    /// 忽略的应用进程名（小写，含 .exe）
    pub ignored_processes: Vec<String>,
    pub auto_paste: bool,
    /// 全局热键描述，如 `Ctrl+Shift+V`
    pub hotkey: String,
    pub capture_images: bool,
    pub capture_files: bool,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_items: 200,
            max_bytes: 512 * 1024 * 1024,
            max_text_len: 100_000,
            ignore_password_windows: true,
            ignored_processes: vec!["keepass.exe".to_string(), "1password.exe".to_string()],
            auto_paste: true,
            hotkey: "Ctrl+Shift+V".to_string(),
            capture_images: true,
            capture_files: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub autostart: bool,
    pub show_overlay: bool,
    pub log_debug: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            autostart: false,
            show_overlay: true,
            log_debug: false,
        }
    }
}

/// 待办到期提醒的全局设置。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReminderConfig {
    /// 提醒总开关（关闭后不扫描、不弹气泡，盒内文案仍显示）。
    pub enabled: bool,
    /// 「只设日期」的提醒时刻（本地分钟，默认 09:00）。
    pub daily_minute: u16,
    /// 新建到期时间时的默认提前量（分钟）。
    pub lead_min: u16,
    /// 稍后提醒的默认时长（分钟）。
    pub snooze_min: u16,
    /// 是否用系统托盘气泡提醒。
    pub balloon: bool,
}

impl Default for ReminderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            daily_minute: DEFAULT_DAILY_REMINDER_MINUTE,
            lead_min: 0,
            snooze_min: 10,
            balloon: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub version: u32,
    pub theme: ThemeMode,
    pub desktop: DesktopConfig,
    pub boxes: Vec<BoxConfig>,
    pub clipboard: ClipboardConfig,
    pub reminder: ReminderConfig,
    pub general: GeneralConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 1,
            theme: ThemeMode::Auto,
            desktop: DesktopConfig::default(),
            boxes: Vec::new(),
            clipboard: ClipboardConfig::default(),
            reminder: ReminderConfig::default(),
            general: GeneralConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn load(path: &PathBuf) -> (Self, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<AppConfig>(&text) {
                Ok(cfg) => (cfg, None),
                Err(e) => {
                    log::warn!("配置文件解析失败，使用默认配置: {e}");
                    (AppConfig::default(), Some(format!("配置文件解析失败：{e}")))
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (AppConfig::default(), None),
            Err(e) => {
                log::warn!("读取配置文件失败: {e}");
                (AppConfig::default(), Some(format!("读取配置文件失败：{e}")))
            }
        }
    }

    /// 紧凑（非 pretty）TOML 写入，先写临时文件再原子替换。
    pub fn save_now(path: &PathBuf, cfg: &AppConfig) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string(cfg)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// 节流保存器：400ms 内的多次请求合并为一次写盘（超时由后台线程 `recv_timeout` 触发）。
pub struct ConfigSaver {
    tx: Sender<AppConfig>,
    path: PathBuf,
}

impl ConfigSaver {
    pub fn new(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let worker_path = path.clone();
        std::thread::Builder::new()
            .name("config-saver".into())
            .spawn(move || run_saver(rx, worker_path))
            .ok();
        Self { tx, path }
    }

    /// 请求保存（非阻塞）。通道断开返回 false。
    pub fn request(&self, cfg: &AppConfig) -> bool {
        self.tx.send(cfg.clone()).is_ok()
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// 立即同步保存（退出时使用）。
    pub fn flush_now(&self, cfg: &AppConfig) {
        if let Err(e) = AppConfig::save_now(&self.path, cfg) {
            log::error!("退出时保存配置失败: {e}");
        }
    }
}

fn run_saver(rx: Receiver<AppConfig>, path: PathBuf) {
    let mut pending: Option<AppConfig> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(400)) {
            Ok(cfg) => pending = Some(cfg),
            Err(RecvTimeoutError::Timeout) => {
                if let Some(cfg) = pending.take() {
                    if let Err(e) = AppConfig::save_now(&path, &cfg) {
                        log::error!("保存配置失败: {e}");
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(cfg) = pending.take() {
                    if let Err(e) = AppConfig::save_now(&path, &cfg) {
                        log::error!("保存配置失败: {e}");
                    }
                }
                break;
            }
        }
    }
}

/// 解析 `Ctrl+Shift+V` 形式的快捷键为 (ctrl, alt, shift, win, 键名)。
pub fn parse_hotkey(s: &str) -> Option<(bool, bool, bool, bool, String)> {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut win = false;
    let mut key = None;
    for part in s.split('+').map(|p| p.trim()) {
        if part.is_empty() {
            continue;
        }
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            "win" | "super" | "cmd" => win = true,
            other => {
                if key.is_some() {
                    return None;
                }
                key = Some(other.to_uppercase());
            }
        }
    }
    key.map(|k| (ctrl, alt, shift, win, k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let mut cfg = AppConfig::default();
        cfg.boxes.push(BoxConfig {
            id: "b1".into(),
            name: "工作".into(),
            kind: BoxKind::Normal,
            clip_filter: ClipFilter::All,
            file_filter: FileFilter::All,
            todos: Vec::new(),
            todo_sort: TodoSort::Manual,
            view: BoxView::default(),
            members: vec![r"C:\Users\Neil\Desktop\a.txt".into()],
            manual_order: Vec::new(),
            x: 10.0,
            y: 20.0,
            w: 300.0,
            h: 200.0,
            color: [20, 40, 80],
            alpha: 180,
            collapsed: true,
            locked: true,
            closed: false,
            sort: SortMode::Modified,
        });
        cfg.clipboard.max_items = 42;
        let text = toml::to_string(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.clipboard.max_items, 42);
        assert_eq!(back.boxes.len(), 1);
        assert_eq!(back.boxes[0].name, "工作");
        assert!(back.boxes[0].locked);
        assert_eq!(back.boxes[0].sort, SortMode::Modified);
        assert_eq!(back.boxes[0].members.len(), 1);
    }

    #[test]
    fn legacy_box_without_members_still_loads() {
        let text = r#"
version = 1

[[boxes]]
id = "box-old"
name = "旧盒子"
kind = "normal"
x = 0.0
y = 0.0
w = 400.0
h = 300.0
color = [58, 78, 120]
alpha = 190
collapsed = false
locked = false
sort = "name"
"#;
        let cfg: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.boxes.len(), 1);
        assert!(cfg.boxes[0].members.is_empty());
    }

    #[test]
    fn closed_and_multi_todo_boxes_roundtrip() {
        let mut cfg = AppConfig::default();
        cfg.boxes.push(BoxConfig {
            id: "todo-a".into(),
            name: "待办 A".into(),
            kind: BoxKind::Todo,
            clip_filter: ClipFilter::All,
            file_filter: FileFilter::All,
            todos: vec![
                TodoItem {
                    id: "t1".into(),
                    text: "买牛奶".into(),
                    done: false,
                    ..TodoItem::default()
                },
                TodoItem {
                    id: "t2".into(),
                    text: "写周报".into(),
                    done: true,
                    ..TodoItem::default()
                },
            ],
            todo_sort: TodoSort::Pending,
            view: BoxView::Icon,
            members: Vec::new(),
            manual_order: Vec::new(),
            x: 10.0,
            y: 20.0,
            w: 300.0,
            h: 220.0,
            color: [20, 40, 80],
            alpha: 180,
            collapsed: true,
            locked: false,
            closed: true,
            sort: SortMode::Name,
        });
        cfg.boxes.push(BoxConfig {
            id: "todo-b".into(),
            name: "待办 B".into(),
            kind: BoxKind::Todo,
            clip_filter: ClipFilter::All,
            file_filter: FileFilter::All,
            todos: vec![TodoItem {
                id: "t3".into(),
                text: "回邮件".into(),
                done: false,
                ..TodoItem::default()
            }],
            todo_sort: TodoSort::Name,
            view: BoxView::Icon,
            members: Vec::new(),
            manual_order: Vec::new(),
            x: 400.0,
            y: 60.0,
            w: 280.0,
            h: 190.0,
            color: [80, 40, 20],
            alpha: 160,
            collapsed: false,
            locked: true,
            closed: false,
            sort: SortMode::Name,
        });

        let text = toml::to_string(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.boxes.len(), 2);
        let a = &back.boxes[0];
        assert_eq!(a.id, "todo-a");
        assert_eq!(a.todos.len(), 2);
        assert_eq!(a.todos[0].text, "买牛奶");
        assert!(a.todos[1].done);
        assert_eq!(a.todo_sort, TodoSort::Pending);
        assert_eq!((a.x, a.y, a.w, a.h), (10.0, 20.0, 300.0, 220.0));
        assert!(a.collapsed);
        assert!(a.closed);
        let b = &back.boxes[1];
        assert_eq!(b.id, "todo-b");
        assert_eq!(b.todos.len(), 1);
        assert_eq!(b.todos[0].text, "回邮件");
        assert_eq!(b.todo_sort, TodoSort::Name);
        assert_eq!((b.x, b.y, b.w, b.h), (400.0, 60.0, 280.0, 190.0));
        assert!(!b.collapsed);
        assert!(!b.closed);
        assert!(b.locked);
    }

    #[test]
    fn legacy_single_todo_box_upgrades_without_losing_data() {
        let text = r#"
version = 1

[[boxes]]
id = "todo-old"
name = "待办"
kind = "todo"
x = 10.0
y = 20.0
w = 300.0
h = 200.0
color = [58, 78, 120]
alpha = 190
collapsed = false
locked = false
sort = "name"

[[boxes.todos]]
id = "t1"
text = "旧条目"
done = false
"#;
        let cfg: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.boxes.len(), 1);
        let old = &cfg.boxes[0];
        assert_eq!(old.kind, BoxKind::Todo);
        assert_eq!(old.todos.len(), 1);
        assert_eq!(old.todos[0].text, "旧条目");
        assert!(!old.closed);
        assert!(old.members.is_empty());
        assert_eq!(old.todo_sort, TodoSort::Manual);
    }

    #[test]
    fn legacy_todo_without_due_fields_gets_defaults() {
        let text = r#"
[[boxes]]
id = "todo-old"
name = "待办"
kind = "todo"

[[boxes.todos]]
id = "t1"
text = "旧条目"
done = false
"#;
        let cfg: AppConfig = toml::from_str(text).unwrap();
        let t = &cfg.boxes[0].todos[0];
        assert_eq!(t.text, "旧条目");
        assert_eq!(t.due_at, None);
        assert!(!t.all_day);
        assert_eq!(t.local_date, None);
        assert_eq!(t.repeat, RepeatRule::Once);
        assert_eq!(t.interval_min, 0);
        assert_eq!(t.interval_minutes(), MIN_INTERVAL_MIN);
        assert_eq!(t.interval_base, IntervalBase::DueTime);
        assert_eq!(t.lead_min, 0);
        assert_eq!(t.snooze_until, None);
        assert_eq!(t.last_fired, None);
    }

    #[test]
    fn todo_due_fields_roundtrip() {
        let mut cfg = AppConfig::default();
        cfg.boxes.push(BoxConfig {
            id: "todo".into(),
            name: "巡检".into(),
            kind: BoxKind::Todo,
            todos: vec![TodoItem {
                id: "t1".into(),
                text: "巡检".into(),
                due_at: Some(1_718_000_000),
                local_date: Some(19_813),
                local_minute: 540,
                repeat: RepeatRule::Interval,
                interval_min: 120,
                interval_base: IntervalBase::CompletionTime,
                lead_min: 5,
                snooze_until: Some(1_718_000_600),
                last_fired: Some(1_717_999_000),
                ..TodoItem::default()
            }],
            ..BoxConfig::default()
        });
        let text = toml::to_string(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&text).unwrap();
        let t = &back.boxes[0].todos[0];
        assert_eq!(t.due_at, Some(1_718_000_000));
        assert_eq!(t.local_date, Some(19_813));
        assert_eq!(t.local_minute, 540);
        assert_eq!(t.repeat, RepeatRule::Interval);
        assert_eq!(t.interval_min, 120);
        assert_eq!(t.interval_base, IntervalBase::CompletionTime);
        assert_eq!(t.lead_min, 5);
        assert_eq!(t.snooze_until, Some(1_718_000_600));
        assert_eq!(t.last_fired, Some(1_717_999_000));
    }

    #[test]
    fn box_geometry_roundtrip_keeps_physical_pixels() {
        // 盒子几何以物理像素存储，换算可能留下 f32 尾差；保存/加载必须逐位一致，
        // 否则重启后盒子会偏移或缩放 1px。
        let mut cfg = AppConfig::default();
        cfg.boxes.push(BoxConfig {
            id: "b-geo".into(),
            name: "文件盒子".into(),
            kind: BoxKind::Files,
            clip_filter: ClipFilter::All,
            file_filter: FileFilter::All,
            todos: Vec::new(),
            todo_sort: TodoSort::Manual,
            view: BoxView::default(),
            members: Vec::new(),
            manual_order: Vec::new(),
            x: 3138.9998,
            y: 861.0,
            w: 520.0,
            h: 481.00012,
            color: [20, 40, 80],
            alpha: 180,
            collapsed: false,
            locked: false,
            closed: false,
            sort: SortMode::Name,
        });
        let text = toml::to_string(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.boxes[0].x, 3138.9998);
        assert_eq!(back.boxes[0].y, 861.0);
        assert_eq!(back.boxes[0].w, 520.0);
        assert_eq!(back.boxes[0].h, 481.00012);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let cfg: AppConfig = toml::from_str("version = 1\n").unwrap();
        assert_eq!(cfg.clipboard.max_items, 200);
        assert_eq!(cfg.desktop.icon_size, 48);
        assert!(cfg.general.show_overlay);
    }

    #[test]
    fn box_view_parses_per_box_and_defaults_to_icon() {
        let text = "[[boxes]]\nid = \"a\"\nview = \"list\"\n[[boxes]]\nid = \"b\"\nview = \"thumb\"\n[[boxes]]\nid = \"c\"\n";
        let cfg: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.boxes[0].view, BoxView::List);
        assert_eq!(cfg.boxes[1].view, BoxView::Thumb);
        assert_eq!(cfg.boxes[2].view, BoxView::Icon);
        assert_eq!(cfg.desktop.default_box_view, BoxView::Icon);
    }

    #[test]
    fn hotkey_parsing() {
        let hk = parse_hotkey("Ctrl+Shift+V").unwrap();
        assert!(hk.0 && hk.2 && !hk.1);
        assert_eq!(hk.4, "V");
        assert!(parse_hotkey("Ctrl+Shift+A+B").is_none());
        assert!(parse_hotkey("Ctrl").is_none());
    }

    #[test]
    fn throttled_saver_writes_file() {
        let dir = std::env::temp_dir().join("deskkeeper_test_cfg");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let saver = ConfigSaver::new(path.clone());
        let mut cfg = AppConfig::default();
        cfg.clipboard.max_items = 7;
        assert!(saver.request(&cfg));
        cfg.clipboard.max_items = 9;
        assert!(saver.request(&cfg));
        std::thread::sleep(Duration::from_millis(900));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("max_items = 9"));
        assert!(!text.contains("max_items = 7"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
