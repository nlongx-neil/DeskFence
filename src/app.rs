//! 应用主体：桌面整理覆盖层（盒子 + 桌面图标）与剪贴板盒。
//!
//! 线程边界：UI 线程只做绘制与交互；目录扫描、目录监听、图标/缩略图解码、
//! 文件移动、SQLite 读写、剪贴板采样与粘贴全部在后台线程，
//! 通过 [`BgEvent`] 回传结果（见 `events.rs`）。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};

use egui::{Align2, Color32, CornerRadius, FontId, Id, Pos2, Rect, Sense, Stroke, Vec2};

use crate::clipboard::service::{paste_record, ClipService};
use crate::clipboard::store::{record_matches, ClipRecord, Store, StoreCmd};
use crate::config::{
    AppConfig, BoxConfig, BoxKind, BoxView, ClipFilter, ConfigSaver, DesktopSlot, FileFilter,
    GroupTarget, OccupiedDropMode, SortMode, TodoSort,
};
use crate::events::{BgEvent, DesktopMenuAction, EventSink, ShellMenuAction, TrayCommand};
use crate::fileops::{self, MoveRecord, UndoStack};
use crate::model::{self, ItemMeta};
use crate::platform::clipboard_native::{self, ClipContent};
use crate::platform::icons::{self, IconTask};
use crate::platform::input::WindowRef;
use crate::platform::message_window::{self, HotkeySpec};
use crate::platform::single_instance::SingleInstance;
use crate::platform::{autostart, desktop, key_hook, shell};
use crate::preview::{self, PreviewData};
use crate::ui::{self, fit_text, theme, TextureCache};
use crate::util;
use crate::watcher::{self, WatchSpec, WatcherHandle};

const HEADER_H: f32 = 30.0;
const PAD: f32 = 14.0;
const RADIUS: u8 = 14;
const MIN_W: f32 = 220.0;
const MIN_H: f32 = 130.0;
const BTN: f32 = 22.0;
/// 上一次同步给键盘钩子的预览接管状态，仅用于在状态翻转时打一条判定日志。
static PREVIEW_ARMED_LOGGED: AtomicBool = AtomicBool::new(false);
/// 盒子缩放命中带宽度（逻辑点，随所属屏 DPI 缩放）：三边与四角都用它。
const EDGE_HIT: f32 = 6.0;
const EDGE_LEFT: u8 = 1;
const EDGE_RIGHT: u8 = 2;
const EDGE_TOP: u8 = 4;
const EDGE_BOTTOM: u8 = 8;
/// 独立设置窗口默认尺寸（逻辑点）。
const SETTINGS_W: f32 = 640.0;
const SETTINGS_H: f32 = 680.0;
const UNDO_CAPACITY: usize = 30;
/// 图标尺寸变更的合并窗口：滑杆连续步进时只保留最后一次，避免每一步都全量重解码。
const ICON_RESIZE_DEBOUNCE: f64 = 0.030;
/// 旧尺寸图标纹理在新尺寸就绪后的保留时长（期间作为拉伸占位，避免闪烁）。
const ICON_PRUNE_DELAY: f64 = 1.0;

/// 盒子的运行时状态（布局数据存放在 [`BoxConfig`]）。
struct BoxRuntime {
    /// 已解析的成员（源文件真实路径的元数据）。
    items: Vec<ItemMeta>,
    /// 无法解析的成员引用（路径不存在/不可读）；保留在配置中并给出可见提示。
    invalid: Vec<String>,
    /// 剪贴板盒内的搜索词（仅运行时，不持久化）。
    clip_search: String,
    scroll: f32,
    /// 多选集合：`model::path_key` 键。按路径而不是下标存储，
    /// 排序/重扫/切视图/滚动后都不会错位。
    selected: HashSet<String>,
    /// 键盘当前焦点条目（`path_key`）。
    focus: Option<String>,
    /// Shift 区间选择的锚点（`path_key`）。
    anchor: Option<String>,
    /// 每帧由内容区绘制写入：网格列数（列表视图为 1）。
    nav_cols: usize,
    /// 每帧由内容区绘制写入：列表行高。
    nav_row_h: f32,
    /// 每帧由内容区绘制写入：内容可视区（逻辑点）。
    nav_list: Rect,
    /// 每帧由内容区绘制写入：条目格子尺寸（列表为整行）。
    nav_cell: Vec2,
    /// 每帧由内容区绘制写入：内容区左上角（逻辑点，未滚动）。
    nav_origin: Pos2,
    /// 当前内容视图（用于方向键换算）。
    nav_view: BoxView,
}

impl BoxRuntime {
    fn empty() -> Self {
        Self {
            items: Vec::new(),
            invalid: Vec::new(),
            clip_search: String::new(),
            scroll: 0.0,
            selected: HashSet::new(),
            focus: None,
            anchor: None,
            nav_cols: 1,
            nav_row_h: 1.0,
            nav_list: Rect::NOTHING,
            nav_cell: Vec2::ZERO,
            nav_origin: Pos2::ZERO,
            nav_view: BoxView::Icon,
        }
    }
}

enum DragKind {
    None,
    Item {
        path: PathBuf,
        icon: Option<egui::TextureId>,
        grab: Vec2,
        from_box: Option<usize>,
        /// 盒子内拖拽时，把拖动图标夹在该盒子内容区（逻辑点）；`None` 表示桌面图标。
        clamp: Option<Rect>,
    },
    /// 整组拖动（多选态）：一次移动多个成员引用。
    Items {
        paths: Vec<PathBuf>,
        icon: Option<egui::TextureId>,
        grab: Vec2,
        from_box: Option<usize>,
        clamp: Option<Rect>,
        /// 拖动起点所在的条目：桌面整组移动时作为「整组平移」的基准锚点。
        anchor: PathBuf,
    },
    BoxMove {
        idx: usize,
        grab: Vec2,
    },
    BoxResize {
        idx: usize,
        /// 参与缩放的边（`EDGE_LEFT|EDGE_RIGHT|EDGE_TOP|EDGE_BOTTOM` 组合）。
        edges: u8,
        /// 按下瞬间的盒几何（物理像素 x/y/w/h），全程以此为准计算，避免累计误差。
        start: (f32, f32, f32, f32),
        origin: Pos2,
    },
}

/// egui 在 press→move→release 合并到同一帧时不会判定拖动（interaction.rs 明确注释），
/// 这里记下按住图标时的起点，释放帧用它兜底完成落点判定。
struct PendingItemPress {
    pos: Pos2,
    path: PathBuf,
    from_box: Option<usize>,
}

enum Modal {
    None,
    Confirmation(BoxConfirm),
    Color {
        idx: usize,
        color: [u8; 3],
        alpha: u8,
    },
}

/// 需要二次确认的盒子操作。关闭（隐藏）不需要确认，这里只有不可撤销的删除。
enum BoxConfirm {
    /// 删除待办盒子：盒子与其全部待办事项一并清空（不涉及文件）。
    DeleteTodo { idx: usize },
    /// 解散盒子：删除盒子与成员引用，绝不删除任何真实文件。
    Dissolve { idx: usize },
}

/// 本帧绘制过程中产生的动作，绘制结束后统一处理，避免借用冲突。
pub(crate) enum UiAction {
    NewBox,
    RenameBox(usize),
    ColorBox(usize),
    ToggleCollapse(usize),
    ToggleLock(usize),
    /// 关闭盒子：只隐藏并保留全部数据，可从设置页重新打开
    CloseBox(usize),
    ReopenBox(usize),
    /// 解散盒子：删除盒子与成员引用，真实文件保持原位
    DissolveBox(usize),
    /// 删除待办盒子：盒子与其全部事项一并清空（不涉及文件）
    DeleteTodoBox(usize),
    PasteIntoBox(usize),
    /// 在桌面新建文件夹并加入该盒子
    NewFolderInBox(usize),
    /// 在桌面新建文本文档并加入该盒子
    NewTextInBox(usize),
    /// 按盒子运行时的失效引用列表清理成员
    PruneInvalid(usize),
    /// 立即按类型整理桌面图标（只记录引用）
    AutoGroupAll,
    /// 清空剪贴板历史（保留剪贴板盒本身）
    ClearClipboard,
    /// 删除单条剪贴板记录
    ClipDelete(i64),
    /// 置顶/取消置顶单条剪贴板记录
    ClipSetPinned {
        id: i64,
        pinned: bool,
    },
    /// 粘贴单条剪贴板记录到当前前台窗口
    ClipPaste(ClipRecord),
    /// 暂停/继续剪贴板记录
    ClipTogglePause,
    SetBoxSort(usize, SortMode),
    /// 文件盒/普通盒的类型 tab：只改显示筛选，并把滚动位置重置到顶部
    SetFileFilter(usize, FileFilter),
    /// 切换盒子内容视图（图标/列表/缩略图），持久化到配置
    SetBoxView(usize, crate::config::BoxView),
    SetDeskSort(SortMode),
    /// 清空棋盘占位，回到按排序紧凑排列
    RestoreAutoArrange,
    /// 把当前紧凑排列快照成棋盘占位，启用「拖到任意格」
    EnableBoardPlacement,
    /// 设置拖到已占格时的处理方式
    SetOccupiedDropMode(OccupiedDropMode),
    Open(PathBuf),
    PasteIntoDesktop,
    Undo,
    OpenSettings,
    HideOverlay,
    Refresh,
    /// 剪贴板盒条目点击：复制回剪贴板
    CopyClip(ClipRecord),
    /// 进入图标名称行内编辑（桌面图标或盒子内条目）
    BeginRename {
        box_idx: Option<usize>,
        item: usize,
    },
    /// 在屏幕物理坐标处弹出 Windows 原生右键菜单（尾部追加我们的项）
    ShowShellMenu {
        box_idx: Option<usize>,
        item: usize,
        x: i32,
        y: i32,
    },
    /// 直接执行一条菜单动作（内置降级菜单复用原生菜单的处理逻辑）
    ShellMenu(ShellMenuAction),
    /// 新建剪贴板盒
    NewClipBox,
    /// 新建待办盒
    NewTodoBox,
    /// 待办盒：新增一条
    TodoAdd(usize, String),
    /// 待办盒：切换完成状态
    TodoToggle(usize, String),
    /// 待办盒：删除一条
    TodoDelete(usize, String),
    /// 待办盒：提交行内编辑
    TodoCommit(usize, String, String),
    /// 待办盒：把一条事项移到最前并切换为手动排序
    TodoMoveTop(usize, String),
    /// 多选：删除整组到回收站
    GroupDelete(Vec<PathBuf>),
    /// 多选：复制整组路径（每行一个）
    GroupCopyPaths(Vec<PathBuf>),
    /// 多选：从盒子移出引用
    GroupEject {
        box_idx: usize,
        paths: Vec<PathBuf>,
    },
    /// 多选：把整组加入另一个盒子（只移动引用）
    GroupMoveToBox {
        from: usize,
        paths: Vec<PathBuf>,
        to: usize,
    },
    /// 桌面多选：把整组加入某个盒子（系统项自动跳过）
    GroupAddToBox {
        paths: Vec<PathBuf>,
        to: usize,
    },
    /// 多选：把整组路径写入剪贴板盒
    GroupAddClip(Vec<PathBuf>),
}

/// 名称文本选择的唯一目标：桌面图标或某个盒子里的某个条目。
///
/// 用紧凑的下标而不是字符串键，避免每帧为每个图标分配 `format!`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LabelTarget {
    /// `None` = 桌面；`Some(i)` = 第 i 个盒子。
    box_idx: Option<usize>,
    item: usize,
}

impl LabelTarget {
    fn desktop(item: usize) -> Self {
        Self {
            box_idx: None,
            item,
        }
    }

    fn in_box(box_idx: usize, item: usize) -> Self {
        Self {
            box_idx: Some(box_idx),
            item,
        }
    }
}

/// 名称文本选择状态（跨帧保留，供 Ctrl+C 与选区高亮使用）。
#[derive(Default)]
struct TextSelection {
    target: Option<LabelTarget>,
    anchor: usize,
    cursor: usize,
    /// 上一帧该名称的文字交互区域；在区域外按下时清除选中。
    zone: Option<Rect>,
    /// 当前选中的文本（字符区间对应的原文；无选中时为空）。
    selected: String,
    /// 是否处于按住拖动中（松开后保留选区）。
    dragging: bool,
}

/// 待办项行内编辑状态（同一时刻只编辑一项）。
#[derive(Clone, Debug)]
struct TodoEdit {
    box_idx: usize,
    id: String,
    text: String,
    /// 下一帧是否需要把键盘焦点交给文本框
    focus: bool,
}

/// 原生菜单不可用时替代显示的内置菜单（位置为叠加层 egui 点坐标）。
struct FallbackMenu {
    request: crate::platform::shell_menu::ShellMenuRequest,
    pos: Pos2,
}

/// 盒内框选（拖出矩形选择范围内条目）。
struct Marquee {
    box_idx: usize,
    start: Pos2,
    current: Pos2,
    /// Ctrl 框选为追加；否则替换。
    additive: bool,
}

/// 框选命中项：(真实下标, 路径, 名称, 修改时间)。
type MarqueeHit = (usize, PathBuf, String, u64);

/// 桌面空白区框选（拖出矩形选择范围内图标）。
struct DeskMarquee {
    start: Pos2,
    current: Pos2,
    /// Ctrl 框选为追加；否则替换。
    additive: bool,
}

/// 多选右键菜单（纯 egui 自绘）：动作作用于整组。
#[derive(Clone)]
struct GroupMenu {
    /// `Some(i)` = 盒子内多选；`None` = 桌面多选。
    box_idx: Option<usize>,
    paths: Vec<PathBuf>,
    pos: Pos2,
}

/// 当前唯一选中项：空格预览的目标（路径 + 点击时的格子矩形）。
#[derive(Clone, Debug)]
struct Selection {
    path: PathBuf,
    name: String,
    modified: u64,
    /// 叠加层逻辑点坐标里的所属格子，用于预览面板就近弹出。
    anchor: Rect,
}

/// 快速预览面板状态（`data` 为 `None` 表示后台抽取仍在进行）。
struct PreviewView {
    path: PathBuf,
    name: String,
    modified: u64,
    anchor: Rect,
    data: Option<PreviewData>,
    /// 打开时刻（egui 秒），用于耗时与存活时间日志。
    opened_at: f64,
    /// 上一帧面板实际矩形（逻辑点），用于限定 Esc / Ctrl+C 的屏幕物理矩形。
    rect: Option<Rect>,
}

/// 图标名称行内编辑状态（同一时刻只编辑一个名称）。
#[derive(Clone, Debug)]
struct NameEdit {
    target: LabelTarget,
    /// 编辑前的真实路径（重命名成功后据此更新图标缓存）。
    path: PathBuf,
    /// 编辑中的文本；提交时作为真实文件名。
    text: String,
    /// 下一帧是否需要把键盘焦点交给文本框
    focus: bool,
}

/// 盒子名称行内编辑状态（标题栏双击进入，同一时刻只编辑一个盒子）。
#[derive(Clone, Debug)]
struct BoxNameEdit {
    idx: usize,
    text: String,
    /// 下一帧是否需要把键盘焦点交给文本框
    focus: bool,
}

impl TextSelection {
    fn range(&self) -> (usize, usize) {
        (self.anchor.min(self.cursor), self.anchor.max(self.cursor))
    }

    fn has_selection(&self) -> bool {
        self.target.is_some() && self.anchor != self.cursor
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

pub struct DeskApp {
    ctx: egui::Context,
    cfg: AppConfig,
    saver: ConfigSaver,
    dark: bool,
    pal: theme::Palette,
    sink: EventSink,
    rx: Receiver<BgEvent>,
    icon_tx: Sender<IconTask>,
    textures: TextureCache,
    /// 纹理请求实际使用的图标尺寸（配置变更经过 30ms 合并后才更新）。
    tex_icon_size: u32,
    /// 图标尺寸变更的合并截止时间（egui 秒）。
    icon_resize_at: Option<f64>,
    /// 旧尺寸纹理的清理截止时间；到期前保留旧纹理作为占位。
    icon_prune_at: Option<f64>,
    /// 图标大小滑杆是否处于按住/拖动状态（跨 viewport，故由设置窗口写入）。
    icon_slider_held: bool,
    overlay_visible: bool,
    /// 上一次写入键盘钩子的 NAV_ARMED 值（仅在变化时记日志，便于真机排查）。
    nav_armed_prev: bool,
    /// 钩子转交来的空格请求：延迟到本帧绘制结束（点击已处理）后再判定打开/忽略。
    pending_toggle_preview: bool,
    /// 单帧拖动兜底：按住图标时的起点；仅在手指按住期间存在。
    pending_item_press: Option<PendingItemPress>,
    overlay: crate::platform::PxRect,
    /// 窗口的 pixels_per_point（物理像素 ↔ egui 点的唯一换算基准）
    scale: f32,
    /// 每块显示器的物理工作区与各自 DPI 缩放
    monitors: Vec<desktop::MonitorInfo>,
    boxes: Vec<BoxRuntime>,
    /// 用户桌面的原始扫描结果（未剔除盒子成员，供重建时复用）
    desktop_user: Vec<ItemMeta>,
    /// 合并后的桌面网格（已剔除已归属盒子的条目）
    desktop_items: Vec<ItemMeta>,
    desktop_public: Vec<ItemMeta>,
    /// 回收站是否非空（决定「回收站」系统项使用空桶还是满桶图标）。
    recycle_full: bool,
    /// 桌面目录已见过的路径集合，用于识别「新出现」的图标（首次扫描不搬动）。
    desktop_user_paths: std::collections::HashSet<PathBuf>,
    desktop_public_paths: std::collections::HashSet<PathBuf>,
    desktop_user_ready: bool,
    desktop_public_ready: bool,
    /// 剪贴板盒的基线数据（全部类型、无搜索），盒内再做搜索与类型筛选。
    clip_box_records: Vec<ClipRecord>,
    /// 待办盒新建输入框内容（同一时刻只编辑一个盒子）
    todo_draft: String,
    /// 待办项行内编辑状态
    todo_edit: Option<TodoEdit>,
    /// 图标名称行内编辑状态
    name_edit: Option<NameEdit>,
    /// 盒子标题行内编辑状态
    box_name_edit: Option<BoxNameEdit>,
    /// 桌面原生菜单不可用时的内置菜单位置
    desktop_fallback_menu: Option<egui::Pos2>,
    /// 原生菜单不可用时临时显示的内置菜单
    fallback_menu: Option<FallbackMenu>,
    /// 名称编辑框所在区域（由绘制循环按当前命中区写入）
    name_edit_zone: Option<Rect>,
    /// 名称编辑框字号（跟随图标尺寸联动）
    name_edit_font: f32,
    /// 性能诊断（DESKKEEPER_PERF=1）：进程启动时刻与首个缩略图到达时刻
    perf_start: Option<std::time::Instant>,
    perf_first_thumb: Option<std::time::Instant>,
    /// 叠加层是否处于临时接收键盘输入的状态
    text_input_active: bool,
    drag: DragKind,
    modal: Modal,
    /// 独立设置窗口是否打开
    settings_open: bool,
    settings_needs_focus: bool,
    /// 盒子拖动的磁吸提示线（物理像素，当前帧有效）
    snap_guides: Vec<model::SnapGuide>,
    store: Store,
    clip: ClipService,
    clip_cfg: Arc<RwLock<crate::config::ClipboardConfig>>,
    clip_paused: Arc<AtomicBool>,
    layer: desktop::DesktopLayer,
    undo: UndoStack,
    pending_moves: usize,
    toast: Option<(String, f64)>,
    _single: Option<SingleInstance>,
    desktop_watcher: WatcherHandle,
    shutting_down: bool,
    /// 需要重排窗口层级（首帧、显示器变化、重新显示叠加层后）
    zorder_pending: bool,
    /// 剩余重试次数：显示器变化后窗口位置命令是异步生效的，区域裁剪需要等窗口到位。
    region_retry: u8,
    /// debug 重绘计数：(区间起点秒, 帧数)，用于验证"鼠标移动不产生可见重绘"。
    frame_marks: Option<(f64, u32)>,
    /// 帧耗时统计：(区间起点秒, 帧数, 累计毫秒, 最大毫秒)
    frame_cost: Option<(f64, u32, f64, f64)>,
    /// 分项耗时统计：(区间起点秒, 帧数, 叠加层累计 ms, 模态累计 ms, 设置累计 ms)
    frame_split: Option<(f64, u32, f64, f64, f64)>,
    /// 图标名称换行缓存：真实字形测量较贵，按（名称 + 可用宽度 + 字号）缓存命中。
    label_cache: LabelCache,
    /// 名称文本选择状态（同一时刻只支持一个名称的选区）。
    text_sel: TextSelection,
    /// 当前唯一选中项（空格预览目标）；点空白处清除。
    sel_item: Option<Selection>,
    /// 快速预览面板状态。
    preview: Option<PreviewView>,
    /// 预览内容抽取请求通道（后台线程解析，结果经 BgEvent 回传）。
    preview_tx: Sender<preview::PreviewRequest>,
    /// 盒内框选状态。
    marquee: Option<Marquee>,
    /// 桌面框选状态。
    desk_marquee: Option<DeskMarquee>,
    /// 桌面多选集合（`model::path_key`）。
    desk_selected: HashSet<String>,
    /// 桌面 Shift 区间选择的锚点（`model::path_key`）。
    desk_anchor: Option<String>,
    /// 桌面选区变化时是否已记录过「多选不预览」日志（避免每帧刷屏）。
    desk_multi_logged: bool,
    /// 多选右键菜单。
    group_menu: Option<GroupMenu>,
    /// 键盘导航当前聚焦的盒子。
    focus_box: Option<usize>,
    /// 成员存在性监听（文件被删除后自动清理引用）。
    probe: crate::member_probe::ProbeHandle,
    /// 上一次下发给探测线程的成员签名，避免重复下发。
    probe_paths_sig: String,
    /// 已用 egui 实际 pixels_per_point 校正过缩放
    pp_synced: bool,
    /// 进程启动时刻，用于"启动→首帧"测量
    started_at: std::time::Instant,
    first_frame_logged: bool,
}

impl DeskApp {
    pub fn new(
        ctx: egui::Context,
        mut cfg: AppConfig,
        warning: Option<String>,
        single: Option<SingleInstance>,
    ) -> Self {
        let (sink, rx) = EventSink::new(ctx.clone());
        let (icon_tx, icon_rx) = icons::spawn_worker();
        {
            let sink_icons = sink.clone();
            std::thread::Builder::new()
                .name("icon-bridge".into())
                .spawn(move || {
                    while let Ok(resp) = icon_rx.recv() {
                        sink_icons.emit(BgEvent::IconLoaded(resp));
                    }
                })
                .ok();
        }
        let (preview_tx, preview_rx) = preview::spawn_worker();
        preview::spawn_bridge(sink.clone(), preview_rx);

        // 盒子 id 同时是磁盘目录名，必须保证存在
        for b in cfg.boxes.iter_mut() {
            if b.id.is_empty() {
                b.id = util::gen_id("box-");
            }
        }

        // 旧配置没有全局「盒子背景不透明度」字段，首次升级沿用原默认配色的 alpha，
        // 避免升级瞬间所有盒子外观突变；之后设置页滑杆以全局值为准。
        // 缺省值随默认 alpha 一起从 190 调到 16，哨兵值需同步，否则旧配置永远不会命中。
        if cfg.desktop.box_opacity == 16 && cfg.desktop.box_defaults.alpha != 16 {
            cfg.desktop.box_opacity = cfg.desktop.box_defaults.alpha;
        }

        // 旧版盒子内容在遗留目录里：接管为路径引用（只读，不移动文件）。
        let seeded = crate::migration::seed_legacy_members(&mut cfg, &util::boxes_dir());
        if seeded > 0 {
            log::info!("已从旧盒子目录接管 {seeded} 个成员引用（文件保持原位）");
        }
        let mut boxes: Vec<BoxRuntime> = cfg.boxes.iter().map(|_| BoxRuntime::empty()).collect();

        // 首次运行：默认创建「文件盒子」「目录盒子」，作为自动归类目标。
        // 只在从未创建过时执行一次；用户删除后不再自动重建。
        let mut created_group_boxes = false;
        if cfg.desktop.auto_group && !cfg.desktop.group_boxes_created {
            let scale = desktop::system_scale().max(1.0);
            let area = desktop::primary_work_area();
            let work = (area.x as f32, area.y as f32, area.w as f32, area.h as f32);
            let margin = 18.0 * scale;
            let gap = 14.0 * scale;
            let defaults = cfg.desktop.box_defaults.clone();
            let mut others: Vec<(f32, f32, f32, f32)> =
                cfg.boxes.iter().map(|b| (b.x, b.y, b.w, b.h)).collect();
            for (kind, name) in [(BoxKind::Files, "文件盒子"), (BoxKind::Dirs, "目录盒子")]
            {
                if cfg.boxes.iter().any(|b| b.kind == kind) {
                    continue;
                }
                let w = (defaults.w * scale).clamp(280.0 * scale, (work.2 - 2.0 * margin).max(0.0));
                let h = (defaults.h * scale).clamp(220.0 * scale, (work.3 - 2.0 * margin).max(0.0));
                let (x, y) = model::docked_box_pos(work, w, h, margin, gap, &others);
                let id = util::gen_id("box-");
                others.push((x, y, w, h));
                boxes.push(BoxRuntime::empty());
                cfg.boxes.push(BoxConfig {
                    id,
                    name: name.to_string(),
                    kind,
                    clip_filter: ClipFilter::All,
                    file_filter: FileFilter::All,
                    todos: Vec::new(),
                    todo_sort: TodoSort::Manual,
                    view: cfg.desktop.default_box_view,
                    members: Vec::new(),
                    manual_order: Vec::new(),
                    x,
                    y,
                    w,
                    h,
                    color: defaults.color,
                    alpha: defaults.alpha,
                    collapsed: false,
                    locked: false,
                    closed: false,
                    sort: SortMode::Name,
                });
                created_group_boxes = true;
            }
            cfg.desktop.group_boxes_created = true;
        }

        let clip = Arc::new(RwLock::new(cfg.clipboard.clone()));
        let paused = Arc::new(AtomicBool::new(!cfg.clipboard.enabled));
        let store = Store::spawn(sink.clone());
        let service = ClipService::spawn(sink.clone(), store.clone(), clip.clone(), paused.clone());

        message_window::spawn(sink.clone(), HotkeySpec::from_config(&cfg.clipboard.hotkey));
        message_window::set_paused(!cfg.clipboard.enabled);
        message_window::set_overlay_visible(cfg.general.show_overlay);
        desktop::spawn_layer_keeper();
        // 回收站不在桌面目录监听范围内：后台线程每 2 秒轻量查询一次，状态变化才唤醒 UI。
        let recycle_full = shell::recycle_bin_has_items().unwrap_or(false);
        shell::spawn_recycle_bin_watcher(sink.clone(), recycle_full);
        crate::platform::key_hook::install(sink.clone());
        let probe = crate::member_probe::spawn(sink.clone());

        let desktop_specs = vec![
            WatchSpec {
                dir: util::user_desktop_dir(),
                recursive: false,
            },
            WatchSpec {
                dir: util::public_desktop_dir(),
                recursive: false,
            },
        ];
        watcher::initial_scan(sink.clone(), desktop_specs.clone());
        let desktop_watcher = watcher::spawn(sink.clone(), desktop_specs);

        if autostart::is_enabled() != cfg.general.autostart {
            if let Err(e) = autostart::set_enabled(cfg.general.autostart) {
                log::warn!("同步开机自启设置失败: {e}");
            }
        }

        let dark = theme::is_dark(cfg.theme);
        theme::apply(&ctx, dark);
        let pal = theme::palette(dark);
        let overlay = desktop::work_area();
        let scale = desktop::system_scale().max(1.0);
        let monitors = desktop::monitor_infos();
        let overlay_visible = cfg.general.show_overlay;
        let tex_icon_size = cfg.desktop.icon_size;

        let toast = warning.map(|w| (format!("配置提示：{w}"), f64::INFINITY));

        let mut app = Self {
            ctx,
            cfg,
            saver: ConfigSaver::new(util::config_path()),
            dark,
            pal,
            sink,
            rx,
            icon_tx,
            textures: TextureCache::new(),
            tex_icon_size,
            icon_resize_at: None,
            icon_prune_at: None,
            icon_slider_held: false,
            overlay_visible,
            nav_armed_prev: false,
            pending_toggle_preview: false,
            pending_item_press: None,
            overlay,
            scale,
            monitors,
            boxes,
            desktop_user: Vec::new(),
            desktop_items: Vec::new(),
            desktop_public: Vec::new(),
            recycle_full,
            desktop_user_paths: std::collections::HashSet::new(),
            desktop_public_paths: std::collections::HashSet::new(),
            desktop_user_ready: false,
            desktop_public_ready: false,
            clip_box_records: Vec::new(),
            todo_draft: String::new(),
            todo_edit: None,
            name_edit: None,
            box_name_edit: None,
            desktop_fallback_menu: None,
            fallback_menu: None,
            name_edit_zone: None,
            name_edit_font: 12.0,
            perf_start: std::env::var("DESKKEEPER_PERF")
                .is_ok()
                .then(std::time::Instant::now),
            perf_first_thumb: None,
            text_input_active: false,
            drag: DragKind::None,
            modal: Modal::None,
            settings_open: false,
            settings_needs_focus: false,
            snap_guides: Vec::new(),
            store,
            clip: service,
            clip_cfg: clip,
            clip_paused: paused,
            layer: desktop::DesktopLayer::new(),
            undo: UndoStack::new(UNDO_CAPACITY),
            pending_moves: 0,
            toast,
            _single: single,
            desktop_watcher,
            shutting_down: false,
            zorder_pending: true,
            region_retry: 8,
            frame_marks: None,
            frame_cost: None,
            frame_split: None,
            label_cache: LabelCache::default(),
            text_sel: TextSelection::default(),
            sel_item: None,
            preview: None,
            preview_tx,
            marquee: None,
            desk_marquee: None,
            desk_selected: HashSet::new(),
            desk_anchor: None,
            desk_multi_logged: false,
            group_menu: None,
            focus_box: None,
            probe,
            probe_paths_sig: String::new(),
            pp_synced: false,
            started_at: std::time::Instant::now(),
            first_frame_logged: false,
        };
        app.clamp_boxes_to_work_area();
        app.refresh_all_boxes();
        let invalid: usize = app.boxes.iter().map(|b| b.invalid.len()).sum();
        if invalid > 0 {
            let msg = format!("{invalid} 个盒子引用已失效（源文件可能已被移动或删除）");
            log::warn!("{msg}");
            match app.toast.take() {
                Some((prev, at)) => app.toast = Some((format!("{prev}；{msg}"), at)),
                None => app.toast = Some((msg, f64::INFINITY)),
            }
        }
        app.apply_desktop_layer();
        if created_group_boxes {
            app.persist();
            log::info!("已创建默认分类盒子（文件盒子 / 目录盒子）");
        }
        if app.box_index_by_kind(BoxKind::Clipboard).is_some() {
            app.query_clip_box();
        }
        if !app.overlay_visible {
            // 配置为不显示时不能留下可见窗口：区域裁空即可，同时保留事件处理能力。
            desktop::set_empty_region();
            app.region_retry = 0;
            app.zorder_pending = false;
        }
        app
    }

    fn persist(&self) {
        if !self.saver.request(&self.cfg) {
            log::warn!("配置保存通道已关闭，本次修改可能未落盘");
        }
    }

    /// 重新枚举显示器（按左上到右下排序，保证图标填充顺序稳定）。
    /// 返回是否有变化；变化时逐屏打印 DPI 证据。
    fn refresh_monitors(&mut self) -> bool {
        let mut found = desktop::monitor_infos();
        found.sort_by(|a, b| {
            (a.work.x, a.work.y)
                .cmp(&(b.work.x, b.work.y))
                .then(a.work.w.cmp(&b.work.w))
                .then(a.work.h.cmp(&b.work.h))
        });
        if found == self.monitors {
            return false;
        }
        self.monitors = found;
        self.log_monitors();
        true
    }

    /// 逐屏打印工作区与有效 DPI（启动时与显示器变化后各一次，作为混合 DPI 证据）。
    fn log_monitors(&self) {
        for (i, m) in self.monitors.iter().enumerate() {
            log::info!(
                "显示器 #{i}: 工作区 {},{} {}x{}，有效 DPI 缩放 {:.2}{}",
                m.work.x,
                m.work.y,
                m.work.w,
                m.work.h,
                m.scale,
                if m.primary { "（主屏）" } else { "" }
            );
        }
    }

    /// 物理点所在显示器的 DPI 缩放。
    fn monitor_scale_at(&self, x: f32, y: f32) -> f32 {
        desktop::scale_at(&self.monitors, x, y)
    }

    /// 物理点所在显示器的工作区（物理像素）；找不到时退回叠加层范围。
    fn monitor_work_at(&self, x: f32, y: f32) -> (f32, f32, f32, f32) {
        self.monitors
            .iter()
            .find(|m| {
                (x >= m.work.x as f32)
                    && (x <= (m.work.x + m.work.w) as f32)
                    && (y >= m.work.y as f32)
                    && (y <= (m.work.y + m.work.h) as f32)
            })
            .map(|m| {
                (
                    m.work.x as f32,
                    m.work.y as f32,
                    m.work.w as f32,
                    m.work.h as f32,
                )
            })
            .unwrap_or((
                self.overlay.x as f32,
                self.overlay.y as f32,
                self.overlay.w as f32,
                self.overlay.h as f32,
            ))
    }

    /// 主屏缩放：配置中的图标/间距以主屏物理像素为基准，换算成 DIP 后跨屏一致。
    fn primary_scale(&self) -> f32 {
        desktop::primary_scale(&self.monitors).max(1.0)
    }

    /// 某块屏的 UI 缩放系数 = 该屏 DPI / 窗口 pixels_per_point。
    ///
    /// 150% 主屏且窗口 pp=1.5 时 k=1，所有既有尺寸不变；
    /// 100% 副屏 k≈0.667，文字/控件在该屏上保持同样的逻辑大小。
    fn ui_k_at(&self, x: f32, y: f32) -> f32 {
        (self.monitor_scale_at(x, y) / self.scale.max(1.0)).max(0.1)
    }

    /// 图标在指定屏上的物理像素边长（配置值以主屏物理像素为准）。
    fn icon_px_for(&self, icon_size: u32, scale: f32) -> u32 {
        let dip = icon_size as f32 / self.primary_scale();
        // 上限放宽到 256：设置滑杆最大值在 150% 屏上会超过 96 物理像素，
        // 系统 Jumbo 图标正好是 256，超出后再放大已无意义。
        (dip * scale.max(1.0)).round().clamp(16.0, 256.0) as u32
    }

    fn icon_px_on(&self, scale: f32) -> u32 {
        self.icon_px_for(self.cfg.desktop.icon_size, scale)
    }

    /// 纹理请求用的物理尺寸：跟随已合并的 `tex_icon_size`，而不是滑杆当前值。
    fn tex_icon_px_on(&self, scale: f32) -> u32 {
        self.icon_px_for(self.tex_icon_size, scale)
    }

    /// 已应用（纹理实际使用）的各屏图标物理尺寸。
    fn tex_icon_sizes(&self) -> Vec<u32> {
        let mut sizes: Vec<u32> = self
            .monitors
            .iter()
            .map(|m| self.tex_icon_px_on(m.scale))
            .collect();
        sizes.sort_unstable();
        sizes.dedup();
        sizes
    }

    fn toast_msg(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), f64::INFINITY));
    }

    /// 开始行内编辑：让叠加层临时接收键盘（默认 `WS_EX_NOACTIVATE` 收不到按键）。
    fn begin_text_input(&mut self) {
        if self.text_input_active {
            return;
        }
        self.text_input_active = desktop::begin_text_input();
        if !self.text_input_active {
            self.toast_msg("当前窗口无法获得键盘焦点，编辑未开始");
        }
        self.ctx.request_repaint();
    }

    /// 结束行内编辑：恢复叠加层「不抢焦点」并交还前台。
    fn end_text_input(&mut self) {
        if self.text_input_active {
            desktop::end_text_input();
            self.text_input_active = false;
        }
    }

    /// 当前是否有文本输入在编辑：行内编辑框（图标/盒子重命名、待办、搜索）
    /// 或任何获得键盘焦点的 TextEdit。用于区分「文本框复制/粘贴」与「名称选区复制」。
    fn text_editing(&self) -> bool {
        self.text_input_active || self.ctx.memory(|m| m.focused()).is_some()
    }

    /// 把叠加层窗口按当前工作区与缩放重新定位/定尺（显示器变化或 DPI 校正后调用）。
    fn reposition_overlay(&self) {
        let vs = self.overlay;
        let scale = self.scale.max(1.0);
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                vs.x as f32 / scale,
                vs.y as f32 / scale,
            )));
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                vs.w as f32 / scale,
                vs.h as f32 / scale,
            )));
    }

    /// 启动/显示器变化后把盒子夹回当前工作区包围盒内。
    ///
    /// 修复场景：上一次运行在双屏/更高分辨率下保存的坐标，本次单屏启动后会落在屏幕外，
    /// 盒子变成"看不见也点不到"。
    fn clamp_boxes_to_work_area(&mut self) {
        let vs = self.overlay;
        let scale = self.scale.max(1.0);
        let min_w = MIN_W * scale;
        let min_h = MIN_H * scale;
        let mut moved = 0usize;
        for b in self.cfg.boxes.iter_mut() {
            let before = (b.x, b.y, b.w, b.h);
            b.w = b.w.max(min_w).min((vs.w as f32).max(min_w));
            b.h = b.h.max(min_h).min((vs.h as f32).max(min_h));
            b.x =
                b.x.clamp(vs.x as f32, (vs.right() as f32 - 80.0).max(vs.x as f32));
            b.y =
                b.y.clamp(vs.y as f32, (vs.bottom() as f32 - 40.0).max(vs.y as f32));
            b.w = b.w.min((vs.right() as f32 - b.x).max(min_w));
            b.h = b.h.min((vs.bottom() as f32 - b.y).max(min_h));
            if (b.x, b.y, b.w, b.h) != before {
                moved += 1;
            }
        }
        if moved > 0 {
            log::info!(
                "已把 {moved} 个越界盒子夹回工作区：{},{} {}x{}",
                vs.x,
                vs.y,
                vs.w,
                vs.h
            );
            self.persist();
        }
    }

    /// 解析单个盒子的成员引用：源文件真实路径 → 元数据；失败项进入 `invalid`。
    fn refresh_box(&mut self, idx: usize) {
        let Some(cfg) = self.cfg.boxes.get(idx) else {
            return;
        };
        let (mut items, invalid) = model::resolve_members(&cfg.members);
        if cfg.kind == BoxKind::Files {
            model::sort_items_grouped(&mut items);
        } else {
            model::sort_items(&mut items, cfg.sort);
            // 用户在盒子内拖出的手动顺序优先；新成员不在列表里，自动落到末尾。
            model::apply_manual_order(&mut items, &cfg.manual_order);
        }
        // 保留运行时状态（滚动、选区、键盘焦点、本帧布局缓存），只替换成员解析结果。
        let mut rt = std::mem::replace(&mut self.boxes[idx], BoxRuntime::empty());
        rt.items = items;
        rt.invalid = invalid;
        rt.scroll = rt.scroll.max(0.0);
        self.boxes[idx] = rt;
    }

    fn refresh_all_boxes(&mut self) {
        for idx in 0..self.boxes.len() {
            self.refresh_box(idx);
        }
        self.rebuild_desktop();
        self.sync_probe_paths();
    }

    /// 成员集合变化时下发给后台探测线程（去重，避免每帧重复发送）。
    fn sync_probe_paths(&mut self) {
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut keys: Vec<String> = Vec::new();
        for b in &self.cfg.boxes {
            for member in &b.members {
                let path = Path::new(member);
                let key = model::path_key(path);
                if !keys.contains(&key) {
                    keys.push(key);
                    paths.push(path.to_path_buf());
                }
            }
        }
        keys.sort();
        let sig = keys.join("\n");
        if sig != self.probe_paths_sig {
            self.probe_paths_sig = sig;
            log::info!("成员存在性监听更新：{} 项", paths.len());
            self.probe.set_paths(paths);
        }
    }

    /// 所有盒子成员路径的比较键集合（用于桌面排除）。
    fn member_keys(&self) -> std::collections::HashSet<String> {
        self.cfg
            .boxes
            .iter()
            .flat_map(|b| b.members.iter())
            .map(|m| model::path_key(Path::new(m)))
            .collect()
    }

    /// 记录引用归属：不移动/复制文件，只在盒子成员里登记源路径。
    ///
    /// 一个文件只属于一个盒子：加入新盒子时从其它盒子移除。返回是否新增。
    fn add_member(&mut self, idx: usize, path: &Path) -> bool {
        let key = model::path_key(path);
        let raw = path.to_string_lossy().to_string();
        let mut added = false;
        for (i, b) in self.cfg.boxes.iter_mut().enumerate() {
            if i == idx {
                if !b
                    .members
                    .iter()
                    .any(|m| model::path_key(Path::new(m)) == key)
                {
                    b.members.push(raw.clone());
                    added = true;
                }
            } else {
                b.members.retain(|m| model::path_key(Path::new(m)) != key);
                b.manual_order.retain(|k| *k != key);
            }
        }
        if added {
            self.refresh_all_boxes();
        }
        added
    }

    /// 删除引用归属（文件保持原位）。返回是否有改动。
    fn remove_member(&mut self, idx: usize, path: &Path) -> bool {
        let key = model::path_key(path);
        let Some(b) = self.cfg.boxes.get_mut(idx) else {
            return false;
        };
        let before = b.members.len();
        b.members.retain(|m| model::path_key(Path::new(m)) != key);
        b.manual_order.retain(|k| *k != key);
        if b.members.len() != before {
            self.refresh_all_boxes();
            true
        } else {
            false
        }
    }

    fn apply_theme(&mut self) {
        self.dark = theme::is_dark(self.cfg.theme);
        self.pal = theme::palette(self.dark);
        theme::apply(&self.ctx, self.dark);
    }

    fn apply_desktop_layer(&mut self) {
        let want = self.cfg.desktop.hide_system_icons && self.overlay_visible;
        desktop::set_takeover_enabled(want);
        if want {
            if self.layer.hide() {
                desktop::write_session_flag(true);
            } else {
                desktop::write_session_flag(false);
            }
        } else {
            self.layer.restore();
            desktop::write_session_flag(false);
        }
        desktop::refresh_desktop();
    }

    fn set_overlay_visible(&mut self, visible: bool) {
        self.overlay_visible = visible;
        self.cfg.general.show_overlay = visible;
        message_window::set_overlay_visible(visible);
        if visible {
            // 隐藏期间区域被裁空；重新显示后重新应用工作区裁剪与层级。
            self.zorder_pending = true;
            self.region_retry = 8;
        } else {
            // 不真正隐藏窗口：窗口隐藏后 winit 不再派发重绘，托盘"显示/退出"会失效。
            // 裁成空区域即可达到"看不见、不挡鼠标"，同时保留事件循环。
            desktop::set_empty_region();
            self.region_retry = 0;
            self.zorder_pending = false;
        }
        self.apply_desktop_layer();
        // 叠加层隐藏/恢复会改变空格接管条件，立即同步，避免失效期间仍吞空格。
        self.sync_preview_hook();
        log::info!(
            "叠加层{}（系统图标接管: {}）",
            if visible { "已显示" } else { "已隐藏" },
            if self.overlay_visible && self.cfg.desktop.hide_system_icons {
                "开"
            } else {
                "关"
            }
        );
        self.persist();
    }

    fn watch_specs(&self) -> Vec<WatchSpec> {
        let specs = vec![
            WatchSpec {
                dir: util::user_desktop_dir(),
                recursive: false,
            },
            WatchSpec {
                dir: util::public_desktop_dir(),
                recursive: false,
            },
        ];
        specs
    }

    fn rescan_all(&mut self) {
        watcher::initial_scan(self.sink.clone(), self.watch_specs());
    }

    fn rebuild_desktop(&mut self) {
        let mut items = self.desktop_user.clone();
        let mut seen: std::collections::HashSet<String> =
            items.iter().map(|i| i.name.to_lowercase()).collect();
        for item in &self.desktop_public {
            if seen.insert(item.name.to_lowercase()) {
                items.push(item.clone());
            }
        }
        // 已归属盒子的条目仍在桌面磁盘上，但网格不再重复显示。
        model::retain_non_members(&mut items, &self.member_keys());
        // 「此电脑」「回收站」是程序合成的系统项：不写盘、不占成员，也不与真实桌面文件重名冲突。
        if !items
            .iter()
            .any(|i| i.name.eq_ignore_ascii_case("此电脑") || ItemMeta::is_computer_path(&i.path))
        {
            items.push(ItemMeta::computer());
        }
        if !items.iter().any(|i| {
            i.name.eq_ignore_ascii_case("回收站") || ItemMeta::is_recycle_bin_path(&i.path)
        }) {
            items.push(ItemMeta::recycle_bin(self.recycle_full));
        }
        model::sort_items(&mut items, self.cfg.desktop.sort);
        // 桌面 manual_order 已废弃：只作为一次性迁移输入（旧顺序 → 棋盘占位快照），
        // 之后位置唯一来源是 `desktop.slots`。
        //
        // 迁移必须等「桌面扫描完成 + 显示器几何就绪」：启动早期 desktop_items 只有
        // 「此电脑/回收站」两项，此时快照会只写入 2 个槽位并清掉旧顺序，随后其余
        // 图标只能按空格回填（曾经导致 84 项挤成 3 行、旧顺序彻底丢失）。
        let ready = self.board_layout_ready();
        let migrating =
            self.cfg.desktop.slots.is_empty() && !self.cfg.desktop.manual_order.is_empty() && ready;
        let legacy_order = if migrating {
            let order = std::mem::take(&mut self.cfg.desktop.manual_order);
            model::apply_manual_order(&mut items, &order);
            order
        } else {
            Vec::new()
        };
        // 系统项固定排最前（此电脑 → 回收站），紧凑排列与棋盘快照都会占住左上角前两格。
        model::system_first(&mut items);
        self.desktop_items = items;
        self.prune_desk_selection();
        if migrating {
            if self.snapshot_current_layout() {
                log::info!("桌面占位：已按旧手动顺序迁移为棋盘占位");
            } else {
                self.cfg.desktop.manual_order = legacy_order;
            }
            return;
        }
        if self.cfg.desktop.board_placement && ready {
            if self.cfg.desktop.slots.is_empty() {
                self.snapshot_current_layout();
            } else {
                self.sync_desktop_board();
            }
        }
    }
}

impl DeskApp {
    fn drain_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                BgEvent::ClipboardChanged => self.clip.notify(),
                BgEvent::Tray(cmd) => self.handle_tray(cmd),
                BgEvent::DisplayChanged => self.on_display_changed(),
                BgEvent::SystemThemeChanged => {
                    // 只刷新主题，不重排窗口，避免设置变化时叠加层跳一下。
                    self.apply_theme();
                }
                BgEvent::ToggleDesktop => {
                    crate::platform::show_desktop::toggle();
                    // 普通窗口的最小化/还原可能触发系统重排 z 序：立刻校准一次
                    // 「原生桌面之上、普通窗口之下」，并补一帧重绘避免 DWM 空表面。
                    crate::platform::desktop::ensure_overlay_layer();
                    self.zorder_pending = true;
                    self.ctx.request_repaint();
                }
                BgEvent::CopySelection => self.copy_selected_name(),
                BgEvent::ShowClipboardBox => {
                    // 旧面板已移除：热键改为唤出/恢复剪贴板盒子。
                    self.show_clipboard_box();
                }
                BgEvent::Activate => {
                    // 第二次启动唤起：先确保叠加层可见（可能此前被隐藏），再唤出剪贴板盒。
                    if !self.overlay_visible {
                        self.set_overlay_visible(true);
                    }
                    self.show_clipboard_box();
                    self.zorder_pending = true;
                    self.ctx.request_repaint();
                }
                BgEvent::RecycleBinState(full) => {
                    if full != self.recycle_full {
                        self.recycle_full = full;
                        self.rebuild_desktop();
                        log::info!("回收站状态变化：{}", if full { "非空" } else { "空" });
                        self.ctx.request_repaint();
                    }
                }
                BgEvent::DirContents { dir, items } => self.on_dir_contents(dir, items),
                BgEvent::IconLoaded(resp) => {
                    if resp.key.starts_with("thumb:")
                        && self.perf_first_thumb.is_none()
                        && !resp.rgba.is_empty()
                    {
                        self.perf_first_thumb = Some(std::time::Instant::now());
                        if let Some(t0) = self.perf_start {
                            log::info!(
                                "[perf] 首个缩略图就绪：启动后 {:.0} ms（{}）",
                                t0.elapsed().as_secs_f64() * 1000.0,
                                resp.key
                            );
                        }
                    }
                    let ctx = self.ctx.clone();
                    self.textures.insert_response(&ctx, &resp);
                }
                BgEvent::History { records } => {
                    self.clip_box_records = records;
                }
                BgEvent::HistoryChanged => {
                    if self.box_index_by_kind(BoxKind::Clipboard).is_some() {
                        self.query_clip_box();
                    }
                }
                BgEvent::FileMoved { record, error } => {
                    self.pending_moves = self.pending_moves.saturating_sub(1);
                    match (record, error) {
                        (Some(rec), _) => {
                            let name = rec
                                .to
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default();
                            self.undo.push(rec);
                            self.toast_msg(format!("已移动 {name} · Ctrl+Z 可撤销"));
                            // 批量移动结束：强制补扫一次，避免监听事件合并造成的中间态残留。
                            if self.pending_moves == 0 {
                                self.rescan_all();
                            }
                        }
                        (None, Some(err)) => {
                            log::error!("文件操作失败: {err}");
                            self.toast_msg(format!("文件操作失败：{err}"));
                        }
                        (None, None) => self.toast_msg("已撤销上一步"),
                    }
                }
                BgEvent::Status(msg) => self.toast_msg(msg),
                BgEvent::ShellMenu(action) => self.handle_shell_menu(action),
                BgEvent::DesktopMenu(action) => self.handle_desktop_menu(action),
                BgEvent::DesktopMenuFallback { x, y, reason } => {
                    if crate::platform::shell_menu::menu_open() {
                        log::info!("菜单：原生菜单仍在显示，忽略迟到的桌面降级事件");
                    } else {
                        log::warn!("桌面原生菜单不可用（{reason}），显示内置菜单");
                        let pos = self.phys_to_logical(x, y);
                        log::info!(
                            "菜单[t={}] UI 绘制内置桌面菜单 物理@{x},{y} -> 逻辑@{:.1},{:.1}",
                            crate::platform::shell_menu::now_ms(),
                            pos.x,
                            pos.y
                        );
                        self.desktop_fallback_menu = Some(pos);
                    }
                }
                BgEvent::ShellMenuFallback(req) => {
                    if crate::platform::shell_menu::menu_open() {
                        log::info!("菜单：原生菜单仍在显示，忽略迟到的文件降级事件");
                    } else {
                        self.open_fallback_menu(req);
                    }
                }
                BgEvent::TogglePreview => {
                    // 不在 drain 阶段立即处理：同一帧里鼠标点选可能还没被 egui 输入处理，
                    // 立即判定会看到旧的 sel_item=None。延迟到 draw_overlay 之后再判定。
                    self.pending_toggle_preview = true;
                    self.ctx.request_repaint();
                }
                BgEvent::ClosePreview => self.close_preview("Esc"),
                BgEvent::CopyPreviewText => self.copy_preview_text(),
                BgEvent::PreviewReady(data) => {
                    if let Some(pv) = self.preview.as_mut() {
                        if pv.path == data.path {
                            log::info!(
                                "[preview] 数据就绪 {}（{} 字，{:.1} ms），从请求到显示 {:.1} ms",
                                data.path.display(),
                                data.text.as_ref().map(|t| t.chars().count()).unwrap_or(0),
                                data.elapsed_ms,
                                (self.ctx.input(|i| i.time) - pv.opened_at) * 1000.0
                            );
                            pv.data = Some(data);
                            self.ctx.request_repaint();
                        }
                    }
                }
                BgEvent::NavKey { dir, extend } => self.handle_nav_key(dir, extend),
                BgEvent::SelectAllBox => self.handle_select_all_in_box(),
                BgEvent::SelectAllDesktop => self.handle_select_all_desktop(),
                BgEvent::ClearBoxSelection => {
                    self.clear_box_selection_all();
                    self.sel_item = None;
                    self.group_menu = None;
                    self.clear_desk_selection("Esc 清空");
                    self.sync_preview_hook();
                    self.ctx.request_repaint();
                }
                BgEvent::FileRecycled { paths, ok, error } => {
                    if ok {
                        self.purge_paths(&paths, "本应用删除");
                    } else {
                        let msg = error.unwrap_or_else(|| "未知错误".to_string());
                        log::warn!("回收站删除失败: {msg}");
                        self.toast_msg(format!("删除失败：{msg}"));
                    }
                }
                BgEvent::MembersProbed {
                    deleted,
                    unreachable,
                    checked,
                    elapsed_ms,
                } => {
                    if !unreachable.is_empty() {
                        log::info!(
                            "成员探测：{} 条暂时不可达（盘符卸载/网络断开），保留引用",
                            unreachable.len()
                        );
                    }
                    if !deleted.is_empty() {
                        let keys: HashSet<String> = deleted.into_iter().collect();
                        let n = self.purge_keys(&keys, "文件已删除");
                        log::info!(
                            "成员探测：检查 {checked} 项（{elapsed_ms:.1} ms），清理 {n} 条失效引用"
                        );
                    }
                }
            }
        }
    }

    fn handle_desktop_menu(&mut self, action: DesktopMenuAction) {
        match action {
            DesktopMenuAction::AutoGroup => self.auto_group_all(),
            DesktopMenuAction::NewBox => self.create_box(),
            DesktopMenuAction::NewClipboardBox => self.create_clip_box(),
            DesktopMenuAction::NewTodoBox => self.create_todo_box(),
            DesktopMenuAction::Settings => self.open_settings(),
            DesktopMenuAction::ReloadDesktop => {
                self.rescan_all();
                self.toast_msg("已重新加载桌面与盒子内容");
            }
            DesktopMenuAction::UndoLast => self.undo_last(),
            DesktopMenuAction::RestoreAutoArrange => self.restore_auto_arrange(),
            DesktopMenuAction::EnableBoardPlacement => self.enable_board_placement(),
            DesktopMenuAction::ToggleOverlay => {
                self.set_overlay_visible(!self.overlay_visible);
            }
        }
    }

    /// 原生桌面菜单不可用时，在右键位置绘制内置菜单。
    fn draw_desktop_fallback_menu(&mut self, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
        let Some(pos) = self.desktop_fallback_menu else {
            return;
        };
        // 原生菜单存在期间绝不叠加自绘菜单；迟到的降级状态直接丢弃。
        if crate::platform::shell_menu::menu_open() {
            self.desktop_fallback_menu = None;
            return;
        }
        let mut close = false;
        let area = egui::Area::new(Id::unique("desktop-fallback-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ui.ctx(), |ui| {
                theme::menu_frame(&self.pal, self.dark).show(ui, |ui| {
                    ui.set_min_width(200.0);
                    if ui.button("自动整理").clicked() {
                        actions.push(UiAction::AutoGroupAll);
                        close = true;
                    }
                    if ui.button("恢复自动排列").clicked() {
                        actions.push(UiAction::RestoreAutoArrange);
                        close = true;
                    }
                    if ui.button("启用棋盘摆放（拖到空格）").clicked() {
                        actions.push(UiAction::EnableBoardPlacement);
                        close = true;
                    }
                    ui.menu_button("拖到已占格", |ui| {
                        for mode in [OccupiedDropMode::Swap, OccupiedDropMode::NewBox] {
                            let selected = self.cfg.desktop.drop_on_occupied == mode;
                            if ui.radio(selected, mode.label()).clicked() {
                                actions.push(UiAction::SetOccupiedDropMode(mode));
                                ui.close();
                                close = true;
                            }
                        }
                    });
                    ui.separator();
                    if ui.button("新建盒子").clicked() {
                        actions.push(UiAction::NewBox);
                        close = true;
                    }
                    if ui.button("新建剪贴板盒").clicked() {
                        actions.push(UiAction::NewClipBox);
                        close = true;
                    }
                    if ui.button("新建待办盒").clicked() {
                        actions.push(UiAction::NewTodoBox);
                        close = true;
                    }
                    if ui.button("粘贴文件到桌面").clicked() {
                        actions.push(UiAction::PasteIntoDesktop);
                        close = true;
                    }
                    ui.menu_button("桌面图标排序", |ui| {
                        for mode in SortMode::ALL {
                            let selected = self.cfg.desktop.sort == mode;
                            if ui.radio(selected, mode.label()).clicked() {
                                actions.push(UiAction::SetDeskSort(mode));
                                ui.close();
                                close = true;
                            }
                        }
                    });
                    if ui.button("刷新内容").clicked() {
                        actions.push(UiAction::Refresh);
                        close = true;
                    }
                    if ui.button("撤销上一步移动  Ctrl+Z").clicked() {
                        actions.push(UiAction::Undo);
                        close = true;
                    }
                    ui.separator();
                    if ui.button("设置").clicked() {
                        actions.push(UiAction::OpenSettings);
                        close = true;
                    }
                    if ui.button("隐藏整理层").clicked() {
                        actions.push(UiAction::HideOverlay);
                        close = true;
                    }
                });
            });
        let rect = area.response.rect;
        let pointer = ui.ctx().input(|i| i.pointer.interact_pos());
        let clicked_outside = ui
            .ctx()
            .input(|i| i.pointer.any_click() && pointer.is_some_and(|p| !rect.contains(p)));
        if close || clicked_outside || ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
            self.desktop_fallback_menu = None;
        }
    }

    fn handle_tray(&mut self, cmd: TrayCommand) {
        match cmd {
            TrayCommand::ShowClipboardBox => self.show_clipboard_box(),
            TrayCommand::ToggleOverlay => self.set_overlay_visible(!self.overlay_visible),
            TrayCommand::TogglePause => {
                self.cfg.clipboard.enabled = !self.cfg.clipboard.enabled;
                self.apply_clipboard_state();
                self.persist();
                let msg = if self.cfg.clipboard.enabled {
                    "已开始记录剪贴板"
                } else {
                    "已暂停记录剪贴板"
                };
                self.toast_msg(msg);
            }
            TrayCommand::Settings => self.open_settings(),
            TrayCommand::NewBox => self.create_box(),
            TrayCommand::AutoGroup => self.auto_group_all(),
            TrayCommand::RestoreAutoArrange => self.restore_auto_arrange(),
            TrayCommand::EnableBoardPlacement => self.enable_board_placement(),
            TrayCommand::Exit => {
                log::info!("托盘菜单选择退出：恢复桌面图标层并关闭进程");
                self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn apply_clipboard_state(&mut self) {
        if let Ok(mut guard) = self.clip_cfg.write() {
            *guard = self.cfg.clipboard.clone();
        }
        let paused = !self.cfg.clipboard.enabled;
        self.clip_paused.store(paused, Ordering::Relaxed);
        self.clip.set_paused(paused);
        message_window::set_paused(paused);
        self.store.send(StoreCmd::SetLimits {
            max_items: self.cfg.clipboard.max_items.max(1),
            max_bytes: self.cfg.clipboard.max_bytes,
        });
    }

    fn on_config_changed(&mut self, old: &AppConfig) {
        if old.clipboard != self.cfg.clipboard {
            self.apply_clipboard_state();
            if old.clipboard.hotkey != self.cfg.clipboard.hotkey {
                match HotkeySpec::from_config(&self.cfg.clipboard.hotkey) {
                    Some(spec) => {
                        if !message_window::update_hotkey(Some(spec)) {
                            self.toast_msg("热键更新失败（消息窗口未就绪）");
                        }
                    }
                    None => self.toast_msg("热键格式无效，已保留原热键"),
                }
            }
        }
        if old.desktop.sort != self.cfg.desktop.sort {
            if self.cfg.desktop.board_placement {
                // 占位布局下排序不改变已占位项，只影响「恢复自动排列」后的顺序。
                self.toast_msg("已占位图标位置不变；恢复自动排列后按新排序");
            } else {
                model::sort_items(&mut self.desktop_items, self.cfg.desktop.sort);
            }
        }
        if old.desktop.icon_size != self.cfg.desktop.icon_size
            || old.desktop.spacing != self.cfg.desktop.spacing
            || old.desktop.label_gap != self.cfg.desktop.label_gap
            || old.desktop.show_labels != self.cfg.desktop.show_labels
        {
            // 网格尺寸变化后夹取旧占位；冲突项回填第一个空格，绝不丢项。
            self.sync_desktop_board();
        }
        if old.desktop.icon_size != self.cfg.desktop.icon_size {
            // 布局立即用新尺寸（绘制读 cfg）；纹理请求延迟 30ms 合并，滑杆连续步进时
            // 只解码最终尺寸。旧尺寸纹理先保留，绘制端会拉伸占位，避免图标消失/闪烁。
            let now = self.ctx.input(|i| i.time);
            self.icon_resize_at = Some(now + ICON_RESIZE_DEBOUNCE);
            // 滑杆松开后可能不再有输入事件，主动安排一帧来应用合并后的尺寸。
            self.ctx
                .request_repaint_after(std::time::Duration::from_millis(35));
        }
        if old.desktop.box_opacity != self.cfg.desktop.box_opacity {
            // 全局不透明度：立即同步到所有盒子的背景填充与新盒子默认值。
            let alpha = self.cfg.desktop.box_opacity;
            self.cfg.desktop.box_defaults.alpha = alpha;
            for b in &mut self.cfg.boxes {
                b.alpha = alpha;
            }
        }
        if old.theme != self.cfg.theme {
            self.apply_theme();
        }
        if old.general.autostart != self.cfg.general.autostart {
            if let Err(e) = autostart::set_enabled(self.cfg.general.autostart) {
                log::warn!("设置开机自启失败: {e}");
                self.toast_msg(format!("设置开机自启失败：{e}"));
            }
        }
        if old.general.log_debug != self.cfg.general.log_debug {
            log::set_max_level(if self.cfg.general.log_debug {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            });
        }
        if old.desktop.hide_system_icons != self.cfg.desktop.hide_system_icons {
            self.apply_desktop_layer();
        }
        if old.general.show_overlay != self.cfg.general.show_overlay {
            self.set_overlay_visible(self.cfg.general.show_overlay);
        }
        self.persist();
    }

    fn on_dir_contents(&mut self, dir: PathBuf, items: Vec<ItemMeta>) {
        if dir == util::user_desktop_dir() {
            let prev = std::mem::take(&mut self.desktop_user_paths);
            let ready = self.desktop_user_ready;
            self.desktop_user_paths = items.iter().map(|i| i.path.clone()).collect();
            self.desktop_user_ready = true;
            self.desktop_user = items.clone();
            self.rebuild_desktop();
            // 桌面扫描结果本身就是"文件已不在桌面目录"的证据：旧集合里消失且仍是
            // 盒子成员的路径，直接清理引用（盒子与桌面同帧一致）。
            let disappeared = self.disappeared_member_keys(&prev, &self.desktop_user_paths);
            if !disappeared.is_empty() {
                self.purge_keys(&disappeared, "桌面文件消失");
            }
            if ready {
                self.auto_group_new(items, &prev);
            }
            // 成员仍在桌面磁盘上：借桌面扫描同步大小/修改时间，并按需剔除重复显示。
            self.refresh_all_boxes();
            return;
        }
        if dir == util::public_desktop_dir() {
            let prev = std::mem::take(&mut self.desktop_public_paths);
            let ready = self.desktop_public_ready;
            self.desktop_public_paths = items.iter().map(|i| i.path.clone()).collect();
            self.desktop_public_ready = true;
            self.desktop_public = items.clone();
            self.rebuild_desktop();
            let disappeared = self.disappeared_member_keys(&prev, &self.desktop_public_paths);
            if !disappeared.is_empty() {
                self.purge_keys(&disappeared, "桌面文件消失");
            }
            if ready {
                self.auto_group_new(items, &prev);
            }
            self.refresh_all_boxes();
        }
    }

    /// 旧扫描集合里消失、且仍然被某个盒子引用的路径键。
    fn disappeared_member_keys(
        &self,
        prev: &std::collections::HashSet<PathBuf>,
        current: &std::collections::HashSet<PathBuf>,
    ) -> HashSet<String> {
        prev.iter()
            .filter(|p| !current.contains(*p))
            .map(|p| model::path_key(p))
            .filter(|key| {
                self.cfg.boxes.iter().any(|b| {
                    b.members
                        .iter()
                        .any(|m| model::path_key(Path::new(m)) == *key)
                })
            })
            .collect()
    }

    /// 新出现的桌面图标按配置自动归类；首次扫描（`ready == false`）不搬动既有图标。
    fn auto_group_new(&mut self, items: Vec<ItemMeta>, prev: &std::collections::HashSet<PathBuf>) {
        if !self.cfg.desktop.auto_group {
            return;
        }
        let added: Vec<ItemMeta> = items
            .into_iter()
            .filter(|i| !prev.contains(&i.path))
            .collect();
        if !added.is_empty() {
            self.auto_group_items(added);
        }
    }

    /// 盒子类型 → 索引。
    fn box_index_by_kind(&self, kind: BoxKind) -> Option<usize> {
        self.cfg.boxes.iter().position(|b| b.kind == kind)
    }

    /// 某个图标应进入哪个盒子；`None` 表示不归类。
    fn group_target_for(&self, item: &ItemMeta) -> Option<usize> {
        let ext = item.extension();
        let rule = self
            .cfg
            .desktop
            .category_targets
            .get(model::category_of(&ext))
            .copied()
            .unwrap_or(GroupTarget::Auto);
        let kind = match rule {
            GroupTarget::Off => return None,
            GroupTarget::Files => BoxKind::Files,
            GroupTarget::Dirs => BoxKind::Dirs,
            GroupTarget::Auto => {
                // `.website` 是文件夹形态的网页快捷方式，归一化到文件盒子里的「网页」段。
                if item.is_dir && ext != "website" {
                    BoxKind::Dirs
                } else {
                    BoxKind::Files
                }
            }
        };
        self.box_index_by_kind(kind)
    }

    fn auto_group_items(&mut self, items: Vec<ItemMeta>) -> usize {
        let overrides: std::collections::HashSet<String> =
            self.cfg.desktop.manual_overrides.iter().cloned().collect();
        let mut grouped = 0;
        for item in items {
            // 合成系统项（此电脑）没有磁盘路径，不进入任何盒子。
            if item.is_system() {
                continue;
            }
            let key = item.path.to_string_lossy().to_string();
            if overrides.contains(&key) {
                log::info!("自动归类跳过（手动覆盖）: {key}");
                continue;
            }
            let Some(idx) = self.group_target_for(&item) else {
                continue;
            };
            // 纯引用：只在盒子成员里登记路径，绝不移动/复制文件。
            if self.add_member(idx, &item.path) {
                let name = self.cfg.boxes[idx].name.clone();
                log::info!("自动归类（引用）{} -> {}", item.path.display(), name);
                grouped += 1;
            }
        }
        if grouped > 0 {
            self.persist();
        }
        grouped
    }

    /// 立即把当前桌面上的图标按类型整理进分类盒子（设置页按钮触发）。
    fn auto_group_all(&mut self) {
        let items = self.desktop_items.clone();
        let grouped = self.auto_group_items(items);
        if grouped > 0 {
            self.toast_msg(format!("已整理 {grouped} 项（文件保持原位未被移动）"));
        } else if !self.desktop_items.is_empty() {
            self.toast_msg("没有可收纳的图标（可能已被手动覆盖排除）");
        } else {
            self.toast_msg("桌面上没有可整理的图标");
        }
    }

    fn record_manual_override(&mut self, path: &Path) {
        let key = path.to_string_lossy().to_string();
        if self.cfg.desktop.manual_overrides.contains(&key) {
            return;
        }
        self.cfg.desktop.manual_overrides.push(key);
        if self.cfg.desktop.manual_overrides.len() > 500 {
            self.cfg.desktop.manual_overrides.drain(0..100);
        }
        self.persist();
    }

    fn clear_manual_override(&mut self, path: &Path) {
        let key = path.to_string_lossy().to_string();
        if let Some(pos) = self
            .cfg
            .desktop
            .manual_overrides
            .iter()
            .position(|k| *k == key)
        {
            self.cfg.desktop.manual_overrides.remove(pos);
            self.persist();
        }
    }

    fn on_display_changed(&mut self) {
        // 以 egui 实际 pixels_per_point 为准：窗口逻辑尺寸/鼠标坐标都按它换算，
        // 若与 GetDpiForSystem 不一致（如混合 DPI），用系统值会导致盒子整体偏移。
        self.scale = self.ctx.pixels_per_point().max(1.0);
        self.overlay = desktop::work_area();
        log::info!(
            "显示器变化：工作区 {},{} {}x{}，窗口 pixels_per_point {:.2}",
            self.overlay.x,
            self.overlay.y,
            self.overlay.w,
            self.overlay.h,
            self.scale
        );
        self.refresh_monitors();
        // 换屏后图标物理尺寸立刻变化：纹理按新尺寸请求；旧尺寸先留作占位，1s 后清理。
        self.tex_icon_size = self.cfg.desktop.icon_size;
        self.icon_resize_at = None;
        self.icon_prune_at = Some(self.ctx.input(|i| i.time) + ICON_PRUNE_DELAY);
        self.reposition_overlay();
        self.clamp_boxes_to_work_area();
        // 显示器数量/分辨率变化会改变棋盘容量：旧占位夹取到新网格，冲突项回填空。
        self.sync_desktop_board();
        self.apply_theme();
        self.apply_desktop_layer();
        if self.overlay_visible {
            self.zorder_pending = true;
            self.region_retry = 8;
        } else {
            desktop::set_empty_region();
            self.zorder_pending = false;
            self.region_retry = 0;
        }
        self.persist();
    }

    /// 唤出/聚焦剪贴板盒；若盒子被关闭则恢复显示，若不存在则创建。
    /// （旧的独立浮动面板已移除，热键改为唤出桌面盒子。）
    fn show_clipboard_box(&mut self) {
        if !self.overlay_visible {
            self.overlay_visible = true;
            message_window::set_overlay_visible(true);
            self.apply_desktop_layer();
            self.zorder_pending = true;
            self.region_retry = 8;
            self.persist();
        }
        let idx = match self.box_index_by_kind(BoxKind::Clipboard) {
            Some(i) => i,
            None => {
                self.create_clip_box();
                match self.box_index_by_kind(BoxKind::Clipboard) {
                    Some(i) => i,
                    None => return,
                }
            }
        };
        if self.cfg.boxes[idx].closed {
            self.cfg.boxes[idx].closed = false;
            self.persist();
            log::info!("剪贴板盒已从关闭状态恢复显示");
        }
        // 关闭时可能积压了采样事件，重新拉取最新历史。
        self.query_clip_box();
        self.toast_msg("剪贴板盒已显示");
    }

    /// 打开独立的设置窗口（托盘菜单与设置入口共用）。
    fn open_settings(&mut self) {
        self.settings_open = true;
        self.settings_needs_focus = true;
        log::info!("打开独立设置窗口");
    }

    /// 设置窗口默认居中到主显示器工作区。
    fn settings_center(&self) -> Pos2 {
        let area = desktop::primary_work_area();
        let scale = self.scale.max(1.0);
        let (w, h) = (SETTINGS_W, SETTINGS_H);
        let x = area.x as f32 / scale + ((area.w as f32 / scale - w) * 0.5).max(8.0);
        let y = area.y as f32 / scale + ((area.h as f32 / scale - h) * 0.45).max(8.0);
        egui::pos2(x, y)
    }

    fn create_clip_box(&mut self) {
        if self.box_index_by_kind(BoxKind::Clipboard).is_some() {
            self.toast_msg("剪贴板盒已存在");
            return;
        }
        self.create_box_kind(BoxKind::Clipboard, Some("剪贴板盒"));
        self.query_clip_box();
    }

    /// 待办盒支持多开：每次创建都是独立盒子（条目、排序、几何、收起互不影响）。
    fn create_todo_box(&mut self) {
        let mut n = self
            .cfg
            .boxes
            .iter()
            .filter(|b| b.kind == BoxKind::Todo)
            .count()
            + 1;
        let name = loop {
            let candidate = format!("待办盒 {n}");
            if !self.cfg.boxes.iter().any(|b| b.name == candidate) {
                break candidate;
            }
            n += 1;
        };
        self.create_box_kind(BoxKind::Todo, Some(&name));
        self.toast_msg("已新建待办盒，点击顶部输入框添加待办");
    }

    /// 盒子内容计数（折叠时显示）。
    fn box_count(&self, idx: usize) -> usize {
        match self.cfg.boxes.get(idx).map(|b| b.kind) {
            Some(BoxKind::Clipboard) => {
                let filter = self.cfg.boxes[idx].clip_filter;
                self.clip_box_records
                    .iter()
                    .filter(|r| record_matches(r, filter, ""))
                    .count()
            }
            Some(BoxKind::Todo) => self.cfg.boxes[idx].todos.len(),
            _ => self.boxes.get(idx).map(|b| b.items.len()).unwrap_or(0),
        }
    }

    /// 桌面剪贴板盒的基线查询：全部类型、无搜索，盒内再做筛选。
    fn query_clip_box(&mut self) {
        let _ = self.store.send(StoreCmd::List { limit: 500 });
    }

    /// 点击剪贴板盒条目：把内容写回剪贴板（不模拟粘贴）。
    fn copy_record_back(&mut self, rec: ClipRecord) {
        let sink = self.sink.clone();
        let spawned = std::thread::Builder::new()
            .name("clip-box-copy".into())
            .spawn(move || match paste_record(&rec, false, WindowRef(0)) {
                Ok(()) => sink.emit(BgEvent::Status("已复制到剪贴板".into())),
                Err(e) => sink.emit(BgEvent::Status(format!("复制失败：{e}"))),
            });
        if let Err(e) = spawned {
            log::error!("启动剪贴板盒复制线程失败: {e}");
        }
    }

    /// 双击剪贴板盒条目：写回剪贴板并（按配置）粘贴到当前前台窗口。
    fn start_paste(&mut self, rec: ClipRecord, previous: WindowRef) {
        let auto_paste = self.cfg.clipboard.auto_paste;
        let sink = self.sink.clone();
        let spawned = std::thread::Builder::new()
            .name("clip-paste".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(120));
                match paste_record(&rec, auto_paste, previous) {
                    Ok(()) => sink.emit(BgEvent::Status("已粘贴到目标窗口".into())),
                    Err(e) => sink.emit(BgEvent::Status(format!("粘贴失败：{e}"))),
                }
            });
        if let Err(e) = spawned {
            log::error!("启动粘贴线程失败: {e}");
        }
    }

    fn spawn_move(&mut self, path: PathBuf, dest: PathBuf) {
        self.pending_moves += 1;
        let sink = self.sink.clone();
        let spawned = std::thread::Builder::new()
            .name("file-move".into())
            .spawn(move || match fileops::move_into(&path, &dest) {
                Ok(to) => sink.emit(BgEvent::FileMoved {
                    record: Some(MoveRecord { from: path, to }),
                    error: None,
                }),
                Err(e) => sink.emit(BgEvent::FileMoved {
                    record: None,
                    error: Some(e.to_string()),
                }),
            });
        if let Err(e) = spawned {
            self.pending_moves = self.pending_moves.saturating_sub(1);
            log::error!("启动移动线程失败: {e}");
            self.toast_msg(format!("无法启动后台移动：{e}"));
        }
    }

    fn undo_last(&mut self) {
        if self.pending_moves > 0 {
            self.toast_msg("还有文件操作未完成，请稍后再撤销");
            return;
        }
        let Some(record) = self.undo.pop() else {
            self.toast_msg("没有可撤销的操作");
            return;
        };
        // 撤销后如果回到桌面，视为用户手动放回，避免又被自动归类拉进盒子。
        if let Some(parent) = record.from.parent() {
            if parent == util::user_desktop_dir() || parent == util::public_desktop_dir() {
                self.record_manual_override(&record.from);
            }
        }
        self.pending_moves += 1;
        let sink = self.sink.clone();
        let spawned = std::thread::Builder::new()
            .name("file-undo".into())
            .spawn(move || match fileops::undo_record(&record) {
                Ok(_) => sink.emit(BgEvent::FileMoved {
                    record: None,
                    error: None,
                }),
                Err(e) => sink.emit(BgEvent::FileMoved {
                    record: None,
                    error: Some(format!("撤销失败：{e}")),
                }),
            });
        if let Err(e) = spawned {
            self.pending_moves = self.pending_moves.saturating_sub(1);
            log::error!("启动撤销线程失败: {e}");
            self.toast_msg(format!("无法启动后台撤销：{e}"));
        }
    }

    fn paste_into(&mut self, box_idx: Option<usize>) {
        let Some(ClipContent::Files(paths)) = clipboard_native::read_clipboard() else {
            self.toast_msg("剪贴板里没有文件列表");
            return;
        };
        if paths.is_empty() {
            self.toast_msg("剪贴板里没有文件");
            return;
        }
        match box_idx {
            Some(idx) => {
                // 粘贴进盒子 = 记录引用，不移动文件。
                let mut added = 0;
                for path in paths {
                    if self.add_member(idx, &path) {
                        added += 1;
                    }
                }
                if added > 0 {
                    self.persist();
                }
                self.toast_msg(format!("已向盒子添加 {added} 个引用（文件保持原位）"));
            }
            None => {
                let dest = util::user_desktop_dir();
                for path in paths {
                    self.spawn_move(path, dest.clone());
                }
            }
        }
    }

    /// 复制当前名称选区文本（由键盘钩子转交，或叠加层恰好有焦点时的 Ctrl+C 走这里）。
    fn copy_selected_name(&mut self) {
        if !self.text_sel.has_selection() {
            return;
        }
        let text = self.text_sel.selected.clone();
        if text.is_empty() {
            return;
        }
        let chars = text.chars().count();
        // 名称选区复制仍走项目自己的原生剪贴板写入：叠加层通常没有前台焦点，
        // egui 的平台复制输出依赖窗口聚焦，直接写系统剪贴板更可靠。
        let ok = crate::platform::clipboard_native::write_text(&text, None);
        if ok {
            self.toast_msg(format!("已复制名称选中文本（{chars} 字）"));
        } else {
            self.toast_msg("写入剪贴板失败");
        }
    }

    /// 请求在消息窗口线程弹出 Windows 原生菜单；「在盒子内移动」列出可移动目标。
    fn open_shell_menu(&mut self, box_idx: Option<usize>, item: usize, x: i32, y: i32) {
        let meta = match box_idx {
            None => self.desktop_items.get(item),
            Some(i) => self.boxes.get(i).and_then(|b| b.items.get(item)),
        };
        let Some(meta) = meta else {
            return;
        };
        // 系统项（此电脑）只弹系统菜单，不追加文件类动作，也不列「在盒子内移动」。
        let system = meta.is_system();
        let boxes = if system {
            Vec::new()
        } else {
            self.cfg
                .boxes
                .iter()
                .enumerate()
                .filter(|(i, b)| {
                    Some(*i) != box_idx
                        && matches!(b.kind, BoxKind::Normal | BoxKind::Files | BoxKind::Dirs)
                })
                .map(|(i, b)| (i, b.name.clone()))
                .collect()
        };
        let request = crate::platform::shell_menu::ShellMenuRequest {
            path: meta.path.clone(),
            screen_x: x,
            screen_y: y,
            from_box: box_idx,
            item,
            boxes,
            system,
        };
        // 原生菜单不可用（消息窗口不存在 / 入队或投递失败）时退回内置菜单；
        // 原生菜单在线程内建立失败则由 BgEvent::ShellMenuFallback 走同一入口。
        if let Err(err) = crate::platform::shell_menu::request(request) {
            log::warn!("原生右键菜单不可用（{}），退回内置菜单", err.reason);
            self.open_fallback_menu(err.request);
        }
    }

    /// 物理屏幕坐标 → 叠加层逻辑点：先减去叠加层原点，再按 DPI 缩放换算。
    fn phys_to_logical(&self, x: i32, y: i32) -> egui::Pos2 {
        let s = self.scale.max(1.0);
        egui::pos2(
            (x - self.overlay.x) as f32 / s,
            (y - self.overlay.y) as f32 / s,
        )
    }

    /// 在点击位置显示内置菜单（原生菜单降级路径）。
    fn open_fallback_menu(&mut self, req: crate::platform::shell_menu::ShellMenuRequest) {
        log::info!(
            "菜单[t={}] UI 绘制内置文件菜单 menu_open={}",
            crate::platform::shell_menu::now_ms(),
            crate::platform::shell_menu::menu_open()
        );
        let pos = self.phys_to_logical(req.screen_x, req.screen_y);
        self.fallback_menu = Some(FallbackMenu { request: req, pos });
    }

    /// 处理原生菜单里我们追加项的选择结果。
    fn handle_shell_menu(&mut self, action: ShellMenuAction) {
        match action {
            ShellMenuAction::Rename { box_idx, item } => {
                self.begin_rename(box_idx, item);
            }
            ShellMenuAction::CopyPath(path) => {
                let text = path.to_string_lossy().to_string();
                if crate::platform::clipboard_native::write_text(&text, None) {
                    self.toast_msg("已复制文件路径");
                } else {
                    self.toast_msg("写入剪贴板失败");
                }
            }
            ShellMenuAction::AddToClipboard(path) => {
                let text = path.to_string_lossy().to_string();
                if crate::platform::clipboard_native::write_text(&text, None) {
                    self.toast_msg("已加入剪贴板盒");
                } else {
                    self.toast_msg("写入剪贴板失败");
                }
            }
            ShellMenuAction::MoveToBox { from, item, to } => {
                let path = match from {
                    None => self.desktop_items.get(item).map(|i| i.path.clone()),
                    Some(b) => self
                        .boxes
                        .get(b)
                        .and_then(|rt| rt.items.get(item))
                        .map(|i| i.path.clone()),
                };
                let Some(path) = path else {
                    return;
                };
                if to >= self.cfg.boxes.len() {
                    return;
                }
                if self.add_member(to, &path) {
                    self.persist();
                    self.toast_msg("已加入目标盒子（文件保持原位）");
                } else {
                    self.toast_msg("该文件已在目标盒子中");
                }
            }
            ShellMenuAction::Eject { box_idx, item } => {
                let Some(path) = self
                    .boxes
                    .get(box_idx)
                    .and_then(|rt| rt.items.get(item))
                    .map(|i| i.path.clone())
                else {
                    return;
                };
                // 移出只删引用，文件从未离开原位。
                if let Some(name) = path.file_name() {
                    self.record_manual_override(&path.with_file_name(name));
                }
                if self.remove_member(box_idx, &path) {
                    self.persist();
                    self.toast_msg("已移出盒子（文件仍在原位置）");
                }
            }
            ShellMenuAction::SystemVerb { verb, path } => {
                let zh = if verb == "cut" { "剪切" } else { "删除" };
                log::info!("系统{zh}已执行，转入后台存在性确认: {}", path.display());
                // 不在这里用 Path::exists()：网络路径/卸载盘符会阻塞且会误判。
                // 交给探测线程确认「卷可用且路径不存在」后再清理引用。
                self.probe.confirm_now(vec![path.clone()]);
                self.toast_msg(format!("系统{zh}已执行，文件消失后将自动清理盒子引用"));
            }
            ShellMenuAction::Delete { box_idx, item } => {
                let path = match box_idx {
                    None => self.desktop_items.get(item).map(|i| i.path.clone()),
                    Some(b) => self
                        .boxes
                        .get(b)
                        .and_then(|rt| rt.items.get(item))
                        .map(|i| i.path.clone()),
                };
                let Some(path) = path else {
                    return;
                };
                crate::platform::recycle::spawn_recycle(vec![path], self.sink.clone());
            }
            ShellMenuAction::Reveal(path) => {
                if let Err(e) = shell::reveal_in_explorer(&path) {
                    self.toast_msg(format!("定位失败：{e}"));
                }
            }
        }
    }

    /// 进入图标名称行内编辑。
    fn begin_rename(&mut self, box_idx: Option<usize>, item: usize) {
        let meta = match box_idx {
            None => self.desktop_items.get(item),
            Some(i) => self.boxes.get(i).and_then(|b| b.items.get(item)),
        };
        let Some(meta) = meta else {
            return;
        };
        // 合成系统项（此电脑/回收站）没有真实文件名可改。
        if meta.is_system() {
            self.toast_msg(format!("「{}」是系统项，不能重命名", meta.name));
            return;
        }
        self.text_sel.clear();
        self.name_edit = Some(NameEdit {
            target: match box_idx {
                None => LabelTarget::desktop(item),
                Some(i) => LabelTarget::in_box(i, item),
            },
            path: meta.path.clone(),
            text: meta.name.clone(),
            focus: true,
        });
        self.begin_text_input();
    }

    /// 进入盒子标题行内编辑。
    fn begin_box_rename(&mut self, idx: usize) {
        let Some(b) = self.cfg.boxes.get(idx) else {
            return;
        };
        self.box_name_edit = Some(BoxNameEdit {
            idx,
            text: b.name.clone(),
            focus: true,
        });
        self.begin_text_input();
    }

    /// 绘制盒子标题行内编辑框：全选原标题、Enter 提交、Esc 取消、失焦提交。
    fn draw_box_name_editor(&mut self, ui: &mut egui::Ui, idx: usize, rect: Rect, k: f32) {
        let Some(mut edit) = self.box_name_edit.take() else {
            return;
        };
        if edit.idx != idx {
            self.box_name_edit = Some(edit);
            return;
        }
        let id = Id::unique(("box-name-edit", idx));
        ui.painter().rect_filled(
            rect.expand2(egui::vec2(2.0, 1.0)),
            CornerRadius::same(4),
            Color32::from_black_alpha(190),
        );
        let resp = ui
            .scope(|ui| {
                theme::apply_inline_edit_visuals(ui);
                ui.put(
                    rect,
                    egui::TextEdit::singleline(&mut edit.text)
                        .id(id)
                        .font(FontId::proportional(14.0 * k))
                        .text_color(Color32::WHITE)
                        .frame(egui::Frame::NONE)
                        .desired_width(rect.width())
                        .margin(egui::vec2(2.0, 1.0)),
                )
            })
            .inner;
        if edit.focus {
            resp.request_focus();
            // 进入编辑时全选原标题，直接输入即可整体替换。
            if let Some(mut state) = egui::text_edit::TextEditState::load(ui.ctx(), id) {
                let end = edit.text.chars().count();
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        egui::text::CCursor::new(end),
                    )));
                state.store(ui.ctx(), id);
            }
            edit.focus = false;
        }
        if resp.clicked() && !resp.has_focus() {
            self.begin_text_input();
            resp.request_focus();
        }
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if esc {
            self.end_text_input();
            self.toast_msg("已取消重命名");
        } else if enter || resp.lost_focus() {
            self.end_text_input();
            self.commit_box_rename(idx, &edit.text);
        } else {
            self.box_name_edit = Some(edit);
        }
    }

    /// 提交盒子改名：只改显示名并立即写配置，绝不触发任何文件系统操作。
    fn commit_box_rename(&mut self, idx: usize, text: &str) {
        let name = match validate_box_name(text) {
            Ok(name) => name,
            Err(msg) => {
                self.toast_msg(format!("重命名失败：{msg}"));
                // 保留编辑态，用户可以继续修改，不会无声失败。
                if let Some(b) = self.cfg.boxes.get(idx) {
                    self.box_name_edit = Some(BoxNameEdit {
                        idx,
                        text: text.to_string(),
                        focus: true,
                    });
                    let _ = b;
                }
                return;
            }
        };
        let duplicate = self
            .cfg
            .boxes
            .iter()
            .enumerate()
            .any(|(i, b)| i != idx && b.name == name);
        if let Some(b) = self.cfg.boxes.get_mut(idx) {
            b.name = name.clone();
        }
        self.persist();
        if duplicate {
            self.toast_msg(format!("已重命名为「{name}」（存在同名盒子）"));
        } else {
            self.toast_msg(format!("已重命名为「{name}」"));
        }
    }
    /// 绘制并处理名称行内编辑框（放在盒子绘制之后，保证盖住原标签的命中区）。
    fn draw_name_editor(&mut self, ui: &mut egui::Ui) {
        let Some(mut edit) = self.name_edit.take() else {
            self.name_edit_zone = None;
            return;
        };
        let Some(zone) = self.name_edit_zone else {
            // 目标本帧不可见（滚动出界/被筛选隐藏）：保留编辑状态，等它重新可见。
            self.name_edit = Some(edit);
            return;
        };
        let font = self.name_edit_font.clamp(8.0, 40.0);
        let rect = Rect::from_min_size(
            zone.min,
            egui::vec2(
                zone.width().max(40.0),
                (font * 1.55).max(zone.height().min(font * 2.5)),
            ),
        );
        ui.painter().rect_filled(
            rect.expand2(egui::vec2(2.0, 1.0)),
            CornerRadius::same(4),
            Color32::from_black_alpha(190),
        );
        let resp = ui
            .scope(|ui| {
                theme::apply_inline_edit_visuals(ui);
                ui.put(
                    rect,
                    egui::TextEdit::singleline(&mut edit.text)
                        .id(Id::unique((
                            "name-edit",
                            match edit.target.box_idx {
                                None => usize::MAX,
                                Some(i) => i,
                            },
                            edit.target.item,
                        )))
                        .font(FontId::proportional(font))
                        .text_color(Color32::WHITE)
                        .frame(egui::Frame::NONE)
                        .desired_width(rect.width())
                        .margin(egui::vec2(2.0, 1.0)),
                )
            })
            .inner;
        if edit.focus {
            resp.request_focus();
            edit.focus = false;
        }
        if resp.clicked() && !resp.has_focus() {
            // 程序化 request_focus 在个别环境下不生效，用户点进输入框时再兜底一次。
            self.begin_text_input();
            resp.request_focus();
        }
        // 单行 TextEdit 回车同帧失焦：has_focus / lost_focus 都要判断。
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if enter || (resp.lost_focus() && !esc) {
            self.end_text_input();
            self.commit_rename(edit);
        } else if esc {
            self.end_text_input();
            self.toast_msg("已取消重命名");
        } else {
            self.name_edit = Some(edit);
        }
    }

    /// 提交重命名：真实文件/目录走 `fs::rename`，失败必须给出可见 toast。
    fn commit_rename(&mut self, edit: NameEdit) {
        let old_name = edit
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some(parent) = edit.path.parent() else {
            self.toast_msg("重命名失败：无法确定所在目录");
            return;
        };
        // 同目录兄弟名只用于纯函数判重；读目录失败时退回 fs::rename 的系统错误提示。
        let siblings: Vec<String> = std::fs::read_dir(parent)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().to_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let new = match validate_rename(&old_name, &edit.text, &siblings) {
            Ok(None) => return,
            Ok(Some(new)) => new,
            Err(msg) => {
                self.toast_msg(format!("重命名失败：{msg}"));
                return;
            }
        };
        let target = parent.join(&new);
        match std::fs::rename(&edit.path, &target) {
            Ok(()) => {
                self.after_rename(&edit.path, &target, &new);
                self.toast_msg(format!("已重命名为「{new}」"));
            }
            Err(e) => {
                log::warn!(
                    "重命名失败 {} -> {}: {e}",
                    edit.path.display(),
                    target.display()
                );
                self.toast_msg(rename_failure_msg(&e));
            }
        }
    }

    /// 重命名成功后同步内存模型与缓存：路径、标签排版、图标纹理、手动覆盖记录。
    fn after_rename(&mut self, old: &Path, new_path: &Path, new_name: &str) {
        let old_name = old
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let update = |items: &mut Vec<ItemMeta>| {
            if let Some(item) = items.iter_mut().find(|i| i.path == *old) {
                item.path = new_path.to_path_buf();
                item.name = new_name.to_string();
            }
        };
        update(&mut self.desktop_items);
        update(&mut self.desktop_public);
        for rt in &mut self.boxes {
            update(&mut rt.items);
        }
        // 文件盒子在重命名后仍保持「类型连续」排序；其它盒子沿用自身排序规则。
        for (i, cfg) in self.cfg.boxes.iter().enumerate() {
            let Some(rt) = self.boxes.get_mut(i) else {
                continue;
            };
            if cfg.kind == BoxKind::Files {
                model::sort_items_grouped(&mut rt.items);
            } else {
                model::sort_items(&mut rt.items, cfg.sort);
                model::apply_manual_order(&mut rt.items, &cfg.manual_order);
            }
        }
        // 盒子成员是源路径引用：重命名后必须同步，否则引用会失效。
        let old_path_key = model::path_key(old);
        let new_path_raw = new_path.to_string_lossy().to_string();
        let mut members_changed = false;
        for b in &mut self.cfg.boxes {
            for m in &mut b.members {
                if model::path_key(Path::new(m)) == old_path_key {
                    *m = new_path_raw.clone();
                    members_changed = true;
                }
            }
            // 盒子手动顺序同样按路径保存，改名后同步，避免顺序键失效。
            for k in &mut b.manual_order {
                if model::path_key(Path::new(k)) == old_path_key {
                    *k = new_path_raw.clone();
                    members_changed = true;
                }
            }
        }
        if members_changed {
            self.persist();
        }
        // 旧路径的图标纹理与旧名称的换行测量缓存必须清掉，否则会残留旧名字纹理。
        self.textures.forget_path(old);
        self.label_cache.forget(&old_name);
        // 桌面手动覆盖记录按路径保存：重命名后同步到新路径，否则再次出现会被自动归类拉走。
        let old_key = old.to_string_lossy().to_string();
        if let Some(key) = self
            .cfg
            .desktop
            .manual_overrides
            .iter_mut()
            .find(|k| *k == &old_key)
        {
            *key = new_path.to_string_lossy().to_string();
            self.persist();
        }
        // 棋盘占位按 path_key 保存：重命名后同步键，否则该项会失去原格。
        let old_order_key = model::path_key(old);
        let new_order_key = model::path_key(new_path);
        let mut slot_changed = false;
        for slot in &mut self.cfg.desktop.slots {
            if slot.key == old_order_key {
                slot.key = new_order_key.clone();
                slot_changed = true;
            }
        }
        if slot_changed {
            self.persist();
        }
        // 桌面多选集合同样按 path_key 保存：重命名后同步，避免选区丢项。
        if self.desk_selected.remove(&old_order_key) {
            self.desk_selected.insert(new_order_key.clone());
        }
        if self.desk_anchor.as_deref() == Some(old_order_key.as_str()) {
            self.desk_anchor = Some(new_order_key);
        }
        self.text_sel.clear();
        // 兜底补扫一次，确保监听事件合并时不出现旧名残留。
        self.rescan_all();
    }

    /// 把当前名称选区矩形换算成屏幕物理像素交给键盘钩子，用于限定 Ctrl+C 拦截范围。
    /// 编辑态下不武装：此时 Ctrl+C 必须留给聚焦的文本框，不能走名称选区复制。
    fn sync_copy_region(&self) {
        let region = if !self.text_editing() && self.text_sel.has_selection() {
            self.text_sel.zone.map(|z| {
                let s = self.scale.max(1.0);
                let ox = self.overlay.x as f32;
                let oy = self.overlay.y as f32;
                (
                    (ox + z.min.x * s).floor() as i32,
                    (oy + z.min.y * s).floor() as i32,
                    (ox + z.max.x * s).ceil() as i32,
                    (oy + z.max.y * s).ceil() as i32,
                )
            })
        } else {
            None
        };
        crate::platform::key_hook::set_copy_region(region);
    }

    fn create_box(&mut self) {
        self.create_box_kind(BoxKind::Normal, None);
    }

    fn create_box_kind(&mut self, kind: BoxKind, name: Option<&str>) {
        self.create_box_kind_at(kind, name, None);
    }

    /// 新建盒子并返回其下标。
    ///
    /// `anchor_px = Some((cx, cy))` 时把盒子中心对准落点（虚拟屏幕物理像素）并夹回所在屏
    /// 工作区，用于「拖到已占格 → 生成新盒子」；否则沿用光标所在屏右侧停靠。
    fn create_box_kind_at(
        &mut self,
        kind: BoxKind,
        name: Option<&str>,
        anchor_px: Option<(f32, f32)>,
    ) -> usize {
        let n = self.cfg.boxes.len();
        let (work, box_scale) = if let Some((cx, cy)) = anchor_px {
            (
                self.monitor_work_at(cx, cy),
                self.monitor_scale_at(cx, cy).max(1.0),
            )
        } else {
            // 新建盒子默认停靠到光标的当前屏右侧边栏（统一边距，纵向依次排布）。
            let area = desktop::cursor_work_area();
            (
                (area.x as f32, area.y as f32, area.w as f32, area.h as f32),
                self.monitor_scale_at((area.x + area.w / 2) as f32, (area.y + area.h / 2) as f32)
                    .max(1.0),
            )
        };
        let margin = 18.0 * box_scale;
        let gap = 14.0 * box_scale;
        let defaults = self.cfg.desktop.box_defaults.clone();
        let w =
            (defaults.w.max(240.0) * box_scale).min((work.2 - margin * 2.0).max(MIN_W * box_scale));
        let h =
            (defaults.h.max(160.0) * box_scale).min((work.3 - margin * 2.0).max(MIN_H * box_scale));
        let others: Vec<(f32, f32, f32, f32)> = self
            .cfg
            .boxes
            .iter()
            .map(|b| (b.x, b.y, b.w, b.h))
            .collect();
        let (mut x, mut y) = if let Some((cx, cy)) = anchor_px {
            (
                (cx - w * 0.5).clamp(work.0, (work.0 + work.2 - w).max(work.0)),
                (cy - h * 0.5).clamp(work.1, (work.1 + work.3 - h).max(work.1)),
            )
        } else {
            model::docked_box_pos(work, w, h, margin, gap, &others)
        };
        // 复用已有磁吸：让新盒子初始即与右侧边栏/相邻盒子对齐（落点新建不做磁吸，避免偏离落点）。
        if anchor_px.is_none() && self.cfg.desktop.snap_enabled {
            let out = model::snap_rect(
                x,
                y,
                w,
                h,
                work,
                &others,
                self.cfg.desktop.snap_threshold as f32,
            );
            x = out.x;
            y = out.y;
        }
        let id = util::gen_id("box-");
        self.cfg.boxes.push(BoxConfig {
            id,
            name: name
                .map(str::to_string)
                .unwrap_or_else(|| format!("盒子 {}", n + 1)),
            kind,
            x,
            y,
            w,
            h,
            color: defaults.color,
            alpha: defaults.alpha,
            view: self.cfg.desktop.default_box_view,
            ..Default::default()
        });
        self.boxes.push(BoxRuntime::empty());
        self.persist();
        self.toast_msg("已新建盒子，把桌面图标拖进来即可收纳（文件保持原位）");
        n
    }

    fn delete_box(&mut self, idx: usize) {
        if idx >= self.cfg.boxes.len() || idx >= self.boxes.len() {
            return;
        }
        // 解散盒子只丢弃引用，文件从未被搬走，因此不做任何文件操作。
        self.boxes.remove(idx);
        self.cfg.boxes.remove(idx);
        self.persist();
        self.refresh_all_boxes();
        self.toast_msg("盒子已解散（未删除任何文件，桌面已重新显示这些图标）");
    }

    /// 删除待办盒子：连同其全部待办事项一起清空，与「关闭（可重开）」是两条路径。
    fn delete_todo_box(&mut self, idx: usize) {
        if idx >= self.cfg.boxes.len() || idx >= self.boxes.len() {
            return;
        }
        let count = self.cfg.boxes.get(idx).map(|b| b.todos.len()).unwrap_or(0);
        self.boxes.remove(idx);
        self.cfg.boxes.remove(idx);
        self.persist();
        self.refresh_all_boxes();
        self.toast_msg(format!("待办盒子已删除，清空 {count} 条事项"));
    }

    fn apply_actions(&mut self, actions: Vec<UiAction>) {
        for action in actions {
            match action {
                UiAction::NewBox => self.create_box(),
                UiAction::RenameBox(idx) => self.begin_box_rename(idx),
                UiAction::ColorBox(idx) => {
                    if let Some(b) = self.cfg.boxes.get(idx) {
                        self.modal = Modal::Color {
                            idx,
                            color: b.color,
                            alpha: b.alpha,
                        };
                    }
                }
                UiAction::ToggleCollapse(idx) => {
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        b.collapsed = !b.collapsed;
                    }
                    self.persist();
                }
                UiAction::ToggleLock(idx) => {
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        b.locked = !b.locked;
                    }
                    self.persist();
                }
                UiAction::CloseBox(idx) => {
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        b.closed = true;
                    }
                    self.persist();
                    self.rebuild_desktop();
                    self.toast_msg("盒子已关闭（内容与几何保留，可在设置页重新打开）");
                }
                UiAction::ReopenBox(idx) => {
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        b.closed = false;
                    }
                    self.persist();
                    self.refresh_all_boxes();
                    self.toast_msg("盒子已重新打开");
                }
                UiAction::AutoGroupAll => self.auto_group_all(),
                UiAction::ClearClipboard => {
                    let _ = self.store.send(StoreCmd::Clear);
                    self.query_clip_box();
                    self.toast_msg("剪贴板历史已清空");
                }
                UiAction::ClipDelete(id) => {
                    let _ = self.store.send(StoreCmd::Delete(id));
                    self.query_clip_box();
                }
                UiAction::ClipSetPinned { id, pinned } => {
                    let _ = self.store.send(StoreCmd::SetPinned { id, pinned });
                    self.query_clip_box();
                }
                UiAction::ClipPaste(rec) => {
                    self.start_paste(rec, WindowRef::foreground_if_other());
                }
                UiAction::ClipTogglePause => {
                    let enabled = self.cfg.clipboard.enabled;
                    self.cfg.clipboard.enabled = !enabled;
                    self.apply_clipboard_state();
                    self.persist();
                    self.toast_msg(if enabled {
                        "已暂停剪贴板记录"
                    } else {
                        "已恢复剪贴板记录"
                    });
                }
                UiAction::SetFileFilter(idx, filter) => {
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        if b.file_filter != filter {
                            b.file_filter = filter;
                            // 切换 tab 后回到顶部：旧偏移可能超出新分类的内容高度。
                            if let Some(rt) = self.boxes.get_mut(idx) {
                                rt.scroll = 0.0;
                            }
                            self.persist();
                        }
                    }
                }
                UiAction::SetBoxView(idx, view) => {
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        if b.view != view {
                            b.view = view;
                            if let Some(rt) = self.boxes.get_mut(idx) {
                                rt.scroll = 0.0;
                            }
                            self.persist();
                        }
                    }
                }
                UiAction::DissolveBox(idx) => {
                    if self.cfg.boxes.get(idx).is_some() {
                        self.modal = Modal::Confirmation(BoxConfirm::Dissolve { idx });
                    }
                }
                UiAction::DeleteTodoBox(idx) => {
                    let is_todo = self
                        .cfg
                        .boxes
                        .get(idx)
                        .is_some_and(|b| b.kind == BoxKind::Todo);
                    if is_todo {
                        self.modal = Modal::Confirmation(BoxConfirm::DeleteTodo { idx });
                    }
                }
                UiAction::PasteIntoBox(idx) => self.paste_into(Some(idx)),
                UiAction::NewFolderInBox(idx) => {
                    match crate::platform::shell_menu::create_new_folder() {
                        Ok(path) => {
                            let added = self.add_members(idx, std::slice::from_ref(&path));
                            log::info!(
                                "盒子菜单：新建文件夹 {} 加入盒子 #{idx} added={added}",
                                path.display()
                            );
                            self.ctx.request_repaint();
                        }
                        Err(e) => {
                            log::warn!("盒子菜单：新建文件夹失败: {e}");
                            self.toast_msg(format!("新建文件夹失败：{e}"));
                        }
                    }
                }
                UiAction::NewTextInBox(idx) => {
                    match crate::platform::shell_menu::create_new_text_document() {
                        Ok(path) => {
                            let added = self.add_members(idx, std::slice::from_ref(&path));
                            log::info!(
                                "盒子菜单：新建文本文档 {} 加入盒子 #{idx} added={added}",
                                path.display()
                            );
                            self.ctx.request_repaint();
                        }
                        Err(e) => {
                            log::warn!("盒子菜单：新建文本文档失败: {e}");
                            self.toast_msg(format!("新建文本文档失败：{e}"));
                        }
                    }
                }
                UiAction::PruneInvalid(idx) => {
                    let invalid = self
                        .boxes
                        .get(idx)
                        .map(|rt| rt.invalid.clone())
                        .unwrap_or_default();
                    if !invalid.is_empty() {
                        let keys: Vec<String> = invalid
                            .iter()
                            .map(|s| model::path_key(Path::new(s)))
                            .collect();
                        let mut removed = 0usize;
                        if let Some(b) = self.cfg.boxes.get_mut(idx) {
                            let before = b.members.len();
                            b.members.retain(|m| {
                                !keys.iter().any(|k| *k == model::path_key(Path::new(m)))
                            });
                            removed = before.saturating_sub(b.members.len());
                        }
                        log::info!("清理失效引用：盒子 {idx} 移除 {removed} 条成员");
                        self.persist();
                        self.refresh_all_boxes();
                        self.toast_msg(format!("已清理 {removed} 条失效引用"));
                    }
                }
                UiAction::SetBoxSort(idx, mode) => {
                    let grouped = self
                        .cfg
                        .boxes
                        .get(idx)
                        .is_some_and(|b| b.kind == BoxKind::Files);
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        b.sort = mode;
                        // 切换排序即视为放弃手动顺序，避免旧键把新排序结果拉回原位。
                        b.manual_order.clear();
                    }
                    if let Some(rt) = self.boxes.get_mut(idx) {
                        if grouped {
                            model::sort_items_grouped(&mut rt.items);
                        } else {
                            model::sort_items(&mut rt.items, mode);
                        }
                    }
                    self.persist();
                }
                UiAction::SetDeskSort(mode) => {
                    self.cfg.desktop.sort = mode;
                    if self.cfg.desktop.board_placement {
                        // 占位是位置唯一来源：排序只影响「恢复自动排列」后的顺序。
                        self.toast_msg("已占位图标位置不变；恢复自动排列后按新排序");
                    } else {
                        model::sort_items(&mut self.desktop_items, mode);
                    }
                    self.persist();
                }
                UiAction::RestoreAutoArrange => self.restore_auto_arrange(),
                UiAction::EnableBoardPlacement => self.enable_board_placement(),
                UiAction::SetOccupiedDropMode(mode) => {
                    self.cfg.desktop.drop_on_occupied = mode;
                    self.persist();
                    self.toast_msg(format!("拖到已占格：{}", mode.label()));
                }
                UiAction::Open(path) => {
                    if let Err(e) = shell::open_path(&path) {
                        self.toast_msg(format!("打开失败：{e}"));
                    }
                }
                UiAction::PasteIntoDesktop => self.paste_into(None),
                UiAction::CopyClip(rec) => self.copy_record_back(rec),
                UiAction::BeginRename { box_idx, item } => {
                    // 多选态下不做单条行内重命名（与资源管理器一致）。
                    let blocked = match box_idx {
                        Some(idx) => self
                            .boxes
                            .get(idx)
                            .and_then(|rt| rt.items.get(item))
                            .map(|it| self.box_multi_selected(idx, &it.path))
                            .unwrap_or(false),
                        None => self
                            .desktop_items
                            .get(item)
                            .is_some_and(|it| self.desk_multi_selected(&it.path)),
                    };
                    if blocked {
                        self.toast_msg("多选状态下不支持行内重命名".to_string());
                    } else {
                        self.begin_rename(box_idx, item);
                    }
                }
                UiAction::ShowShellMenu {
                    box_idx,
                    item,
                    x,
                    y,
                } => self.open_shell_menu(box_idx, item, x, y),
                UiAction::ShellMenu(action) => self.handle_shell_menu(action),
                UiAction::NewClipBox => self.create_clip_box(),
                UiAction::NewTodoBox => self.create_todo_box(),
                UiAction::TodoAdd(idx, text) => {
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        b.todos.push(crate::config::TodoItem {
                            id: util::gen_id("todo-"),
                            text,
                            done: false,
                        });
                        self.persist();
                    }
                }
                UiAction::TodoToggle(idx, id) => {
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        if let Some(t) = b.todos.iter_mut().find(|t| t.id == id) {
                            t.done = !t.done;
                        }
                        self.persist();
                    }
                }
                UiAction::TodoMoveTop(idx, id) => {
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        if let Some(pos) = b.todos.iter().position(|t| t.id == id) {
                            let item = b.todos.remove(pos);
                            b.todos.insert(0, item);
                            b.todo_sort = TodoSort::Manual;
                        }
                        self.persist();
                    }
                }
                UiAction::TodoDelete(idx, id) => {
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        b.todos.retain(|t| t.id != id);
                        self.persist();
                    }
                }
                UiAction::TodoCommit(idx, id, text) => {
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        if let Some(t) = b.todos.iter_mut().find(|t| t.id == id) {
                            if text.trim().is_empty() {
                                b.todos.retain(|t| t.id != id);
                            } else {
                                t.text = text;
                            }
                        }
                        self.persist();
                    }
                }
                UiAction::GroupDelete(paths) => {
                    self.group_menu = None;
                    // 系统项（此电脑/回收站）是合成项，没有可回收的真实路径。
                    let movable: Vec<PathBuf> = paths
                        .iter()
                        .filter(|p| !ItemMeta::is_system_path(p))
                        .cloned()
                        .collect();
                    let skipped = paths.len() - movable.len();
                    if skipped > 0 {
                        self.toast_msg(format!("{skipped} 个系统项不能删除，已跳过"));
                    }
                    if !movable.is_empty() {
                        crate::platform::recycle::spawn_recycle(movable, self.sink.clone());
                    }
                }
                UiAction::GroupAddToBox { paths, to } => {
                    self.group_menu = None;
                    let movable: Vec<PathBuf> = paths
                        .iter()
                        .filter(|p| !ItemMeta::is_system_path(p))
                        .cloned()
                        .collect();
                    let skipped = paths.len() - movable.len();
                    let n = self.add_members(to, &movable);
                    if skipped > 0 {
                        self.toast_msg(format!(
                            "已加入 {n} 项；{skipped} 个系统项未移动（文件保持原位）"
                        ));
                    } else if n > 0 {
                        self.toast_msg(format!("已加入 {n} 项到目标盒子（文件保持原位）"));
                    }
                    if n > 0 {
                        self.clear_desk_selection("整组加入盒子");
                    }
                }
                UiAction::GroupCopyPaths(paths) => {
                    self.group_menu = None;
                    let text = paths
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect::<Vec<_>>()
                        .join("\r\n");
                    if crate::platform::clipboard_native::write_text(&text, None) {
                        self.toast_msg(format!("已复制 {} 条路径", paths.len()));
                    } else {
                        self.toast_msg("写入剪贴板失败".to_string());
                    }
                }
                UiAction::GroupEject { box_idx, paths } => {
                    self.group_menu = None;
                    let n = self.remove_members(box_idx, &paths);
                    if n > 0 {
                        self.toast_msg(format!("已移出 {n} 项（文件仍在原位置）"));
                    }
                }
                UiAction::GroupMoveToBox { from, paths, to } => {
                    self.group_menu = None;
                    let n = self.add_members(to, &paths);
                    if n > 0 {
                        self.boxes[from].selected.clear();
                        self.boxes[from].focus = None;
                        self.boxes[from].anchor = None;
                        self.toast_msg(format!("已移动 {n} 项到目标盒子（文件保持原位）"));
                    }
                }
                UiAction::GroupAddClip(paths) => {
                    self.group_menu = None;
                    let text = paths
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect::<Vec<_>>()
                        .join("\r\n");
                    if crate::platform::clipboard_native::write_text(&text, None) {
                        self.toast_msg(format!("已加入 {} 条路径到剪贴板", paths.len()));
                    } else {
                        self.toast_msg("写入剪贴板失败".to_string());
                    }
                }
                UiAction::Undo => self.undo_last(),
                UiAction::OpenSettings => self.open_settings(),
                UiAction::HideOverlay => self.set_overlay_visible(false),
                UiAction::Refresh => {
                    self.rescan_all();
                    self.toast_msg("已刷新桌面与盒子内容");
                }
            }
        }
    }
}

impl DeskApp {
    /// 点击/轻点一个真实条目：记录唯一选中项；预览已打开时跟随切换到新条目。
    fn select_item(&mut self, path: &Path, name: &str, modified: u64, anchor: Rect) {
        let same = self.sel_item.as_ref().is_some_and(|s| s.path == path);
        log::info!(
            "[preview] 选中 {}（之前同项={same}，格子 {:.0}x{:.0} @ {:.0},{:.0}，指针 {:?}，叠加层 {:?} 缩放 {:.2}）",
            path.display(),
            anchor.width(),
            anchor.height(),
            anchor.min.x,
            anchor.min.y,
            self.ctx.input(|i| i.pointer.interact_pos()),
            (self.overlay.x, self.overlay.y),
            self.scale
        );
        self.sel_item = Some(Selection {
            path: path.to_path_buf(),
            name: name.to_string(),
            modified,
            anchor,
        });
        if !same && self.preview.is_some() {
            log::info!("[preview] 选中项变化，预览跟随切换 {}", path.display());
            self.request_preview(path, name, modified, anchor);
        }
        // 选区变化立即同步给键盘钩子：等到帧末再同步会让「点选后马上按空格」的
        // 第一次按键落在旧 armed=false 上而被放行，表现为"偶发打不开预览"。
        self.sync_preview_hook();
    }

    /// 盒子当前筛选下的显示顺序（显示下标 → 真实下标）。
    fn box_filtered_indices(&self, idx: usize) -> Vec<usize> {
        match self.cfg.boxes.get(idx) {
            Some(b) => model::filtered_indices(&self.boxes[idx].items, b.file_filter),
            None => Vec::new(),
        }
    }

    /// 显示下标对应的格子矩形（列表为整行，网格为整格），已扣除滚动偏移。
    fn box_cell_rect(&self, idx: usize, display: usize) -> Rect {
        let rt = &self.boxes[idx];
        if rt.nav_view == BoxView::List {
            Rect::from_min_size(
                egui::pos2(
                    rt.nav_origin.x,
                    rt.nav_origin.y + display as f32 * rt.nav_row_h - rt.scroll,
                ),
                egui::vec2(rt.nav_list.width(), rt.nav_row_h),
            )
        } else {
            let cols = rt.nav_cols.max(1);
            let col = (display % cols) as f32;
            let row = (display / cols) as f32;
            Rect::from_min_size(
                egui::pos2(
                    rt.nav_origin.x + col * rt.nav_cell.x,
                    rt.nav_origin.y + row * rt.nav_cell.y - rt.scroll,
                ),
                rt.nav_cell,
            )
        }
    }

    /// 真实下标 → 当前筛选下的显示下标 → 预览锚点矩形。
    ///
    /// 盒子在筛选态/列表/缩略图视图下，格子序都是「显示序」；真实下标不能直接当格子序用。
    fn box_display_anchor(&self, idx: usize, real: usize) -> Option<Rect> {
        let filtered = self.box_filtered_indices(idx);
        let display = filtered.iter().position(|&r| r == real)?;
        Some(self.box_cell_rect(idx, display))
    }

    /// 点击条目：Ctrl 切换、Shift 区间、无修饰单选；并让其它盒子/桌面失去选区。
    fn apply_box_click(&mut self, idx: usize, real: usize, path: &Path, mods: egui::Modifiers) {
        self.clear_box_selection_except(idx);
        self.focus_box = Some(idx);
        let key = model::path_key(path);
        if mods.shift {
            let filtered = self.box_filtered_indices(idx);
            let target = filtered
                .iter()
                .position(|&r| self.boxes[idx].items[r].path == *path);
            let anchor_key = self.boxes[idx].anchor.clone();
            let anchor_pos = anchor_key.and_then(|a| {
                filtered
                    .iter()
                    .position(|&r| model::path_key(&self.boxes[idx].items[r].path) == a)
            });
            match (target, anchor_pos) {
                (Some(t), Some(a)) => {
                    let (lo, hi) = model::range_endpoints(a, t);
                    let keys: Vec<String> = filtered[lo..=hi]
                        .iter()
                        .map(|&r| model::path_key(&self.boxes[idx].items[r].path))
                        .collect();
                    let rt = &mut self.boxes[idx];
                    rt.selected.clear();
                    rt.selected.extend(keys);
                    rt.focus = Some(key);
                }
                _ => {
                    let rt = &mut self.boxes[idx];
                    rt.selected.clear();
                    rt.selected.insert(key.clone());
                    rt.focus = Some(key.clone());
                    rt.anchor = Some(key);
                }
            }
        } else if mods.command {
            let rt = &mut self.boxes[idx];
            if !rt.selected.remove(&key) {
                rt.selected.insert(key.clone());
            }
            rt.focus = Some(key.clone());
            rt.anchor = Some(key);
        } else {
            let rt = &mut self.boxes[idx];
            rt.selected.clear();
            rt.selected.insert(key.clone());
            rt.focus = Some(key.clone());
            rt.anchor = Some(key);
        }
        // 空格预览只跟随“唯一选中项”：多选时解除接管，单选时同步 sel_item。
        if self.boxes[idx].selected.len() == 1 {
            let (name, modified) = (
                self.boxes[idx].items[real].display_name().to_string(),
                self.boxes[idx].items[real].modified,
            );
            // 锚点用“显示下标”（筛选后位置）：真实下标在筛选态下不能直接当格子序。
            let anchor = self
                .box_display_anchor(idx, real)
                .unwrap_or_else(|| self.box_cell_rect(idx, 0));
            self.select_item(path, &name, modified, anchor);
        } else {
            self.sel_item = None;
            // 多选/空选立即解除键盘接管，避免下一次空格仍按旧状态触发。
            self.sync_preview_hook();
        }
    }

    /// 右键落在未选中条目：先把选区替换为这一条，再走单条原生菜单。
    fn apply_box_click_plain(&mut self, idx: usize, _real: usize, path: &Path) {
        self.clear_box_selection_except(idx);
        self.focus_box = Some(idx);
        let key = model::path_key(path);
        let rt = &mut self.boxes[idx];
        rt.selected.clear();
        rt.selected.insert(key.clone());
        rt.focus = Some(key.clone());
        rt.anchor = Some(key);
    }

    /// 只清空除 `keep` 之外的盒子选区（点击另一个盒子/桌面时调用）。
    fn clear_box_selection_except(&mut self, keep: usize) {
        for (i, rt) in self.boxes.iter_mut().enumerate() {
            if i == keep {
                continue;
            }
            rt.selected.clear();
            rt.focus = None;
            rt.anchor = None;
        }
        self.marquee = None;
        // 盒子获得点击即退出桌面多选，避免两个选区同时高亮。
        self.clear_desk_selection("点击盒子条目");
    }

    /// 清空全部盒子选区（桌面空白点击、Esc、盒子空白点击）。
    fn clear_box_selection_all(&mut self) {
        for rt in self.boxes.iter_mut() {
            rt.selected.clear();
            rt.focus = None;
            rt.anchor = None;
        }
        self.focus_box = None;
        self.marquee = None;
    }

    /// 该路径是否属于当前多选（≥2 项）且被选中。
    fn box_multi_selected(&self, idx: usize, path: &Path) -> bool {
        let rt = &self.boxes[idx];
        rt.selected.len() > 1 && rt.selected.contains(&model::path_key(path))
    }

    /// 当前筛选内可见的选中项路径（多选菜单/整组拖动的动作范围）。
    fn visible_selected_paths(&self, idx: usize) -> Vec<PathBuf> {
        let filtered = self.box_filtered_indices(idx);
        filtered
            .iter()
            .filter(|&&r| {
                self.boxes[idx]
                    .selected
                    .contains(&model::path_key(&self.boxes[idx].items[r].path))
            })
            .map(|&r| self.boxes[idx].items[r].path.clone())
            .collect()
    }

    /// 方向键导航：移动焦点，Shift 时按锚点扩展区间；滚动只做最小位移。
    fn handle_nav_key(&mut self, dir: model::NavDir, extend: bool) {
        let Some(idx) = self.focus_box else {
            return;
        };
        if idx >= self.boxes.len() {
            return;
        }
        if let Some(b) = self.cfg.boxes.get(idx) {
            if b.closed || b.collapsed {
                return;
            }
        }
        log::debug!(
            "[nav] 方向键 dir={:?} extend={} focus={:?} anchor={:?} sel={}",
            dir,
            extend,
            self.boxes[idx].focus,
            self.boxes[idx].anchor,
            self.boxes[idx].selected.len()
        );
        let filtered = self.box_filtered_indices(idx);
        if filtered.is_empty() {
            return;
        }
        let len = filtered.len();
        let cols = if self.boxes[idx].nav_view == BoxView::List {
            1
        } else {
            self.boxes[idx].nav_cols.max(1)
        };
        let current = self.boxes[idx]
            .focus
            .as_ref()
            .and_then(|k| {
                filtered
                    .iter()
                    .position(|&r| model::path_key(&self.boxes[idx].items[r].path) == *k)
            })
            .unwrap_or(0);
        let target = model::nav_target(dir, cols, len, current);
        let real = filtered[target];
        let key = model::path_key(&self.boxes[idx].items[real].path);
        if extend {
            let anchor = self.boxes[idx]
                .anchor
                .as_ref()
                .and_then(|a| {
                    filtered
                        .iter()
                        .position(|&r| model::path_key(&self.boxes[idx].items[r].path) == *a)
                })
                .unwrap_or(current);
            let (lo, hi) = model::range_endpoints(anchor, target);
            let keys: Vec<String> = filtered[lo..=hi]
                .iter()
                .map(|&r| model::path_key(&self.boxes[idx].items[r].path))
                .collect();
            let rt = &mut self.boxes[idx];
            rt.selected.clear();
            rt.selected.extend(keys);
            rt.focus = Some(key.clone());
            if rt.anchor.is_none() {
                rt.anchor = Some(key);
            }
        } else {
            let rt = &mut self.boxes[idx];
            rt.selected.clear();
            rt.selected.insert(key.clone());
            rt.focus = Some(key.clone());
            rt.anchor = Some(key);
        }
        self.focus_box = Some(idx);
        // 把焦点格子滚进可视区（只移动最小距离，不做居中）。
        let cell = self.box_cell_rect(idx, target);
        let rt = &mut self.boxes[idx];
        if cell.min.y < rt.nav_list.top() {
            rt.scroll = (rt.scroll - (rt.nav_list.top() - cell.min.y)).max(0.0);
        } else if cell.max.y > rt.nav_list.bottom() {
            rt.scroll += cell.max.y - rt.nav_list.bottom();
        }
        let (path, name, modified) = (
            self.boxes[idx].items[real].path.clone(),
            self.boxes[idx].items[real].display_name().to_string(),
            self.boxes[idx].items[real].modified,
        );
        let anchor = self.box_cell_rect(idx, target);
        self.select_item(&path, &name, modified, anchor);
        self.ctx.request_repaint();
    }

    /// Ctrl+A：选中聚焦盒子当前筛选内的全部条目。
    fn handle_select_all_in_box(&mut self) {
        let Some(idx) = self.focus_box else {
            return;
        };
        if idx >= self.boxes.len() {
            return;
        }
        // 盒子上下文的全选：退出桌面多选，避免两个选区同时存在。
        self.clear_desk_selection("盒子 Ctrl+A");
        let filtered = self.box_filtered_indices(idx);
        if filtered.is_empty() {
            return;
        }
        let keys: Vec<String> = filtered
            .iter()
            .map(|&r| model::path_key(&self.boxes[idx].items[r].path))
            .collect();
        let first_key = filtered
            .first()
            .map(|&r| model::path_key(&self.boxes[idx].items[r].path));
        let rt = &mut self.boxes[idx];
        rt.selected.clear();
        rt.selected.extend(keys);
        if rt.focus.is_none() {
            rt.focus = first_key;
        }
        // Ctrl+A 是多选语义：立即解除空格预览接管（仅剩一条时仍允许预览）。
        if self.boxes[idx].selected.len() == 1 {
            let real = filtered[0];
            let path = self.boxes[idx].items[real].path.clone();
            let name = self.boxes[idx].items[real].display_name().to_string();
            let modified = self.boxes[idx].items[real].modified;
            let anchor = self
                .box_display_anchor(idx, real)
                .unwrap_or_else(|| self.box_cell_rect(idx, 0));
            self.select_item(&path, &name, modified, anchor);
        } else {
            self.sel_item = None;
            self.sync_preview_hook();
        }
        self.ctx.request_repaint();
    }

    /// Ctrl+A：桌面上下文全选（盒子无焦点时由键盘钩子转交）。
    fn handle_select_all_desktop(&mut self) {
        if self.desktop_items.is_empty() {
            return;
        }
        self.clear_box_selection_all();
        self.desk_selected = self
            .desktop_items
            .iter()
            .map(|item| model::path_key(&item.path))
            .collect();
        self.desk_anchor = self
            .desktop_items
            .first()
            .map(|item| model::path_key(&item.path));
        self.desk_multi_logged = false;
        self.sel_item = None;
        self.sync_preview_hook();
        let n = self.desk_selected.len();
        log::info!("[select] 桌面 Ctrl+A 全选：{n} 项（多选不预览）");
        self.ctx.request_repaint();
    }

    /// 桌面条目的视觉顺序（列优先：同列自上而下，再自左向右）。
    fn desktop_visual_order(
        &self,
        cells: &[model::DesktopCell],
        item_cell: &[Option<usize>],
    ) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.desktop_items.len())
            .filter(|i| item_cell.get(*i).copied().flatten().is_some())
            .collect();
        order.sort_by_key(|&i| {
            item_cell[i]
                .and_then(|ci| cells.get(ci))
                .map(|c| (c.monitor, c.col, c.row))
                .unwrap_or((usize::MAX, 0, 0))
        });
        order
    }

    /// 清除桌面选区（不触碰盒子选区）。
    fn clear_desk_selection(&mut self, reason: &str) {
        if self.desk_selected.is_empty()
            && self.desk_anchor.is_none()
            && self.desk_marquee.is_none()
        {
            return;
        }
        let n = self.desk_selected.len();
        self.desk_selected.clear();
        self.desk_anchor = None;
        self.desk_marquee = None;
        self.desk_multi_logged = false;
        log::info!("[select] 清空桌面选区（{reason}，原有 {n} 项）");
    }

    /// 条目是否属于「桌面多选」整组（用于整组右键与整组拖动）。
    fn desk_multi_selected(&self, path: &Path) -> bool {
        self.desk_selected.len() > 1 && self.desk_selected.contains(&model::path_key(path))
    }

    /// 当前桌面选区内的真实路径（按视觉顺序排列）。
    fn desktop_selected_paths(&self) -> Vec<PathBuf> {
        let (cells, item_cell) = self.desktop_layout();
        self.desktop_visual_order(&cells, &item_cell)
            .into_iter()
            .filter(|&i| {
                self.desktop_items
                    .get(i)
                    .is_some_and(|it| self.desk_selected.contains(&model::path_key(&it.path)))
            })
            .filter_map(|i| self.desktop_items.get(i).map(|it| it.path.clone()))
            .collect()
    }

    /// 某个桌面条目所在格子的逻辑矩形（预览面板就近弹出用）。
    fn desktop_anchor_rect(&self, index: usize) -> Option<Rect> {
        let (cells, item_cell) = self.desktop_layout();
        let ci = item_cell.get(index).copied().flatten()?;
        cells.get(ci).map(desktop_cell_rect)
    }

    /// 桌面普通单选：清空后只选该条；系统项不设置预览目标。
    fn apply_desk_click_plain(&mut self, index: usize) {
        let Some(item) = self.desktop_items.get(index).cloned() else {
            return;
        };
        let key = model::path_key(&item.path);
        self.desk_selected.clear();
        self.desk_selected.insert(key.clone());
        self.desk_anchor = Some(key);
        self.desk_multi_logged = false;
        if item.is_system() {
            self.sel_item = None;
            self.sync_preview_hook();
            log::info!("[select] 桌面单选（系统项）：{}", item.display_name());
            return;
        }
        let anchor = self.desktop_anchor_rect(index).unwrap_or(Rect::NOTHING);
        self.select_item(&item.path, item.display_name(), item.modified, anchor);
        log::info!("[select] 桌面单选：{}", item.display_name());
    }

    /// 桌面点击 + 修饰键：单选 / Ctrl 切换 / Shift 区间。
    fn apply_desk_click(&mut self, index: usize, mods: egui::Modifiers) {
        let Some(item) = self.desktop_items.get(index).cloned() else {
            return;
        };
        let key = model::path_key(&item.path);
        let (cells, item_cell) = self.desktop_layout();
        let order = self.desktop_visual_order(&cells, &item_cell);
        if mods.shift {
            if let Some(anchor) = self.desk_anchor.clone() {
                let apos = order
                    .iter()
                    .position(|&i| model::path_key(&self.desktop_items[i].path) == anchor);
                let tpos = order.iter().position(|&i| i == index);
                if let (Some(a), Some(t)) = (apos, tpos) {
                    let (lo, hi) = model::range_endpoints(a, t);
                    self.desk_selected.clear();
                    for &i in &order[lo..=hi] {
                        self.desk_selected
                            .insert(model::path_key(&self.desktop_items[i].path));
                    }
                    self.desk_multi_logged = false;
                    log::info!(
                        "[select] 桌面 Shift 区间：锚点 {} ↔ 目标 {}，共 {} 项",
                        anchor,
                        key,
                        self.desk_selected.len()
                    );
                    self.sync_desk_preview_after_select();
                    return;
                }
            }
            // 无有效锚点：退化为普通单选。
        }
        if mods.command {
            let removed = self.desk_selected.remove(&key);
            if !removed {
                self.desk_selected.insert(key.clone());
            }
            self.desk_anchor = Some(key);
            self.desk_multi_logged = false;
            log::info!(
                "[select] 桌面 Ctrl 切换：{} → {} 项",
                if removed { "取消" } else { "选中" },
                self.desk_selected.len()
            );
        } else {
            self.desk_selected.clear();
            self.desk_selected.insert(key.clone());
            self.desk_anchor = Some(key);
            self.desk_multi_logged = false;
            log::info!("[select] 桌面单选：{}", item.display_name());
        }
        self.sync_desk_preview_after_select();
    }

    /// 桌面选区变化后同步空格预览目标：只有「唯一且非系统」的选中项才可预览。
    fn sync_desk_preview_after_select(&mut self) {
        let n = self.desk_selected.len();
        if n == 1 {
            let hit = self
                .desktop_items
                .iter()
                .position(|it| self.desk_selected.contains(&model::path_key(&it.path)));
            if let Some(index) = hit {
                let item = self.desktop_items[index].clone();
                if !item.is_system() {
                    let anchor = self.desktop_anchor_rect(index).unwrap_or(Rect::NOTHING);
                    self.select_item(&item.path, item.display_name(), item.modified, anchor);
                    return;
                }
            }
            self.sel_item = None;
            self.sync_preview_hook();
            log::info!("[preview] 桌面选中系统项，空格不预览");
        } else {
            self.sel_item = None;
            self.sync_preview_hook();
            if n > 1 && !self.desk_multi_logged {
                self.desk_multi_logged = true;
                log::info!("[preview] 桌面多选 {n} 项，空格不预览");
            }
        }
        self.ctx.request_repaint();
    }

    /// 桌面框选提交：命中格子内的图标全部进入选区。
    fn commit_desk_marquee(&mut self, m: DeskMarquee) {
        let rect = Rect::from_two_pos(m.start, m.current);
        let (cells, item_cell) = self.desktop_layout();
        let mut entries: Vec<(usize, model::RectF)> = Vec::new();
        for (i, ci) in item_cell.iter().enumerate() {
            let Some(c) = ci.and_then(|ci| cells.get(ci)) else {
                continue;
            };
            let r = desktop_cell_rect(c);
            entries.push((i, (r.min.x, r.min.y, r.width(), r.height())));
        }
        let hits = model::marquee_hits(
            &entries,
            (rect.min.x, rect.min.y, rect.width(), rect.height()),
        );
        if !m.additive {
            self.desk_selected.clear();
        }
        let before = self.desk_selected.len();
        for &i in &hits {
            if let Some(item) = self.desktop_items.get(i) {
                self.desk_selected.insert(model::path_key(&item.path));
            }
        }
        if let Some(&first) = hits.first() {
            if let Some(item) = self.desktop_items.get(first) {
                self.desk_anchor = Some(model::path_key(&item.path));
            }
        } else if !m.additive {
            self.desk_anchor = None;
        }
        self.desk_multi_logged = false;
        log::info!(
            "[select] 桌面框选完成：additive={} 命中 {} 项，选区 {} 项（原 {before}）",
            m.additive,
            hits.len(),
            self.desk_selected.len()
        );
        if hits.is_empty() && !m.additive {
            self.sel_item = None;
            self.sync_preview_hook();
        } else {
            self.sync_desk_preview_after_select();
        }
    }

    /// 扫描/清理后剔除已消失条目的桌面选区。
    fn prune_desk_selection(&mut self) {
        if self.desk_selected.is_empty() {
            return;
        }
        let keys: HashSet<String> = self
            .desktop_items
            .iter()
            .map(|item| model::path_key(&item.path))
            .collect();
        let before = self.desk_selected.len();
        self.desk_selected.retain(|k| keys.contains(k));
        if self.desk_anchor.as_ref().is_some_and(|k| !keys.contains(k)) {
            self.desk_anchor = None;
        }
        if self.desk_selected.len() != before {
            log::info!(
                "[select] 桌面选区随扫描清理：{} → {} 项",
                before,
                self.desk_selected.len()
            );
            if self.desk_selected.is_empty() {
                self.desk_anchor = None;
            }
        }
    }

    /// 桌面多选整组拖动：以拖动起点为锚点做「整组平移」。
    ///
    /// 选择整组平移而不是「按落点依次填充空位」：用户框选后拖动的是一块图形，
    /// 保持相对位置最符合直觉；目标区域若与未选中的图标冲突，宁可整体取消并提示，
    /// 也不覆盖或挤走别人的位置。系统项也参与平移（它同样有棋盘槽位）。
    fn move_desktop_group(&mut self, pos: Pos2, paths: &[PathBuf], anchor: &Path) {
        if paths.is_empty() {
            return;
        }
        if !self.cfg.desktop.board_placement {
            if !self.snapshot_current_layout() {
                self.toast_msg("棋盘布局尚未就绪，整组移动已取消");
                return;
            }
        } else {
            self.sync_desktop_board();
        }
        let cells = self.desktop_cell_layout();
        let Some(target_ci) = cell_index_exact_at(&cells, pos) else {
            self.toast_msg("未落在桌面格子上，整组移动已取消");
            log::info!("[drag] 桌面整组移动取消：落点不在格子上");
            return;
        };
        let (_, item_cell) = self.desktop_layout();
        let Some(anchor_item) = self.desktop_items.iter().position(|it| it.path == *anchor) else {
            return;
        };
        let Some(anchor_ci) = item_cell.get(anchor_item).copied().flatten() else {
            return;
        };
        let (Some(from), Some(to)) = (cells.get(anchor_ci), cells.get(target_ci)) else {
            return;
        };
        if from.monitor != to.monitor {
            self.toast_msg("跨屏整组移动暂不支持");
            log::info!("[drag] 桌面整组移动取消：跨屏");
            return;
        }
        let dcol = to.col as isize - from.col as isize;
        let drow = to.row as isize - from.row as isize;
        if dcol == 0 && drow == 0 {
            return;
        }
        let sel: Vec<usize> = self
            .desktop_items
            .iter()
            .enumerate()
            .filter(|(_, it)| self.desk_selected.contains(&model::path_key(&it.path)))
            .map(|(i, _)| i)
            .collect();
        if sel.len() < 2 {
            return;
        }
        let mut lookup: HashMap<(usize, usize, usize), usize> = HashMap::new();
        for (ci, c) in cells.iter().enumerate() {
            lookup.insert((c.monitor, c.col, c.row), ci);
        }
        let mut occupied: HashMap<usize, usize> = HashMap::new();
        for (i, ci) in item_cell.iter().enumerate() {
            if let Some(ci) = ci {
                occupied.insert(*ci, i);
            }
        }
        let mut targets: Vec<(usize, usize)> = Vec::with_capacity(sel.len());
        for &i in &sel {
            let Some(ci) = item_cell.get(i).copied().flatten() else {
                continue;
            };
            let c = cells[ci];
            let ncol = c.col as isize + dcol;
            let nrow = c.row as isize + drow;
            let Some(&nci) = lookup.get(&(c.monitor, ncol.max(0) as usize, nrow.max(0) as usize))
            else {
                self.toast_msg("整组超出棋盘边界，已取消移动");
                log::info!("[drag] 桌面整组移动取消：越界");
                return;
            };
            if ncol < 0 || nrow < 0 {
                self.toast_msg("整组超出棋盘边界，已取消移动");
                log::info!("[drag] 桌面整组移动取消：越界");
                return;
            }
            if let Some(&occ) = occupied.get(&nci) {
                if !sel.contains(&occ) {
                    self.toast_msg("目标区域已被占用，整组未移动");
                    log::info!(
                        "[drag] 桌面整组移动取消：目标格被 {} 占用",
                        self.desktop_items[occ].path.display()
                    );
                    return;
                }
            }
            targets.push((i, nci));
        }
        for (i, nci) in &targets {
            let c = cells[*nci];
            let path = self.desktop_items[*i].path.clone();
            self.set_desktop_slot(&path, &c);
        }
        self.persist();
        log::info!(
            "[drag] 桌面整组移动：{} 项，delta=({dcol},{drow})，锚点 {} → 格 {}",
            targets.len(),
            anchor.display(),
            target_ci
        );
        self.toast_msg(format!("已整体移动 {} 项", targets.len()));
        self.ctx.request_repaint();
    }

    /// 批量加入盒子：只移动引用，不做真实文件操作。
    fn add_members(&mut self, idx: usize, paths: &[PathBuf]) -> usize {
        let keys: Vec<String> = paths.iter().map(|p| model::path_key(p)).collect();
        let mut added = 0usize;
        for (i, b) in self.cfg.boxes.iter_mut().enumerate() {
            if i == idx {
                for (path, key) in paths.iter().zip(keys.iter()) {
                    if !b
                        .members
                        .iter()
                        .any(|m| model::path_key(Path::new(m)) == *key)
                    {
                        b.members.push(path.to_string_lossy().to_string());
                        added += 1;
                    }
                }
            } else {
                b.members
                    .retain(|m| !keys.iter().any(|k| *k == model::path_key(Path::new(m))));
                b.manual_order.retain(|k| !keys.iter().any(|x| x == k));
            }
        }
        if added > 0 {
            self.refresh_all_boxes();
            self.persist();
        }
        added
    }

    /// 批量移出盒子引用（文件保持原位）。
    fn remove_members(&mut self, idx: usize, paths: &[PathBuf]) -> usize {
        let keys: Vec<String> = paths.iter().map(|p| model::path_key(p)).collect();
        let Some(b) = self.cfg.boxes.get_mut(idx) else {
            return 0;
        };
        let before = b.members.len();
        b.members
            .retain(|m| !keys.iter().any(|k| *k == model::path_key(Path::new(m))));
        b.manual_order.retain(|k| !keys.iter().any(|x| x == k));
        let removed = before.saturating_sub(b.members.len());
        if removed > 0 {
            self.boxes[idx].selected.clear();
            self.boxes[idx].focus = None;
            self.boxes[idx].anchor = None;
            self.persist();
            self.refresh_all_boxes();
        }
        removed
    }

    /// 清理这些路径在所有盒子里的引用，并同步移出桌面网格/扫描缓存。
    /// 返回真正移除的引用条数。
    fn purge_keys(&mut self, keys: &HashSet<String>, reason: &str) -> usize {
        let mut removed = 0usize;
        let mut box_order_changed = false;
        for b in self.cfg.boxes.iter_mut() {
            let before = b.members.len();
            b.members
                .retain(|m| !keys.contains(&model::path_key(Path::new(m))));
            removed += before.saturating_sub(b.members.len());
            let order_before = b.manual_order.len();
            b.manual_order.retain(|k| !keys.contains(k));
            box_order_changed |= order_before != b.manual_order.len();
        }
        // 失效路径同步移出棋盘占位，释放格子；避免留下永远匹配不到的死键。
        let slot_before = self.cfg.desktop.slots.len();
        self.cfg.desktop.slots.retain(|s| !keys.contains(&s.key));
        let order_changed = slot_before != self.cfg.desktop.slots.len() || box_order_changed;
        // 当前选中项若被清理，立即解除预览接管：否则空格会被钩子吞掉却无目标可预览。
        if self
            .sel_item
            .as_ref()
            .is_some_and(|sel| keys.contains(&model::path_key(&sel.path)))
        {
            self.sel_item = None;
            self.close_preview("选中项已失效");
            self.sync_preview_hook();
            log::info!("[preview] 选中项已被清理，已解除预览接管");
        }
        // 桌面多选中被清理的条目同步移出选区，避免整组动作作用到不存在的路径。
        self.desk_selected.retain(|k| !keys.contains(k));
        if self.desk_anchor.as_ref().is_some_and(|k| keys.contains(k)) {
            self.desk_anchor = None;
        }
        if removed == 0 {
            if order_changed {
                self.persist();
            }
            return 0;
        }
        // 桌面网格与扫描缓存同帧移除，避免「盒子没了但桌面还在」。
        self.desktop_user
            .retain(|i| !keys.contains(&model::path_key(&i.path)));
        self.desktop_public
            .retain(|i| !keys.contains(&model::path_key(&i.path)));
        self.desktop_user_paths
            .retain(|p| !keys.contains(&model::path_key(p)));
        self.desktop_public_paths
            .retain(|p| !keys.contains(&model::path_key(p)));
        for rt in self.boxes.iter_mut() {
            rt.selected.retain(|k| !keys.contains(k));
            if rt.focus.as_ref().is_some_and(|k| keys.contains(k)) {
                rt.focus = None;
            }
            if rt.anchor.as_ref().is_some_and(|k| keys.contains(k)) {
                rt.anchor = None;
            }
        }
        log::info!("{reason}：清理 {removed} 条盒子引用（桌面网格同步移除）");
        self.persist();
        self.refresh_all_boxes();
        self.toast_msg(format!("已清理 {removed} 条失效引用（{reason}）"));
        removed
    }

    fn purge_paths(&mut self, paths: &[PathBuf], reason: &str) -> usize {
        if paths.is_empty() {
            return 0;
        }
        let keys: HashSet<String> = paths.iter().map(|p| model::path_key(p)).collect();
        self.purge_keys(&keys, reason)
    }

    /// 空格：打开当前选中项的预览；已打开则关闭。
    fn toggle_preview(&mut self) {
        if self.preview.is_some() {
            self.close_preview("空格");
            return;
        }
        if !self.overlay_visible || self.text_editing() {
            log::info!(
                "[preview] 空格忽略：overlay_visible={} text_editing={} settings_open={} menu_open={}",
                self.overlay_visible,
                self.text_editing(),
                self.settings_open,
                self.group_menu.is_some() || crate::platform::shell_menu::menu_open()
            );
            return;
        }
        let Some(sel) = self.sel_item.clone() else {
            // 多选（≥2）或空选：UI 侧没有唯一预览目标，日志里带上多选规模便于判定。
            let multi = self.boxes.iter().map(|b| b.selected.len()).sum::<usize>();
            let desk = self.desk_selected.len();
            log::info!(
                "[preview] 空格忽略：无唯一选中项（盒子选中总数={multi}，桌面选中={desk}，焦点盒子={:?}）",
                self.focus_box
            );
            if desk > 1 {
                log::info!("[preview] 桌面多选 {desk} 项，空格不预览");
            }
            return;
        };
        // 选中项源文件可能已被删除/重命名：先校验，避免钩子吞掉空格却没有任何可见结果。
        if !sel.path.exists() {
            log::info!("[preview] 空格打开失败：源已不存在 {}", sel.path.display());
            self.sel_item = None;
            self.close_preview("源已不存在");
            self.sync_preview_hook();
            self.toast_msg("预览源已不存在，已取消选中");
            return;
        }
        log::info!(
            "[preview] 空格打开 {}（锚点 {:.0},{:.0}）",
            sel.path.display(),
            sel.anchor.center().x,
            sel.anchor.center().y
        );
        self.request_preview(&sel.path, &sel.name, sel.modified, sel.anchor);
    }

    fn request_preview(&mut self, path: &Path, name: &str, modified: u64, anchor: Rect) {
        let _ = self.preview_tx.send(preview::PreviewRequest {
            path: path.to_path_buf(),
        });
        self.preview = Some(PreviewView {
            path: path.to_path_buf(),
            name: name.to_string(),
            modified,
            anchor,
            data: None,
            opened_at: self.ctx.input(|i| i.time),
            rect: None,
        });
        self.ctx.request_repaint();
    }

    fn close_preview(&mut self, reason: &str) {
        if let Some(pv) = self.preview.take() {
            let alive = (self.ctx.input(|i| i.time) - pv.opened_at) * 1000.0;
            log::info!(
                "[preview] 关闭（{}）{}，面板存活 {:.0} ms",
                reason,
                pv.path.display(),
                alive
            );
        }
        key_hook::set_preview_rect(None);
        self.ctx.request_repaint();
    }

    /// 预览面板内的 Ctrl+C：整份预览文本写入系统剪贴板（与名称选区共用同一原生写入）。
    fn copy_preview_text(&mut self) {
        let Some(pv) = self.preview.as_ref() else {
            return;
        };
        let Some(data) = pv.data.as_ref() else {
            self.toast_msg("预览内容仍在读取，稍后再复制");
            return;
        };
        let Some(text) = data.text.as_ref() else {
            self.toast_msg("当前预览没有可复制的文本");
            return;
        };
        if text.is_empty() {
            self.toast_msg("预览内容为空");
            return;
        }
        let chars = text.chars().count();
        if crate::platform::clipboard_native::write_text(text, None) {
            log::info!("[preview] 已复制预览全文（{chars} 字）");
            self.toast_msg(format!("已复制预览全文（{chars} 字）"));
        } else {
            self.toast_msg("写入剪贴板失败");
        }
    }

    /// 每帧把预览状态同步给键盘钩子（空格是否接管、Esc/Ctrl+C 的有效矩形）。
    fn sync_preview_hook(&self) {
        let editing = self.text_editing();
        let armed = self.overlay_visible
            && self.sel_item.is_some()
            && !editing
            && !crate::platform::shell_menu::menu_open()
            && !self.settings_open;
        key_hook::set_preview_armed(armed);
        // 允许接管只排除"会丢失输入"的场景（编辑/菜单/设置/叠加层不可见），
        // 不依赖是否已有选中项，避免钩子读到点选前的旧状态而漏掉这一次空格。
        let allowed = self.overlay_visible
            && !editing
            && !crate::platform::shell_menu::menu_open()
            && self.group_menu.is_none()
            && self.fallback_menu.is_none()
            && self.desktop_fallback_menu.is_none()
            && !self.settings_open;
        key_hook::set_space_capture_allowed(allowed);
        key_hook::set_preview_active(self.preview.is_some());
        let prev = PREVIEW_ARMED_LOGGED.swap(armed, Ordering::Relaxed);
        if prev != armed {
            log::info!(
                "[preview] 键盘接管 {}（overlay={} selected={:?} edit={} menu={} settings={} space_allowed={}）",
                if armed { "开启" } else { "关闭" },
                self.overlay_visible,
                self.sel_item.as_ref().map(|s| s.path.display().to_string()),
                self.text_editing(),
                crate::platform::shell_menu::menu_open()
                    || self.group_menu.is_some()
                    || self.fallback_menu.is_some()
                    || self.desktop_fallback_menu.is_some(),
                self.settings_open,
                allowed
            );
        }
        let rect = self.preview.as_ref().and_then(|p| p.rect).map(|r| {
            let s = self.scale.max(1.0);
            (
                self.overlay.x + (r.min.x * s).round() as i32,
                self.overlay.y + (r.min.y * s).round() as i32,
                self.overlay.x + (r.max.x * s).round() as i32,
                self.overlay.y + (r.max.y * s).round() as i32,
            )
        });
        key_hook::set_preview_rect(rect);
    }

    /// 每帧把多选键盘状态同步给低级键盘钩子：只有聚焦盒子展开、无编辑/菜单时接管。
    fn sync_nav_hook(&mut self) {
        let focused_box_open = self
            .focus_box
            .and_then(|idx| self.cfg.boxes.get(idx))
            .is_some_and(|b| !b.closed && !b.collapsed);
        let armed = self.overlay_visible
            && focused_box_open
            && !self.text_editing()
            && !self.settings_open
            && !self.group_menu.is_some()
            && self.fallback_menu.is_none()
            && self.desktop_fallback_menu.is_none()
            && !crate::platform::shell_menu::menu_open();
        if armed != self.nav_armed_prev {
            self.nav_armed_prev = armed;
            log::debug!(
                "[nav] 盒子键盘多选接管 {}（focus={:?} overlay={} edit={} settings={} menu={}）",
                if armed { "开启" } else { "关闭" },
                self.focus_box,
                self.overlay_visible,
                self.text_editing(),
                self.settings_open,
                self.group_menu.is_some() || crate::platform::shell_menu::menu_open()
            );
        }
        key_hook::set_nav_armed(armed);
        // 桌面多选：没有盒子焦点时接管控 Ctrl+A / Esc；盒子有焦点时盒子优先。
        let desk_armed = self.overlay_visible
            && !focused_box_open
            && !self.text_editing()
            && !self.settings_open
            && self.group_menu.is_none()
            && self.fallback_menu.is_none()
            && self.desktop_fallback_menu.is_none()
            && !crate::platform::shell_menu::menu_open();
        key_hook::set_desk_select_armed(desk_armed);
    }

    /// 绘制无边框快速预览面板：贴在选中项旁，按所属屏 DPI 缩放并夹在工作区内。
    fn draw_quick_preview(&mut self, ui: &mut egui::Ui) {
        let Some(pv) = self.preview.as_ref() else {
            self.sync_preview_hook();
            return;
        };
        let path = pv.path.clone();
        let name = pv.name.clone();
        let modified = pv.modified;
        let anchor = pv.anchor;
        let data = pv.data.clone();

        if !path.exists() {
            self.close_preview("源文件已消失");
            // 源消失后选区必须一起清掉，否则空格仍被接管却永远没有可见结果。
            if self.sel_item.take().is_some() {
                log::info!("[preview] 源文件已消失，已清空选区 {}", path.display());
            }
            self.sync_preview_hook();
            return;
        }

        let scale = self.scale.max(1.0);
        let pcx = self.overlay.x as f32 + anchor.center().x * scale;
        let pcy = self.overlay.y as f32 + anchor.center().y * scale;
        let s = self.monitor_scale_at(pcx, pcy);
        let k = (s / scale).max(0.1);
        let (wx, wy, ww, wh) = self.monitor_work_at(pcx, pcy);
        let work = Rect::from_min_size(
            egui::pos2(
                (wx - self.overlay.x as f32) / scale,
                (wy - self.overlay.y as f32) / scale,
            ),
            egui::vec2(ww / scale, wh / scale),
        );
        let w = (560.0 * k).min(work.width() - 16.0 * k).max(220.0);
        let h = (640.0 * k).min(work.height() - 16.0 * k).max(140.0);
        let gap = 12.0 * k;
        let mut x = anchor.right() + gap;
        if x + w > work.right() - 8.0 * k {
            x = anchor.left() - gap - w;
        }
        if x < work.left() + 8.0 * k {
            x = work.left() + 8.0 * k;
        }
        let y = anchor
            .top()
            .min(work.bottom() - 8.0 * k - h)
            .max(work.top() + 8.0 * k);

        let pal = self.pal;
        let dark = self.dark;
        let textures = &mut self.textures;
        let icon_tx = &self.icon_tx;
        let mut close = false;
        let mut copy = false;
        let area = egui::Area::new(Id::unique("quick-preview"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(x, y))
            .show(ui.ctx(), |ui| {
                theme::popup_frame(&pal, dark)
                    .show(ui, |ui| {
                        ui.set_width(w - 32.0);
                        // 高度不再写死：由内容撑开，最长到 h（超过时正文内部滚动）。
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(&name)
                                    .strong()
                                    .size(15.5 * k)
                                    .color(pal.text),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if theme::ghost_button(ui, "关闭", &pal).clicked() {
                                        close = true;
                                    }
                                },
                            );
                        });
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(4.0);
                        match &data {
                            None => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(
                                        egui::RichText::new("正在读取…")
                                            .size(12.0 * k)
                                            .color(pal.text_dim),
                                    );
                                });
                            }
                            Some(d) => {
                                let meta = format!(
                                    "{} · {} · {}{}",
                                    d.kind.label(),
                                    crate::util::format_size(d.total_bytes),
                                    if d.encoding == "-" {
                                        "系统缩略图".to_string()
                                    } else {
                                        d.encoding.clone()
                                    },
                                    if d.truncated { " · 已截断" } else { "" }
                                );
                                ui.label(
                                    egui::RichText::new(meta)
                                        .size(11.0 * k)
                                        .color(pal.text_dim),
                                );
                                let body_max = (h - 150.0 * k).max(60.0);
                                match (d.kind, d.text.as_deref()) {
                                    (preview::PreviewKind::Image, _)
                                    | (preview::PreviewKind::Pdf, _) => {
                                        let px = (700.0 * s).round() as u32;
                                        let key = format!(
                                            "preview-thumb:{px}:{modified}:{}",
                                            path.display()
                                        );
                                        let thumb = textures.ensure_shell_thumb(
                                            icon_tx,
                                            key,
                                            path.clone(),
                                            px,
                                        );
                                        match thumb {
                                            Some((tid, natural)) => {
                                                // 只占图片实际需要的空间（等比缩放到可用宽/高内），
                                                // 不再用固定的正文高度，短内容下方不会留下大块空白。
                                                let pad = 6.0 * k;
                                                let avail_w = (ui.available_width() - 2.0 * pad)
                                                    .max(1.0);
                                                let avail_h =
                                                    (body_max - 2.0 * pad).max(1.0);
                                                let fit_scale = (avail_w / natural.x)
                                                    .min(avail_h / natural.y);
                                                let img_size =
                                                    egui::vec2(natural.x, natural.y)
                                                        * fit_scale;
                                                let (rect, _) = ui.allocate_exact_size(
                                                    img_size + egui::vec2(2.0 * pad, 2.0 * pad),
                                                    Sense::hover(),
                                                );
                                                let fit =
                                                    fit_image_rect(rect.shrink(pad), natural);
                                                ui.painter().image(
                                                    tid,
                                                    fit,
                                                    Rect::from_min_max(
                                                        egui::pos2(0.0, 0.0),
                                                        egui::pos2(1.0, 1.0),
                                                    ),
                                                    Color32::WHITE,
                                                );
                                            }
                                            None => {
                                                ui.horizontal(|ui| {
                                                    ui.spinner();
                                                    ui.label(
                                                        egui::RichText::new(
                                                            "正在生成系统缩略图（IShellItemImageFactory）…",
                                                        )
                                                        .size(11.5 * k)
                                                        .color(pal.text_dim),
                                                    );
                                                });
                                            }
                                        }
                                    }
                                    (_, Some(text)) => {
                                        let lines: Vec<&str> = text.split('\n').collect();
                                        let row_h = (12.5 * k * 1.4).round().max(8.0);
                                        let font = FontId::monospace(12.5 * k);
                                        egui::ScrollArea::both()
                                            // 宽度填满面板；高度按内容收缩，超长时在 body_max 内滚动。
                                            .auto_shrink([false, true])
                                            .max_height(body_max)
                                            .show_rows(ui, row_h, lines.len(), |ui, range| {
                                                for i in range {
                                                    let job = highlight_job(
                                                        lines[i],
                                                        d.lang,
                                                        &font,
                                                        pal.text,
                                                        dark,
                                                    );
                                                    ui.add(
                                                        egui::Label::new(job).wrap_mode(
                                                            egui::TextWrapMode::Extend,
                                                        ),
                                                    );
                                                }
                                            });
                                    }
                                    (_, None) => {
                                        let msg = d
                                            .note
                                            .clone()
                                            .unwrap_or_else(|| "该类型暂不支持内容预览".into());
                                        // 不支持的类型只占文案高度，不再撑满整个正文区。
                                        ui.add_space(10.0 * k);
                                        ui.vertical_centered(|ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(msg)
                                                        .size(12.0 * k)
                                                        .color(pal.text_dim),
                                                )
                                                .wrap(),
                                            );
                                        });
                                        ui.add_space(10.0 * k);
                                    }
                                }
                                if let Some(note) = &d.note {
                                    ui.label(
                                        egui::RichText::new(note)
                                            .size(10.5 * k)
                                            .color(pal.text_dim),
                                    );
                                }
                            }
                        }
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if theme::ghost_button(ui, "复制全文 (Ctrl+C)", &pal).clicked() {
                                copy = true;
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new("空格 / Esc 关闭")
                                            .size(11.0 * k)
                                            .color(pal.text_dim),
                                    );
                                },
                            );
                        });
                    })
            });
        let panel = area.response.rect;
        if close {
            self.close_preview("关闭按钮");
        } else if copy {
            self.copy_preview_text();
        }
        if let Some(pv) = self.preview.as_mut() {
            pv.rect = Some(panel);
        }
        self.sync_preview_hook();
    }

    fn draw_overlay(&mut self, ui: &mut egui::Ui) {
        let mut actions: Vec<UiAction> = Vec::new();
        let full = ui.max_rect();
        let bg = ui.interact(full, Id::unique("desk-background"), Sense::click_and_drag());
        // 点击名称区域之外的任何地方（图标主体、盒子、桌面）即清除上一次的文字选区；
        // 上一帧的交互区域足够判断"这次按下是否还落在这个名称上"。
        if ui.input(|i| i.pointer.any_pressed()) {
            if let (Some(origin), Some(zone)) =
                (ui.input(|i| i.pointer.press_origin()), self.text_sel.zone)
            {
                if !zone.contains(origin) {
                    self.text_sel.clear();
                }
            }
        }
        if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Z)) {
            self.undo_last();
        }
        // 点击空白处：关闭预览并清除唯一选中项。点在预览面板内的按下不算“空白”。
        if bg.clicked() {
            let origin = ui.input(|i| i.pointer.press_origin());
            let inside_preview = origin.is_some_and(|p| {
                self.preview
                    .as_ref()
                    .and_then(|pv| pv.rect)
                    .is_some_and(|r| r.contains(p))
            });
            if !inside_preview {
                if self.preview.is_some() {
                    self.close_preview("点击空白处");
                }
                self.sel_item = None;
                // 点桌面空白同时取消盒子多选与桌面多选。
                self.clear_box_selection_all();
                self.group_menu = None;
                self.clear_desk_selection("点击桌面空白");
                // 选区清空后立即解除空格接管，别让下一次空格还按旧状态被钩子吞掉。
                self.sync_preview_hook();
            }
        }
        if bg.secondary_clicked() {
            if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                // 右键空白：清空桌面多选，再弹桌面原生菜单。
                self.clear_desk_selection("右键桌面空白");
                // 原生菜单已打开或首个请求仍在途：不再发起任何新菜单，避免两套菜单并存。
                if !crate::platform::shell_menu::menu_open() {
                    log::info!(
                        "菜单[t={}] UI 桌面 secondary_clicked @{:.1},{:.1} menu_open=false",
                        crate::platform::shell_menu::now_ms(),
                        pos.x,
                        pos.y
                    );
                    // 原生菜单按物理屏幕坐标定位；egui 给的是逻辑点，需换算后再传。
                    let s = self.scale.max(1.0);
                    let x = self.overlay.x + (pos.x * s).round() as i32;
                    let y = self.overlay.y + (pos.y * s).round() as i32;
                    match crate::platform::shell_menu::request_desktop(x, y) {
                        Ok(()) => {}
                        Err(err) => {
                            log::warn!("桌面原生菜单不可用（{}），退回内置菜单", err.reason);
                            self.desktop_fallback_menu = Some(pos);
                        }
                    }
                } else {
                    log::info!(
                        "菜单[t={}] 原生菜单已打开，忽略本次桌面右键 @{:.1},{:.1}",
                        crate::platform::shell_menu::now_ms(),
                        pos.x,
                        pos.y
                    );
                }
            }
        }

        // 桌面空白区拖拽 = 框选。仅当按下的位置不在图标/名称内容区、也不在任何盒子上时开始，
        // 避免与图标拖动、名称文本选择和盒内框选抢事件。
        if bg.drag_started() {
            // 用「按下原点」而不是当前指针：越过拖动阈值时指针已可能滑进图标格，
            // 用当前位置判定会把从图标旁空白起步的框选误判成图标拖动。
            if let Some(pos) = ui.input(|i| i.pointer.press_origin()) {
                let s = self.scale.max(1.0);
                let (px, py) = (pos.x * s, pos.y * s);
                let in_box = self
                    .cfg
                    .boxes
                    .iter()
                    .any(|b| !b.closed && model::point_in_rect(px, py, (b.x, b.y, b.w, b.h)));
                if !in_box && !self.pointer_on_desktop_item(pos) {
                    let additive = ui.input(|i| i.modifiers.command);
                    self.desk_marquee = Some(DeskMarquee {
                        start: pos,
                        current: pos,
                        additive,
                    });
                    log::info!(
                        "[select] 桌面框选开始 @{:.1},{:.1} additive={additive}",
                        pos.x,
                        pos.y
                    );
                }
            }
        }
        if let Some(m) = self.desk_marquee.as_mut() {
            if ui.input(|i| i.pointer.any_down()) {
                if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                    m.current = pos;
                }
            }
        }

        self.name_edit_zone = None;
        self.draw_desktop(ui, &mut actions);
        self.draw_boxes(ui, &mut actions);
        self.draw_desk_marquee(ui, bg.drag_stopped());
        self.draw_desktop_fallback_menu(ui, &mut actions);
        self.draw_name_editor(ui);
        // 名称选区存在时 Ctrl+C 复制选中文本。叠加层带 WS_EX_NOACTIVATE，键盘事件
        // 通常到不了 egui，真正路径是 key_hook 拦截（见 `set_copy_region`）；这里保留
        // 「叠加层恰好有焦点」时的回退，不会重复触发（钩子会吞掉按键）。
        // 编辑态下必须让位给聚焦的 TextEdit（Ctrl+C 由文本框自己处理）。
        if !self.text_editing()
            && self.text_sel.has_selection()
            && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C))
        {
            self.copy_selected_name();
        }
        self.sync_copy_region();
        self.draw_snap_guides(ui);
        self.draw_drag(ui);
        self.draw_group_menu(ui, &mut actions);
        self.draw_fallback_menu(ui, &mut actions);
        self.draw_quick_preview(ui);

        if self.cfg.boxes.is_empty() {
            ui.painter().text(
                egui::pos2(full.left() + 24.0, full.bottom() - 28.0),
                Align2::LEFT_BOTTOM,
                "右键桌面可新建盒子 · 把桌面图标拖进盒子即可收纳（文件保持原位）",
                FontId::proportional(12.5),
                self.pal.text_dim,
            );
        }
        self.apply_actions(actions);
    }

    /// 绘制盒子的降级内置菜单：原生菜单不可用时替代显示，位置与原生菜单一致。
    fn draw_fallback_menu(&mut self, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
        let Some(menu) = self.fallback_menu.as_ref() else {
            return;
        };
        // 原生菜单存在期间绝不叠加自绘菜单。
        if crate::platform::shell_menu::menu_open() {
            self.fallback_menu = None;
            return;
        }
        let pos = menu.pos;
        let request = menu.request.clone();
        let mut picked = false;
        let area = egui::Area::new(Id::unique("fallback-item-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ui.ctx(), |ui| {
                theme::menu_frame(&self.pal, self.dark)
                    .show(ui, |ui| {
                        ui.set_min_width(190.0);
                        picked = fallback_item_menu(ui, &request, actions);
                    })
                    .response
                    .rect
            });
        let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let outside = ui.input(|i| {
            i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| !area.inner.contains(p))
        });
        if picked || esc || outside {
            self.fallback_menu = None;
        }
    }

    /// 多选右键菜单：所有动作作用于整组（删除/复制路径/移出盒子/换盒/加剪贴板盒）。
    fn draw_group_menu(&mut self, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
        let Some(menu) = self.group_menu.clone() else {
            return;
        };
        if menu.paths.len() < 2 {
            self.group_menu = None;
            return;
        }
        let count = menu.paths.len();
        let mut chosen: Option<UiAction> = None;
        let mut close = false;
        let area = egui::Area::new(Id::unique("group-item-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(menu.pos)
            .show(ui.ctx(), |ui| {
                theme::menu_frame(&self.pal, self.dark)
                    .show(ui, |ui| {
                        ui.set_min_width(200.0);
                        ui.label(
                            egui::RichText::new(format!("已选中 {count} 项"))
                                .size(11.5)
                                .color(self.pal.text_dim),
                        );
                        ui.separator();
                        if ui
                            .button(theme::danger_menu_text("删除（移到回收站）"))
                            .clicked()
                        {
                            chosen = Some(UiAction::GroupDelete(menu.paths.clone()));
                            close = true;
                        }
                        if ui.button(format!("复制路径（{count} 项）")).clicked() {
                            chosen = Some(UiAction::GroupCopyPaths(menu.paths.clone()));
                            close = true;
                        }
                        if let Some(from) = menu.box_idx {
                            if ui.button(format!("移出盒子（{count} 项）")).clicked() {
                                chosen = Some(UiAction::GroupEject {
                                    box_idx: from,
                                    paths: menu.paths.clone(),
                                });
                                close = true;
                            }
                            ui.menu_button("在盒子内移动", |ui| {
                                for (target, b) in self.cfg.boxes.iter().enumerate() {
                                    if target == from || b.closed {
                                        continue;
                                    }
                                    if ui.button(&b.name).clicked() {
                                        chosen = Some(UiAction::GroupMoveToBox {
                                            from,
                                            paths: menu.paths.clone(),
                                            to: target,
                                        });
                                        close = true;
                                        ui.close();
                                    }
                                }
                            });
                        } else {
                            // 桌面多选：与盒内整组菜单一致，可直接把整组收进某个文件盒子。
                            ui.menu_button("加入盒子", |ui| {
                                let mut any = false;
                                for (target, b) in self.cfg.boxes.iter().enumerate() {
                                    if b.closed
                                        || matches!(b.kind, BoxKind::Todo | BoxKind::Clipboard)
                                    {
                                        continue;
                                    }
                                    any = true;
                                    if ui.button(&b.name).clicked() {
                                        chosen = Some(UiAction::GroupAddToBox {
                                            paths: menu.paths.clone(),
                                            to: target,
                                        });
                                        close = true;
                                        ui.close();
                                    }
                                }
                                if !any {
                                    ui.label(
                                        egui::RichText::new("没有可用的文件盒子")
                                            .size(11.5)
                                            .color(self.pal.text_dim),
                                    );
                                }
                            });
                        }
                        if ui.button("添加到剪贴板盒").clicked() {
                            chosen = Some(UiAction::GroupAddClip(menu.paths.clone()));
                            close = true;
                        }
                    })
                    .response
                    .rect
            });
        let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let outside = ui.input(|i| {
            i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| !area.inner.contains(p))
        });
        if let Some(action) = chosen {
            actions.push(action);
        }
        if close || esc || outside {
            self.group_menu = None;
        }
    }

    /// 绘制盒子拖动时的磁吸对齐提示线（物理坐标 → 叠加层局部点坐标）。
    fn draw_snap_guides(&self, ui: &mut egui::Ui) {
        if self.snap_guides.is_empty() {
            return;
        }
        let scale = self.scale.max(1.0);
        let ox = self.overlay.x as f32;
        let oy = self.overlay.y as f32;
        let stroke = Stroke::new(2.0, self.pal.accent);
        for g in &self.snap_guides {
            if g.vertical {
                let x = (g.at - ox) / scale;
                let y0 = (g.start - oy) / scale;
                let y1 = (g.end - oy) / scale;
                ui.painter()
                    .line_segment([egui::pos2(x, y0), egui::pos2(x, y1)], stroke);
            } else {
                let y = (g.at - oy) / scale;
                let x0 = (g.start - ox) / scale;
                let x1 = (g.end - ox) / scale;
                ui.painter()
                    .line_segment([egui::pos2(x0, y), egui::pos2(x1, y)], stroke);
            }
        }
    }

    /// 指针是否落在某个桌面条目的图标或名称内容区（含系统项）。
    /// 框选只从真正的空白处开始，避免与图标拖动、名称文本选择抢事件。
    fn pointer_on_desktop_item(&self, pos: Pos2) -> bool {
        let pp = self.scale.max(1.0);
        let (cells, item_cell) = self.desktop_layout();
        for (i, _item) in self.desktop_items.iter().enumerate() {
            let Some(c) = item_cell
                .get(i)
                .copied()
                .flatten()
                .and_then(|ci| cells.get(ci))
                .copied()
            else {
                continue;
            };
            let cell = desktop_cell_rect(&c);
            if !cell.contains(pos) {
                continue;
            }
            let icon_rect = item_icon_rect(cell, c.icon, pp);
            let label_h = if self.cfg.desktop.show_labels {
                let label_font = model::label_font_for_icon(c.icon);
                model::label_block_height(label_font, self.cfg.desktop.label_gap * c.k)
            } else {
                0.0
            };
            let content = Rect::from_min_max(
                cell.min,
                egui::pos2(cell.max.x, icon_rect.bottom() + label_h),
            );
            if content.contains(pos) {
                return true;
            }
        }
        false
    }

    /// 绘制桌面框选矩形；松手时提交命中项。单线描边，避免与选中高亮叠成双线。
    fn draw_desk_marquee(&mut self, ui: &mut egui::Ui, drag_stopped: bool) {
        let Some(m) = self.desk_marquee.as_ref() else {
            return;
        };
        let rect = Rect::from_two_pos(m.start, m.current);
        ui.painter().rect_filled(
            rect,
            CornerRadius::ZERO,
            self.pal.accent.gamma_multiply(0.10),
        );
        ui.painter().rect_stroke(
            rect,
            CornerRadius::ZERO,
            Stroke::new(1.0, self.pal.accent.gamma_multiply(0.85)),
            egui::StrokeKind::Inside,
        );
        let any_down = ui.input(|i| i.pointer.any_down());
        if !(drag_stopped || !any_down) || (rect.width() < 2.0 && rect.height() < 2.0) {
            return;
        }
        let Some(m) = self.desk_marquee.take() else {
            return;
        };
        self.commit_desk_marquee(m);
    }

    fn draw_desktop(&mut self, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
        if self.desktop_items.is_empty() {
            return;
        }
        let pal = self.pal;
        let pp = self.scale.max(1.0);
        let show_labels = self.cfg.desktop.show_labels;
        let primary = self.primary_scale();
        // 纹理按"已合并的图标尺寸"请求；绘制尺寸仍用当前配置，保证滑杆即时响应。
        let tex_px_by_monitor: Vec<u32> = self
            .monitors
            .iter()
            .map(|m| self.tex_icon_px_on(m.scale))
            .collect();
        let fallback_tex_px = self.tex_icon_px_on(primary);
        // 与落点判定共用同一布局函数，保证拖动落点与绘制格子严格一致。
        let (cells, item_cell) = self.desktop_layout();
        if cells.is_empty() {
            return;
        }
        // 拖动桌面图标时高亮落点格子；指针移入盒子内容区后不再提示（释放会进盒子）。
        // 已占格按当前落格模式着色：交换=蓝，生成新盒子=橙。
        let drop_hint = match &self.drag {
            DragKind::Item {
                from_box: None,
                path,
                ..
            } => {
                let from = self.desktop_items.iter().position(|i| i.path == *path);
                ui.ctx()
                    .input(|i| i.pointer.interact_pos())
                    .and_then(|pos| {
                        let px = pos.x * pp;
                        let py = pos.y * pp;
                        if self
                            .cfg
                            .boxes
                            .iter()
                            .any(|b| model::point_in_rect(px, py, self.box_content_px(b)))
                        {
                            return None;
                        }
                        let target = cell_index_exact_at(&cells, pos)?;
                        let from_cell = from.and_then(|i| item_cell.get(i).copied().flatten())?;
                        if from_cell == target {
                            return None;
                        }
                        let occupied = item_cell
                            .iter()
                            .enumerate()
                            .any(|(i, c)| Some(i) != from && *c == Some(target));
                        Some((target, occupied))
                    })
            }
            _ => None,
        };
        if let Some((target, occupied)) = drop_hint {
            if let Some(c) = cells.get(target) {
                let cell = desktop_cell_rect(c);
                let color = if occupied {
                    match self.cfg.desktop.drop_on_occupied {
                        OccupiedDropMode::Swap => pal.accent,
                        OccupiedDropMode::NewBox => Color32::from_rgb(255, 149, 0),
                    }
                } else {
                    pal.accent
                };
                ui.painter().rect_stroke(
                    cell.shrink(2.0 * c.k),
                    CornerRadius::same((8.0 * c.k).round() as u8),
                    Stroke::new((2.0 * c.k).max(1.5), color),
                    egui::StrokeKind::Inside,
                );
            }
        }
        let icon_tx = &self.icon_tx;
        let items = &self.desktop_items;
        let mods = ui.input(|i| i.modifiers);
        let mut clicked_index: Option<usize> = None;
        // (真实下标, 屏幕坐标, 是否整组右键)
        let mut secondary_sel: Option<(usize, Pos2, bool)> = None;
        for (index, item) in items.iter().enumerate() {
            let Some(c) = item_cell
                .get(index)
                .copied()
                .flatten()
                .and_then(|ci| cells.get(ci))
                .copied()
            else {
                continue;
            };
            let cell = desktop_cell_rect(&c);
            if !ui.is_rect_visible(cell) {
                continue;
            }
            let icon_px = tex_px_by_monitor
                .get(c.monitor)
                .copied()
                .unwrap_or(fallback_tex_px);
            let k = c.k;
            let key = icon_key(item, icon_px);
            let tex = self.textures.ensure_shell_icon(
                icon_tx,
                key,
                item.path.clone(),
                item.is_dir,
                icon_px,
                item.system,
            );
            let icon_rect = item_icon_rect(cell, c.icon, pp);
            let selected = self.desk_selected.contains(&model::path_key(&item.path));
            let label_top = item_label_top(icon_rect, self.cfg.desktop.label_gap, k);
            let label_font_size = model::label_font_for_icon(c.icon);
            let label_layout = if show_labels {
                let max_w = item_label_max_w(c.w, k);
                let lines =
                    self.label_cache
                        .lines(item.display_name(), max_w, label_font_size, |text| {
                            measure_text_width(ui.painter(), text, label_font_size)
                        });
                let clip = cell.intersect(ui.painter().clip_rect());
                Some(LabelLayout::new(
                    ui.painter(),
                    &lines,
                    cell.center().x,
                    label_top,
                    max_w,
                    label_font_size,
                    clip,
                ))
            } else {
                None
            };
            // 选中/悬停框：按「图标顶 → 名称实际最后一行」的内容高度绘制，上下留相同内边距；
            // 不再用整格（整格为最多两行名称预留，单行名称会让框底多出一大块空白）。
            let item_rect = item_content_rect(cell, icon_rect, label_layout.as_ref(), k, pp, cell);
            if selected {
                ui.painter().rect_filled(
                    item_rect,
                    CornerRadius::same((8.0 * k).round() as u8),
                    Color32::from_rgba_unmultiplied(
                        pal.accent.r(),
                        pal.accent.g(),
                        pal.accent.b(),
                        46,
                    ),
                );
            }
            draw_icon(ui.painter(), icon_rect, tex, item.is_dir, &item.name, &pal);
            let target = LabelTarget::desktop(index);
            let editing = self.name_edit.as_ref().is_some_and(|e| e.target == target);
            if editing {
                let fallback_w = item_label_max_w(c.w, k);
                let fallback = Rect::from_min_size(
                    egui::pos2(cell.center().x - fallback_w * 0.5, label_top),
                    egui::vec2(
                        fallback_w,
                        label_font_size * model::LABEL_LINE_RATIO * model::LABEL_MAX_LINES as f32,
                    ),
                )
                .intersect(cell);
                self.name_edit_zone =
                    Some(label_layout.as_ref().map(|l| l.zone).unwrap_or(fallback));
                self.name_edit_font = label_font_size;
            }
            // 名称区域内按下进入文本选择模式；图标主体仍走原有的打开/拖拽交互。
            let icon_zone =
                Rect::from_min_max(cell.min, egui::pos2(cell.max.x, label_top.min(cell.max.y)))
                    .intersect(cell);
            let icon_resp = ui.interact(
                icon_zone,
                Id::unique(("desk-item", index)),
                Sense::click_and_drag(),
            );
            // 编辑中把标签交互让给文本框，避免框选/右键与编辑抢事件。
            let label_resp = if editing {
                None
            } else {
                label_layout.as_ref().and_then(|lay| {
                    (lay.zone.width() > 0.5 && lay.zone.height() > 0.5).then(|| {
                        ui.interact(
                            lay.zone,
                            Id::unique(("desk-label", index)),
                            Sense::click_and_drag(),
                        )
                    })
                })
            };
            if !editing {
                if let (Some(layout), Some(lresp)) = (&label_layout, &label_resp) {
                    handle_label_selection(&mut self.text_sel, ui, target, layout, lresp);
                }
            }
            let hovered = icon_resp.hovered() || label_resp.as_ref().is_some_and(|r| r.hovered());
            let item_clicked =
                icon_resp.clicked() || label_resp.as_ref().is_some_and(|r| r.clicked());
            if item_clicked {
                clicked_index = Some(index);
            }
            if let (Some(layout), false) = (&label_layout, editing) {
                let sel = self
                    .text_sel
                    .target
                    .filter(|t| *t == target)
                    .map(|_| self.text_sel.range());
                layout.paint(ui.painter(), hovered, sel);
            }
            // 选中项不再叠 hover 描边，避免「选中填充 + hover 边框」看起来像双线。
            if hovered && !selected {
                ui.painter().rect_stroke(
                    item_rect,
                    CornerRadius::same((8.0 * k).round() as u8),
                    Stroke::new((1.0 * k).max(1.0), pal.accent),
                    egui::StrokeKind::Inside,
                );
            }
            if icon_resp.double_clicked() {
                actions.push(UiAction::Open(item.path.clone()));
            }
            let secondary = icon_resp.secondary_clicked()
                || label_resp.as_ref().is_some_and(|r| r.secondary_clicked());
            if secondary {
                log::info!(
                    "菜单[t={}] UI 图标 secondary_clicked path={}",
                    crate::platform::shell_menu::now_ms(),
                    item.path.display()
                );
                if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                    let multi = self.desk_selected.contains(&model::path_key(&item.path))
                        && self.desk_selected.len() > 1;
                    secondary_sel = Some((index, pos, multi));
                }
            }
            if let Some(lresp) = &label_resp {
                if lresp.double_clicked() {
                    // 双击名称进入行内重命名；图标主体双击仍是打开。
                    // 系统项（此电脑）没有真实文件名，双击名称按打开处理。
                    if item.is_system() {
                        actions.push(UiAction::Open(item.path.clone()));
                    } else {
                        actions.push(UiAction::BeginRename {
                            box_idx: None,
                            item: index,
                        });
                    }
                }
            }
            // 系统项没有磁盘路径，不能进入盒子；但在桌面棋盘内可以拖动换格
            // （位置只写 `desktop.slots`，不涉及文件引用），拖进盒子由落点判定明确拒绝。
            if icon_resp.drag_started() && ui.input(|i| i.pointer.any_down()) {
                let pos = ui
                    .input(|i| i.pointer.interact_pos())
                    .unwrap_or(cell.center());
                let key = model::path_key(&item.path);
                if self.desk_selected.len() > 1 && self.desk_selected.contains(&key) {
                    // 拖动的是多选整组中的一员：整组一起走（保持相对位置）。
                    let paths: Vec<PathBuf> = items
                        .iter()
                        .filter(|it| self.desk_selected.contains(&model::path_key(&it.path)))
                        .map(|it| it.path.clone())
                        .collect();
                    self.drag = DragKind::Items {
                        paths,
                        icon: tex,
                        grab: pos - cell.min,
                        from_box: None,
                        clamp: None,
                        anchor: item.path.clone(),
                    };
                    log::info!(
                        "[drag] 开始桌面整组拖动：{} 项，锚点 {} @{:.1},{:.1}",
                        self.desk_selected.len(),
                        item.path.display(),
                        pos.x,
                        pos.y
                    );
                } else {
                    self.drag = DragKind::Item {
                        path: item.path.clone(),
                        icon: tex,
                        grab: pos - cell.min,
                        from_box: None,
                        clamp: None,
                    };
                    log::info!(
                        "[drag] 开始桌面拖动：{} @{:.1},{:.1}",
                        item.path.display(),
                        pos.x,
                        pos.y
                    );
                }
            }
        }
        if let Some(index) = clicked_index {
            // 点击桌面条目：盒子多选退出（同一时间只有一个选区上下文）。
            self.clear_box_selection_all();
            self.group_menu = None;
            self.apply_desk_click(index, mods);
        } else if let Some((index, pos, multi)) = secondary_sel {
            self.clear_box_selection_all();
            self.group_menu = None;
            if multi {
                let paths = self.desktop_selected_paths();
                self.desk_marquee = None;
                self.group_menu = Some(GroupMenu {
                    box_idx: None,
                    paths: paths.clone(),
                    pos,
                });
                log::info!("[menu] 桌面整组右键：{} 项", paths.len());
            } else {
                // 右键单条：先把选区收敛到该条，再走原生菜单。
                self.apply_desk_click_plain(index);
                let s = self.scale.max(1.0);
                let x = self.overlay.x + (pos.x * s).round() as i32;
                let y = self.overlay.y + (pos.y * s).round() as i32;
                actions.push(UiAction::ShowShellMenu {
                    box_idx: None,
                    item: index,
                    x,
                    y,
                });
            }
        }
    }

    fn draw_boxes(&mut self, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
        let mut idx = 0;
        while idx < self.cfg.boxes.len() {
            if !self.cfg.boxes[idx].closed {
                self.draw_box(ui, idx, actions);
            }
            idx += 1;
        }
    }

    fn draw_box(&mut self, ui: &mut egui::Ui, idx: usize, actions: &mut Vec<UiAction>) {
        let pal = self.pal;
        let scale = self.scale.max(1.0);
        let Some(box_cfg) = self.cfg.boxes.get(idx).cloned() else {
            return;
        };
        // 盒子内容按"所属显示器"的 DPI 渲染：物理几何仍用窗口 pp 换算，
        // 文字/按钮/圆角等逻辑尺寸按该屏缩放系数 k 还原，混合 DPI 下跨屏不模糊、不变形。
        let k = self.ui_k_at(box_cfg.x + box_cfg.w * 0.5, box_cfg.y + box_cfg.h * 0.5);
        let header_h = HEADER_H * k;
        let pad = PAD * k;
        let btn = BTN * k;
        let radius = (RADIUS as f32 * k).round() as u8;
        // 收起时把窗口高度真正压到标题栏；展开高度始终保存在 box_cfg.h 里，
        // 收起/展开都不会改写它，因此恢复后几何与滚动位置都不变。
        let expanded_h = box_cfg.h / scale;
        let collapsed_h = header_h;
        let t = if self.cfg.desktop.animations {
            ui.ctx().animate_bool_with_time(
                Id::unique(("box-fold-anim", idx)),
                !box_cfg.collapsed,
                0.12,
            )
        } else if box_cfg.collapsed {
            0.0
        } else {
            1.0
        };
        let rect = Rect::from_min_size(
            egui::pos2(box_cfg.x / scale, box_cfg.y / scale),
            egui::vec2(
                box_cfg.w / scale,
                collapsed_h + (expanded_h - collapsed_h) * t,
            ),
        );
        let header = Rect::from_min_size(rect.min, egui::vec2(rect.width(), header_h));
        let body = Rect::from_min_max(
            egui::pos2(rect.left() + pad, rect.top() + header_h),
            egui::pos2(rect.right() - pad, rect.bottom() - pad),
        );
        let fill = Color32::from_rgba_unmultiplied(
            box_cfg.color[0],
            box_cfg.color[1],
            box_cfg.color[2],
            box_cfg.alpha,
        );
        ui::rounded_card(ui.painter(), rect, radius, fill, true);

        let hovered = ui.rect_contains_pointer(rect);
        let dropping =
            matches!(self.drag, DragKind::Item { .. } | DragKind::Items { .. }) && hovered;
        let stroke = if dropping {
            Stroke::new((2.0 * k).max(1.0), pal.accent)
        } else if hovered && !box_cfg.locked {
            Stroke::new((1.4 * k).max(1.0), pal.accent)
        } else {
            Stroke::new((1.0 * k).max(1.0), pal.border)
        };
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(radius),
            stroke,
            egui::StrokeKind::Inside,
        );

        // 标题栏：拖动、重命名、右键菜单；原生菜单显示期间不挂 egui 菜单，避免两套并存。
        let head = ui.interact(
            header,
            Id::unique(("box-head", idx)),
            Sense::click_and_drag(),
        );
        if head.double_clicked() {
            actions.push(UiAction::RenameBox(idx));
        }
        if !crate::platform::shell_menu::menu_open() {
            let invalid = self.boxes.get(idx).map_or(0, |rt| rt.invalid.len());
            head.context_menu(|ui| box_menu(ui, idx, &box_cfg, invalid, actions));
        }
        if head.drag_started() && !box_cfg.locked {
            let pos = ui
                .input(|i| i.pointer.interact_pos())
                .unwrap_or(header.center());
            self.drag = DragKind::BoxMove {
                idx,
                grab: pos - rect.min,
            };
        }

        // 右上角按钮
        let by = header.center().y;
        let mut bx = rect.right() - pad - btn * 0.5;
        let close_rect = Rect::from_center_size(egui::pos2(bx, by), Vec2::splat(btn));
        bx -= btn + 4.0 * k;
        let lock_rect = Rect::from_center_size(egui::pos2(bx, by), Vec2::splat(btn));
        bx -= btn + 4.0 * k;
        let color_rect = Rect::from_center_size(egui::pos2(bx, by), Vec2::splat(btn));
        bx -= btn + 4.0 * k;
        let collapse_rect = Rect::from_center_size(egui::pos2(bx, by), Vec2::splat(btn));
        let title_w = (collapse_rect.left() - rect.left() - pad - 8.0 * k).max(40.0 * k);
        let title_left = rect.left() + pad;
        let title_rect = Rect::from_min_max(
            egui::pos2(title_left, header.top() + 3.0 * k),
            egui::pos2(title_left + title_w, header.bottom() - 3.0 * k),
        );
        if self.box_name_edit.as_ref().is_some_and(|e| e.idx == idx) {
            self.draw_box_name_editor(ui, idx, title_rect, k);
        } else {
            let title = box_title_display(&box_cfg.name, box_cfg.collapsed, self.box_count(idx));
            ui.painter().text(
                egui::pos2(title_left, by),
                Align2::LEFT_CENTER,
                fit_text(&title, title_w, 14.0 * k),
                FontId::proportional(14.0 * k),
                pal.text,
            );
            // 名称被截断时，悬停标题栏可看到完整名称。
            ui.interact(title_rect, Id::unique(("box-title", idx)), Sense::hover())
                .on_hover_text(box_cfg.name.clone());
        }
        if glyph_button(ui, close_rect, ("box-close", idx), "×", pal.text_dim, &pal) {
            // 关闭 ≠ 删除：× 只隐藏盒子，内容/几何/样式全部保留。
            actions.push(UiAction::CloseBox(idx));
        }
        if lock_button(ui, lock_rect, ("box-lock", idx), box_cfg.locked, &pal) {
            actions.push(UiAction::ToggleLock(idx));
        }
        if glyph_button(ui, color_rect, ("box-color", idx), "●", pal.text_dim, &pal) {
            actions.push(UiAction::ColorBox(idx));
        }
        // 使用 GB2312 内已有字形（▲/▼），避免 ▾/▸ 在中文字体里缺字变成豆腐块
        let collapse_glyph = if box_cfg.collapsed { "▼" } else { "▲" };
        if glyph_button(
            ui,
            collapse_rect,
            ("box-fold", idx),
            collapse_glyph,
            pal.text_dim,
            &pal,
        ) {
            actions.push(UiAction::ToggleCollapse(idx));
        }

        // 盒子空白区右键：与标题栏同一套盒子菜单。
        // 本响应先于条目内容注册，条目自身的右键命中优先级更高，不会抢占条目菜单。
        // `click_and_drag` 同时承载「点空白取消选择」和「框选」。
        let mut body_resp = None;
        if t > 0.01 && body.height() > 8.0 && !crate::platform::shell_menu::menu_open() {
            let invalid = self.boxes.get(idx).map_or(0, |rt| rt.invalid.len());
            let resp = ui.interact(body, Id::unique(("box-body", idx)), Sense::click_and_drag());
            resp.context_menu(|ui| box_menu(ui, idx, &box_cfg, invalid, actions));
            body_resp = Some(resp);
        }

        // 展开/折叠：高度由上面的 t 驱动；内容只在盒子区域还有空间时绘制，
        // 收起后不再布局内容，但 egui 会保留 ScrollArea 的滚动位置，展开即恢复。
        if t > 0.01 && body.height() > 8.0 {
            ui.scope(|ui| {
                ui.multiply_opacity(t);
                match box_cfg.kind {
                    BoxKind::Clipboard => self.draw_clip_box(ui, idx, body, &box_cfg, actions, k),
                    BoxKind::Todo => self.draw_todo_box(ui, idx, body, &box_cfg, actions, k),
                    _ => self.draw_box_items(ui, idx, body, &box_cfg, actions, body_resp.as_ref()),
                }
            });
        }

        // 缩放：左/右/下三边 + 四角。命中带 6 逻辑点、随所属屏 DPI 缩放；
        // 注册在标题栏与图标之后，因此重叠处命中优先级由缩放句柄取得。
        // 收起状态不参与缩放，避免把收起的矮高度写回展开高度。
        if !box_cfg.locked && !box_cfg.collapsed {
            let hit = EDGE_HIT * k;
            let half = hit * 0.5;
            let (left, right, top, bottom) = (rect.left(), rect.right(), rect.top(), rect.bottom());
            let handles: [(u8, Rect, egui::CursorIcon); 8] = [
                (
                    EDGE_LEFT,
                    Rect::from_min_max(
                        egui::pos2(left - half, top + hit),
                        egui::pos2(left + half, bottom - hit),
                    ),
                    egui::CursorIcon::ResizeHorizontal,
                ),
                (
                    EDGE_RIGHT,
                    Rect::from_min_max(
                        egui::pos2(right - half, top + hit),
                        egui::pos2(right + half, bottom - hit),
                    ),
                    egui::CursorIcon::ResizeHorizontal,
                ),
                (
                    EDGE_TOP,
                    Rect::from_min_max(
                        egui::pos2(left + hit, top - half),
                        egui::pos2(right - hit, top + half),
                    ),
                    egui::CursorIcon::ResizeVertical,
                ),
                (
                    EDGE_BOTTOM,
                    Rect::from_min_max(
                        egui::pos2(left + hit, bottom - half),
                        egui::pos2(right - hit, bottom + half),
                    ),
                    egui::CursorIcon::ResizeVertical,
                ),
                (
                    EDGE_LEFT | EDGE_TOP,
                    Rect::from_center_size(egui::pos2(left, top), Vec2::splat(hit)),
                    egui::CursorIcon::ResizeNwSe,
                ),
                (
                    EDGE_RIGHT | EDGE_TOP,
                    Rect::from_center_size(egui::pos2(right, top), Vec2::splat(hit)),
                    egui::CursorIcon::ResizeNeSw,
                ),
                (
                    EDGE_LEFT | EDGE_BOTTOM,
                    Rect::from_center_size(egui::pos2(left, bottom), Vec2::splat(hit)),
                    egui::CursorIcon::ResizeNeSw,
                ),
                (
                    EDGE_RIGHT | EDGE_BOTTOM,
                    Rect::from_center_size(egui::pos2(right, bottom), Vec2::splat(hit)),
                    egui::CursorIcon::ResizeNwSe,
                ),
            ];
            for (tag, hr, cursor) in handles {
                let resp = ui.interact(
                    hr,
                    Id::unique(("box-resize", idx, tag as u16)),
                    Sense::drag(),
                );
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(cursor);
                }
                if resp.drag_started() {
                    let pos = ui
                        .input(|i| i.pointer.interact_pos())
                        .unwrap_or(hr.center());
                    self.drag = DragKind::BoxResize {
                        idx,
                        edges: tag,
                        start: (box_cfg.x, box_cfg.y, box_cfg.w, box_cfg.h),
                        origin: pos,
                    };
                }
            }

            // 右下角的斜纹仅作视觉提示，实际命中由上面的四角/四边负责。
            let active = matches!(self.drag, DragKind::BoxResize { idx: i, .. } if i == idx);
            let color = if active { pal.accent } else { pal.text_dim };
            for line in 0..3 {
                let off = (4.0 + line as f32 * 4.0) * k;
                ui.painter().line_segment(
                    [
                        egui::pos2(rect.right() - off, rect.bottom() - 2.0 * k),
                        egui::pos2(rect.right() - 2.0 * k, rect.bottom() - off),
                    ],
                    Stroke::new((1.0 * k).max(1.0), color),
                );
            }
        }
    }

    fn draw_box_items(
        &mut self,
        ui: &mut egui::Ui,
        idx: usize,
        body: Rect,
        box_cfg: &BoxConfig,
        actions: &mut Vec<UiAction>,
        body_resp: Option<&egui::Response>,
    ) {
        let pal = self.pal;
        let scale = self.scale.max(1.0);
        let show_labels = self.cfg.desktop.show_labels;
        let primary = self.primary_scale();
        let s = self.monitor_scale_at(box_cfg.x + box_cfg.w * 0.5, box_cfg.y + box_cfg.h * 0.5);
        let k = (s / scale).max(0.1);
        // 绘制尺寸跟随当前配置（立即重排）；纹理请求尺寸跟随已合并的 tex_icon_size。
        let icon_cfg = self.icon_px_on(s);
        let tex_icon_cfg = self.tex_icon_px_on(s);
        let icon = icon_cfg as f32 / scale;
        let gap = (self.cfg.desktop.spacing as f32 / primary) * s / scale;
        // 名称字号与标签区高度跟随图标逻辑尺寸联动（与桌面图标同一套换算）。
        let label_font = model::label_font_for_icon(icon);
        let label_h = if show_labels {
            model::label_block_height(label_font, self.cfg.desktop.label_gap * k)
        } else {
            0.0
        };
        let total = self.boxes.get(idx).map(|b| b.items.len()).unwrap_or(0);
        // 类型 tab 只用作文档/普通盒的「显示筛选」，目录盒全是文件夹故不显示。
        let has_tabs = matches!(box_cfg.kind, BoxKind::Files | BoxKind::Normal);
        let filter = if has_tabs {
            box_cfg.file_filter
        } else {
            FileFilter::All
        };
        let counts: [usize; 8] = {
            let items = &self.boxes[idx].items;
            let mut c = [0usize; 8];
            for (i, f) in FileFilter::TABS.iter().enumerate() {
                c[i] = items
                    .iter()
                    .filter(|it| model::file_filter_matches(*f, it))
                    .count();
            }
            c
        };
        let mut list = body;
        if has_tabs {
            // 数量多时按宽度自动折行；行高固定，不挤压下方图标网格。
            let font = FontId::proportional(11.5 * k);
            let chip_h = 24.0 * k;
            let gap = 6.0 * k;
            let mut x = body.left();
            let mut y = body.top();
            for (i, f) in FileFilter::TABS.iter().enumerate() {
                let label = if counts[i] > 0 {
                    format!("{} {}", f.label(), counts[i])
                } else {
                    f.label().to_string()
                };
                let tw = measure_text_width(ui.painter(), &label, 11.5 * k);
                let chip_w = (tw + 16.0 * k).max(34.0 * k);
                if x + chip_w > body.right() && x > body.left() {
                    x = body.left();
                    y += chip_h + gap;
                }
                let r = Rect::from_min_size(egui::pos2(x, y), egui::vec2(chip_w, chip_h));
                let resp = ui.interact(r, Id::unique(("file-tab", idx, i as u16)), Sense::click());
                let selected = *f == filter;
                let fill = if selected {
                    pal.accent
                } else {
                    Color32::from_rgba_unmultiplied(
                        pal.card_bg.r(),
                        pal.card_bg.g(),
                        pal.card_bg.b(),
                        150,
                    )
                };
                ui.painter()
                    .rect_filled(r, CornerRadius::same((6.0 * k).round() as u8), fill);
                ui.painter().text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    &label,
                    font.clone(),
                    if selected {
                        Color32::WHITE
                    } else {
                        pal.text_dim
                    },
                );
                if resp.clicked() {
                    actions.push(UiAction::SetFileFilter(idx, *f));
                }
                x += chip_w + gap;
            }
            let tabs_h = y - body.top() + chip_h + 8.0 * k;
            list = Rect::from_min_max(egui::pos2(body.left(), body.top() + tabs_h), body.max);
        }
        let filtered = model::filtered_indices(&self.boxes[idx].items, filter);
        let count = filtered.len();
        let invalid = self.boxes[idx].invalid.len();
        if total == 0 {
            let msg = if invalid > 0 {
                format!("{invalid} 个引用已失效（源文件不存在或不可读）")
            } else {
                "把桌面图标拖进盒子即可收纳（文件保持原位）".into()
            };
            ui.painter().text(
                list.center(),
                Align2::CENTER_CENTER,
                msg,
                FontId::proportional(12.5 * k),
                pal.text_dim,
            );
            return;
        }
        if invalid > 0 {
            ui.painter().text(
                egui::pos2(list.left() + 4.0 * k, list.bottom() - 2.0 * k),
                Align2::LEFT_BOTTOM,
                format!("{invalid} 个引用已失效"),
                FontId::proportional(11.0 * k),
                Color32::from_rgb(224, 132, 120),
            );
        }
        if count == 0 {
            ui.painter().text(
                list.center(),
                Align2::CENTER_CENTER,
                "该分类下暂无项目",
                FontId::proportional(12.5 * k),
                pal.text_dim,
            );
            self.boxes[idx].scroll = 0.0;
            return;
        }
        let view = box_cfg.view;
        let row_h = (30.0 * k).max(18.0);
        // 缩略图视图用更大的格子；列表视图一行一项，不使用网格。
        let thumb_icon = (icon * 2.0).max(76.0 * k);
        let grid = match view {
            BoxView::Thumb => model::grid_layout(
                list.width(),
                list.height(),
                thumb_icon,
                label_h,
                gap,
                12.0 * k,
                count,
            ),
            _ => model::grid_layout(
                list.width(),
                list.height(),
                icon,
                label_h,
                gap,
                10.0 * k,
                count,
            ),
        };
        let content_h = match view {
            BoxView::List => count as f32 * row_h,
            _ => grid.cell_h * grid.total_rows_for(count) as f32 + grid.padding,
        };
        let max_scroll = (content_h - list.height()).max(0.0);
        // 筛选切换或条目减少后，旧偏移可能越界：每帧先夹回合法范围。
        self.boxes[idx].scroll = self.boxes[idx].scroll.min(max_scroll);
        if content_h > list.height() {
            let delta = ui.input(|i| i.smooth_scroll_delta.y);
            if ui.rect_contains_pointer(list) && delta.abs() > 0.5 {
                let scroll = self.boxes[idx].scroll;
                self.boxes[idx].scroll = (scroll - delta).clamp(0.0, max_scroll);
            }
        }
        let scroll = self.boxes[idx].scroll;
        // 键盘导航换算所需的布局快照（每帧写入，方向键事件时无需重算布局）。
        {
            let rt = &mut self.boxes[idx];
            rt.nav_cols = if view == BoxView::List {
                1
            } else {
                grid.cols.max(1)
            };
            rt.nav_row_h = row_h;
            rt.nav_list = list;
            rt.nav_cell = if view == BoxView::List {
                egui::vec2(list.width(), row_h)
            } else {
                egui::vec2(grid.cell_w, grid.cell_h)
            };
            rt.nav_origin = list.min;
            rt.nav_view = view;
        }
        // 空白区点击取消选择；空白区拖动开始框选（条目自身优先获得拖动命中）。
        let mods = ui.input(|i| i.modifiers);
        let body_clicked = body_resp.is_some_and(|r| r.clicked());
        if let Some(resp) = body_resp {
            if resp.drag_started() {
                if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                    self.marquee = Some(Marquee {
                        box_idx: idx,
                        start: pos,
                        current: pos,
                        additive: mods.command,
                    });
                }
            }
        }
        if let Some(m) = self.marquee.as_mut() {
            if m.box_idx == idx {
                if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                    m.current = pos;
                }
            }
        }

        let (first, last) = match view {
            BoxView::List => {
                let first = (scroll / row_h).floor() as usize;
                let last = (((scroll + list.height()) / row_h).ceil() as usize + 1).min(count);
                (first.min(count), last)
            }
            _ => {
                let (first_row, last_row) =
                    model::visible_row_range(&grid, scroll, list.height(), count);
                (
                    first_row.saturating_mul(grid.cols),
                    last_row.saturating_mul(grid.cols).min(count),
                )
            }
        };

        let painter = ui.painter().with_clip_rect(list);
        let icon_tx = &self.icon_tx;
        let items = &self.boxes[idx].items;
        let selected_keys = &self.boxes[idx].selected;
        let focus_key = self.boxes[idx].focus.as_deref();
        // 同盒拖动时高亮目标格；落点判定与释放共用 box_drop_display_index。
        let same_box_drag = match &self.drag {
            DragKind::Item {
                from_box: Some(from),
                ..
            }
            | DragKind::Items {
                from_box: Some(from),
                ..
            } => *from == idx,
            _ => false,
        };
        let drop_hint = if same_box_drag {
            ui.input(|i| i.pointer.interact_pos())
                .and_then(|p| self.box_drop_display_index(idx, p))
        } else {
            None
        };
        let mut clicked_sel: Option<(usize, PathBuf, String, u64, Rect)> = None;
        // (真实下标, 路径, 屏幕物理坐标, 是否多选中的一员)
        let mut secondary_sel: Option<(usize, PathBuf, egui::Pos2, bool)> = None;
        // 性能诊断：设置 DESKKEEPER_PERF=1 后，对 >=100 项的盒子记录每帧绘制耗时。
        let perf_t0 = (count >= 100 && std::env::var("DESKKEEPER_PERF").is_ok())
            .then(std::time::Instant::now);
        for index in first..last {
            // index 是筛选后的显示位次；real 是盒子里的真实下标（id/操作都用它）。
            let Some(&real) = filtered.get(index) else {
                continue;
            };
            let Some(item) = items.get(real) else {
                continue;
            };
            let cell = match view {
                BoxView::List => Rect::from_min_size(
                    egui::pos2(list.left(), list.top() + index as f32 * row_h - scroll),
                    egui::vec2(list.width(), row_h),
                ),
                _ => {
                    let (cx, cy) = grid.cell_pos(index);
                    Rect::from_min_size(
                        egui::pos2(list.left() + cx, list.top() + cy - scroll),
                        egui::vec2(grid.cell_w, grid.cell_h),
                    )
                }
            };
            if !ui.is_rect_visible(cell) {
                continue;
            }
            // 命中区域裁到盒子内容区：半格滑出盒子的图标，只有露在盒内的部分可点/可拖。
            let Some(hit) =
                model::intersect_rect(rect_tuple(cell), rect_tuple(list)).map(rect_from_tuple)
            else {
                continue;
            };
            if drop_hint == Some(index) {
                painter.rect_stroke(
                    cell.shrink((1.0 * k).max(1.0)),
                    CornerRadius::same((8.0 * k).round() as u8),
                    Stroke::new((1.6 * k).max(1.0), pal.accent),
                    egui::StrokeKind::Inside,
                );
            }
            let key = icon_key(item, tex_icon_cfg);
            let tex = self.textures.ensure_shell_icon(
                icon_tx,
                key,
                item.path.clone(),
                item.is_dir,
                tex_icon_cfg,
                item.system,
            );
            let icon_rect = match view {
                BoxView::List => pixel_aligned(
                    Rect::from_center_size(
                        egui::pos2(cell.left() + row_h * 0.5, cell.center().y),
                        Vec2::splat((row_h - 10.0 * k).max(12.0 * k)),
                    ),
                    scale,
                ),
                BoxView::Thumb => item_icon_rect(cell, thumb_icon, scale),
                BoxView::Icon => item_icon_rect(cell, icon, scale),
            };
            // 缩略图：后台线程用 IShellItemImageFactory 取图；未就绪时先画系统图标。
            let thumb_tex = if view == BoxView::Thumb
                && !item.is_dir
                && item.system.is_none()
                && !item.path.to_string_lossy().starts_with("::")
            {
                let px = (thumb_icon * scale).round() as u32;
                let thumb_key = format!("thumb:{px}:{}:{}", item.modified, item.path.display());
                self.textures
                    .ensure_shell_thumb(icon_tx, thumb_key, item.path.clone(), px)
            } else {
                None
            };
            // 名称几何与桌面共用同一来源；先算好才能把选中/hover 框画在内容矩形上。
            let label_top = item_label_top(icon_rect, self.cfg.desktop.label_gap, k);
            let label_layout = if show_labels && view != BoxView::List {
                let max_w = item_label_max_w(cell.width(), k);
                let lines =
                    self.label_cache
                        .lines(item.display_name(), max_w, label_font, |text| {
                            measure_text_width(&painter, text, label_font)
                        });
                Some(LabelLayout::new(
                    &painter,
                    &lines,
                    cell.center().x,
                    label_top,
                    max_w,
                    label_font,
                    hit,
                ))
            } else {
                None
            };
            let item_key = model::path_key(&item.path);
            let selected = selected_keys.contains(&item_key);
            let focused = focus_key.is_some_and(|f| f == item_key);
            // 列表视图一行一项，选中框仍是整行；图标/缩略图视图用与桌面一致的内容矩形。
            let visual_rect = if view == BoxView::List {
                hit.shrink2(egui::vec2(2.0 * k, 1.0 * k))
            } else {
                item_content_rect(cell, icon_rect, label_layout.as_ref(), k, scale, hit)
            };
            if selected {
                painter.rect_filled(
                    visual_rect,
                    CornerRadius::same((6.0 * k).round() as u8),
                    Color32::from_rgba_unmultiplied(
                        pal.accent.r(),
                        pal.accent.g(),
                        pal.accent.b(),
                        58,
                    ),
                );
                painter.rect_stroke(
                    visual_rect,
                    CornerRadius::same((6.0 * k).round() as u8),
                    Stroke::new((1.8 * k).max(1.0), pal.accent),
                    egui::StrokeKind::Inside,
                );
            }
            if focused && !selected {
                // 焦点描边只在「有焦点但未选中」时画；选中态已由 accent 描边表达，
                // 两者叠加会在同一矩形上留下内外两条边线。
                let focus_rect = if view == BoxView::List {
                    hit.shrink(3.0 * k)
                } else {
                    visual_rect.shrink((1.0 * k).max(1.0))
                };
                painter.rect_stroke(
                    focus_rect,
                    CornerRadius::same((8.0 * k).round() as u8),
                    Stroke::new((1.2 * k).max(1.0), Color32::WHITE),
                    egui::StrokeKind::Inside,
                );
            }
            if let Some((tid, natural)) = thumb_tex {
                let fit = fit_image_rect(icon_rect, natural);
                painter.image(
                    tid,
                    fit,
                    Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            } else {
                draw_icon(&painter, icon_rect, tex, item.is_dir, &item.name, &pal);
            }
            // 列表视图的「名称区」同时用于重命名编辑框定位；名称被截断时悬停显示全名。
            let mut name_rect: Option<Rect> = None;
            let mut full_name: Option<&str> = None;
            if view == BoxView::List {
                let name_x = icon_rect.right() + 8.0 * k;
                let meta = list_meta(item);
                let meta_font = FontId::proportional(11.0 * k);
                let meta_w = measure_text_width(&painter, &meta, 11.0 * k);
                let name_max = (cell.right() - 8.0 * k - meta_w - 8.0 * k - name_x).max(16.0);
                let name = fit_text(item.display_name(), name_max, 12.5 * k);
                if name != item.display_name() {
                    full_name = Some(item.display_name());
                }
                name_rect = Some(Rect::from_min_max(
                    egui::pos2(name_x, cell.top() + 3.0 * k),
                    egui::pos2(
                        cell.right() - 8.0 * k - meta_w - 8.0 * k,
                        cell.bottom() - 3.0 * k,
                    ),
                ));
                painter.text(
                    egui::pos2(name_x, cell.center().y),
                    Align2::LEFT_CENTER,
                    name,
                    FontId::proportional(12.5 * k),
                    pal.text,
                );
                painter.text(
                    egui::pos2(cell.right() - 8.0 * k, cell.center().y),
                    Align2::RIGHT_CENTER,
                    &meta,
                    meta_font,
                    pal.text_dim,
                );
            }
            let icon_zone = if view == BoxView::List {
                hit
            } else {
                Rect::from_min_max(hit.min, egui::pos2(hit.max.x, label_top.min(hit.max.y)))
                    .intersect(hit)
            };
            let target = LabelTarget::in_box(idx, real);
            let editing = self.name_edit.as_ref().is_some_and(|e| e.target == target);
            if editing {
                let fallback = if let Some(r) = name_rect {
                    r
                } else {
                    let fallback_w = item_label_max_w(cell.width(), k);
                    Rect::from_min_size(
                        egui::pos2(cell.center().x - fallback_w * 0.5, label_top),
                        egui::vec2(
                            fallback_w,
                            label_font * model::LABEL_LINE_RATIO * model::LABEL_MAX_LINES as f32,
                        ),
                    )
                    .intersect(hit)
                };
                self.name_edit_zone =
                    Some(label_layout.as_ref().map(|l| l.zone).unwrap_or(fallback));
                self.name_edit_font = if view == BoxView::List {
                    12.5 * k
                } else {
                    label_font
                };
            }
            let icon_resp = ui.interact(
                icon_zone,
                Id::unique(("box-item", idx, real)),
                Sense::click_and_drag(),
            );
            let label_resp = if editing {
                None
            } else {
                label_layout.as_ref().and_then(|lay| {
                    (lay.zone.width() > 0.5 && lay.zone.height() > 0.5).then(|| {
                        ui.interact(
                            lay.zone,
                            Id::unique(("box-label", idx, real)),
                            Sense::click_and_drag(),
                        )
                    })
                })
            };
            if !editing {
                if let (Some(layout), Some(lresp)) = (&label_layout, &label_resp) {
                    handle_label_selection(&mut self.text_sel, ui, target, layout, lresp);
                }
            }
            let hovered = icon_resp.hovered() || label_resp.as_ref().is_some_and(|r| r.hovered());
            if icon_resp.clicked() || label_resp.as_ref().is_some_and(|r| r.clicked()) {
                let name = item.display_name().to_string();
                clicked_sel = Some((real, item.path.clone(), name, item.modified, hit));
            }
            if let (Some(full), true) = (full_name, hovered) {
                icon_resp.clone().on_hover_text(full.to_owned());
            }
            if let (Some(layout), false) = (&label_layout, editing) {
                let sel = self
                    .text_sel
                    .target
                    .filter(|t| *t == target)
                    .map(|_| self.text_sel.range());
                layout.paint(&painter, hovered, sel);
            }
            if hovered {
                if view == BoxView::List && !selected {
                    painter.rect_filled(
                        hit.shrink2(egui::vec2(2.0 * k, 1.0 * k)),
                        CornerRadius::same((6.0 * k).round() as u8),
                        Color32::from_rgba_unmultiplied(
                            pal.accent.r(),
                            pal.accent.g(),
                            pal.accent.b(),
                            36,
                        ),
                    );
                }
                if !selected {
                    painter.rect_stroke(
                        cell.shrink(2.0 * k),
                        CornerRadius::same((8.0 * k).round() as u8),
                        Stroke::new((1.0 * k).max(1.0), pal.accent),
                        egui::StrokeKind::Inside,
                    );
                }
            }
            if icon_resp.double_clicked() {
                actions.push(UiAction::Open(item.path.clone()));
            }
            let secondary = icon_resp.secondary_clicked()
                || label_resp.as_ref().is_some_and(|r| r.secondary_clicked());
            if secondary {
                log::info!(
                    "菜单[t={}] UI 图标 secondary_clicked path={}",
                    crate::platform::shell_menu::now_ms(),
                    item.path.display()
                );
                if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                    let key = model::path_key(&item.path);
                    let multi = selected_keys.len() > 1 && selected_keys.contains(&key);
                    secondary_sel = Some((real, item.path.clone(), pos, multi));
                }
            }
            if let Some(lresp) = &label_resp {
                if lresp.double_clicked() {
                    // 双击名称进入行内重命名；图标主体双击仍是打开。
                    actions.push(UiAction::BeginRename {
                        box_idx: Some(idx),
                        item: real,
                    });
                }
            }
            if icon_resp.drag_started() && ui.input(|i| i.pointer.any_down()) {
                let pos = ui
                    .input(|i| i.pointer.interact_pos())
                    .unwrap_or(cell.center());
                let key = model::path_key(&item.path);
                if selected_keys.len() > 1 && selected_keys.contains(&key) {
                    // 多选态：拖动任一选中项 = 整组移动（只移动引用）。
                    let mut paths: Vec<PathBuf> = filtered
                        .iter()
                        .filter(|&&real| {
                            selected_keys.contains(&model::path_key(&items[real].path))
                        })
                        .map(|&real| items[real].path.clone())
                        .collect();
                    if !paths.iter().any(|p| p == &item.path) {
                        paths.push(item.path.clone());
                    }
                    self.drag = DragKind::Items {
                        paths,
                        icon: tex,
                        grab: pos - cell.min,
                        from_box: Some(idx),
                        clamp: Some(list),
                        anchor: item.path.clone(),
                    };
                } else {
                    self.drag = DragKind::Item {
                        path: item.path.clone(),
                        icon: tex,
                        grab: pos - cell.min,
                        from_box: Some(idx),
                        clamp: Some(list),
                    };
                }
                log::info!(
                    "[drag] 开始盒子拖动：{} box={idx} @{:.1},{:.1}",
                    item.path.display(),
                    pos.x,
                    pos.y
                );
            }
        }
        // 框选：先画矩形（在条目之上），再在松手帧提交选区。
        let mut marquee_draw: Option<Rect> = None;
        let mut marquee_commit_now = false;
        if let Some(m) = self.marquee.as_ref() {
            if m.box_idx == idx {
                let sel = Rect::from_two_pos(m.start, m.current).intersect(list);
                marquee_draw = Some(sel);
                if body_resp.is_some_and(|r| r.drag_stopped()) {
                    marquee_commit_now = true;
                }
            }
        }
        let mut marquee_commit: Option<(Marquee, Vec<MarqueeHit>)> = None;
        if marquee_commit_now {
            if let Some(m) = self.marquee.take() {
                let sel = Rect::from_two_pos(m.start, m.current).intersect(list);
                let rectf = (sel.min.x, sel.min.y, sel.width(), sel.height());
                let mut cells: Vec<(usize, model::RectF)> = Vec::with_capacity(filtered.len());
                for (display, real) in filtered.iter().enumerate() {
                    let cell = self.box_cell_rect(idx, display);
                    cells.push((*real, (cell.min.x, cell.min.y, cell.width(), cell.height())));
                }
                let hits = model::marquee_hits(&cells, rectf);
                log::debug!(
                    "[marquee] {:?} rect={:?} cell={:?} cols={} origin={:?} scroll={} cells={} hits={}",
                    view,
                    rectf,
                    self.boxes[idx].nav_cell,
                    self.boxes[idx].nav_cols,
                    self.boxes[idx].nav_origin,
                    self.boxes[idx].scroll,
                    cells.len(),
                    hits.len()
                );
                let hit_meta: Vec<(usize, PathBuf, String, u64)> = hits
                    .iter()
                    .map(|&real| {
                        (
                            real,
                            items[real].path.clone(),
                            items[real].display_name().to_string(),
                            items[real].modified,
                        )
                    })
                    .collect();
                marquee_commit = Some((m, hit_meta));
            }
        }
        if let Some(sel) = marquee_draw {
            painter.rect_filled(
                sel,
                CornerRadius::ZERO,
                Color32::from_rgba_unmultiplied(pal.accent.r(), pal.accent.g(), pal.accent.b(), 40),
            );
            painter.rect_stroke(
                sel,
                CornerRadius::ZERO,
                Stroke::new((1.0 * k).max(1.0), pal.accent),
                egui::StrokeKind::Inside,
            );
        }

        // —— 以下不再借用 items，可以安全修改选区状态 ——
        if body_clicked {
            self.clear_box_selection_all();
            self.sel_item = None;
            self.group_menu = None;
            self.clear_desk_selection("点击盒子空白");
            self.sync_preview_hook();
        }
        if let Some((real, path, ..)) = clicked_sel {
            // 选区/预览目标统一由 apply_box_click 决定：多选时清空 sel_item，
            // 这里再无条件 select_item 会把多选状态覆盖成“最后点击项”。
            self.apply_box_click(idx, real, &path, mods);
        }
        if let Some((real, path, pos, multi)) = secondary_sel {
            if multi && !path.as_os_str().is_empty() {
                let paths = self.visible_selected_paths(idx);
                self.focus_box = Some(idx);
                self.marquee = None;
                self.group_menu = Some(GroupMenu {
                    box_idx: Some(idx),
                    paths,
                    pos,
                });
            } else {
                self.apply_box_click_plain(idx, real, &path);
                let s = self.scale.max(1.0);
                let x = self.overlay.x + (pos.x * s).round() as i32;
                let y = self.overlay.y + (pos.y * s).round() as i32;
                actions.push(UiAction::ShowShellMenu {
                    box_idx: Some(idx),
                    item: real,
                    x,
                    y,
                });
            }
        }
        if let Some((m, hit_meta)) = marquee_commit {
            // 盒内框选生效即退出桌面多选。
            self.clear_desk_selection("盒内框选");
            if !m.additive {
                self.boxes[idx].selected.clear();
            }
            for (_, path, _, _) in &hit_meta {
                self.boxes[idx].selected.insert(model::path_key(path));
            }
            if let Some((real, path, name, modified)) = hit_meta.first().cloned() {
                let key = model::path_key(&path);
                self.boxes[idx].focus = Some(key.clone());
                self.boxes[idx].anchor = Some(key);
                self.focus_box = Some(idx);
                let display = self
                    .box_filtered_indices(idx)
                    .iter()
                    .position(|&r| r == real)
                    .unwrap_or(0);
                let anchor = self.box_cell_rect(idx, display);
                self.select_item(&path, &name, modified, anchor);
            } else {
                self.focus_box = Some(idx);
            }
        }
        if let Some(t0) = perf_t0 {
            log::info!(
                "[perf] 盒 {} 视图 {}：{} 项可见 {}，本帧绘制 {:.2} ms",
                box_cfg.name,
                view.label(),
                count,
                last.saturating_sub(first),
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    /// 剪贴板盒：与普通盒子同一套背景/标题/边框，内容来自剪贴板 store。
    fn draw_clip_box(
        &mut self,
        ui: &mut egui::Ui,
        idx: usize,
        body: Rect,
        box_cfg: &BoxConfig,
        actions: &mut Vec<UiAction>,
        k: f32,
    ) {
        let pal = self.pal;
        let chip_h = 24.0 * k;
        let chip_w = 46.0 * k;
        let mut x = body.left();
        let mut filter = box_cfg.clip_filter;
        for f in [
            ClipFilter::All,
            ClipFilter::Text,
            ClipFilter::Image,
            ClipFilter::Files,
        ] {
            let label = match f {
                ClipFilter::All => "全部",
                ClipFilter::Text => "文本",
                ClipFilter::Image => "图片",
                ClipFilter::Files => "文件",
            };
            let r = Rect::from_min_size(egui::pos2(x, body.top()), egui::vec2(chip_w, chip_h));
            let resp = ui.interact(r, Id::unique(("clip-box-chip", idx, label)), Sense::click());
            let selected = f == filter;
            let fill = if selected {
                pal.accent
            } else {
                Color32::from_rgba_unmultiplied(
                    pal.card_bg.r(),
                    pal.card_bg.g(),
                    pal.card_bg.b(),
                    150,
                )
            };
            ui.painter()
                .rect_filled(r, CornerRadius::same((6.0 * k).round() as u8), fill);
            ui.painter().text(
                r.center(),
                Align2::CENTER_CENTER,
                label,
                FontId::proportional(11.5 * k),
                if selected {
                    Color32::WHITE
                } else {
                    pal.text_dim
                },
            );
            if resp.clicked() {
                filter = f;
                if let Some(b) = self.cfg.boxes.get_mut(idx) {
                    b.clip_filter = f;
                }
                self.persist();
            }
            x += chip_w + 6.0 * k;
        }
        // 面板移除后补齐能力：暂停/继续、清空、搜索全部搬进盒子。
        // 工具按钮放在搜索行右侧，避免窄盒子时与四个筛选芯片重叠。
        let tools_w = 100.0 * k;
        let search_rect = Rect::from_min_max(
            egui::pos2(body.left(), body.top() + chip_h + 6.0 * k),
            egui::pos2(
                (body.right() - tools_w - 6.0 * k).max(body.left() + 80.0 * k),
                body.top() + chip_h + 30.0 * k,
            ),
        );
        let mut search = self.boxes[idx].clip_search.clone();
        let search_resp = ui.put(
            search_rect,
            egui::TextEdit::singleline(&mut search)
                .hint_text("搜索内容或来源")
                .font(FontId::proportional(12.0 * k)),
        );
        if search_resp.changed() {
            self.boxes[idx].clip_search = search;
        }
        // 搜索框同样需要临时获得键盘焦点，否则叠加层收不到键入与剪贴板快捷键。
        if search_resp.clicked() && !search_resp.has_focus() {
            self.begin_text_input();
            search_resp.request_focus();
        }
        if search_resp.lost_focus() && ui.ctx().memory(|m| m.focused()).is_none() {
            self.end_text_input();
        }
        let paused = !self.cfg.clipboard.enabled;
        let pause_label = if paused { "继续" } else { "暂停" };
        let pause_rect = Rect::from_min_size(
            egui::pos2(body.right() - tools_w, search_rect.top()),
            egui::vec2(46.0 * k, chip_h),
        );
        let clear_rect = Rect::from_min_size(
            egui::pos2(body.right() - 50.0 * k, search_rect.top()),
            egui::vec2(46.0 * k, chip_h),
        );
        for (rect, label, action, selected) in [
            (pause_rect, pause_label, UiAction::ClipTogglePause, paused),
            (clear_rect, "清空", UiAction::ClearClipboard, false),
        ] {
            let resp = ui.interact(
                rect,
                Id::unique(("clip-box-tool", idx, label)),
                Sense::click(),
            );
            ui.painter().rect_filled(
                rect,
                CornerRadius::same((6.0 * k).round() as u8),
                if selected {
                    pal.accent
                } else {
                    Color32::from_rgba_unmultiplied(
                        pal.card_bg.r(),
                        pal.card_bg.g(),
                        pal.card_bg.b(),
                        150,
                    )
                },
            );
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                label,
                FontId::proportional(11.5 * k),
                if selected {
                    Color32::WHITE
                } else {
                    pal.text_dim
                },
            );
            if resp.clicked() {
                actions.push(action);
            }
        }

        let list = Rect::from_min_max(
            egui::pos2(body.left(), search_rect.bottom() + 6.0 * k),
            body.max,
        );
        if list.height() < 20.0 * k {
            return;
        }
        let needle = self.boxes[idx].clip_search.trim().to_lowercase();
        let visible: Vec<ClipRecord> = self
            .clip_box_records
            .iter()
            .filter(|r| record_matches(r, filter, &needle))
            .cloned()
            .collect();
        if visible.is_empty() {
            ui.painter().text(
                list.center(),
                Align2::CENTER_CENTER,
                if needle.is_empty() {
                    "剪贴板历史为空，复制内容后自动出现"
                } else {
                    "没有匹配的记录"
                },
                FontId::proportional(12.0 * k),
                pal.text_dim,
            );
            return;
        }
        let row_h = 46.0 * k;
        let row_gap = 6.0 * k;
        let content_h = visible.len() as f32 * (row_h + row_gap) - row_gap;
        let max_scroll = (content_h - list.height()).max(0.0);
        if max_scroll > 0.0 {
            let delta = ui.input(|i| i.smooth_scroll_delta.y);
            if ui.rect_contains_pointer(list) && delta.abs() > 0.1 {
                let scroll = self.boxes[idx].scroll;
                self.boxes[idx].scroll = (scroll - delta).clamp(0.0, max_scroll);
            }
        }
        let scroll = self.boxes[idx].scroll.min(max_scroll);
        let painter = ui.painter().with_clip_rect(list);
        for (i, rec) in visible.iter().enumerate() {
            let top = list.top() + i as f32 * (row_h + row_gap) - scroll;
            if top + row_h < list.top() || top > list.bottom() {
                continue;
            }
            let row = Rect::from_min_size(
                egui::pos2(list.left(), top),
                egui::vec2(list.width(), row_h),
            );
            painter.rect_filled(
                row,
                CornerRadius::same((8.0 * k).round() as u8),
                Color32::from_rgba_unmultiplied(
                    pal.card_bg.r(),
                    pal.card_bg.g(),
                    pal.card_bg.b(),
                    170,
                ),
            );
            let kind_color = match rec.kind.as_str() {
                "image" => Color32::from_rgb(110, 180, 130),
                "files" => Color32::from_rgb(214, 168, 96),
                _ => pal.accent,
            };
            let badge = Rect::from_min_size(
                egui::pos2(row.left() + 8.0 * k, row.top() + 8.0 * k),
                egui::vec2(34.0 * k, 19.0 * k),
            );
            painter.rect_filled(
                badge,
                CornerRadius::same((5.0 * k).round() as u8),
                Color32::from_rgba_unmultiplied(kind_color.r(), kind_color.g(), kind_color.b(), 40),
            );
            painter.text(
                badge.center(),
                Align2::CENTER_CENTER,
                rec.kind_label(),
                FontId::proportional(10.5 * k),
                kind_color,
            );
            let text_x = badge.right() + 10.0 * k;
            let action_w = 22.0 * k;
            let action_h = 20.0 * k;
            let action_gap = 5.0 * k;
            let del_rect = Rect::from_min_size(
                egui::pos2(row.right() - 8.0 * k - action_w, row.top() + 7.0 * k),
                egui::vec2(action_w, action_h),
            );
            let pin_rect = Rect::from_min_size(
                egui::pos2(del_rect.left() - action_gap - action_w, del_rect.top()),
                egui::vec2(action_w, action_h),
            );
            let text_w = (pin_rect.left() - 8.0 * k - text_x).max(30.0 * k);
            if rec.pinned {
                painter.rect_filled(
                    Rect::from_min_size(row.min, egui::vec2(2.0 * k, row.height())),
                    CornerRadius::same((2.0 * k).round() as u8),
                    pal.accent,
                );
            }
            painter.text(
                egui::pos2(text_x, row.top() + 6.0 * k),
                Align2::LEFT_TOP,
                fit_text(&rec.preview(), text_w, 12.0 * k),
                FontId::proportional(12.0 * k),
                pal.text,
            );
            let source = rec
                .source
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("来源未知");
            let source_line = if source == "来源未知" {
                source.to_string()
            } else {
                format!("来源 {source}")
            };
            painter.text(
                egui::pos2(text_x, row.top() + 26.0 * k),
                Align2::LEFT_TOP,
                fit_text(&source_line, (text_w - 6.0 * k).max(20.0 * k), 10.5 * k),
                FontId::proportional(10.5 * k),
                pal.text_dim,
            );
            painter.text(
                egui::pos2(row.right() - 8.0 * k, row.top() + 28.0 * k),
                Align2::RIGHT_TOP,
                crate::util::format_time(rec.created_at),
                FontId::proportional(10.0 * k),
                pal.text_dim,
            );
            let pin_resp = ui.interact(
                pin_rect.intersect(list),
                Id::unique(("clip-box-pin", idx, rec.id)),
                Sense::click(),
            );
            let del_resp = ui.interact(
                del_rect.intersect(list),
                Id::unique(("clip-box-del", idx, rec.id)),
                Sense::click(),
            );
            let small_radius = CornerRadius::same((5.0 * k).round() as u8);
            painter.rect_filled(
                pin_rect,
                small_radius,
                if rec.pinned {
                    pal.accent
                } else {
                    Color32::from_rgba_unmultiplied(
                        pal.card_bg.r(),
                        pal.card_bg.g(),
                        pal.card_bg.b(),
                        150,
                    )
                },
            );
            painter.text(
                pin_rect.center(),
                Align2::CENTER_CENTER,
                "顶",
                FontId::proportional(10.5 * k),
                if rec.pinned {
                    Color32::WHITE
                } else {
                    pal.text_dim
                },
            );
            painter.rect_filled(
                del_rect,
                small_radius,
                Color32::from_rgba_unmultiplied(
                    pal.card_bg.r(),
                    pal.card_bg.g(),
                    pal.card_bg.b(),
                    150,
                ),
            );
            painter.text(
                del_rect.center(),
                Align2::CENTER_CENTER,
                "删",
                FontId::proportional(10.5 * k),
                Color32::from_rgb(214, 92, 92),
            );
            if pin_resp.clicked() {
                actions.push(UiAction::ClipSetPinned {
                    id: rec.id,
                    pinned: !rec.pinned,
                });
            } else if del_resp.clicked() {
                actions.push(UiAction::ClipDelete(rec.id));
            }
            let click_rect =
                Rect::from_min_max(row.min, egui::pos2(pin_rect.left() - 6.0 * k, row.bottom()));
            let resp = ui.interact(
                click_rect.intersect(list),
                Id::unique(("clip-box-row", idx, rec.id)),
                Sense::click(),
            );
            if resp.hovered() {
                painter.rect_stroke(
                    row,
                    CornerRadius::same((8.0 * k).round() as u8),
                    Stroke::new((1.0 * k).max(1.0), pal.accent),
                    egui::StrokeKind::Inside,
                );
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            } else if pin_resp.hovered() || del_resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.double_clicked() {
                actions.push(UiAction::ClipPaste(rec.clone()));
            } else if resp.clicked() {
                actions.push(UiAction::CopyClip(rec.clone()));
            }
            if !crate::platform::shell_menu::menu_open() {
                resp.context_menu(|ui| {
                    if ui.button("粘贴到前台窗口").clicked() {
                        actions.push(UiAction::ClipPaste(rec.clone()));
                        ui.close();
                    }
                    let pin_label = if rec.pinned { "取消置顶" } else { "置顶" };
                    if ui.button(pin_label).clicked() {
                        actions.push(UiAction::ClipSetPinned {
                            id: rec.id,
                            pinned: !rec.pinned,
                        });
                        ui.close();
                    }
                    if ui.button("复制文本").clicked() {
                        actions.push(UiAction::CopyClip(rec.clone()));
                        ui.close();
                    }
                    if ui.button("删除").clicked() {
                        actions.push(UiAction::ClipDelete(rec.id));
                        ui.close();
                    }
                });
            }
        }
    }

    /// 待办盒：与普通盒子同一套背景/标题/边框，数据保存在配置中。
    fn draw_todo_box(
        &mut self,
        ui: &mut egui::Ui,
        idx: usize,
        body: Rect,
        box_cfg: &BoxConfig,
        actions: &mut Vec<UiAction>,
        k: f32,
    ) {
        let pal = self.pal;
        let chip_h = 24.0 * k;
        let gap = 6.0 * k;
        let radius = (8.0 * k).round() as u8;

        // 排序 chips：手动 / 名称 / 未完成优先
        let mut x = body.left();
        let mut sort = box_cfg.todo_sort;
        for s in TodoSort::ALL {
            let chip_w = match s {
                TodoSort::Pending => 80.0 * k,
                _ => 46.0 * k,
            };
            let r = Rect::from_min_size(egui::pos2(x, body.top()), egui::vec2(chip_w, chip_h));
            let resp = ui.interact(
                r,
                Id::unique(("todo-sort-chip", idx, s.label())),
                Sense::click(),
            );
            let selected = s == sort;
            let fill = if selected {
                pal.accent
            } else {
                Color32::from_rgba_unmultiplied(
                    pal.card_bg.r(),
                    pal.card_bg.g(),
                    pal.card_bg.b(),
                    150,
                )
            };
            let painter = ui.painter();
            painter.rect_filled(r, CornerRadius::same((6.0 * k).round() as u8), fill);
            painter.text(
                r.center(),
                Align2::CENTER_CENTER,
                s.label(),
                FontId::proportional(11.5 * k),
                if selected {
                    Color32::WHITE
                } else {
                    pal.text_dim
                },
            );
            if resp.clicked() {
                sort = s;
                if let Some(b) = self.cfg.boxes.get_mut(idx) {
                    b.todo_sort = s;
                }
                self.persist();
            }
            x += chip_w + gap;
        }

        // 新增输入框
        let input_rect = Rect::from_min_size(
            egui::pos2(body.left(), body.top() + chip_h + 8.0 * k),
            egui::vec2(body.width(), 30.0 * k),
        );
        ui.painter().rect_filled(
            input_rect,
            CornerRadius::same(radius),
            Color32::from_rgba_unmultiplied(pal.card_bg.r(), pal.card_bg.g(), pal.card_bg.b(), 150),
        );
        let mut draft = std::mem::take(&mut self.todo_draft);
        let add_rect = Rect::from_min_size(
            egui::pos2(input_rect.right() - 58.0 * k, input_rect.top() + 3.0 * k),
            egui::vec2(52.0 * k, input_rect.height() - 6.0 * k),
        );
        let resp = ui.put(
            Rect::from_min_max(
                input_rect.min + egui::vec2(9.0 * k, 3.0 * k),
                egui::pos2(add_rect.left() - 6.0 * k, input_rect.bottom() - 3.0 * k),
            ),
            egui::TextEdit::singleline(&mut draft)
                .id(Id::unique(("todo-draft", idx)))
                .hint_text("+ 添加待办，回车确认")
                .font(FontId::proportional(12.5 * k))
                .text_color(pal.text)
                .frame(egui::Frame::NONE)
                .desired_width(input_rect.width() - 18.0 * k - 64.0 * k)
                .align(Align2::LEFT_CENTER),
        );
        self.todo_draft = draft;
        let add_resp = ui.interact(add_rect, Id::unique(("todo-add", idx)), Sense::click());
        ui.painter()
            .rect_filled(add_rect, CornerRadius::same(radius), pal.accent);
        ui.painter().text(
            add_rect.center(),
            Align2::CENTER_CENTER,
            "添加",
            FontId::proportional(11.5 * k),
            Color32::WHITE,
        );
        if add_resp.clicked() {
            let text = self.todo_draft.trim().to_string();
            if !text.is_empty() {
                actions.push(UiAction::TodoAdd(idx, text));
                self.todo_draft.clear();
            }
            self.end_text_input();
        }
        if resp.clicked() && !resp.has_focus() {
            self.begin_text_input();
            resp.request_focus();
        }
        // 单行 TextEdit 收到回车会在同一帧放弃焦点，因此不能只看 has_focus。
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if (resp.has_focus() || resp.lost_focus()) && enter {
            let text = self.todo_draft.trim().to_string();
            if !text.is_empty() {
                actions.push(UiAction::TodoAdd(idx, text));
                self.todo_draft.clear();
            }
            self.end_text_input();
        } else if resp.has_focus() && esc {
            self.todo_draft.clear();
            self.end_text_input();
        } else if resp.lost_focus() {
            self.end_text_input();
        }

        let list = Rect::from_min_max(
            egui::pos2(body.left(), input_rect.bottom() + 8.0 * k),
            body.max,
        );
        if list.height() < 20.0 * k {
            return;
        }
        let mut todos = box_cfg.todos.clone();
        match sort {
            TodoSort::Manual => {}
            TodoSort::Name => todos.sort_by(|a, b| model::natural_cmp(&a.text, &b.text)),
            // 稳定排序：同组内保持新增顺序
            TodoSort::Pending => todos.sort_by_key(|t| t.done),
        }
        if todos.is_empty() {
            ui.painter().text(
                list.center(),
                Align2::CENTER_CENTER,
                "还没有待办，在上方输入框添加",
                FontId::proportional(12.0 * k),
                pal.text_dim,
            );
            return;
        }
        let row_h = 32.0 * k;
        let row_gap = 4.0 * k;
        let content_h = todos.len() as f32 * (row_h + row_gap) - row_gap;
        let max_scroll = (content_h - list.height()).max(0.0);
        if max_scroll > 0.0 {
            let delta = ui.input(|i| i.smooth_scroll_delta.y);
            if ui.rect_contains_pointer(list) && delta.abs() > 0.1 {
                let scroll = self.boxes[idx].scroll;
                self.boxes[idx].scroll = (scroll - delta).clamp(0.0, max_scroll);
            }
        }
        let scroll = self.boxes[idx].scroll.min(max_scroll);
        let painter = ui.painter().with_clip_rect(list);
        let font = FontId::proportional(12.5 * k);
        for (i, t) in todos.iter().enumerate() {
            let top = list.top() + i as f32 * (row_h + row_gap) - scroll;
            if top + row_h < list.top() || top > list.bottom() {
                continue;
            }
            let row = Rect::from_min_size(
                egui::pos2(list.left(), top),
                egui::vec2(list.width(), row_h),
            );
            painter.rect_filled(
                row,
                CornerRadius::same(radius),
                Color32::from_rgba_unmultiplied(
                    pal.card_bg.r(),
                    pal.card_bg.g(),
                    pal.card_bg.b(),
                    if t.done { 120 } else { 170 },
                ),
            );
            let check = Rect::from_center_size(
                egui::pos2(row.left() + 16.0 * k, row.center().y),
                Vec2::splat(15.0 * k),
            );
            let check_color = if t.done { pal.accent } else { pal.text_dim };
            painter.rect_stroke(
                check,
                CornerRadius::same((4.0 * k).round() as u8),
                Stroke::new((1.2 * k).max(1.0), check_color),
                egui::StrokeKind::Inside,
            );
            if t.done {
                painter.rect_filled(
                    check.shrink(2.0 * k),
                    CornerRadius::same((3.0 * k).round() as u8),
                    pal.accent,
                );
                let c = check.center();
                let arm = 4.0 * k;
                painter.line_segment(
                    [c + egui::vec2(-arm, 0.0), c + egui::vec2(0.0, arm * 0.9)],
                    Stroke::new((1.6 * k).max(1.0), Color32::WHITE),
                );
                painter.line_segment(
                    [
                        c + egui::vec2(0.0, arm * 0.9),
                        c + egui::vec2(arm, -arm * 0.8),
                    ],
                    Stroke::new((1.6 * k).max(1.0), Color32::WHITE),
                );
            }
            let check_resp = ui.interact(
                check,
                Id::unique(("todo-check", idx, &t.id)),
                Sense::click(),
            );
            if check_resp.clicked() {
                actions.push(UiAction::TodoToggle(idx, t.id.clone()));
            }

            let del = Rect::from_center_size(
                egui::pos2(row.right() - 14.0 * k, row.center().y),
                Vec2::splat(18.0 * k),
            );
            let text_area = Rect::from_min_max(
                egui::pos2(row.left() + 28.0 * k, row.top()),
                egui::pos2(del.left() - 4.0 * k, row.bottom()),
            );
            let editing = self
                .todo_edit
                .clone()
                .filter(|e| e.box_idx == idx && e.id == t.id);
            if let Some(mut e) = editing {
                let mut text = std::mem::take(&mut e.text);
                let r = ui.put(
                    text_area.shrink2(egui::vec2(2.0 * k, 2.0 * k)),
                    egui::TextEdit::singleline(&mut text)
                        .id(Id::unique(("todo-edit", idx, &t.id)))
                        .font(font.clone())
                        .text_color(pal.text)
                        .frame(egui::Frame::NONE)
                        .desired_width(text_area.width() - 6.0 * k)
                        .align(Align2::LEFT_CENTER),
                );
                e.text = text;
                if e.focus {
                    r.request_focus();
                    e.focus = false;
                }
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
                if enter {
                    actions.push(UiAction::TodoCommit(
                        idx,
                        t.id.clone(),
                        e.text.trim().to_string(),
                    ));
                    self.todo_edit = None;
                    self.end_text_input();
                } else if esc {
                    self.todo_edit = None;
                    self.end_text_input();
                } else if r.lost_focus() {
                    actions.push(UiAction::TodoCommit(
                        idx,
                        t.id.clone(),
                        e.text.trim().to_string(),
                    ));
                    self.todo_edit = None;
                    self.end_text_input();
                } else {
                    self.todo_edit = Some(e);
                }
            } else {
                let color = if t.done { pal.text_dim } else { pal.text };
                let max_w = (text_area.width() - 4.0 * k).max(20.0 * k);
                let shown = fit_text(&t.text, max_w, 12.5 * k);
                let galley = painter.layout_no_wrap(shown, font.clone(), color);
                let pos = egui::pos2(
                    text_area.left() + 2.0 * k,
                    row.center().y - galley.size().y * 0.5,
                );
                let width = galley.size().x;
                painter.galley(pos, galley, color);
                if t.done {
                    painter.line_segment(
                        [
                            egui::pos2(pos.x, row.center().y),
                            egui::pos2(pos.x + width, row.center().y),
                        ],
                        Stroke::new((1.0 * k).max(1.0), pal.text_dim),
                    );
                }
                let text_resp = ui.interact(
                    text_area,
                    Id::unique(("todo-text", idx, &t.id)),
                    Sense::click(),
                );
                if text_resp.double_clicked() {
                    self.todo_edit = Some(TodoEdit {
                        box_idx: idx,
                        id: t.id.clone(),
                        text: t.text.clone(),
                        focus: true,
                    });
                    self.begin_text_input();
                }
                if !crate::platform::shell_menu::menu_open() {
                    let done = t.done;
                    let tid = t.id.clone();
                    let text = t.text.clone();
                    text_resp.context_menu(|ui| {
                        if ui.button("编辑").clicked() {
                            self.todo_edit = Some(TodoEdit {
                                box_idx: idx,
                                id: tid.clone(),
                                text: text.clone(),
                                focus: true,
                            });
                            self.begin_text_input();
                            ui.close();
                        }
                        let toggle_label = if done {
                            "标记未完成"
                        } else {
                            "标记完成"
                        };
                        if ui.button(toggle_label).clicked() {
                            actions.push(UiAction::TodoToggle(idx, tid.clone()));
                            ui.close();
                        }
                        if ui.button("置顶（手动排序）").clicked() {
                            actions.push(UiAction::TodoMoveTop(idx, tid.clone()));
                            ui.close();
                        }
                        if ui.button("删除").clicked() {
                            if self.todo_edit.as_ref().is_some_and(|e| e.id == tid) {
                                self.todo_edit = None;
                                self.end_text_input();
                            }
                            actions.push(UiAction::TodoDelete(idx, tid.clone()));
                            ui.close();
                        }
                    });
                }
            }

            let del_resp = ui.interact(del, Id::unique(("todo-del", idx, &t.id)), Sense::click());
            if del_resp.hovered() {
                painter.rect_filled(
                    del,
                    CornerRadius::same((5.0 * k).round() as u8),
                    pal.card_hover,
                );
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            painter.text(
                del.center(),
                Align2::CENTER_CENTER,
                "×",
                FontId::proportional(13.0 * k),
                if del_resp.hovered() {
                    pal.text
                } else {
                    pal.text_dim
                },
            );
            if del_resp.clicked() {
                if self.todo_edit.as_ref().is_some_and(|e| e.id == t.id) {
                    self.todo_edit = None;
                    self.end_text_input();
                }
                actions.push(UiAction::TodoDelete(idx, t.id.clone()));
            }
        }
    }

    fn draw_drag(&mut self, ui: &mut egui::Ui) {
        let Some(pos) = ui.input(|i| i.pointer.interact_pos()) else {
            return;
        };
        if !ui.input(|i| i.pointer.any_down()) {
            return;
        }
        if let Some((icon, grab, clamp, count)) = match &self.drag {
            DragKind::Item {
                icon, grab, clamp, ..
            } => Some((icon, grab, clamp, 1usize)),
            DragKind::Items {
                icon,
                grab,
                clamp,
                paths,
                ..
            } => Some((icon, grab, clamp, paths.len())),
            _ => None,
        } {
            let drag_icon = 56.0 * self.ui_k_at(pos.x * self.scale, pos.y * self.scale);
            let mut min = pos - *grab;
            // 盒子内拖拽：拖动图标夹在内容区，不能视觉上越出盒子。
            if let Some(content) = clamp {
                let (x, y) = model::clamp_icon_pos(min.x, min.y, drag_icon, rect_tuple(*content));
                min = egui::pos2(x, y);
            }
            let icon_rect = Rect::from_min_size(min, Vec2::splat(drag_icon));
            if let Some(id) = icon {
                ui.painter().image(
                    *id,
                    icon_rect,
                    Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::from_white_alpha(230),
                );
            } else {
                ui.painter().rect_filled(
                    icon_rect,
                    CornerRadius::same(10),
                    Color32::from_black_alpha(90),
                );
            }
            if count > 1 {
                // 整组拖动角标。
                let badge = Rect::from_min_size(
                    icon_rect.right_bottom() - egui::vec2(26.0, 20.0),
                    egui::vec2(26.0, 20.0),
                );
                ui.painter()
                    .rect_filled(badge, CornerRadius::same(6), self.pal.accent);
                ui.painter().text(
                    badge.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{count}"),
                    egui::FontId::proportional(11.0),
                    Color32::WHITE,
                );
            }
        }
    }

    fn handle_drag(&mut self) {
        let pointer = self.ctx.input(|i| i.pointer.interact_pos());
        let released = self.ctx.input(|i| i.pointer.any_released());
        let down = self.ctx.input(|i| i.pointer.any_down());
        let scale = self.scale.max(1.0);
        let vx = self.overlay.x as f32;
        let vy = self.overlay.y as f32;
        let vw = self.overlay.w as f32;
        let vh = self.overlay.h as f32;
        let drag = std::mem::replace(&mut self.drag, DragKind::None);
        if !down && !released && !matches!(drag, DragKind::None) {
            // 合并帧里 draw_overlay 可能在释放帧之后又置了一次 drag_started，
            // 之后既无按下也无释放：直接丢弃，避免永久 re-arm 后产生幻影落点。
            log::info!("[drag] 丢弃失效拖动状态（无按键且未释放）");
            return;
        }
        match drag {
            DragKind::None => {}
            DragKind::BoxMove { idx, grab } => {
                if let Some(pos) = pointer {
                    self.snap_guides.clear();
                    let (w, h) = self
                        .cfg
                        .boxes
                        .get(idx)
                        .map(|b| (b.w, b.h))
                        .unwrap_or((0.0, 0.0));
                    let mut nx = ((pos.x - grab.x) * scale).clamp(vx, (vx + vw - 80.0).max(vx));
                    let mut ny = ((pos.y - grab.y) * scale).clamp(vy, (vy + vh - 40.0).max(vy));
                    if self.cfg.desktop.snap_enabled && w > 0.0 && h > 0.0 {
                        let (cx, cy) = (nx + w * 0.5, ny + h * 0.5);
                        let work = self.monitor_work_at(cx, cy);
                        let others: Vec<(f32, f32, f32, f32)> = self
                            .cfg
                            .boxes
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| *i != idx)
                            .map(|(_, b)| (b.x, b.y, b.w, b.h))
                            .collect();
                        let out = model::snap_rect(
                            nx,
                            ny,
                            w,
                            h,
                            work,
                            &others,
                            self.cfg.desktop.snap_threshold as f32,
                        );
                        nx = out.x;
                        ny = out.y;
                        self.snap_guides = out.guides;
                    }
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        if !b.locked {
                            b.x = nx;
                            b.y = ny;
                        }
                    }
                }
                if released {
                    self.snap_guides.clear();
                    self.persist();
                } else {
                    self.drag = DragKind::BoxMove { idx, grab };
                }
            }
            DragKind::BoxResize {
                idx,
                edges,
                start,
                origin,
            } => {
                let (sx, sy, sw, sh) = start;
                let min_scale = self.monitor_scale_at(sx + sw * 0.5, sy + sh * 0.5);
                let work = self.monitor_work_at(sx + sw * 0.5, sy + sh * 0.5);
                if let Some(pos) = pointer {
                    let dx = (pos.x - origin.x) * scale;
                    let dy = (pos.y - origin.y) * scale;
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        if !b.locked {
                            let min_w = MIN_W * min_scale;
                            let min_h = MIN_H * min_scale;
                            let (mut x, mut y, mut w, mut h) = (sx, sy, sw, sh);
                            if edges & EDGE_LEFT != 0 {
                                // 对边（右边）固定：只移动左边并据此反算宽度。
                                let fixed_right = sx + sw;
                                x = (sx + dx).clamp(work.0, (fixed_right - min_w).max(work.0));
                                w = fixed_right - x;
                            }
                            if edges & EDGE_RIGHT != 0 {
                                w = (sw + dx).max(min_w).min((work.0 + work.2 - x).max(min_w));
                            }
                            if edges & EDGE_TOP != 0 {
                                let fixed_bottom = sy + sh;
                                y = (sy + dy).clamp(work.1, (fixed_bottom - min_h).max(work.1));
                                h = fixed_bottom - y;
                            }
                            if edges & EDGE_BOTTOM != 0 {
                                h = (sh + dy).max(min_h).min((work.1 + work.3 - y).max(min_h));
                            }
                            b.x = x;
                            b.y = y;
                            b.w = w;
                            b.h = h;
                        }
                    }
                }
                if released {
                    self.persist();
                } else {
                    self.drag = DragKind::BoxResize {
                        idx,
                        edges,
                        start,
                        origin,
                    };
                }
            }
            DragKind::Item {
                path,
                icon,
                grab,
                from_box,
                clamp,
            } => {
                if released {
                    // 跨帧拖动已经走到这里，清掉单帧兜底起点，避免释放被处理两次。
                    self.pending_item_press = None;
                    log::info!(
                        "[drag] 释放：{} box={:?} pointer={:?}",
                        path.display(),
                        from_box,
                        pointer
                    );
                    if let Some(pos) = pointer {
                        self.finish_item_drag(pos, &path, from_box);
                    }
                } else {
                    self.ctx.request_repaint();
                    self.drag = DragKind::Item {
                        path,
                        icon,
                        grab,
                        from_box,
                        clamp,
                    };
                }
            }
            DragKind::Items {
                paths,
                icon,
                grab,
                from_box,
                clamp,
                anchor,
            } => {
                if released {
                    self.pending_item_press = None;
                    log::info!(
                        "[drag] 整组释放：{} 项 box={:?} pointer={:?}",
                        paths.len(),
                        from_box,
                        pointer
                    );
                    if let Some(pos) = pointer {
                        self.finish_items_drag(pos, &paths, from_box, &anchor);
                    }
                } else {
                    self.ctx.request_repaint();
                    self.drag = DragKind::Items {
                        paths,
                        icon,
                        grab,
                        from_box,
                        clamp,
                        anchor,
                    };
                }
            }
        }
    }

    /// 整组拖放：盒子之间只移动引用；桌面整组落到格子时整体平移。
    fn finish_items_drag(
        &mut self,
        pos: Pos2,
        paths: &[PathBuf],
        from: Option<usize>,
        anchor: &Path,
    ) {
        let scale = self.scale.max(1.0);
        let px = pos.x * scale;
        let py = pos.y * scale;
        let target = self
            .cfg
            .boxes
            .iter()
            .position(|b| model::point_in_rect(px, py, self.box_content_px(b)));
        match (target, from) {
            (Some(target), Some(from)) if from == target => {
                // 同一盒子内释放：盒内是网格/列表布局，不是自由坐标，按落点重排显示顺序。
                self.reorder_box_items_block(target, pos, paths);
            }
            (Some(target), from) if from != Some(target) => {
                // 系统项是合成项，只能留在桌面；跳过并给出可见提示。
                let movable: Vec<PathBuf> = paths
                    .iter()
                    .filter(|p| !ItemMeta::is_system_path(p))
                    .cloned()
                    .collect();
                let skipped = paths.len() - movable.len();
                for path in &movable {
                    self.clear_manual_override(path);
                }
                let n = self.add_members(target, &movable);
                if skipped > 0 {
                    self.toast_msg(format!(
                        "已移动 {n} 项到目标盒子；{skipped} 个系统项未移动（文件保持原位）"
                    ));
                    log::info!("[drag] 整组进入盒子：移动 {n} 项，跳过 {skipped} 个系统项");
                } else if n > 0 {
                    self.toast_msg(format!("已移动 {n} 项到目标盒子（文件保持原位）"));
                }
            }
            (None, Some(_)) => {
                self.toast_msg("已取消：盒子内的图标不能拖出盒子边界");
            }
            // 桌面整组拖动：保持相对位置整体平移。
            (None, None) => self.move_desktop_group(pos, paths, anchor),
            _ => {}
        }
    }

    fn finish_item_drag(&mut self, pos: Pos2, path: &Path, from: Option<usize>) {
        let scale = self.scale.max(1.0);
        let px = pos.x * scale;
        let py = pos.y * scale;
        // 目标只认盒子内容区：落在标题栏或盒子外都不算命中。
        let target = self
            .cfg
            .boxes
            .iter()
            .position(|b| model::point_in_rect(px, py, self.box_content_px(b)));
        match (target, from) {
            (Some(target), Some(from)) if from == target => {
                self.reorder_box_item(target, pos, path);
            }
            (Some(target), from) if from != Some(target) => {
                // 系统项（此电脑/回收站）没有磁盘路径：放进盒子只会变成失效引用，
                // 这里显式拒绝并说明原因，而不是静默忽略。
                if ItemMeta::is_system_path(path) {
                    log::info!("系统项拖入盒子被拒绝：{} box={target}", path.display());
                    self.toast_msg("「此电脑」「回收站」是系统项，不能放进盒子；可拖到桌面空格");
                    return;
                }
                // 用户手动放进盒子 → 取消该路径的手动覆盖；只改引用，文件不动。
                self.clear_manual_override(path);
                if let Some(from) = from {
                    self.remove_member(from, path);
                }
                if self.add_member(target, path) {
                    self.persist();
                    self.toast_msg("已加入盒子（文件保持原位）");
                }
            }
            (None, Some(_)) => {
                // 盒子内的图标禁止拖出盒子：越界释放视为取消，引用保持不变。
                // 需要移出时用右键菜单「移出盒子」。
                self.toast_msg("已取消：盒子内的图标不能拖出盒子边界");
            }
            (None, None) => {
                // 桌面图标落到桌面上：按棋盘占位固定；已占格按配置交换或生成新盒子。
                self.place_desktop_item(pos, path);
            }
            _ => {}
        }
    }

    /// 当前是否允许从图标开始拖动（与预览/菜单/设置互斥）。
    fn item_drag_ui_ready(&self) -> bool {
        self.overlay_visible
            && !self.text_editing()
            && !self.settings_open
            && matches!(self.modal, Modal::None)
            && self.group_menu.is_none()
            && !crate::platform::shell_menu::menu_open()
            && self.preview.is_none()
    }

    /// 记录本帧主键按下的位置：若落在桌面/盒子的图标上，留作单帧拖动兜底。
    fn note_item_press(&mut self) {
        let mut pressed: Option<Pos2> = None;
        self.ctx.input(|i| {
            for ev in &i.events {
                if let egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    ..
                } = ev
                {
                    pressed = Some(*pos);
                }
            }
        });
        let Some(pos) = pressed else {
            return;
        };
        self.pending_item_press = if self.item_drag_ui_ready() {
            self.hit_item_icon_at(pos)
        } else {
            None
        };
        if let Some(p) = &self.pending_item_press {
            log::info!(
                "[drag] 按下图标：{} box={:?} @{:.1},{:.1}",
                p.path.display(),
                p.from_box,
                p.pos.x,
                p.pos.y
            );
        }
    }

    /// 释放帧兜底：egui 把 press→move→release 合并成一帧时不会置 drag_started，
    /// 这里按「按下的图标 + 释放位置 + 位移超过点击阈值」补做一次落点判定。
    fn maybe_finish_pending_press(&mut self) {
        let Some(press) = self.pending_item_press.take() else {
            return;
        };
        let (released, pos) = self.ctx.input(|i| {
            (
                i.pointer.any_released(),
                i.pointer.interact_pos().or(i.pointer.latest_pos()),
            )
        });
        if !released {
            // 仍在按住：保留起点，等释放帧。
            self.pending_item_press = Some(press);
            return;
        }
        if !matches!(self.drag, DragKind::None) {
            return;
        }
        let Some(pos) = pos else {
            return;
        };
        // 位移超过点击容差即视为拖动：egui 把 press→move→release 合并到同一帧时
        // 会按「点击」处理（could_any_button_be_click 仍为真），单靠它无法区分
        // 快速拖动，因此这里只按实际位移判定。
        if pos.distance(press.pos) < 6.0 || !self.item_drag_ui_ready() {
            return;
        }
        log::info!(
            "[drag] 单帧拖动兜底：{} box={:?} 释放 @{:.1},{:.1}",
            press.path.display(),
            press.from_box,
            pos.x,
            pos.y
        );
        self.finish_item_drag(pos, &press.path, press.from_box);
    }

    /// 指针下的图标（桌面或盒子内）：只认图标矩形，名称区域留给文本选择。
    fn hit_item_icon_at(&self, pos: Pos2) -> Option<PendingItemPress> {
        let pp = self.scale.max(1.0);
        let (cells, item_cell) = self.desktop_layout();
        if let Some(cell_idx) = cell_index_at(&cells, pos) {
            if let Some(item_idx) = item_cell.iter().position(|c| *c == Some(cell_idx)) {
                if let (Some(c), Some(item)) =
                    (cells.get(cell_idx), self.desktop_items.get(item_idx))
                {
                    if !item.is_system() {
                        let cell = desktop_cell_rect(c);
                        if item_icon_rect(cell, c.icon, pp).contains(pos) {
                            return Some(PendingItemPress {
                                pos,
                                path: item.path.clone(),
                                from_box: None,
                            });
                        }
                    }
                }
            }
        }
        let px = pos.x * pp;
        let py = pos.y * pp;
        for (idx, b) in self.cfg.boxes.iter().enumerate() {
            if !model::point_in_rect(px, py, self.box_content_px(b)) {
                continue;
            }
            let filtered = self.box_filtered_indices(idx);
            for (display, &real) in filtered.iter().enumerate() {
                let cell = self.box_cell_rect(idx, display);
                if !cell.contains(pos) {
                    continue;
                }
                if self.box_item_icon_rect(idx, display).contains(pos) {
                    if let Some(item) = self.boxes[idx].items.get(real) {
                        return Some(PendingItemPress {
                            pos,
                            path: item.path.clone(),
                            from_box: Some(idx),
                        });
                    }
                }
            }
        }
        None
    }

    /// 盒子内某显示项的图标矩形（与 draw_box_items 同一套尺寸换算）。
    fn box_item_icon_rect(&self, idx: usize, display: usize) -> Rect {
        let rt = &self.boxes[idx];
        let cell = self.box_cell_rect(idx, display);
        let scale = self.scale.max(1.0);
        match rt.nav_view {
            BoxView::List => {
                let row_h = rt.nav_row_h;
                let icon = (row_h
                    - 10.0
                        * self.ui_k_at(
                            self.cfg.boxes[idx].x + self.cfg.boxes[idx].w * 0.5,
                            self.cfg.boxes[idx].y + self.cfg.boxes[idx].h * 0.5,
                        ))
                .max(
                    12.0 * self.ui_k_at(
                        self.cfg.boxes[idx].x + self.cfg.boxes[idx].w * 0.5,
                        self.cfg.boxes[idx].y + self.cfg.boxes[idx].h * 0.5,
                    ),
                );
                pixel_aligned(
                    Rect::from_center_size(
                        egui::pos2(cell.left() + row_h * 0.5, cell.center().y),
                        Vec2::splat(icon),
                    ),
                    scale,
                )
            }
            view => {
                let s = self.monitor_scale_at(
                    self.cfg.boxes[idx].x + self.cfg.boxes[idx].w * 0.5,
                    self.cfg.boxes[idx].y + self.cfg.boxes[idx].h * 0.5,
                );
                let k = (s / scale).max(0.1);
                let icon = self.icon_px_on(s) as f32 / scale;
                let icon = if view == BoxView::Thumb {
                    (icon * 2.0).max(76.0 * k)
                } else {
                    icon
                };
                item_icon_rect(cell, icon, scale)
            }
        }
    }

    /// 桌面网格当前布局（逻辑点）：绘制、命中、落点判定共用同一套格子几何。
    ///
    /// 返回每屏完整棋盘；自动紧凑排列只是「items 依次占前 N 个格子」。
    fn desktop_cell_layout(&self) -> Vec<model::DesktopCell> {
        if self.monitors.is_empty() || self.desktop_items.is_empty() {
            return Vec::new();
        }
        let pp = self.scale.max(1.0);
        let primary = self.primary_scale();
        // 配置里的图标/间距以主屏物理像素为基准，先换算成 DIP，再按各屏 DPI 还原。
        let icon_dip = self.cfg.desktop.icon_size as f32 / primary;
        let gap_dip = self.cfg.desktop.spacing as f32 / primary;
        let areas: Vec<model::MonitorArea> = self
            .monitors
            .iter()
            .map(|m| model::MonitorArea {
                x: (m.work.x - self.overlay.x) as f32,
                y: (m.work.y - self.overlay.y) as f32,
                w: m.work.w as f32,
                h: m.work.h as f32,
                scale: m.scale,
            })
            .collect();
        model::desktop_board_cells(
            &areas,
            pp,
            icon_dip,
            gap_dip,
            self.cfg.desktop.label_gap,
            self.cfg.desktop.show_labels,
            18.0,
            self.desktop_items.len(),
        )
    }

    /// 当前桌面布局：格子 + 「item→cell」映射。
    ///
    /// 棋盘模式下位置唯一来源是 `desktop.slots`；自动模式按 `desktop_items` 顺序占前 N 格。
    fn desktop_layout(&self) -> (Vec<model::DesktopCell>, Vec<Option<usize>>) {
        let cells = self.desktop_cell_layout();
        let item_cell = if self.cfg.desktop.board_placement {
            model::assign_board_slots(&self.desktop_items, &cells, &self.cfg.desktop.slots).0
        } else {
            (0..self.desktop_items.len())
                .map(|i| cells.get(i).map(|_| i))
                .collect()
        };
        (cells, item_cell)
    }

    /// 棋盘占位的读写前提：显示器几何与桌面首次扫描都已完成。
    ///
    /// 启动早期 `monitors`/`scale` 还是未校正值，且 `desktop_items` 只有两个系统项；
    /// 此时读写 `slots` 会把旧顺序快照成 2 格、并归一化掉尚未扫到的项。
    fn board_layout_ready(&self) -> bool {
        self.pp_synced
            && !self.monitors.is_empty()
            && self.desktop_user_ready
            && self.desktop_public_ready
    }

    /// 棋盘模式同步：旧槽位优先，越界/冲突/新项填第一个空格，并把归一化结果写回配置。
    fn sync_desktop_board(&mut self) {
        if !self.cfg.desktop.board_placement {
            if !self.cfg.desktop.slots.is_empty() {
                self.cfg.desktop.slots.clear();
                self.persist();
            }
            return;
        }
        let cells = self.desktop_cell_layout();
        if cells.is_empty() {
            return;
        }
        let (_, normalized) =
            model::assign_board_slots(&self.desktop_items, &cells, &self.cfg.desktop.slots);
        if !slots_equivalent(&self.cfg.desktop.slots, &normalized) {
            let moved = normalized
                .iter()
                .filter(|n| {
                    self.cfg
                        .desktop
                        .slots
                        .iter()
                        .find(|s| s.key == n.key)
                        .is_none_or(|s| (s.monitor, s.col, s.row) != (n.monitor, n.col, n.row))
                })
                .count();
            log::info!(
                "桌面棋盘同步：{} 项，迁移/新增 {moved} 项",
                normalized.len()
            );
            self.cfg.desktop.slots = normalized;
            self.persist();
        }
    }

    /// 把当前紧凑排列快照成棋盘占位（启用棋盘/首次拖动时调用）。
    ///
    /// 几何或扫描未就绪时拒绝快照并返回 `false`，由调用方延后重试：
    /// 否则会把不完整的图标集合固化成槽位。
    fn snapshot_current_layout(&mut self) -> bool {
        if !self.board_layout_ready() {
            log::info!("桌面占位：扫描或几何尚未就绪，延后快照");
            return false;
        }
        let cells = self.desktop_cell_layout();
        if cells.is_empty() {
            log::info!("桌面占位：网格为空，延后快照");
            return false;
        }
        let mut slots = Vec::with_capacity(self.desktop_items.len());
        for (i, item) in self.desktop_items.iter().enumerate() {
            if let Some(c) = cells.get(i) {
                slots.push(DesktopSlot {
                    key: model::path_key(&item.path),
                    monitor: c.monitor,
                    col: c.col,
                    row: c.row,
                });
            }
        }
        self.cfg.desktop.slots = slots;
        self.cfg.desktop.board_placement = true;
        self.persist();
        true
    }

    /// 「恢复自动排列」：清空占位，回到按当前排序的紧凑网格。
    fn restore_auto_arrange(&mut self) {
        if !self.cfg.desktop.board_placement && self.cfg.desktop.slots.is_empty() {
            self.toast_msg("当前已是自动排列");
            return;
        }
        self.cfg.desktop.board_placement = false;
        self.cfg.desktop.slots.clear();
        self.cfg.desktop.manual_order.clear();
        self.rebuild_desktop();
        self.persist();
        self.toast_msg("已恢复自动排列：图标按当前排序紧凑排列");
    }

    /// 重新启用棋盘摆放（明确入口，避免用户以为功能丢失）。
    fn enable_board_placement(&mut self) {
        if self.cfg.desktop.board_placement {
            self.toast_msg("棋盘摆放已启用：直接拖到任意格即可固定位置");
            return;
        }
        if !self.snapshot_current_layout() {
            self.toast_msg("桌面尚未就绪，请稍后再启用棋盘摆放");
            return;
        }
        self.toast_msg("已启用棋盘摆放：拖到任意格即可固定位置");
    }

    /// 桌面拖放落格：空格固定占用；已占格按配置交换或生成新盒子。
    fn place_desktop_item(&mut self, pos: Pos2, path: &Path) {
        if !self.cfg.desktop.board_placement {
            if !self.snapshot_current_layout() {
                self.toast_msg("桌面尚未就绪，已取消移动");
                return;
            }
        } else {
            self.sync_desktop_board();
        }
        let cells = self.desktop_cell_layout();
        let Some(target) = cell_index_exact_at(&cells, pos) else {
            self.toast_msg("未落在桌面格子上，已取消移动");
            return;
        };
        let (map, _) =
            model::assign_board_slots(&self.desktop_items, &cells, &self.cfg.desktop.slots);
        let Some(from_item) = self.desktop_items.iter().position(|i| i.path == *path) else {
            return;
        };
        let Some(from_cell) = map.get(from_item).copied().flatten() else {
            return;
        };
        if from_cell == target {
            return;
        }
        let occupant = map
            .iter()
            .enumerate()
            .find(|(i, c)| *i != from_item && **c == Some(target))
            .map(|(i, _)| i);
        let Some(occ_item) = occupant else {
            let item_path = self.desktop_items[from_item].path.clone();
            self.set_desktop_slot(&item_path, &cells[target]);
            self.persist();
            log::info!(
                "桌面落格：{} → monitor={} col={} row={}",
                path.display(),
                cells[target].monitor,
                cells[target].col,
                cells[target].row
            );
            self.toast_msg("已移动到空格");
            return;
        };
        let from_key = model::path_key(&self.desktop_items[from_item].path);
        let occ_key = model::path_key(&self.desktop_items[occ_item].path);
        match self.cfg.desktop.drop_on_occupied {
            OccupiedDropMode::Swap => {
                if model::swap_desktop_slots(&mut self.cfg.desktop.slots, &from_key, &occ_key) {
                    let from_name = self.desktop_items[from_item].name.clone();
                    let occ_name = self.desktop_items[occ_item].name.clone();
                    self.persist();
                    log::info!("桌面落格交换：{from_name} ↔ {occ_name}");
                    self.toast_msg(format!("已交换「{from_name}」与「{occ_name}」的位置"));
                }
            }
            OccupiedDropMode::NewBox => {
                // 系统项不能被收纳进盒子（无磁盘路径）；落点占位者若是系统项则排除，
                // 只收纳被拖项，保持在原格不动。
                if self.desktop_items[from_item].is_system() {
                    log::info!("系统项落格建盒被拒绝：{}", path.display());
                    self.toast_msg(
                        "「此电脑」「回收站」是系统项，不能收进盒子；可拖到空格或改用交换位置",
                    );
                    return;
                }
                let cell = cells[target];
                let mut members = vec![self.desktop_items[from_item].path.clone()];
                if !self.desktop_items[occ_item].is_system() {
                    members.push(self.desktop_items[occ_item].path.clone());
                }
                self.create_box_with_members_at(cell, &members);
            }
        }
    }

    fn set_desktop_slot(&mut self, path: &Path, cell: &model::DesktopCell) {
        let key = model::path_key(path);
        if let Some(slot) = self.cfg.desktop.slots.iter_mut().find(|s| s.key == key) {
            slot.monitor = cell.monitor;
            slot.col = cell.col;
            slot.row = cell.row;
        } else {
            self.cfg.desktop.slots.push(DesktopSlot {
                key,
                monitor: cell.monitor,
                col: cell.col,
                row: cell.row,
            });
        }
        self.cfg.desktop.board_placement = true;
    }

    /// 在落点格中心新建盒子并收纳给定成员（文件只移动引用）。
    fn create_box_with_members_at(&mut self, cell: model::DesktopCell, members: &[PathBuf]) {
        let scale = self.scale.max(1.0);
        let cx = self.overlay.x as f32 + (cell.x + cell.w * 0.5) * scale;
        let cy = self.overlay.y as f32 + (cell.y + cell.h * 0.5) * scale;
        let idx = self.create_box_kind_at(BoxKind::Normal, None, Some((cx, cy)));
        for path in members {
            self.clear_manual_override(path);
        }
        let added = self.add_members(idx, members);
        log::info!("桌面落格生成盒子：{added} 项 → box={idx}");
        self.toast_msg(format!("已在落点新建盒子并收纳 {added} 项"));
    }

    /// 指针在盒子内的显示下标：命中格子优先，间隙取最近格子中心（半格阈值）。
    fn box_drop_display_index(&self, idx: usize, pos: Pos2) -> Option<usize> {
        let n = self.box_filtered_indices(idx).len();
        let list = self.boxes.get(idx)?.nav_list;
        if n == 0 || !list.contains(pos) {
            return None;
        }
        let mut best: Option<(f32, usize)> = None;
        for display in 0..n {
            let cell = self.box_cell_rect(idx, display).intersect(list);
            if cell.contains(pos) {
                return Some(display);
            }
            let d = cell.center().distance(pos);
            match best {
                Some((best_d, _)) if d >= best_d => {}
                _ => best = Some((d, display)),
            }
        }
        let rt = &self.boxes[idx];
        let max_d = match rt.nav_view {
            BoxView::List => rt.nav_row_h * 0.5,
            _ => rt.nav_cell.x.max(rt.nav_cell.y) * 0.5,
        };
        best.filter(|(d, _)| *d <= max_d).map(|(_, i)| i)
    }

    /// 盒子内单项拖动：在可见序列内重排，写回真实 items 与 manual_order。
    fn reorder_box_item(&mut self, idx: usize, pos: Pos2, path: &Path) {
        if self
            .cfg
            .boxes
            .get(idx)
            .is_some_and(|b| b.kind == BoxKind::Files)
        {
            log::info!("盒子拖放：Files 盒按类型分组，不做手动重排（box={idx}）");
            return;
        }
        let filtered = self.box_filtered_indices(idx);
        let Some(from_disp) = filtered
            .iter()
            .position(|&r| self.boxes[idx].items[r].path == *path)
        else {
            return;
        };
        let Some(target_disp) = self.box_drop_display_index(idx, pos) else {
            return;
        };
        if target_disp == from_disp {
            return;
        }
        let mut display_items: Vec<ItemMeta> = filtered
            .iter()
            .map(|&r| self.boxes[idx].items[r].clone())
            .collect();
        let moved = display_items.remove(from_disp);
        let insert = target_disp.min(display_items.len());
        display_items.insert(insert, moved);
        for (slot, &real) in filtered.iter().enumerate() {
            self.boxes[idx].items[real] = display_items[slot].clone();
        }
        self.cfg.boxes[idx].manual_order = model::manual_order_of(&self.boxes[idx].items);
        self.persist();
        log::info!(
            "盒子拖放：{} 从显示位 {from_disp} 移到 {insert}（box={idx} 手动顺序已保存）",
            path.display()
        );
    }

    /// 盒子内多选整块拖动：保持组内相对顺序，整体插到目标显示位。
    fn reorder_box_items_block(&mut self, idx: usize, pos: Pos2, paths: &[PathBuf]) {
        if self
            .cfg
            .boxes
            .get(idx)
            .is_some_and(|b| b.kind == BoxKind::Files)
        {
            log::info!("盒子整组拖放：Files 盒按类型分组，不做手动重排（box={idx}）");
            return;
        }
        let Some(target_disp) = self.box_drop_display_index(idx, pos) else {
            return;
        };
        let filtered = self.box_filtered_indices(idx);
        let mut display_items: Vec<ItemMeta> = filtered
            .iter()
            .map(|&r| self.boxes[idx].items[r].clone())
            .collect();
        let mut moved: Vec<ItemMeta> = Vec::new();
        let mut removed_before = 0usize;
        let mut i = 0usize;
        while i < display_items.len() {
            if paths.contains(&display_items[i].path) {
                if i < target_disp {
                    removed_before += 1;
                }
                moved.push(display_items.remove(i));
            } else {
                i += 1;
            }
        }
        if moved.is_empty() {
            return;
        }
        let insert = target_disp
            .saturating_sub(removed_before)
            .min(display_items.len());
        for (off, item) in moved.into_iter().enumerate() {
            display_items.insert(insert + off, item);
        }
        for (slot, &real) in filtered.iter().enumerate() {
            self.boxes[idx].items[real] = display_items[slot].clone();
        }
        self.cfg.boxes[idx].manual_order = model::manual_order_of(&self.boxes[idx].items);
        self.persist();
        log::info!(
            "盒子整组拖放：{n} 项移到显示位 {insert}（box={idx} 手动顺序已保存）",
            n = paths.len()
        );
    }

    /// 盒子内容区（物理像素）：标题栏以下、左右/底部内边距以内，与 `draw_box` 的 `body` 一致。
    ///
    /// 命中测试、拖拽落点、拖动夹取都以它为准，避免在标题栏/圆角外触发图标操作。
    fn box_content_px(&self, b: &BoxConfig) -> model::RectF {
        let scale = self.scale.max(1.0);
        let k = self.ui_k_at(b.x + b.w * 0.5, b.y + b.h * 0.5);
        let pad = PAD * k * scale;
        let header = HEADER_H * k * scale;
        (
            b.x + pad,
            b.y + header,
            (b.w - pad * 2.0).max(0.0),
            (b.h - header - pad).max(0.0),
        )
    }
}

impl DeskApp {
    /// 独立设置窗口：托盘菜单与盒子菜单都能打开。
    fn show_settings(&mut self) {
        if !self.settings_open {
            return;
        }
        let ctx = self.ctx.clone();
        let viewport_id = egui::ViewportId::from_hash_of("deskkeeper-settings");
        let pal = self.pal;
        let snapshot = self.cfg.clone();
        let mut actions: Vec<UiAction> = Vec::new();
        let mut close_by_viewport = false;
        let request_focus = self.settings_needs_focus;
        let was_slider_held = self.icon_slider_held;
        let builder = egui::ViewportBuilder::default()
            .with_title("DeskKeeper · 设置")
            .with_inner_size([SETTINGS_W, SETTINGS_H])
            .with_min_inner_size([520.0, 460.0])
            .with_position(self.settings_center())
            .with_resizable(true)
            .with_always_on_top();
        {
            let cfg = &mut self.cfg;
            let icon_slider_held = &mut self.icon_slider_held;
            ctx.show_viewport_immediate(viewport_id, builder, |ui, _class| {
                let full = ui.max_rect();
                ui.painter()
                    .rect_filled(full, egui::CornerRadius::ZERO, pal.window_bg);
                let view_h = ui
                    .ctx()
                    .input(|i| i.viewport().inner_rect.map(|r| r.height()))
                    .unwrap_or(SETTINGS_H);
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(20, 16))
                    .show(ui, |ui| {
                        ui.label(ui::theme::popup_title("设置", &pal));
                        ui.add_space(2.0);
                        ui.label(ui::theme::popup_hint(
                            "修改即时保存；所有弹窗共用同一套圆角、留白与文字层级",
                            &pal,
                        ));
                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .max_height((view_h - 92.0).max(180.0))
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui::settings::show(ui, cfg, &pal, &mut actions, icon_slider_held);
                            });
                    });
                if ui.input(|i| i.viewport().close_requested()) {
                    close_by_viewport = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close_by_viewport = true;
                }
            });
        }
        if request_focus {
            self.settings_needs_focus = false;
            self.ctx
                .send_viewport_cmd_to(viewport_id, egui::ViewportCommand::Focus);
        }
        self.apply_actions(actions);
        if self.cfg != snapshot {
            let old = snapshot;
            self.on_config_changed(&old);
        }
        // 图标尺寸滑杆松开后主动补一帧，让叠加层用最终尺寸发起纹理解码。
        if was_slider_held && !self.icon_slider_held {
            self.ctx.request_repaint();
        }
        if close_by_viewport {
            self.settings_open = false;
            self.icon_slider_held = false;
            log::info!("设置窗口已关闭");
        }
    }

    fn show_modals(&mut self) {
        let ctx = self.ctx.clone();
        match std::mem::replace(&mut self.modal, Modal::None) {
            Modal::None => {}
            Modal::Confirmation(kind) => {
                let (title, detail, danger) = match kind {
                    BoxConfirm::DeleteTodo { idx } => {
                        let name = self
                            .cfg
                            .boxes
                            .get(idx)
                            .map(|b| b.name.clone())
                            .unwrap_or_default();
                        let count = self.cfg.boxes.get(idx).map(|b| b.todos.len()).unwrap_or(0);
                        (
                            "删除待办盒子",
                            format!(
                                "将删除「{name}」并清空其中 {count} 条事项，此操作不可撤销。\n关闭只是隐藏、可在设置页重开；删除不会保留任何内容。"
                            ),
                            true,
                        )
                    }
                    BoxConfirm::Dissolve { idx } => {
                        let name = self
                            .cfg
                            .boxes
                            .get(idx)
                            .map(|b| b.name.clone())
                            .unwrap_or_default();
                        let count = self
                            .cfg
                            .boxes
                            .get(idx)
                            .map(|b| b.members.len())
                            .unwrap_or(0);
                        (
                            "解散盒子",
                            format!(
                                "将解散「{name}」并删除 {count} 条成员引用。\n不会删除任何真实文件，这些图标会立即回到桌面。"
                            ),
                            false,
                        )
                    }
                };
                let pal = self.pal;
                let dark = self.dark;
                let mut decision: Option<bool> = None;
                theme::popup_window(Id::unique("modal-confirm"), title, &pal, dark).show(
                    &ctx,
                    |ui| {
                        ui.label(theme::popup_title(title, &pal));
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(10.0);
                        // 文案异常长时在正文内部滚动，标题与按钮行始终可见、不被裁切。
                        let body_max = (ctx.content_rect().height() * 0.6).max(120.0);
                        egui::ScrollArea::vertical()
                            .max_height(body_max)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.add(egui::Label::new(theme::popup_body(detail, &pal)).wrap());
                            });
                        ui.add_space(6.0);
                        ui.label(theme::popup_hint(
                            if danger {
                                "确认后立即生效，且无法恢复。"
                            } else {
                                "不会影响磁盘上的任何文件。"
                            },
                            &pal,
                        ));
                        ui.add_space(14.0);
                        theme::popup_buttons(ui, |ui| {
                            let ok = if danger {
                                theme::primary_button(ui, "确认删除", true, &pal)
                            } else {
                                theme::primary_button(ui, "确认解散", false, &pal)
                            }
                            .clicked();
                            ui.add_space(8.0);
                            let cancel = theme::ghost_button(ui, "取消", &pal).clicked();
                            if ok {
                                decision = Some(true);
                            }
                            if cancel {
                                decision = Some(false);
                            }
                        });
                    },
                );
                match decision {
                    Some(true) => match kind {
                        BoxConfirm::DeleteTodo { idx } => self.delete_todo_box(idx),
                        BoxConfirm::Dissolve { idx } => self.delete_box(idx),
                    },
                    Some(false) => {}
                    None => self.modal = Modal::Confirmation(kind),
                }
            }
            Modal::Color {
                idx,
                mut color,
                mut alpha,
            } => {
                let pal = self.pal;
                let dark = self.dark;
                let mut apply = false;
                let mut cancel = false;
                theme::popup_window(Id::unique("modal-color"), "盒子配色", &pal, dark).show(
                    &ctx,
                    |ui| {
                        ui.label(theme::popup_title("盒子配色", &pal));
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(10.0);
                        ui.label(theme::popup_body("底色", &pal));
                        ui.add_space(6.0);
                        ui.horizontal_wrapped(|ui| {
                            for preset in ui::BOX_PRESETS {
                                let selected = preset == color;
                                let (rect, resp) =
                                    ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
                                let fill = Color32::from_rgb(preset[0], preset[1], preset[2]);
                                ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
                                let ring = if selected {
                                    Stroke::new(2.0, Color32::WHITE)
                                } else if resp.hovered() {
                                    Stroke::new(2.0, pal.accent)
                                } else {
                                    Stroke::new(1.0, pal.border)
                                };
                                ui.painter().rect_stroke(
                                    rect,
                                    CornerRadius::same(8),
                                    ring,
                                    egui::StrokeKind::Inside,
                                );
                                if resp.clicked() {
                                    color = preset;
                                }
                            }
                        });
                        ui.add_space(4.0);
                        ui.label(theme::popup_hint(
                            "不透明度越低，弹窗与其上文字越需要更实的底板。",
                            &pal,
                        ));
                        ui.add_space(8.0);
                        ui.add(egui::Slider::new(&mut alpha, 10..=235).text("不透明度"));
                        ui.add_space(2.0);
                        ui.label(theme::popup_hint(
                            format!("当前：约 {:.0}% 不透明", alpha as f32 / 255.0 * 100.0),
                            &pal,
                        ));
                        ui.add_space(14.0);
                        theme::popup_buttons(ui, |ui| {
                            if theme::primary_button(ui, "应用", false, &pal).clicked() {
                                apply = true;
                            }
                            ui.add_space(8.0);
                            if theme::ghost_button(ui, "取消", &pal).clicked() {
                                cancel = true;
                            }
                        });
                    },
                );
                if apply {
                    if let Some(b) = self.cfg.boxes.get_mut(idx) {
                        b.color = color;
                        b.alpha = alpha;
                    }
                    self.persist();
                }
                if !apply && !cancel {
                    self.modal = Modal::Color { idx, color, alpha };
                }
            }
        }
    }

    fn draw_status(&mut self, ui: &mut egui::Ui) {
        let Some((text, until)) = self.toast.clone() else {
            return;
        };
        let now = ui.input(|i| i.time);
        let until = if until.is_finite() { until } else { now + 4.0 };
        self.toast = Some((text.clone(), until));
        let full = ui.max_rect();
        let pal = self.pal;
        let dark = self.dark;
        let width = (text.chars().count() as f32 * 12.5 + 36.0).clamp(140.0, 560.0);
        let rect = Rect::from_center_size(
            egui::pos2(full.center().x, full.bottom() - 46.0),
            egui::vec2(width, 34.0),
        );
        // 与弹窗同一套语言：10px 圆角 + 1px 边框 + 轻投影，始终比半透明盒子更实。
        ui.painter().rect_filled(
            rect.translate(egui::vec2(0.0, 3.0)),
            CornerRadius::same(theme::MENU_RADIUS),
            Color32::from_black_alpha(if dark { 90 } else { 40 }),
        );
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(theme::MENU_RADIUS),
            if dark {
                Color32::from_rgba_unmultiplied(30, 33, 41, 250)
            } else {
                Color32::from_rgb(252, 253, 255)
            },
        );
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(theme::MENU_RADIUS),
            Stroke::new(1.0, pal.border),
            egui::StrokeKind::Inside,
        );
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            FontId::proportional(12.5),
            pal.text,
        );
        if now >= until {
            self.toast = None;
        } else {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(220));
        }
    }

    fn tick(&mut self, ui: &mut egui::Ui) {
        let frame_start = std::time::Instant::now();
        if !self.pp_synced {
            self.pp_synced = true;
            let pp = ui.ctx().pixels_per_point().max(1.0);
            let sys = desktop::system_scale().max(1.0);
            log::info!(
                "DPI 一致性检查：egui_pp={pp:.2} system_scale={sys:.2} 工作区 {}x{}",
                self.overlay.w,
                self.overlay.h
            );
            // 首帧再枚举一次：此时窗口已创建，混合 DPI 下各屏缩放才稳定。
            self.refresh_monitors();
            self.log_monitors();
            if (pp - sys).abs() > 0.01 {
                log::warn!(
                    "系统 DPI 与窗口缩放不一致，改用 egui pixels_per_point，避免盒子偏移/尺寸失配"
                );
                self.scale = pp;
                self.overlay = desktop::work_area();
                self.reposition_overlay();
                self.region_retry = 8;
                self.clamp_boxes_to_work_area();
            } else {
                self.scale = pp;
            }
        }
        self.drain_events();
        let now = ui.input(|i| i.time);
        // 图标尺寸变更防抖：滑杆停稳 30ms 后才切换纹理请求尺寸，期间旧纹理拉伸占位。
        // 滑杆仍被按住时尺寸会继续步进，暂不解码；松开后才解码最终尺寸，避免拖动途中
        // 每个档位都全量重解码（设置页在独立 viewport，因此用设置窗口写入的按住状态）。
        if !self.icon_slider_held && self.icon_resize_at.is_some_and(|deadline| now >= deadline) {
            self.icon_resize_at = None;
            if self.tex_icon_size != self.cfg.desktop.icon_size {
                self.tex_icon_size = self.cfg.desktop.icon_size;
                self.icon_prune_at = Some(now + ICON_PRUNE_DELAY);
                self.ctx
                    .request_repaint_after(std::time::Duration::from_secs_f64(
                        ICON_PRUNE_DELAY + 0.05,
                    ));
                log::debug!(
                    "图标纹理尺寸切换：配置 {}px → 各屏物理尺寸 {:?}",
                    self.tex_icon_size,
                    self.tex_icon_sizes()
                );
            }
        }
        // 新尺寸纹理已有 1s 余量，清理旧尺寸，限制显存增长。
        if self.icon_prune_at.is_some_and(|deadline| now >= deadline) {
            self.icon_prune_at = None;
            let sizes = self.tex_icon_sizes();
            self.textures.retain_icon_sizes(&sizes);
            log::debug!("已清理旧图标纹理，仅保留尺寸 {:?}", sizes);
        }
        if self.overlay_visible {
            if self.zorder_pending && desktop::place_overlay_above_desktop() {
                self.zorder_pending = false;
            }
            // 显示器变化后窗口位置命令异步生效，等窗口到位再裁剪，避免用旧原点写错区域。
            if self.region_retry > 0 {
                if desktop::apply_work_region() || self.region_retry == 1 {
                    self.region_retry = 0;
                } else {
                    self.region_retry -= 1;
                }
            }
        } else {
            self.zorder_pending = false;
            self.region_retry = 0;
        }
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            crate::startup::step("首帧已呈现");
            log::info!("启动→首帧 {} ms", self.started_at.elapsed().as_millis());
        }
        if log::log_enabled!(log::Level::Debug) {
            self.frame_marks = match self.frame_marks {
                Some((start, n)) if now - start < 1.0 => Some((start, n + 1)),
                Some((_, n)) => {
                    let (avg, max) = self.frame_cost_stats();
                    let split = match self.frame_split {
                        Some((_, n, o, m, s)) if n > 0 => {
                            let n = f64::from(n);
                            format!(
                                "，分项 叠加层 {:.2}/模态 {:.2}/设置 {:.2} ms",
                                o / n,
                                m / n,
                                s / n
                            )
                        }
                        _ => String::new(),
                    };
                    let causes: Vec<String> = ui
                        .ctx()
                        .repaint_causes()
                        .into_iter()
                        .take(3)
                        .map(|c| c.to_string())
                        .collect();
                    log::debug!(
                        "叠加层重绘 {n} 帧/秒，CPU 帧耗时 平均 {avg:.2} ms / 最大 {max:.2} ms{split}，盒子 {}，桌面项 {}，重绘原因 {:?}，screen={:.0}x{:.0}@{:.2} overlay={}x{}",
                        self.cfg.boxes.len(),
                        self.desktop_items.len(),
                        causes,
                        ui.max_rect().width(),
                        ui.max_rect().height(),
                        ui.ctx().pixels_per_point(),
                        self.overlay.w,
                        self.overlay.h
                    );
                    self.frame_cost = None;
                    self.frame_split = None;
                    Some((now, 0))
                }
                None => Some((now, 0)),
            };
        }
        self.note_item_press();
        self.handle_drag();
        self.maybe_finish_pending_press();
        if ui.input(|i| i.viewport().close_requested()) {
            self.shutdown();
            return;
        }
        let t_overlay = std::time::Instant::now();
        self.draw_overlay(ui);
        // 空格请求在点击处理完之后判定：这样"点选后立刻按空格"不会读到上一帧的选中状态。
        if self.pending_toggle_preview {
            self.pending_toggle_preview = false;
            self.toggle_preview();
        }
        let t_modals = std::time::Instant::now();
        self.show_modals();
        let t_settings = std::time::Instant::now();
        self.show_settings();
        self.draw_status(ui);
        self.sync_nav_hook();
        let overlay_ms = (t_modals - t_overlay).as_secs_f64() * 1000.0;
        let modal_ms = (t_settings - t_modals).as_secs_f64() * 1000.0;
        let settings_ms = t_settings.elapsed().as_secs_f64() * 1000.0;
        self.frame_split = match self.frame_split {
            Some((start, n, o, m, s)) if now - start < 1.0 => {
                Some((start, n + 1, o + overlay_ms, m + modal_ms, s + settings_ms))
            }
            _ => Some((now, 1, overlay_ms, modal_ms, settings_ms)),
        };
        let cost = frame_start.elapsed().as_secs_f64() * 1000.0;
        self.frame_cost = match self.frame_cost {
            Some((start, n, total, max)) if now - start < 1.0 => {
                Some((start, n + 1, total + cost, max.max(cost)))
            }
            Some((_, _n, total, max)) => Some((now, 1, total + cost, max.max(cost))),
            None => Some((now, 1, cost, cost)),
        };
    }

    fn frame_cost_stats(&self) -> (f64, f64) {
        match self.frame_cost {
            Some((_, n, total, max)) if n > 0 => (total / f64::from(n), max),
            _ => (0.0, 0.0),
        }
    }

    fn shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.saver.flush_now(&self.cfg);
        crate::platform::key_hook::uninstall();
        self.probe.stop();
        desktop::set_takeover_enabled(false);
        self.layer.restore();
        if self.cfg.desktop.hide_system_icons {
            desktop::show_icon_layer();
        }
        desktop::write_session_flag(false);
        desktop::refresh_desktop();
        let _ = self.store.send(StoreCmd::Shutdown);
        self.desktop_watcher.stop();
        message_window::request_quit();
        log::info!("DeskKeeper 已退出");
    }
}

impl eframe::App for DeskApp {
    /// 全透明清底（同 sylva 的 `draw_scene`）：壁纸原样透出，不产生任何底色/遮罩。
    /// eframe 默认清屏色是 `rgba(12,12,12,180)`，在透明窗口上会形成 70% 深灰遮罩。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.tick(ui);
    }
}

impl Drop for DeskApp {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 把矩形对齐到物理像素栅格：图标纹理按物理像素生成，若绘制位置落在半像素上，
/// GPU 双线性采样会把边缘"抹"开，看起来发糊。这里提前取整，保证 1 纹理像素 = 1 屏幕像素。
fn pixel_aligned(rect: Rect, pp: f32) -> Rect {
    let pp = pp.max(1.0);
    Rect::from_min_max(
        egui::pos2(
            (rect.min.x * pp).round() / pp,
            (rect.min.y * pp).round() / pp,
        ),
        egui::pos2(
            (rect.max.x * pp).round() / pp,
            (rect.max.y * pp).round() / pp,
        ),
    )
}

/// 桌面格子的逻辑矩形（[`model::DesktopCell`] 几何字段的唯一换算入口）。
fn desktop_cell_rect(c: &model::DesktopCell) -> Rect {
    Rect::from_min_size(egui::pos2(c.x, c.y), egui::vec2(c.w, c.h))
}

/// 指针落在哪个桌面格子：命中格子矩形返回该下标；落在格子间隙时取最近的格子中心，
/// 但超过半个格子则视为无效落点，避免拖到空白处误排。
fn cell_index_at(cells: &[model::DesktopCell], pos: Pos2) -> Option<usize> {
    if cells.is_empty() {
        return None;
    }
    let mut best: Option<(usize, f32)> = None;
    for (i, c) in cells.iter().enumerate() {
        let rect = desktop_cell_rect(c);
        if rect.contains(pos) {
            return Some(i);
        }
        let d = rect.center().distance_sq(pos);
        match best {
            Some((_, best_d)) if d >= best_d => {}
            _ => best = Some((i, d)),
        }
    }
    let (idx, best_d) = best?;
    let c = &cells[idx];
    let limit = (c.w.max(c.h) * 0.5).powi(2);
    (best_d <= limit).then_some(idx)
}

/// 指针严格落在哪个桌面格子矩形内；落在格子外（含右侧空白）返回 None。
///
/// 拖放落格用这个函数：不猜最近格，避免「拖到空白处却落到旁边」的错误占用。
fn cell_index_exact_at(cells: &[model::DesktopCell], pos: Pos2) -> Option<usize> {
    cells
        .iter()
        .position(|c| desktop_cell_rect(c).contains(pos))
}

/// 两组棋盘槽位是否等价（忽略顺序）：避免每次 rebuild 都因 items 排序变化而写盘。
fn slots_equivalent(a: &[DesktopSlot], b: &[DesktopSlot]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    fn signature(slots: &[DesktopSlot]) -> Vec<(&str, usize, usize, usize)> {
        let mut v: Vec<(&str, usize, usize, usize)> = slots
            .iter()
            .map(|s| (s.key.as_str(), s.monitor, s.col, s.row))
            .collect();
        v.sort_unstable();
        v
    }
    signature(a) == signature(b)
}

/// 图标在格子内的统一位置：顶部与格子顶部对齐，水平居中，按 pp 对齐物理像素。
///
/// 桌面与盒子 Icon/Thumb 视图共用，避免两条绘制路径各自加顶部偏移导致上下错位。
fn item_icon_rect(cell: Rect, icon: f32, pp: f32) -> Rect {
    pixel_aligned(
        Rect::from_min_size(
            egui::pos2(cell.center().x - icon * 0.5, cell.top()),
            Vec2::splat(icon),
        ),
        pp,
    )
}

/// 名称起点：图标底部 + label_gap * k。桌面与盒子共用。
fn item_label_top(icon_rect: Rect, label_gap: f32, k: f32) -> f32 {
    icon_rect.bottom() + label_gap * k
}

/// 名称可用的最大宽度。桌面与盒子共用。
fn item_label_max_w(cell_w: f32, k: f32) -> f32 {
    (cell_w - 4.0 * k).max(1.0)
}

/// 图标 + 名称实际内容的外接矩形；桌面与盒子选中/hover 框共用。
///
/// 格子高度总按「最多两行名称」预留；若直接拿整格当选中框，单行名称下方会多出
/// 约一行行高的空白，看起来上边贴住、下边悬空。这里只覆盖实际内容。
fn item_content_rect(
    cell: Rect,
    icon_rect: Rect,
    label: Option<&LabelLayout>,
    k: f32,
    pp: f32,
    clip: Rect,
) -> Rect {
    // 内边距先取整到逻辑点，再整体对齐物理像素，避免非整数 DPI 下半像素描边。
    let pad = (2.0 * k).round().max(1.0);
    let bottom = label.map_or(icon_rect.bottom(), LabelLayout::content_bottom);
    let rect = Rect::from_min_max(
        egui::pos2(cell.left() + pad, icon_rect.top() - pad),
        egui::pos2(cell.right() - pad, bottom + pad),
    );
    pixel_aligned(rect, pp).intersect(clip)
}

/// 图标缓存键：普通文件用 `d`/`f`；系统项用 `pc` / `rb0`(空桶) / `rb1`(满桶)，
/// 回收站状态变化时自然切换到另一套纹理，不需要手动失效旧缓存。
fn icon_key(item: &ItemMeta, size: u32) -> String {
    let kind = match item.system {
        Some(model::SystemItem::Computer) => "pc",
        Some(model::SystemItem::RecycleBin { full: false }) => "rb0",
        Some(model::SystemItem::RecycleBin { full: true }) => "rb1",
        None => {
            if item.is_dir {
                "d"
            } else {
                "f"
            }
        }
    };
    format!("icon:{kind}:{size}:{}", item.path.display())
}

/// 列表视图右侧的「大小 · 修改时间」说明文字。
fn list_meta(item: &ItemMeta) -> String {
    let size = if item.system.is_some() {
        String::new()
    } else if item.is_dir {
        "文件夹".to_string()
    } else {
        util::format_size(item.size)
    };
    let time = if item.modified == 0 {
        String::new()
    } else {
        util::format_time(item.modified)
    };
    match (size.is_empty(), time.is_empty()) {
        (false, false) => format!("{size}  ·  {time}"),
        (false, true) => size,
        (true, false) => time,
        (true, true) => String::new(),
    }
}

/// 把图像按原始宽高比内接填充到给定方框（用于缩略图，避免拉伸变形）。
fn fit_image_rect(bounds: Rect, natural: Vec2) -> Rect {
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return bounds;
    }
    let s = (bounds.width() / natural.x).min(bounds.height() / natural.y);
    Rect::from_center_size(bounds.center(), natural * s)
}

/// 把一行文本按轻量分词结果拼成 LayoutJob（不同 TokenKind 用不同颜色）。
fn highlight_job(
    line: &str,
    lang: preview::Lang,
    font: &FontId,
    base: Color32,
    dark: bool,
) -> egui::text::LayoutJob {
    use preview::TokenKind;
    let color = |kind: TokenKind| match (kind, dark) {
        (TokenKind::Keyword, true) => Color32::from_rgb(199, 146, 234),
        (TokenKind::Keyword, false) => Color32::from_rgb(124, 58, 237),
        (TokenKind::Str, true) => Color32::from_rgb(158, 211, 139),
        (TokenKind::Str, false) => Color32::from_rgb(46, 125, 50),
        (TokenKind::Comment, true) => Color32::from_rgb(127, 132, 142),
        (TokenKind::Comment, false) => Color32::from_rgb(138, 143, 152),
        (TokenKind::Number, true) => Color32::from_rgb(229, 192, 123),
        (TokenKind::Number, false) => Color32::from_rgb(178, 106, 0),
    };
    let fmt = |c: Color32| egui::TextFormat {
        font_id: font.clone(),
        color: c,
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::default();
    let mut cursor = 0usize;
    for (range, kind) in preview::highlight_line(line, lang) {
        if range.start > cursor && range.start <= line.len() {
            job.append(&line[cursor..range.start], 0.0, fmt(base));
        }
        job.append(&line[range.clone()], 0.0, fmt(color(kind)));
        cursor = range.end;
    }
    if cursor < line.len() {
        job.append(&line[cursor..], 0.0, fmt(base));
    }
    job
}

/// 图标名称换行缓存：用真实字形宽度测量后缓存结果，避免每帧重复排版。
///
/// 键为显示名，值为（可用宽度, 字号, 换行结果）；宽度或字号变化（滑杆、DPI）时重算。
#[derive(Default)]
struct LabelCache {
    entries: std::collections::HashMap<String, (f32, f32, std::sync::Arc<Vec<String>>)>,
}

impl LabelCache {
    /// 丢弃某个旧名称的排版结果（重命名后避免命中旧字体宽度缓存）。
    fn forget(&mut self, name: &str) {
        self.entries.remove(name);
    }

    fn lines<F>(
        &mut self,
        name: &str,
        max_w: f32,
        font_size: f32,
        measure: F,
    ) -> std::sync::Arc<Vec<String>>
    where
        F: Fn(&str) -> f32,
    {
        if let Some((w, f, lines)) = self.entries.get(name) {
            if (*w - max_w).abs() < 0.5 && (*f - font_size).abs() < 0.01 {
                return std::sync::Arc::clone(lines);
            }
        }
        if self.entries.len() >= 2048 {
            self.entries.clear();
        }
        let lines = std::sync::Arc::new(model::label_lines_measured(name, max_w, measure));
        self.entries.insert(
            name.to_owned(),
            (max_w, font_size, std::sync::Arc::clone(&lines)),
        );
        lines
    }
}

/// 用 egui 当前字体实际排版测量一段文字的绘制宽度。
fn measure_text_width(painter: &egui::Painter, text: &str, font_size: f32) -> f32 {
    painter
        .layout_no_wrap(
            text.to_owned(),
            FontId::proportional(font_size),
            Color32::WHITE,
        )
        .size()
        .x
}

/// 一行已排版的名称：galley 用于绘制、命中与选区位置换算。
struct LabelLine {
    galley: std::sync::Arc<egui::Galley>,
    /// 该行左上角（galley 原点）。
    pos: Pos2,
    /// 该行第一个字符在整个名称里的字符下标。
    start: usize,
    /// 该行字符数。
    chars: usize,
}

/// 名称排版结果：可交互区域、每行 galley 与字符映射。
///
/// 绘制、点选、拖选、双击选词、Ctrl+C 都基于同一份布局，避免文字位置与命中区域错位。
struct LabelLayout {
    source: std::sync::Arc<Vec<String>>,
    lines: Vec<LabelLine>,
    /// 名称文字的交互/裁剪区域（已与单元格/盒子内容区求交）。
    zone: Rect,
    center_x: f32,
    top: f32,
    font_size: f32,
    total_chars: usize,
}

impl LabelLayout {
    fn new(
        painter: &egui::Painter,
        lines: &std::sync::Arc<Vec<String>>,
        center_x: f32,
        top: f32,
        max_w: f32,
        font_size: f32,
        clip: Rect,
    ) -> Self {
        let font = FontId::proportional(font_size);
        let line_h = font_size * model::LABEL_LINE_RATIO;
        // 名称可交互区域只覆盖文字本身；与图标的间距由格子预留，不计入选区矩形。
        let label_h = font_size * model::LABEL_LINE_RATIO * model::LABEL_MAX_LINES as f32;
        let zone = Rect::from_min_max(
            egui::pos2(center_x - max_w * 0.5, top),
            egui::pos2(center_x + max_w * 0.5, top + label_h),
        )
        .intersect(clip);
        let mut out = Vec::with_capacity(lines.len());
        let mut start = 0usize;
        for (i, text) in lines.iter().enumerate() {
            let galley = painter.layout_no_wrap(text.clone(), font.clone(), Color32::WHITE);
            let x = center_x - galley.size().x * 0.5;
            let y = top + i as f32 * line_h;
            let chars = text.chars().count();
            out.push(LabelLine {
                galley,
                pos: egui::pos2(x, y),
                start,
                chars,
            });
            start += chars;
        }
        Self {
            source: std::sync::Arc::clone(lines),
            lines: out,
            zone,
            center_x,
            top,
            font_size,
            total_chars: start,
        }
    }

    /// 名称文字实际绘制到的底边（按真实行数，而不是最多两行的预留高度）。
    fn content_bottom(&self) -> f32 {
        self.top + self.lines.len() as f32 * self.font_size * model::LABEL_LINE_RATIO
    }

    fn line_index_at(&self, y: f32) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        // 以每行实际起始 y 为分界：只有越过下一行顶部才切换到下一行。
        // 用行高一半做分界会把长名称第一行的下半段误判到较短的第二行，
        // 导致在该位置拖选时下标恒为第一行末尾、选不出文字。
        let mut idx = 0usize;
        for (i, line) in self.lines.iter().enumerate() {
            if y >= line.pos.y {
                idx = i;
            } else {
                break;
            }
        }
        idx
    }

    /// 鼠标位置 → 整个名称的字符下标（越界自动夹到首/尾）。
    fn cursor_at(&self, pos: Pos2) -> usize {
        let Some(line) = self.lines.get(self.line_index_at(pos.y)) else {
            return 0;
        };
        let local = line
            .galley
            .cursor_from_pos(egui::vec2(pos.x - line.pos.x, pos.y - line.pos.y))
            .index
            .0
            .min(line.chars);
        line.start + local
    }

    /// 字符区间 `[a, b)` 对应的原文（选区跨行时直接拼接，不插入换行）。
    fn selected_text(&self, a: usize, b: usize) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            let s = a.max(line.start).min(line.start + line.chars);
            let e = b.max(line.start).min(line.start + line.chars);
            if e > s {
                if let Some(text) = self.source.get(i) {
                    out.extend(text.chars().skip(s - line.start).take(e - s));
                }
            }
        }
        out
    }

    /// 绘制选区高亮 + 名称文字（阴影保持原有可读性）。
    fn paint(&self, painter: &egui::Painter, hovered: bool, sel: Option<(usize, usize)>) {
        let painter = painter.with_clip_rect(self.zone);
        if let Some((a, b)) = sel.filter(|(a, b)| b > a) {
            for line in &self.lines {
                let s = a.max(line.start).min(line.start + line.chars);
                let e = b.max(line.start).min(line.start + line.chars);
                if e <= s {
                    continue;
                }
                let r0 = line
                    .galley
                    .pos_from_cursor(egui::text::CCursor::new(s - line.start));
                let r1 = line
                    .galley
                    .pos_from_cursor(egui::text::CCursor::new(e - line.start));
                let rect = Rect::from_min_max(
                    egui::pos2(line.pos.x + r0.min.x, line.pos.y + r0.min.y),
                    egui::pos2(line.pos.x + r1.min.x, line.pos.y + r1.max.y),
                );
                painter.rect_filled(
                    rect.expand2(egui::vec2(0.0, 1.0)),
                    CornerRadius::same(2),
                    Color32::from_rgba_unmultiplied(92, 152, 255, 175),
                );
            }
        }
        let font = FontId::proportional(self.font_size);
        let line_h = self.font_size * model::LABEL_LINE_RATIO;
        let (near, far): (u8, u8) = if hovered { (150, 70) } else { (120, 45) };
        for (i, line) in self.source.iter().enumerate() {
            let pos = egui::pos2(self.center_x, self.top + i as f32 * line_h);
            painter.text(
                pos + Vec2::new(0.0, 1.0),
                Align2::CENTER_TOP,
                line,
                font.clone(),
                Color32::from_black_alpha(near),
            );
            painter.text(
                pos + Vec2::new(0.0, 2.0),
                Align2::CENTER_TOP,
                line,
                font.clone(),
                Color32::from_black_alpha(far),
            );
            painter.text(pos, Align2::CENTER_TOP, line, font.clone(), Color32::WHITE);
        }
    }
}

/// 名称区域的文本选择：按下才进入选择模式，拖动框选、双击选词、松开后保留选区。
fn handle_label_selection(
    sel: &mut TextSelection,
    ui: &egui::Ui,
    target: LabelTarget,
    layout: &LabelLayout,
    resp: &egui::Response,
) {
    let pointer = ui.input(|i| i.pointer.interact_pos());
    let press = ui
        .input(|i| i.pointer.press_origin())
        .or(pointer)
        .unwrap_or(layout.zone.center());

    if resp.drag_started() {
        let idx = layout.cursor_at(press);
        sel.target = Some(target);
        sel.anchor = idx;
        sel.cursor = idx;
        sel.dragging = true;
        sel.zone = Some(layout.zone);
    } else if resp.clicked() {
        // 单击名称：收起选区（保留光标位置），并把选择目标切到当前名称。
        let idx = layout.cursor_at(press);
        sel.target = Some(target);
        sel.anchor = idx;
        sel.cursor = idx;
        sel.dragging = false;
        sel.zone = Some(layout.zone);
    }

    if sel.target == Some(target) {
        if sel.dragging {
            if let Some(p) = pointer {
                sel.cursor = layout.cursor_at(p);
            }
            if resp.drag_stopped() || !ui.input(|i| i.pointer.any_down()) {
                sel.dragging = false;
            }
        }
        sel.anchor = sel.anchor.min(layout.total_chars);
        sel.cursor = sel.cursor.min(layout.total_chars);
        sel.zone = Some(layout.zone);
        let (a, b) = sel.range();
        sel.selected = if b > a {
            layout.selected_text(a, b)
        } else {
            String::new()
        };
    }
}

fn rect_tuple(r: Rect) -> model::RectF {
    (r.min.x, r.min.y, r.width(), r.height())
}

fn rect_from_tuple(r: model::RectF) -> Rect {
    Rect::from_min_size(egui::pos2(r.0, r.1), egui::vec2(r.2, r.3))
}

fn draw_icon(
    painter: &egui::Painter,
    rect: Rect,
    texture: Option<egui::TextureId>,
    is_dir: bool,
    name: &str,
    pal: &theme::Palette,
) {
    if let Some(id) = texture {
        painter.image(
            id,
            rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        return;
    }
    let fill = if is_dir { pal.accent } else { pal.card_bg };
    painter.rect_filled(
        rect.shrink(rect.width() * 0.08),
        CornerRadius::same(8),
        fill,
    );
    let initial = name
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".to_string());
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        initial,
        FontId::proportional((rect.width() * 0.4).max(10.0)),
        pal.text_dim,
    );
}

fn glyph_button(
    ui: &egui::Ui,
    rect: Rect,
    id: (&'static str, usize),
    glyph: &str,
    color: Color32,
    pal: &theme::Palette,
) -> bool {
    let resp = ui.interact(rect, Id::unique(id), Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), pal.card_hover);
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        FontId::proportional(13.0),
        color,
    );
    resp.clicked()
}

/// 锁定按钮：闭合锁表示已锁定，打开锁表示可拖动/缩放。
fn lock_button(
    ui: &egui::Ui,
    rect: Rect,
    id: (&'static str, usize),
    locked: bool,
    pal: &theme::Palette,
) -> bool {
    let resp = ui.interact(rect, Id::unique(id), Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), pal.card_hover);
    }
    let color = if locked { pal.accent } else { pal.text_dim };
    paint_lock(ui.painter(), rect.center(), rect.width(), color, locked);
    resp.on_hover_text(if locked {
        "已锁定：拖动与缩放已禁用"
    } else {
        "未锁定：可拖动与缩放"
    })
    .clicked()
}

/// 自绘锁形：锁体为圆角矩形，锁梁为圆弧；打开状态锁梁右移并留缺口。
fn paint_lock(
    painter: &egui::Painter,
    center: egui::Pos2,
    size: f32,
    color: Color32,
    locked: bool,
) {
    let body_w = size * 0.46;
    let body_h = size * 0.38;
    let body = Rect::from_center_size(
        egui::pos2(center.x, center.y + size * 0.17),
        egui::vec2(body_w, body_h),
    );
    painter.rect_filled(body, CornerRadius::same(2), color);

    let radius = body_w * 0.36;
    let cx = center.x + if locked { 0.0 } else { body_w * 0.22 };
    let base_y = body.top() + size * 0.02;
    let (start_deg, end_deg) = if locked { (180.0, 0.0) } else { (200.0, 35.0) };
    let mut points = Vec::with_capacity(13);
    for i in 0..=12 {
        let deg = start_deg + (end_deg - start_deg) * i as f32 / 12.0;
        let rad = deg.to_radians();
        points.push(egui::pos2(
            cx + radius * rad.cos(),
            base_y - radius * rad.sin(),
        ));
    }
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(size * 0.12, color),
    ));
}

/// 盒子名称最大长度（按字符计，中文按 1 个字符）。
const BOX_NAME_MAX_CHARS: usize = 40;
/// 标题栏最多展示的字符数，超出用省略号；完整名称可通过悬停标题查看。
const BOX_TITLE_MAX_CHARS: usize = 24;

/// 盒子改名校验：只改显示名，不涉及文件系统，因此允许空格与多数符号。
fn validate_box_name(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("名称不能为空或纯空白");
    }
    if trimmed.chars().count() > BOX_NAME_MAX_CHARS {
        return Err("名称过长（最多 40 个字符）");
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err("名称包含不可见控制字符");
    }
    Ok(trimmed.to_string())
}

/// 按字符截断名称，超出部分用单个省略号表示。
fn truncate_box_name(name: &str, max_chars: usize) -> String {
    let count = name.chars().count();
    if count <= max_chars {
        return name.to_string();
    }
    let mut out: String = name.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 标题栏显示文本：截断超长名称，折叠时附加条目数量。
fn box_title_display(name: &str, collapsed: bool, count: usize) -> String {
    let shown = truncate_box_name(name, BOX_TITLE_MAX_CHARS);
    if collapsed {
        format!("{shown} · 已折叠 {count} 项")
    } else {
        shown
    }
}

/// 重命名输入校验：空名、Windows 非法字符、保留设备名、尾随点/空格都要提前拦下。
///
/// 返回给用户看的中文原因；通过返回 `Ok(())`。
fn validate_rename_name(name: &str) -> Result<(), &'static str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("名称不能为空");
    }
    const ILLEGAL: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    if let Some(c) = trimmed
        .chars()
        .find(|c| ILLEGAL.contains(c) || c.is_control())
    {
        return Err(if c.is_control() {
            "名称包含不可见控制字符"
        } else {
            "名称包含非法字符 < > : \" / \\ | ? *"
        });
    }
    if trimmed.ends_with('.') || trimmed.ends_with(' ') {
        return Err("名称不能以点或空格结尾");
    }
    let stem = trimmed
        .split('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        return Err("该名称是 Windows 保留设备名");
    }
    Ok(())
}

/// 重命名决策（纯函数，不触碰文件系统）：`Ok(None)` 名称未变无需操作，
/// `Ok(Some(name))` 为可执行的新名称，`Err(msg)` 为可直接展示给用户的原因。
///
/// 同目录兄弟名按大小写不敏感比较（Windows 语义）；旧名自身不参与判重，
/// 因此允许仅大小写变化的改名。编辑框预填完整文件名，扩展名不会被自动补全或剥离。
fn validate_rename(
    old_name: &str,
    new_name: &str,
    sibling_names: &[String],
) -> Result<Option<String>, String> {
    if new_name == old_name {
        return Ok(None);
    }
    if new_name.ends_with(' ') {
        return Err("名称不能以空格结尾".to_string());
    }
    let trimmed = new_name.trim();
    if trimmed == old_name {
        return Ok(None);
    }
    validate_rename_name(trimmed).map_err(str::to_string)?;
    let conflict = sibling_names
        .iter()
        .any(|s| s != old_name && s.to_lowercase() == trimmed.to_lowercase());
    if conflict {
        return Err(format!("已存在同名项目「{trimmed}」"));
    }
    Ok(Some(trimmed.to_string()))
}

/// 把 `fs::rename` 的失败翻译成用户能理解的原因，并保留原始系统错误。
fn rename_failure_msg(e: &std::io::Error) -> String {
    let hint = match e.raw_os_error() {
        Some(5) => "无权限，或文件为只读/正被占用",
        Some(32) => "文件正被其他程序占用",
        Some(183) => "目标名称已存在",
        Some(3) => "路径不存在",
        _ => "",
    };
    if hint.is_empty() {
        format!("重命名失败：{e}")
    } else {
        format!("重命名失败：{hint}（{e}）")
    }
}

/// 原生菜单不可用时使用的内置条目菜单；返回 true 表示已选中某项、应收起菜单。
fn fallback_item_menu(
    ui: &mut egui::Ui,
    req: &crate::platform::shell_menu::ShellMenuRequest,
    actions: &mut Vec<UiAction>,
) -> bool {
    let mut picked = false;
    if req.system {
        // 系统项降级菜单：只保留打开与复制命名空间路径，不提供文件类动作。
        let open_label = if ItemMeta::is_recycle_bin_path(&req.path) {
            "打开「回收站」"
        } else {
            "打开「此电脑」"
        };
        if ui.button(open_label).clicked() {
            actions.push(UiAction::Open(req.path.clone()));
            picked = true;
        }
        if ui.button("复制 Shell 路径").clicked() {
            actions.push(UiAction::ShellMenu(ShellMenuAction::CopyPath(
                req.path.clone(),
            )));
            picked = true;
        }
        return picked;
    }
    if ui.button("打开").clicked() {
        actions.push(UiAction::Open(req.path.clone()));
        picked = true;
    }
    if ui.button("在资源管理器中定位").clicked() {
        actions.push(UiAction::ShellMenu(ShellMenuAction::Reveal(
            req.path.clone(),
        )));
        picked = true;
    }
    // 与原生菜单保持同层级：自有动作全部收进「DeskKeeper」二级子菜单。
    ui.menu_button("DeskKeeper", |ui| {
        if ui.button("重命名").clicked() {
            actions.push(UiAction::ShellMenu(ShellMenuAction::Rename {
                box_idx: req.from_box,
                item: req.item,
            }));
            picked = true;
        }
        if ui.button("复制路径").clicked() {
            actions.push(UiAction::ShellMenu(ShellMenuAction::CopyPath(
                req.path.clone(),
            )));
            picked = true;
        }
        if ui
            .button(theme::danger_menu_text("删除（移到回收站）"))
            .clicked()
        {
            actions.push(UiAction::ShellMenu(ShellMenuAction::Delete {
                box_idx: req.from_box,
                item: req.item,
            }));
            picked = true;
        }
        if ui.button("添加到剪贴板盒").clicked() {
            actions.push(UiAction::ShellMenu(ShellMenuAction::AddToClipboard(
                req.path.clone(),
            )));
            picked = true;
        }
        if req.from_box.is_some() && ui.button("移出盒子（放回桌面）").clicked() {
            actions.push(UiAction::ShellMenu(ShellMenuAction::Eject {
                box_idx: req.from_box.unwrap_or(0),
                item: req.item,
            }));
            picked = true;
        }
        if !req.boxes.is_empty() {
            ui.menu_button("在盒子内移动", |ui| {
                for (to, title) in &req.boxes {
                    if ui.button(title).clicked() {
                        actions.push(UiAction::ShellMenu(ShellMenuAction::MoveToBox {
                            from: req.from_box,
                            item: req.item,
                            to: *to,
                        }));
                        picked = true;
                        ui.close();
                    }
                }
            });
        }
    });
    picked
}

fn box_menu(
    ui: &mut egui::Ui,
    idx: usize,
    box_cfg: &BoxConfig,
    invalid: usize,
    actions: &mut Vec<UiAction>,
) {
    if ui.button("新建盒子").clicked() {
        actions.push(UiAction::NewBox);
        ui.close();
    }
    if ui.button("重命名").clicked() {
        actions.push(UiAction::RenameBox(idx));
        ui.close();
    }
    if ui.button("配色与透明度").clicked() {
        actions.push(UiAction::ColorBox(idx));
        ui.close();
    }
    if !matches!(box_cfg.kind, BoxKind::Todo | BoxKind::Clipboard) {
        ui.menu_button("内容视图", |ui| {
            for view in BoxView::ALL {
                if ui.radio(box_cfg.view == view, view.label()).clicked() {
                    actions.push(UiAction::SetBoxView(idx, view));
                    ui.close();
                }
            }
        });
    }
    if ui
        .button(if box_cfg.collapsed {
            "展开"
        } else {
            "折叠"
        })
        .clicked()
    {
        actions.push(UiAction::ToggleCollapse(idx));
        ui.close();
    }
    if ui
        .button(if box_cfg.locked {
            "解锁位置"
        } else {
            "锁定位置"
        })
        .clicked()
    {
        actions.push(UiAction::ToggleLock(idx));
        ui.close();
    }
    ui.menu_button("排序方式", |ui| {
        for mode in SortMode::ALL {
            let selected = box_cfg.sort == mode;
            if ui.radio(selected, mode.label()).clicked() {
                actions.push(UiAction::SetBoxSort(idx, mode));
                ui.close();
            }
        }
    });
    ui.separator();
    if !matches!(box_cfg.kind, BoxKind::Todo | BoxKind::Clipboard) {
        ui.menu_button("新建到桌面", |ui| {
            if ui.button("文件夹").clicked() {
                actions.push(UiAction::NewFolderInBox(idx));
                ui.close();
            }
            if ui.button("文本文档").clicked() {
                actions.push(UiAction::NewTextInBox(idx));
                ui.close();
            }
        });
    }
    if ui.button("粘贴文件到盒子").clicked() {
        actions.push(UiAction::PasteIntoBox(idx));
        ui.close();
    }
    if invalid > 0 && ui.button(format!("清理失效引用（{invalid}）")).clicked() {
        actions.push(UiAction::PruneInvalid(idx));
        ui.close();
    }
    ui.separator();
    if ui.button("关闭盒子（隐藏，可重开）").clicked() {
        actions.push(UiAction::CloseBox(idx));
        ui.close();
    }
    if box_cfg.kind == BoxKind::Todo
        && ui
            .button(theme::danger_menu_text("删除待办盒子（清空全部事项）"))
            .clicked()
    {
        actions.push(UiAction::DeleteTodoBox(idx));
        ui.close();
    }
    if ui.button("解散盒子（不删除文件）").clicked() {
        actions.push(UiAction::DissolveBox(idx));
        ui.close();
    }
    ui.add_enabled(false, egui::Button::new("删除盒子及文件（暂不启用）"));
}

#[cfg(test)]
mod tests {
    use super::{
        box_title_display, truncate_box_name, validate_box_name, validate_rename,
        validate_rename_name,
    };

    fn siblings(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn box_name_validation_rejects_blank_and_overlong() {
        assert!(validate_box_name("   ").is_err());
        assert_eq!(validate_box_name("  工作  ").unwrap(), "工作");
        assert_eq!(validate_box_name("工作 2").unwrap(), "工作 2");
        assert!(validate_box_name(&"长".repeat(41)).is_err());
        assert!(validate_box_name("长".repeat(40).as_str()).is_ok());
    }

    #[test]
    fn box_name_title_truncates_with_ellipsis() {
        let long = "很长很长的盒子名称".repeat(6);
        let shown = truncate_box_name(&long, 24);
        assert_eq!(shown.chars().count(), 24);
        assert!(shown.ends_with('…'));
        assert_eq!(box_title_display("工作", false, 0), "工作");
        assert_eq!(box_title_display("工作", true, 3), "工作 · 已折叠 3 项");
    }

    #[test]
    fn rename_validation_rejects_empty_illegal_and_reserved() {
        assert!(validate_rename_name("  ").is_err());
        assert!(validate_rename_name("a<b.txt").is_err());
        assert!(validate_rename_name("a/b.txt").is_err());
        assert!(validate_rename_name("a?.txt").is_err());
        assert!(validate_rename_name("结尾.").is_err());
        assert!(validate_rename_name("CON").is_err());
        assert!(validate_rename_name("com1.txt").is_err());
        assert!(validate_rename_name("2026 年度报告.pdf").is_ok());
        assert!(validate_rename_name("新建文件夹").is_ok());
    }

    #[test]
    fn rename_conflict_is_rejected_case_insensitively() {
        let existing = siblings(&["已有.txt", "report.docx"]);
        // 目标名已被同目录占用：必须拒绝，不能覆盖。
        assert!(validate_rename("旧名.txt", "已有.txt", &existing).is_err());
        assert!(validate_rename("旧名.txt", "REPORT.DOCX", &existing).is_err());
        // 旧名自身不参与判重：仅大小写变化的改名应放行。
        let only_self = siblings(&["report.docx"]);
        assert_eq!(
            validate_rename("report.docx", "REPORT.DOCX", &only_self).unwrap(),
            Some("REPORT.DOCX".to_string())
        );
    }

    #[test]
    fn rename_keeps_full_name_and_rejects_bad_extension_edits() {
        // 编辑框预填完整文件名，提交值即最终文件名：扩展名随用户输入保留。
        assert_eq!(
            validate_rename("报告.docx", "报告 终稿.docx", &[]).unwrap(),
            Some("报告 终稿.docx".to_string())
        );
        // 去掉扩展名属于用户显式输入，不做自动补全。
        assert_eq!(
            validate_rename("报告.docx", "报告终稿", &[]).unwrap(),
            Some("报告终稿".to_string())
        );
        // 以点结尾会破坏扩展名结构，直接拒绝。
        assert!(validate_rename("报告.docx", "报告.", &[]).is_err());
        // 以空格结尾同样拒绝（不会被静默 trim 掉）。
        assert!(validate_rename("报告.docx", "报告 ", &[]).is_err());
        // 名称未变或仅多了前导空白：视为无需操作。
        assert_eq!(
            validate_rename("报告.docx", "报告.docx", &[]).unwrap(),
            None
        );
        assert_eq!(
            validate_rename("报告.docx", " 报告.docx", &[]).unwrap(),
            None
        );
    }
}
