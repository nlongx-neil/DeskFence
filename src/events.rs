//! 后台线程 → UI 线程的事件通道。

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use crate::clipboard::store::ClipRecord;
use crate::fileops::MoveRecord;
use crate::model::ItemMeta;
use crate::platform::icons::IconResponse;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    ShowClipboardBox,
    ToggleOverlay,
    TogglePause,
    AutoGroup,
    /// 清空桌面棋盘占位，回到按排序紧凑排列
    RestoreAutoArrange,
    /// 重新启用棋盘摆放
    EnableBoardPlacement,
    Settings,
    NewBox,
    Exit,
}

/// 桌面空白处原生菜单中 DeskKeeper 子菜单的动作（在 UI 线程执行）。
#[derive(Clone, Debug)]
pub enum DesktopMenuAction {
    /// 一次性把桌面上尚未归属的图标按类型整理进盒子（纯引用）。
    AutoGroup,
    NewBox,
    NewClipboardBox,
    NewTodoBox,
    Settings,
    ReloadDesktop,
    UndoLast,
    /// 清空棋盘占位，回到紧凑排列
    RestoreAutoArrange,
    /// 重新启用棋盘摆放
    EnableBoardPlacement,
    ToggleOverlay,
}

/// Windows 原生右键菜单中我们追加项被选中后的动作（在 UI 线程执行）。
#[derive(Clone, Debug)]
pub enum ShellMenuAction {
    /// 进入行内重命名：桌面（`None`）或某个盒子的第 `item` 个条目。
    Rename { box_idx: Option<usize>, item: usize },
    /// 把真实路径写入系统剪贴板。
    CopyPath(PathBuf),
    /// 与复制路径同源，但给出“已加入剪贴板盒”的反馈。
    AddToClipboard(PathBuf),
    /// 把条目移动进另一个盒子的目录。
    MoveToBox {
        from: Option<usize>,
        item: usize,
        to: usize,
    },
    /// 把盒子内条目放回桌面。
    Eject { box_idx: usize, item: usize },
    /// 系统菜单项已执行（目前只上报 delete/cut），用于在文件真正消失后清理盒子引用。
    SystemVerb { verb: String, path: PathBuf },
    /// 在资源管理器中定位（内置菜单使用；原生菜单由系统自带该项）。
    Reveal(PathBuf),
    /// 删除（移到回收站）。单条菜单只带一个条目；整组走 app 的多选菜单。
    Delete { box_idx: Option<usize>, item: usize },
}

#[derive(Clone, Debug)]
pub enum BgEvent {
    /// 剪贴板内容发生变化（需要采样）
    ClipboardChanged,
    /// 托盘菜单命令
    Tray(TrayCommand),
    /// 显示器/分辨率/DPI 变化
    DisplayChanged,
    /// 系统主题/个性化设置变化（不需要重排窗口）
    SystemThemeChanged,
    /// Win+D 拦截后的自实现切换：最小化/还原普通顶层窗口（保持本软件桌面）
    ToggleDesktop,
    /// 钩子拦截到名称选区上的 Ctrl+C：把当前选区文本复制到系统剪贴板
    CopySelection,
    /// 请求显示/聚焦剪贴板盒（关闭状态下会自动恢复）。
    ShowClipboardBox,
    /// 第二个进程启动时的唤醒：若叠加层已隐藏则先恢复显示，再显示剪贴板盒。
    Activate,
    /// 回收站空/非空状态变化（后台轻量轮询发现变化时才发送）
    RecycleBinState(bool),
    /// 某个目录的扫描结果
    DirContents { dir: PathBuf, items: Vec<ItemMeta> },
    /// 图标/缩略图加载完成
    IconLoaded(IconResponse),
    /// 剪贴板历史查询结果
    History { records: Vec<ClipRecord> },
    /// 历史数据发生变化（剪贴板盒需要刷新）
    HistoryChanged,
    /// 后台文件移动完成（成功时 `record` 可用于撤销）
    FileMoved {
        record: Option<MoveRecord>,
        error: Option<String>,
    },
    /// 记录状态提示
    Status(String),
    /// 系统原生右键菜单里我们追加项的动作
    ShellMenu(ShellMenuAction),
    /// 桌面空白处系统原生菜单里 DeskKeeper 子菜单的动作
    DesktopMenu(DesktopMenuAction),
    /// 原生右键菜单在当前环境不可用：UI 线程收到请求后改为弹出内置菜单
    ShellMenuFallback(crate::platform::shell_menu::ShellMenuRequest),
    /// 桌面原生菜单不可用：UI 线程在坐标处退回内置桌面菜单
    DesktopMenuFallback { x: i32, y: i32, reason: String },
    /// 空格预览：打开/关闭当前唯一选中项的快速预览
    TogglePreview,
    /// 关闭快速预览（Esc 或面板关闭按钮）
    ClosePreview,
    /// 预览面板内的 Ctrl+C：把预览文本写入系统剪贴板
    CopyPreviewText,
    /// 后台预览内容抽取完成
    PreviewReady(crate::preview::PreviewData),
    /// 方向键：在已聚焦盒子里移动当前焦点（`extend` 表示 Shift 扩展选区）
    NavKey {
        dir: crate::model::NavDir,
        extend: bool,
    },
    /// Ctrl+A：选中聚焦盒子当前筛选内的全部条目
    SelectAllBox,
    /// Ctrl+A（桌面上下文）：选中桌面全部图标
    SelectAllDesktop,
    /// Esc：清空盒子/桌面多选（预览未打开时）
    ClearBoxSelection,
    /// 回收站删除完成（成功时 UI 线程清理引用并同步桌面）
    FileRecycled {
        paths: Vec<PathBuf>,
        ok: bool,
        error: Option<String>,
    },
    /// 成员路径存在性探测结果：`deleted` 为已确认删除（卷可用且路径不存在），
    /// `unreachable` 为暂时不可达（盘符卸载/网络不可用），必须保留引用。
    MembersProbed {
        deleted: Vec<String>,
        unreachable: Vec<String>,
        checked: usize,
        elapsed_ms: f64,
    },
}

/// 可在任意线程克隆使用的事件发送器；发送后主动唤醒 egui。
#[derive(Clone)]
pub struct EventSink {
    tx: Sender<BgEvent>,
    ctx: egui::Context,
}

impl EventSink {
    pub fn new(ctx: egui::Context) -> (Self, Receiver<BgEvent>) {
        let (tx, rx) = mpsc::channel();
        (Self { tx, ctx }, rx)
    }

    pub fn emit(&self, event: BgEvent) {
        if self.tx.send(event).is_err() {
            return;
        }
        self.ctx.request_repaint();
    }

    pub fn raw_sender(&self) -> Sender<BgEvent> {
        self.tx.clone()
    }
}
