//! Windows 原生文件/目录右键菜单（IContextMenu / IContextMenu2 / IContextMenu3）。
//!
//! 设计：菜单在**隐藏消息窗口所在线程**上弹出。该线程已有消息泵，且窗口不属于
//! 任务栏/前台应用；用 `SetForegroundWindow` + `TrackPopupMenuEx` 的托管窗口即可
//! 正常显示系统菜单，避免 WS_EX_NOACTIVATE 叠加层抢焦点。菜单消息
//! （WM_INITMENUPOPUP / WM_DRAWITEM / WM_MEASUREITEM / WM_MENUCHAR）经由本模块
//! 转发给 IContextMenu3（缺失时回退 IContextMenu2），保证系统子菜单与自绘项工作。
//!
//! 我们追加的项使用 0x7000 以上的 id 段，和 QueryContextMenu 分配的 1..=0x6FFF 分离。

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{Interface, HSTRING, PCSTR, PCWSTR, PSTR};
use windows::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_SUCCESS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IServiceProvider, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, IContextMenu, IContextMenu2, IContextMenu3, ILCreateFromPathW, IShellBrowser,
    IShellFolder, IShellView, IShellWindows, SHBindToParent, SHChangeNotify, SHGetDesktopFolder,
    SHGetKnownFolderPath, SID_STopLevelBrowser, ShellWindows, GCS_VERBW, KF_FLAG_DEFAULT,
    SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DeleteMenu, DestroyMenu, EnableMenuItem, GetMenuItemCount,
    GetMenuItemInfoW, GetSubMenu, InsertMenuW, MessageBoxW, PostMessageW, SetForegroundWindow,
    TrackPopupMenuEx, HMENU, MB_ICONWARNING, MB_OK, MENUITEMINFOW, MF_BYCOMMAND, MF_BYPOSITION,
    MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MIIM_ID, MIIM_STRING, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_INITMENUPOPUP, WM_NULL,
};

use crate::events::{BgEvent, DesktopMenuAction, EventSink, ShellMenuAction};

/// 消息窗口线程收到后弹出菜单的自定义消息。
pub const WM_APP_SHELL_MENU: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 4;
/// 桌面空白处系统原生菜单的自定义消息。
pub const WM_APP_DESKTOP_MENU: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 5;

const ID_RENAME: usize = 0x7001;
const ID_COPY_PATH: usize = 0x7002;
const ID_ADD_CLIP: usize = 0x7003;
const ID_EJECT: usize = 0x7004;
const ID_DELETE: usize = 0x7005;
const ID_MOVE_BASE: usize = 0x7010;
/// 我们自己补进系统“新建”子菜单的文本文档项（该机器未注册 .txt ShellNew 时也能用）。
const ID_D_NEW_TEXT: usize = 0x7030;

// 桌面空白菜单里 DeskKeeper 子菜单的 id（与系统项 1..=0x6FFF 分离）。
const ID_D_AUTO_GROUP: usize = 0x7201;
const ID_D_NEW_BOX: usize = 0x7202;
const ID_D_NEW_CLIP: usize = 0x7203;
const ID_D_NEW_TODO: usize = 0x7204;
const ID_D_SETTINGS: usize = 0x7205;
const ID_D_RELOAD: usize = 0x7206;
const ID_D_UNDO: usize = 0x7207;
const ID_D_TOGGLE_OVERLAY: usize = 0x7208;
const ID_D_RESTORE_AUTO: usize = 0x7209;
const ID_D_ENABLE_BOARD: usize = 0x720A;

/// 由 UI 线程构造、消息窗口线程消费的菜单请求。
#[derive(Clone, Debug, Default)]
pub struct ShellMenuRequest {
    pub path: PathBuf,
    pub screen_x: i32,
    pub screen_y: i32,
    /// 来源盒子；桌面为 None。
    pub from_box: Option<usize>,
    pub item: usize,
    /// 「在盒子内移动」子菜单：`(盒子下标, 标题)`。
    pub boxes: Vec<(usize, String)>,
    /// 程序合成的系统项（如「此电脑」）：只弹系统菜单，不追加文件类动作。
    pub system: bool,
}

struct ActiveMenu {
    ctx2: Option<IContextMenu2>,
    ctx3: Option<IContextMenu3>,
}

thread_local! {
    /// 仅在消息窗口线程访问：当前正在 TrackPopupMenuEx 的 Shell 菜单。
    static ACTIVE: RefCell<Option<ActiveMenu>> = const { RefCell::new(None) };
    /// 消息线程是否正处于原生菜单的模态循环（防 TrackPopupMenuEx 内部消息派发重入）。
    static IN_MODAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 当前桌面菜单是否使用了降级本地视图（决定“新建”子菜单里哪些项必须灰置）。
    static HOST_FALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 菜单句柄守卫：确保 `TrackPopupMenuEx` 返回后的 `InvokeCommand` 期间菜单仍有效，
/// 直到本函数所有分支结束才销毁。
struct MenuGuard(HMENU);

impl Drop for MenuGuard {
    fn drop(&mut self) {
        if !self.0 .0.is_null() {
            // SAFETY: 句柄由本函数 CreatePopupMenu 创建，且只在此处销毁一次。
            unsafe {
                let _ = DestroyMenu(self.0);
            }
        }
    }
}

/// 把“当前菜单是否降级”写入线程局部标记，菜单关闭时自动复位。
struct FallbackScope;

impl FallbackScope {
    fn new(fallback: bool) -> Self {
        HOST_FALLBACK.with(|c| c.set(fallback));
        Self
    }
}

impl Drop for FallbackScope {
    fn drop(&mut self) {
        HOST_FALLBACK.with(|c| c.set(false));
    }
}

/// 从请求入队到原生菜单关闭的全局占用标记（UI 线程与消息线程共享）。
/// 单槽 PENDING 只能覆盖“排队中”的请求，无法表达“原生菜单已经显示”，
/// 所以连点右键时必须靠这个标记把后续请求直接丢弃。
static MENU_OWNED: AtomicBool = AtomicBool::new(false);

static PENDING: Mutex<Option<ShellMenuRequest>> = Mutex::new(None);
static PENDING_DESKTOP: Mutex<Option<(i32, i32)>> = Mutex::new(None);
/// 原生菜单弹出序号：1 为进程内冷启动首次，2 起为热态，用于分段耗时对比。
static MENU_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 毫秒时间戳：诊断日志用，便于对齐 UI 线程与消息线程的时序。
pub(crate) fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 原生菜单当前是否占用中（请求已入队或菜单正在显示）。
pub fn menu_open() -> bool {
    MENU_OWNED.load(Ordering::SeqCst)
}

/// 消息线程当前是否正处于 TrackPopupMenuEx 的模态循环。
pub fn in_modal() -> bool {
    IN_MODAL.with(|c| c.get())
}

/// 释放全局占用标记；只应由菜单实际消失的路径调用。
pub fn release_owned() {
    MENU_OWNED.store(false, Ordering::SeqCst);
}

/// 菜单占用期间的 RAII 守卫：菜单关闭（含失败/取消）时自动释放。
struct OwnedGuard;

impl Drop for OwnedGuard {
    fn drop(&mut self) {
        release_owned();
    }
}

/// 桌面原生菜单请求失败时交回 UI 线程的信息（用于退回内置自绘菜单）。
#[derive(Clone, Debug)]
pub struct DesktopMenuUnavailable {
    pub reason: String,
    pub x: i32,
    pub y: i32,
}

/// UI 线程调用：请求在桌面空白处弹出系统原生菜单。
pub fn request_desktop(x: i32, y: i32) -> Result<(), DesktopMenuUnavailable> {
    log::info!(
        "菜单[t={}] UI 请求桌面菜单 @{x},{y} menu_open={}",
        now_ms(),
        menu_open()
    );
    // 原生菜单已存在（或首个请求仍在途）：直接丢弃，不再入队、不触发降级菜单。
    if MENU_OWNED.swap(true, Ordering::SeqCst) {
        log::info!("菜单[t={}] 菜单已打开，丢弃重复的桌面菜单请求", now_ms());
        return Ok(());
    }
    let result = enqueue_desktop(x, y);
    if result.is_err() {
        release_owned();
    }
    result
}

fn enqueue_desktop(x: i32, y: i32) -> Result<(), DesktopMenuUnavailable> {
    let Some(hwnd) = crate::platform::message_window::hwnd() else {
        return Err(DesktopMenuUnavailable {
            reason: "消息窗口不存在".to_string(),
            x,
            y,
        });
    };
    let Ok(mut slot) = PENDING_DESKTOP.lock() else {
        return Err(DesktopMenuUnavailable {
            reason: "PENDING_DESKTOP 锁失败".to_string(),
            x,
            y,
        });
    };
    *slot = Some((x, y));
    if let Err(e) = unsafe { PostMessageW(Some(hwnd), WM_APP_DESKTOP_MENU, WPARAM(0), LPARAM(0)) } {
        let _ = slot.take();
        return Err(DesktopMenuUnavailable {
            reason: format!("PostMessageW 失败: {e}"),
            x,
            y,
        });
    }
    log::info!("菜单[t={}] 桌面请求已投递", now_ms());
    Ok(())
}

/// 消息窗口线程调用：取出待处理的桌面菜单请求。
pub fn take_desktop_request() -> Option<(i32, i32)> {
    PENDING_DESKTOP.lock().ok().and_then(|mut g| g.take())
}

/// 原生菜单无法建立时返回给 UI 线程：携带原因与原始请求，用于退回内置菜单。
#[derive(Clone, Debug)]
pub struct ShellMenuUnavailable {
    pub reason: String,
    pub request: ShellMenuRequest,
}

/// 测试开关：环境变量或数据目录下的标记文件存在时，强制走降级分支，
/// 用于真机验证「原生菜单不可用 → 内置菜单」。
fn fallback_forced() -> bool {
    std::env::var_os("DESKKEEPER_FORCE_FALLBACK_MENU").is_some()
        || crate::util::data_dir().join("force_fallback_menu").exists()
}

/// 供消息线程判断测试开关（强制走降级事件，模拟原生菜单不可用）。
pub(crate) fn desktop_fallback_forced() -> bool {
    fallback_forced()
}

/// 测试开关原因文案。
pub(crate) const FALLBACK_FORCED_REASON: &str =
    "测试开关（环境变量或 force_fallback_menu 标记文件）";

/// UI 线程调用：把请求交给消息窗口线程弹出原生菜单（立即返回，不阻塞 UI）。
///
/// 消息窗口不存在、请求入队失败或 `PostMessageW` 失败时返回 [`ShellMenuUnavailable`]，
/// 调用方应改用内置自绘菜单（见 `DeskApp::open_fallback_menu`）。
pub fn request(req: ShellMenuRequest) -> Result<(), ShellMenuUnavailable> {
    log::info!(
        "菜单[t={}] UI 请求文件菜单 {} @{},{} from_box={:?} menu_open={}",
        now_ms(),
        req.path.display(),
        req.screen_x,
        req.screen_y,
        req.from_box,
        menu_open()
    );
    // 原生菜单已存在（或首个请求仍在途）：直接丢弃，避免叠加第二个菜单。
    if MENU_OWNED.swap(true, Ordering::SeqCst) {
        log::info!("菜单[t={}] 菜单已打开，丢弃重复的文件菜单请求", now_ms());
        return Ok(());
    }
    let result = enqueue_file(req);
    if result.is_err() {
        release_owned();
    }
    result
}

fn enqueue_file(req: ShellMenuRequest) -> Result<(), ShellMenuUnavailable> {
    // 测试开关：强制走降级分支，用于真机验证「原生菜单不可用 → 内置菜单」。
    if fallback_forced() {
        return Err(ShellMenuUnavailable {
            reason: "测试开关（环境变量或 force_fallback_menu 标记文件）".to_string(),
            request: req,
        });
    }
    let Some(hwnd) = crate::platform::message_window::hwnd() else {
        return Err(ShellMenuUnavailable {
            reason: "消息窗口不存在".to_string(),
            request: req,
        });
    };
    let Ok(mut slot) = PENDING.lock() else {
        return Err(ShellMenuUnavailable {
            reason: "PENDING 锁失败".to_string(),
            request: req,
        });
    };
    *slot = Some(req);
    // SAFETY: hwnd 为本进程隐藏消息窗口。
    if let Err(e) = unsafe { PostMessageW(Some(hwnd), WM_APP_SHELL_MENU, WPARAM(0), LPARAM(0)) } {
        // 消息没发出去，把刚入队的请求取回来交给内置菜单。
        let request = slot.take().unwrap_or_default();
        return Err(ShellMenuUnavailable {
            reason: format!("PostMessageW 失败: {e}"),
            request,
        });
    }
    log::info!("菜单[t={}] 文件请求已投递", now_ms());
    Ok(())
}

/// 消息窗口线程调用：取出待处理请求并弹菜单。
pub fn take_request() -> Option<ShellMenuRequest> {
    PENDING.lock().ok().and_then(|mut g| g.take())
}

/// 消息窗口线程调用：显示系统菜单并处理选择。
///
/// 返回 `Some((请求, 原因))` 表示系统菜单在任何阶段建立失败，调用方应把请求交回
/// UI 线程改用内置菜单；返回 `None` 表示菜单已正常显示并处理完毕（含用户取消）。
pub fn show(
    req: ShellMenuRequest,
    owner: HWND,
    sink: &EventSink,
) -> Option<(ShellMenuRequest, &'static str)> {
    // 消息线程已处于另一个原生菜单的模态循环：直接丢弃，不再嵌套弹出。
    if in_modal() {
        log::info!(
            "菜单[t={}] 已有原生菜单在模态循环中，丢弃嵌套文件菜单请求",
            now_ms()
        );
        return None;
    }
    // 本请求已占用 MENU_OWNED；无论成功、取消还是失败都要释放。
    let _owned = OwnedGuard;
    let modal = IN_MODAL.with(|c| c.get());
    let seq = MENU_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let t_start = now_ms();
    log::info!(
        "菜单[t={}] show 文件菜单进入 seq={seq} path={} in_modal={modal}",
        t_start,
        req.path.display()
    );
    // SAFETY: 本线程首次使用 COM；失败通常是已初始化，可继续。
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok();
    let t_com = now_ms();
    // SAFETY: req.path 有效；ILCreateFromPathW 返回由 CoTaskMemFree 释放的 PIDL。
    unsafe {
        let wide = HSTRING::from(req.path.as_os_str());
        let pidl = ILCreateFromPathW(&wide);
        if pidl.is_null() {
            log::warn!(
                "原生菜单：ILCreateFromPathW 返回空 PIDL: {}",
                req.path.display()
            );
            return Some((req, "ILCreateFromPathW 返回空 PIDL"));
        }
        let mut child: *mut windows::Win32::UI::Shell::Common::ITEMIDLIST = std::ptr::null_mut();
        let t_pidl = now_ms();
        let parent = match SHBindToParent::<IShellFolder>(pidl, Some(&mut child)) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("原生菜单：SHBindToParent 失败: {e}");
                CoTaskMemFree(Some(pidl as *const core::ffi::c_void));
                return Some((req, "SHBindToParent 失败"));
            }
        };
        let t_bind = now_ms();
        let ctx = match parent.GetUIObjectOf::<IContextMenu>(owner, &[child as *const _], None) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("原生菜单：GetUIObjectOf(IContextMenu) 失败: {e}");
                CoTaskMemFree(Some(pidl as *const core::ffi::c_void));
                return Some((req, "GetUIObjectOf(IContextMenu) 失败"));
            }
        };
        let t_ctx = now_ms();
        CoTaskMemFree(Some(pidl as *const core::ffi::c_void));

        let Ok(menu) = CreatePopupMenu() else {
            log::warn!("原生菜单：CreatePopupMenu 失败");
            return Some((req, "CreatePopupMenu 失败"));
        };
        let query = ctx.QueryContextMenu(
            menu,
            0,
            1,
            0x6FFF,
            windows::Win32::UI::Shell::CMF_NORMAL | windows::Win32::UI::Shell::CMF_EXPLORE,
        );
        if let Err(e) = query.ok() {
            log::warn!("原生菜单：QueryContextMenu 失败: {e}");
            let _ = DestroyMenu(menu);
            return Some((req, "QueryContextMenu 失败"));
        }
        let t_query = now_ms();
        // 项数用于判断耗时是否随 shell 扩展数量膨胀（QueryContextMenu 同步加载全部扩展）。
        let items = GetMenuItemCount(Some(menu)).max(0) as usize;

        if !req.system {
            // 系统的「重命名」verb 在隐藏 DefView 层执行，用户看不到任何反馈；
            // 按语言无关的 canonical verb 精确删除该项，只保留应用内行内重命名。
            let verbs = menu_verbs(&ctx, menu);
            if let Some((id, _)) = verbs.iter().find(|(_, verb)| verb == "rename") {
                // SAFETY: menu 由本函数创建，id 来自刚枚举出的菜单命令号。
                let _ = DeleteMenu(menu, *id as u32, MF_BYCOMMAND);
                log::info!("原生菜单：已按 verb 抑制系统「重命名」(id={id})，统一走应用内重命名");
            } else {
                log::warn!(
                    "原生菜单：未在菜单项中找到 rename verb（共 {} 项），保留系统项",
                    verbs.len()
                );
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            // 自有动作统一收进「DeskKeeper」二级子菜单，避免与系统项平行堆在底部；
            // 降级自绘菜单保持同样的层级（见 app.rs 的内置文件菜单）。
            if let Ok(sub) = CreatePopupMenu() {
                let _ = AppendMenuW(sub, MF_STRING, ID_RENAME, &HSTRING::from("重命名"));
                let _ = AppendMenuW(sub, MF_STRING, ID_COPY_PATH, &HSTRING::from("复制路径"));
                let _ = AppendMenuW(
                    sub,
                    MF_STRING,
                    ID_DELETE,
                    &HSTRING::from("删除（移到回收站）"),
                );
                let _ = AppendMenuW(
                    sub,
                    MF_STRING,
                    ID_ADD_CLIP,
                    &HSTRING::from("添加到剪贴板盒"),
                );
                if req.from_box.is_some() {
                    let _ = AppendMenuW(
                        sub,
                        MF_STRING,
                        ID_EJECT,
                        &HSTRING::from("移出盒子（放回桌面）"),
                    );
                }
                if !req.boxes.is_empty() {
                    if let Ok(move_sub) = CreatePopupMenu() {
                        for (box_idx, title) in &req.boxes {
                            let _ = AppendMenuW(
                                move_sub,
                                MF_STRING,
                                ID_MOVE_BASE + *box_idx,
                                &HSTRING::from(title.as_str()),
                            );
                        }
                        let _ = AppendMenuW(
                            sub,
                            MF_POPUP | MF_STRING,
                            move_sub.0 as usize,
                            &HSTRING::from("在盒子内移动"),
                        );
                    }
                }
                let _ = AppendMenuW(
                    menu,
                    MF_POPUP | MF_STRING,
                    sub.0 as usize,
                    &HSTRING::from("DeskKeeper"),
                );
            }
        }

        let ctx2 = ctx.cast::<IContextMenu2>().ok();
        let ctx3 = ctx.cast::<IContextMenu3>().ok();
        ACTIVE.with(|slot| {
            *slot.borrow_mut() = Some(ActiveMenu { ctx2, ctx3 });
        });

        let t_build = now_ms();
        // SAFETY: owner 为本线程隐藏消息窗口，最大化兼容 shell 菜单焦点语义。
        let _ = SetForegroundWindow(owner);
        let t_fg = now_ms();
        SetLastError(ERROR_SUCCESS);
        IN_MODAL.with(|c| c.set(true));
        let t_enter = now_ms();
        log::info!(
            "菜单分段[t={}] seq={seq} ext={:?} com={}ms pidl={}ms bind={}ms ctx={}ms query={}ms items={items} build={}ms fg={}ms pre_track={}ms",
            t_enter,
            req.path.extension().and_then(|e| e.to_str()),
            t_com.saturating_sub(t_start),
            t_pidl.saturating_sub(t_start),
            t_bind.saturating_sub(t_start),
            t_ctx.saturating_sub(t_start),
            t_query.saturating_sub(t_start),
            t_build.saturating_sub(t_query),
            t_fg.saturating_sub(t_build),
            t_enter.saturating_sub(t_fg),
        );
        let cmd = TrackPopupMenuEx(
            menu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            req.screen_x,
            req.screen_y,
            owner,
            None,
        )
        .0 as usize;
        IN_MODAL.with(|c| c.set(false));
        // 返回 0 既可能是用户取消，也可能是调用失败；只有 last error 非 0 才算失败。
        let last_error = GetLastError();
        log::info!(
            "菜单[t={}] TrackPopupMenuEx 返回 cmd={cmd} err={} 耗时={}ms",
            now_ms(),
            last_error.0,
            now_ms().saturating_sub(t_enter)
        );
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        ACTIVE.with(|slot| {
            *slot.borrow_mut() = None;
        });
        let _ = DestroyMenu(menu);

        if popup_failed(cmd, last_error.0) {
            log::warn!("原生菜单：TrackPopupMenuEx 失败: {last_error:?}");
            return Some((req, "TrackPopupMenuEx 失败"));
        }
        if cmd == 0 {
            return None;
        }
        if cmd >= ID_MOVE_BASE {
            let to = cmd - ID_MOVE_BASE;
            sink.emit(BgEvent::ShellMenu(ShellMenuAction::MoveToBox {
                from: req.from_box,
                item: req.item,
                to,
            }));
            return None;
        }
        match cmd {
            ID_RENAME => sink.emit(BgEvent::ShellMenu(ShellMenuAction::Rename {
                box_idx: req.from_box,
                item: req.item,
            })),
            ID_COPY_PATH => sink.emit(BgEvent::ShellMenu(ShellMenuAction::CopyPath(req.path))),
            ID_DELETE => sink.emit(BgEvent::ShellMenu(ShellMenuAction::Delete {
                box_idx: req.from_box,
                item: req.item,
            })),
            ID_ADD_CLIP => sink.emit(BgEvent::ShellMenu(ShellMenuAction::AddToClipboard(
                req.path,
            ))),
            ID_EJECT => sink.emit(BgEvent::ShellMenu(ShellMenuAction::Eject {
                box_idx: req.from_box.unwrap_or(0),
                item: req.item,
            })),
            _ => {
                // 系统菜单项：QueryContextMenu 时 idCmdFirst=1，因此动词下标为 cmd-1。
                let verb = command_verb(&ctx, cmd - 1);
                let info = windows::Win32::UI::Shell::CMINVOKECOMMANDINFO {
                    cbSize: std::mem::size_of::<windows::Win32::UI::Shell::CMINVOKECOMMANDINFO>()
                        as u32,
                    hwnd: owner,
                    lpVerb: PCSTR((cmd - 1) as *const u8),
                    nShow: SW_SHOWNORMAL.0,
                    ..Default::default()
                };
                if let Err(e) = ctx.InvokeCommand(&info) {
                    log::warn!("原生菜单：InvokeCommand({}) 失败: {e}", cmd - 1);
                } else if let Some(verb) = verb {
                    // 删除/剪切后文件可能立即消失（删除）或稍后才消失（剪切后粘贴）；
                    // 只上报事实，由 UI 线程在路径真正不存在时清理盒子引用。
                    if verb == "delete" || verb == "cut" {
                        log::info!("原生菜单：系统 {verb} 已执行 path={}", req.path.display());
                        sink.emit(BgEvent::ShellMenu(ShellMenuAction::SystemVerb {
                            verb,
                            path: req.path.clone(),
                        }));
                    }
                }
            }
        }
        None
    }
}

/// 桌面菜单的命令执行宿主。
struct DesktopHost {
    ctx: IContextMenu,
    /// `InvokeCommand` 使用的宿主窗口：真实桌面视图用 DefView 所在 HWND；
    /// 降级本地视图用本进程隐藏窗口。
    invoke_hwnd: HWND,
    /// 是否取到真实桌面视图（决定“新建”叶子项能否按 numeric offset 执行）。
    real: bool,
}

/// 取真实桌面视图的后台上下文菜单。
///
/// 链路：`IShellWindows` → `FindWindowSW(SWC_DESKTOP, SWFO_NEEDDISPATCH)` →
/// `IServiceProvider` → `QueryService(SID_STopLevelBrowser)` → `IShellBrowser` →
/// `QueryActiveShellView()` → `IShellView` → `GetItemObject(SVGIO_BACKGROUND)`。
///
/// 上一轮失败的根因：`FindWindowSW` 的两个 VARIANT 参数必须是 **VT_EMPTY**；
/// 传 `VT_I4(SWC_DESKTOP)` / 空指针会分别得到 0x800706F4 / 0x8000000A。
/// 桌面视图即使被我们 `ShowWindow(SW_HIDE)` 隐藏，这个 COM 对象仍然存活，
/// 命令可以照常执行（已用 Excel/Word/PPT/BMP/文件夹/刷新实测）。
unsafe fn real_desktop_host() -> windows::core::Result<(IContextMenu, HWND)> {
    let sw: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)?;
    let loc = VARIANT::default();
    let mut hwnd: i32 = 0;
    let disp = sw.FindWindowSW(&loc, &loc, SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH)?;
    let sp: IServiceProvider = disp.cast()?;
    let browser: IShellBrowser = sp.QueryService(&SID_STopLevelBrowser)?;
    let view = browser.QueryActiveShellView()?;
    let ctx = view.GetItemObject::<IContextMenu>(SVGIO_BACKGROUND)?;
    Ok((ctx, HWND(hwnd as *mut core::ffi::c_void)))
}

/// 消息窗口线程调用：显示 Windows 桌面原生右键菜单，并在其中追加 DeskKeeper 子菜单。
///
/// 返回 `Some(原因)` 表示系统桌面菜单建立失败，UI 线程应退回内置自绘菜单。
pub fn show_desktop(x: i32, y: i32, owner: HWND, sink: &EventSink) -> Option<&'static str> {
    // 消息线程已处于另一个原生菜单的模态循环：直接丢弃，不再嵌套弹出。
    if in_modal() {
        log::info!(
            "菜单[t={}] 已有原生菜单在模态循环中，丢弃嵌套桌面菜单请求",
            now_ms()
        );
        return None;
    }
    // 本请求已占用 MENU_OWNED；无论成功、取消还是失败都要释放。
    let _owned = OwnedGuard;
    let modal = IN_MODAL.with(|c| c.get());
    log::info!(
        "菜单[t={}] show 桌面菜单进入 @{x},{y} in_modal={modal}",
        now_ms()
    );
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok();
    // SAFETY: 所有 COM 对象都在本线程内使用，生命周期覆盖 TrackPopupMenuEx。
    unsafe {
        // 主路径：真实桌面视图（DefView 隐藏也不影响命令执行）。
        let host = match real_desktop_host() {
            Ok((ctx, hwnd)) => {
                log::info!(
                    "桌面原生菜单：使用真实桌面视图 host=0x{:x}（DefView 隐藏不影响执行）",
                    hwnd.0 as usize
                );
                DesktopHost {
                    ctx,
                    invoke_hwnd: hwnd,
                    real: true,
                }
            }
            Err(e) => {
                log::warn!(
                    "桌面原生菜单：真实桌面视图不可用 0x{:08x} {e}，降级为本地视图；\
                     无法执行的“新建”项将灰置并说明原因",
                    e.code().0
                );
                let folder = match SHGetDesktopFolder() {
                    Ok(f) => f,
                    Err(e) => {
                        log::warn!("桌面原生菜单：SHGetDesktopFolder 失败: {e}");
                        return Some("SHGetDesktopFolder 失败");
                    }
                };
                let view = match folder.CreateViewObject::<IShellView>(owner) {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("桌面原生菜单：CreateViewObject 失败: {e}");
                        return Some("CreateViewObject 失败");
                    }
                };
                let ctx = match view.GetItemObject::<IContextMenu>(SVGIO_BACKGROUND) {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("桌面原生菜单：GetItemObject(SVGIO_BACKGROUND) 失败: {e}");
                        return Some("GetItemObject 失败");
                    }
                };
                DesktopHost {
                    ctx,
                    invoke_hwnd: owner,
                    real: false,
                }
            }
        };
        let ctx = &host.ctx;
        let Ok(menu) = CreatePopupMenu() else {
            log::warn!("桌面原生菜单：CreatePopupMenu 失败");
            return Some("CreatePopupMenu 失败");
        };
        // 菜单必须活到 InvokeCommand 之后：Shell 的 numeric 命令映射依赖
        // QueryContextMenu 时的菜单句柄；提前 DestroyMenu 会让 InvokeCommand
        // 返回 E_FAIL（0x80004005）。
        let _menu_guard = MenuGuard(menu);
        let query = ctx.QueryContextMenu(
            menu,
            0,
            1,
            0x6FFF,
            windows::Win32::UI::Shell::CMF_NORMAL | windows::Win32::UI::Shell::CMF_EXPLORE,
        );
        if let Err(e) = query.ok() {
            log::warn!("桌面原生菜单：QueryContextMenu 失败: {e}");
            let _ = DestroyMenu(menu);
            return Some("QueryContextMenu 失败");
        }

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        if let Ok(sub) = CreatePopupMenu() {
            let _ = AppendMenuW(sub, MF_STRING, ID_D_AUTO_GROUP, &HSTRING::from("自动整理"));
            let _ = AppendMenuW(
                sub,
                MF_STRING,
                ID_D_RESTORE_AUTO,
                &HSTRING::from("恢复自动排列"),
            );
            let _ = AppendMenuW(
                sub,
                MF_STRING,
                ID_D_ENABLE_BOARD,
                &HSTRING::from("启用棋盘摆放（拖到空格）"),
            );
            let _ = AppendMenuW(sub, MF_STRING, ID_D_NEW_BOX, &HSTRING::from("新建盒子"));
            let _ = AppendMenuW(
                sub,
                MF_STRING,
                ID_D_NEW_CLIP,
                &HSTRING::from("新建剪贴板盒"),
            );
            let _ = AppendMenuW(sub, MF_STRING, ID_D_NEW_TODO, &HSTRING::from("新建待办盒"));
            let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(sub, MF_STRING, ID_D_SETTINGS, &HSTRING::from("设置"));
            let _ = AppendMenuW(sub, MF_STRING, ID_D_RELOAD, &HSTRING::from("重新加载桌面"));
            let _ = AppendMenuW(sub, MF_STRING, ID_D_UNDO, &HSTRING::from("撤销上一步移动"));
            let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(
                sub,
                MF_STRING,
                ID_D_TOGGLE_OVERLAY,
                &HSTRING::from("显示/隐藏整理层"),
            );
            let _ = AppendMenuW(
                menu,
                MF_POPUP | MF_STRING,
                sub.0 as usize,
                &HSTRING::from("DeskKeeper"),
            );
        }

        let ctx2 = ctx.cast::<IContextMenu2>().ok();
        let ctx3 = ctx.cast::<IContextMenu3>().ok();
        ACTIVE.with(|slot| {
            *slot.borrow_mut() = Some(ActiveMenu { ctx2, ctx3 });
        });
        // 降级视图的“新建”子菜单填充时，需要把不可执行项灰置并附说明。
        let _fallback = FallbackScope::new(!host.real);

        let _ = SetForegroundWindow(owner);
        SetLastError(ERROR_SUCCESS);
        IN_MODAL.with(|c| c.set(true));
        let t_enter = now_ms();
        let cmd = TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, x, y, owner, None).0
            as usize;
        IN_MODAL.with(|c| c.set(false));
        let last_error = GetLastError();
        log::info!(
            "菜单[t={}] 桌面 TrackPopupMenuEx 返回 cmd={cmd} err={} 耗时={}ms",
            now_ms(),
            last_error.0,
            now_ms().saturating_sub(t_enter)
        );
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        ACTIVE.with(|slot| {
            *slot.borrow_mut() = None;
        });
        // 命令号对应的菜单文字需要在销毁菜单前读取：系统“新建”子菜单是惰性填充的，
        // 只能靠文字兜住没有 canonical verb 的项（如“文本文档”）。
        let item_text = if cmd == 0 {
            None
        } else {
            menu_item_text_by_id(menu, cmd as u32)
        };

        if popup_failed(cmd, last_error.0) {
            log::warn!("桌面原生菜单：TrackPopupMenuEx 失败: {last_error:?}");
            return Some("TrackPopupMenuEx 失败");
        }
        let action = match cmd {
            ID_D_AUTO_GROUP => Some(DesktopMenuAction::AutoGroup),
            ID_D_NEW_BOX => Some(DesktopMenuAction::NewBox),
            ID_D_NEW_CLIP => Some(DesktopMenuAction::NewClipboardBox),
            ID_D_NEW_TODO => Some(DesktopMenuAction::NewTodoBox),
            ID_D_SETTINGS => Some(DesktopMenuAction::Settings),
            ID_D_RELOAD => Some(DesktopMenuAction::ReloadDesktop),
            ID_D_UNDO => Some(DesktopMenuAction::UndoLast),
            ID_D_TOGGLE_OVERLAY => Some(DesktopMenuAction::ToggleOverlay),
            ID_D_RESTORE_AUTO => Some(DesktopMenuAction::RestoreAutoArrange),
            ID_D_ENABLE_BOARD => Some(DesktopMenuAction::EnableBoardPlacement),
            _ => None,
        };
        if let Some(action) = action {
            sink.emit(BgEvent::DesktopMenu(action));
            return None;
        }
        if cmd == 0 {
            return None;
        }
        if cmd == ID_D_NEW_TEXT {
            match create_new_text_document() {
                Ok(path) => {
                    log::info!("桌面原生菜单：已创建文本文档 {}", path.display());
                    sink.emit(BgEvent::DesktopMenu(DesktopMenuAction::ReloadDesktop));
                }
                Err(e) => log::warn!("桌面原生菜单：创建文本文档失败: {e}"),
            }
            return None;
        }
        // 系统项：idCmdFirst=1，动词下标为 cmd-1。
        let text = item_text.as_deref().unwrap_or("");
        log::info!(
            "桌面原生菜单：调用系统命令 #{} text={:?} host_real={}",
            cmd,
            text,
            host.real
        );
        // “新建 → 快捷方式”：numeric 调用只会生成空 .lnk，统一拉起系统原生的
        // “创建快捷方式”向导（AppWiz.Cpl,NewLinkHere）。真实/降级视图都适用。
        let is_shortcut = text.contains("快捷方式")
            || text.to_ascii_lowercase().contains("shortcut")
            || command_verb_raw(ctx, cmd - 1).is_some_and(|v| v.eq_ignore_ascii_case("NewLink"));
        if is_shortcut {
            match desktop_dir() {
                Some(dir) => {
                    let script = format!("AppWiz.Cpl,NewLinkHere {}", dir.display());
                    match std::process::Command::new("rundll32.exe")
                        .arg(script)
                        .spawn()
                    {
                        Ok(_) => {
                            log::info!("桌面原生菜单：已拉起新建快捷方式向导 {}", dir.display());
                        }
                        Err(e) => log::warn!("桌面原生菜单：拉起快捷方式向导失败: {e}"),
                    }
                }
                None => log::warn!("桌面原生菜单：无法定位桌面目录，快捷方式向导未启动"),
            }
            return None;
        }
        // “文本文档”等 ShellNew 项没有 canonical verb，改由我们自己按 ShellNew 语义创建。
        if text.contains("文本文档") || text.contains("Text Document") {
            match create_new_text_document() {
                Ok(path) => {
                    log::info!("桌面原生菜单：已创建文本文档 {}", path.display());
                    sink.emit(BgEvent::DesktopMenu(DesktopMenuAction::ReloadDesktop));
                }
                Err(e) => log::warn!("桌面原生菜单：创建文本文档失败: {e}"),
            }
            return None;
        }

        let is_refresh = text.contains("刷新")
            || text.to_ascii_lowercase().contains("refresh")
            || command_verb_raw(ctx, cmd - 1).is_some_and(|v| v.eq_ignore_ascii_case("refresh"));
        if !host.real && is_refresh {
            // 降级本地视图的 refresh verb 在隐藏 DefView 上返回 0x80070483
            // （ERROR_NO_ASSOCIATION，系统没有为这条命令建立关联）。
            // 改为让 Shell 广播全局刷新，并让整理层重载桌面，保证真的生效。
            SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
            log::info!("桌面原生菜单：降级路径刷新改用 SHChangeNotify(SHCNE_ASSOCCHANGED)");
            sink.emit(BgEvent::DesktopMenu(DesktopMenuAction::ReloadDesktop));
            return None;
        }

        // 降级视图仍可用的项：文件夹（NewFolder verb）等 canonical verb。
        if !host.real {
            if let Some(verb) = command_verb_raw(ctx, cmd - 1) {
                log::info!("桌面原生菜单：降级路径使用 canonical verb {verb:?}");
                match invoke_with_verb(ctx, owner, &verb) {
                    Ok(()) => {
                        log::info!("桌面原生菜单：InvokeCommand verb={verb:?} 成功 hresult=0")
                    }
                    Err(e) => {
                        log::warn!(
                            "桌面原生菜单：InvokeCommand verb={verb:?} 失败 hresult=0x{:08x} {e}",
                            e.code().0
                        );
                        show_failure_dialog(text, e.code().0);
                    }
                }
                return None;
            }
        }

        // 主路径：numeric offset。真实桌面视图的菜单 id 并不连续（刷新=4、Excel=55），
        // 但 InvokeCommand 接收的是相对 idCmdFirst=1 的偏移，cmd-1 就是正确 offset
        // （已用 Excel/Word/PPT/BMP/文件夹/刷新在 DefView 隐藏状态下实测）。
        let offset = cmd - 1;
        match invoke_numeric(ctx, host.invoke_hwnd, offset) {
            Ok(()) => {
                log::info!(
                    "桌面原生菜单：InvokeCommand({offset}) 成功 host_real={}",
                    host.real
                );
                if is_refresh {
                    sink.emit(BgEvent::DesktopMenu(DesktopMenuAction::ReloadDesktop));
                }
            }
            Err(e) => {
                log::warn!(
                    "桌面原生菜单：InvokeCommand({offset}) 失败 hresult=0x{:08x} {e}",
                    e.code().0
                );
                show_failure_dialog(text, e.code().0);
            }
        }
        None
    }
}

/// numeric offset 调用：`lpVerb` 的低位当作相对 `idCmdFirst` 的命令偏移。
fn invoke_numeric(ctx: &IContextMenu, hwnd: HWND, offset: usize) -> windows::core::Result<()> {
    let info = windows::Win32::UI::Shell::CMINVOKECOMMANDINFO {
        cbSize: std::mem::size_of::<windows::Win32::UI::Shell::CMINVOKECOMMANDINFO>() as u32,
        hwnd,
        lpVerb: PCSTR(offset as *const u8),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: offset 编码为 lpVerb 的整数形式，ctx/hwnd 由调用方保证有效。
    unsafe { ctx.InvokeCommand(&info) }
}

/// 命令最终无法执行时给出可见提示，避免“点了没反应”。
fn show_failure_dialog(item_text: &str, hresult: i32) {
    let msg = format!(
        "“{}”未能执行（HRESULT 0x{:08X}）。\n\
         该操作需要系统桌面宿主支持，请确认资源管理器正在运行后重试。",
        item_text, hresult
    );
    // SAFETY: 栈上 HSTRING 生命周期覆盖 MessageBoxW 调用。
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(msg.as_str()),
            &HSTRING::from("DeskKeeper"),
            MB_OK | MB_ICONWARNING,
        );
    }
}

use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// 读取菜单项的语言无关动词（`GCS_VERBW`）：返回 `(菜单命令号, 小写 verb)`。
///
/// `QueryContextMenu` 使用 `idCmdFirst=1`，因此第 `index` 个菜单项对应命令号 `index+1`，
/// `GetCommandString` 的 `idcmd` 参数则是从 0 开始的动词下标。
fn menu_verbs(ctx: &IContextMenu, menu: HMENU) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    // SAFETY: menu 由调用方创建且在本函数期间有效。
    let count = unsafe { GetMenuItemCount(Some(menu)) };
    if count <= 0 {
        return out;
    }
    for index in 0..count as usize {
        if let Some(verb) = command_verb(ctx, index) {
            out.push((index + 1, verb));
        }
    }
    out
}

/// 读取单个菜单项的语言无关动词（`GCS_VERBW`），返回小写。
fn command_verb(ctx: &IContextMenu, index: usize) -> Option<String> {
    let mut buf = [0u16; 128];
    // SAFETY: buf 为栈上有效 UTF-16 缓冲；index 为从 0 开始的动词下标。
    let ok = unsafe {
        ctx.GetCommandString(
            index,
            GCS_VERBW,
            None,
            PSTR(buf.as_mut_ptr() as *mut u8),
            buf.len() as u32,
        )
    };
    if ok.is_err() {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]).to_ascii_lowercase())
}

/// 读取菜单项语言无关动词，保留原始大小写（InvokeCommand 需要 canonical 字符串）。
fn command_verb_raw(ctx: &IContextMenu, index: usize) -> Option<String> {
    let mut buf = [0u16; 128];
    // SAFETY: buf 为栈上有效 UTF-16 缓冲；index 为从 0 开始的动词下标。
    let ok = unsafe {
        ctx.GetCommandString(
            index,
            GCS_VERBW,
            None,
            PSTR(buf.as_mut_ptr() as *mut u8),
            buf.len() as u32,
        )
    };
    if ok.is_err() {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// 用 canonical verb 字符串调用系统菜单命令。
///
/// 实测“新建”子菜单的叶子项 numeric offset 调用返回 `E_FAIL`（0x80004005），
/// 而字符串 verb（NewFolder / NewLink）可正常执行。
fn invoke_with_verb(ctx: &IContextMenu, owner: HWND, verb: &str) -> windows::core::Result<()> {
    let mut bytes: Vec<u8> = verb.as_bytes().to_vec();
    bytes.push(0);
    let info = windows::Win32::UI::Shell::CMINVOKECOMMANDINFO {
        cbSize: std::mem::size_of::<windows::Win32::UI::Shell::CMINVOKECOMMANDINFO>() as u32,
        hwnd: owner,
        lpVerb: PCSTR(bytes.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: bytes 生命周期覆盖调用；ctx/menu 来自调用方且仍有效。
    unsafe { ctx.InvokeCommand(&info) }
}

/// 按 id 递归查找菜单项文字（惰性填充的“新建”子菜单也能取到）。
fn menu_item_text_by_id(menu: HMENU, id: u32) -> Option<String> {
    if id == 0 {
        return None;
    }
    // SAFETY: menu 由调用方创建且在销毁前调用。
    unsafe {
        let count = GetMenuItemCount(Some(menu));
        for pos in 0..count.max(0) as u32 {
            let mut info = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID,
                ..Default::default()
            };
            let _ = GetMenuItemInfoW(menu, pos, true, &mut info);
            if info.wID == id {
                let mut buf = [0u16; 256];
                let mut text = MENUITEMINFOW {
                    cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_STRING,
                    dwTypeData: windows::core::PWSTR(buf.as_mut_ptr()),
                    cch: buf.len() as u32,
                    ..Default::default()
                };
                let _ = GetMenuItemInfoW(menu, pos, true, &mut text);
                let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                return Some(String::from_utf16_lossy(&buf[..end]));
            }
            let sub = GetSubMenu(menu, pos as i32);
            if !sub.0.is_null() {
                if let Some(text) = menu_item_text_by_id(sub, id) {
                    return Some(text);
                }
            }
        }
    }
    None
}

/// 桌面目录（支持 OneDrive 重定向）。
fn desktop_dir() -> Option<PathBuf> {
    // SAFETY: 返回值由 SHGetKnownFolderPath 分配，读取后立即释放。
    unsafe {
        let pw = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None).ok()?;
        let path = pw.to_string().ok()?;
        CoTaskMemFree(Some(pw.0 as *const _));
        Some(PathBuf::from(path))
    }
}

/// 系统“新建 → 文本文档”的等价实现：在桌面创建唯一命名的空 .txt。
///
/// 该菜单项没有 canonical verb，`InvokeCommand` 的 numeric offset 会失败；
/// 内容为空与 ShellNew 的 `NullFile` 语义一致。
pub(crate) fn create_new_text_document() -> std::io::Result<PathBuf> {
    let dir = desktop_dir().ok_or_else(|| std::io::Error::other("无法定位桌面目录"))?;
    for n in 0..100 {
        let name = if n == 0 {
            "新建文本文档.txt".to_string()
        } else {
            format!("新建文本文档 ({n}).txt")
        };
        let path = dir.join(name);
        if !path.exists() {
            std::fs::write(&path, [])?;
            return Ok(path);
        }
    }
    Err(std::io::Error::other("无法生成不重复的文本文档名"))
}

/// 在桌面创建唯一命名的空文件夹（盒子右键“新建到桌面 → 文件夹”）。
pub(crate) fn create_new_folder() -> std::io::Result<PathBuf> {
    let dir = desktop_dir().ok_or_else(|| std::io::Error::other("无法定位桌面目录"))?;
    for n in 0..100 {
        let name = if n == 0 {
            "新建文件夹".to_string()
        } else {
            format!("新建文件夹 ({n})")
        };
        let path = dir.join(name);
        if !path.exists() {
            std::fs::create_dir(&path)?;
            return Ok(path);
        }
    }
    Err(std::io::Error::other("无法生成不重复的文件夹名"))
}

/// `TrackPopupMenuEx` 返回 0 时既可能是用户取消，也可能是调用失败：
/// 只有传入的 last error 非 0 才判定为失败并降级到内置菜单。
fn popup_failed(cmd: usize, last_error: u32) -> bool {
    cmd == 0 && last_error != 0
}

/// 在系统“新建”子菜单里补一项“文本文档”：
/// 部分机器没有注册 `.txt\ShellNew`，系统不会生成该项；补上后与系统项共用同一子菜单。
fn ensure_text_new_item(menu: HMENU) {
    if menu.0.is_null() {
        return;
    }
    // SAFETY: menu 为 WM_INITMENUPOPUP 传入、当前模态菜单中的有效句柄。
    unsafe {
        let count = GetMenuItemCount(Some(menu));
        if count <= 0 {
            return;
        }
        // “新建”子菜单首项是“文件夹”；据此识别，避免污染其它系统菜单。
        let first = menu_item_text_at(menu, 0);
        if !first.contains("文件夹") {
            return;
        }
    }
    if !menu_has_id(menu, ID_D_NEW_TEXT as u32) {
        // 插在“快捷方式”之后（位置 2），避免因为系统项过多而排到屏幕外。
        // SAFETY: menu 为当前模态菜单中的“新建”子菜单句柄。
        if unsafe {
            InsertMenuW(
                menu,
                2,
                MF_BYPOSITION | MF_STRING,
                ID_D_NEW_TEXT,
                &HSTRING::from("文本文档(D)"),
            )
        }
        .is_ok()
        {
            log::info!("桌面原生菜单：已在“新建”子菜单补充文本文档项");
        }
    }
    if HOST_FALLBACK.with(|c| c.get()) {
        disable_unavailable_new_items(menu);
    }
}

/// 降级本地视图下，把无法执行的“新建”叶子项灰置，并追加一条灰色说明，
/// 避免用户点到“没反应”的项。
fn disable_unavailable_new_items(menu: HMENU) {
    let mut grayed = 0u32;
    let mut has_hint = false;
    // SAFETY: menu 为“新建”子菜单的有效句柄。
    unsafe {
        let count = GetMenuItemCount(Some(menu));
        for pos in 0..count.max(0) as u32 {
            let text = menu_item_text_at(menu, pos);
            if text.starts_with("（灰置项") {
                has_hint = true;
            }
            // 仅保留降级视图实际可执行的项：文件夹（NewFolder verb）、
            // 快捷方式（向导）、文本文档（本应用自建）。
            if text.contains("文件夹") || text.contains("快捷方式") || text.contains("Shortcut")
            {
                continue;
            }
            let mut info = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID,
                ..Default::default()
            };
            let _ = GetMenuItemInfoW(menu, pos, true, &mut info);
            let id = info.wID;
            if id == 0 || id == u32::MAX || id == ID_D_NEW_TEXT as u32 || text.is_empty() {
                continue;
            }
            let _ = EnableMenuItem(menu, id, MF_BYCOMMAND | MF_GRAYED);
            grayed += 1;
        }
        if grayed > 0 && !has_hint {
            let _ = AppendMenuW(
                menu,
                MF_STRING | MF_GRAYED,
                0,
                &HSTRING::from("（灰置项暂不可用：未连接到真实桌面宿主）"),
            );
        }
    }
    if grayed > 0 {
        log::info!("桌面原生菜单：降级路径已灰置 {grayed} 个不可用的“新建”项并追加说明");
    }
}

/// 菜单（含子菜单）里是否已有指定命令号。
fn menu_has_id(menu: HMENU, id: u32) -> bool {
    // SAFETY: menu 在调用期间有效。
    unsafe {
        let count = GetMenuItemCount(Some(menu));
        for pos in 0..count.max(0) as u32 {
            let mut info = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID,
                ..Default::default()
            };
            let _ = GetMenuItemInfoW(menu, pos, true, &mut info);
            if info.wID == id {
                return true;
            }
            let sub = GetSubMenu(menu, pos as i32);
            if !sub.0.is_null() && menu_has_id(sub, id) {
                return true;
            }
        }
    }
    false
}

/// 读取指定位置的菜单项文字（不递归）。
fn menu_item_text_at(menu: HMENU, pos: u32) -> String {
    let mut buf = [0u16; 256];
    let mut info = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STRING,
        dwTypeData: windows::core::PWSTR(buf.as_mut_ptr()),
        cch: buf.len() as u32,
        ..Default::default()
    };
    // SAFETY: buf 生命周期覆盖调用；menu/pos 由调用方保证有效。
    unsafe {
        let _ = GetMenuItemInfoW(menu, pos, true, &mut info);
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 消息窗口线程调用：把菜单消息转发给 IContextMenu3 / IContextMenu2。
pub fn forward_menu_msg(msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    ACTIVE.with(|slot| {
        let borrow = slot.borrow();
        let active = borrow.as_ref()?;
        if let Some(ctx3) = &active.ctx3 {
            let mut result = LRESULT(0);
            // SAFETY: ACTIVE 的生命周期覆盖 TrackPopupMenuEx 的整个模态循环。
            unsafe {
                let _ = ctx3.HandleMenuMsg2(msg, wparam, lparam, Some(&mut result));
            }
            // 先让 shell 完成 WM_INITMENUPOPUP 的惰性填充，再补我们的文本文档项。
            if msg == WM_INITMENUPOPUP {
                // SAFETY: wParam 是刚初始化完成的弹出菜单句柄。
                ensure_text_new_item(HMENU(wparam.0 as *mut core::ffi::c_void));
            }
            return Some(result);
        }
        if let Some(ctx2) = &active.ctx2 {
            // SAFETY: 同上，且仅在没有 IContextMenu3 时回退。
            unsafe {
                let _ = ctx2.HandleMenuMsg(msg, wparam, lparam);
            }
            if msg == WM_INITMENUPOPUP {
                // SAFETY: 同上。
                ensure_text_new_item(HMENU(wparam.0 as *mut core::ffi::c_void));
            }
            return Some(LRESULT(0));
        }
        log::debug!("原生菜单消息 {msg:#x} 无 IContextMenu2/3 可转发");
        Some(LRESULT(0))
    })
}

#[cfg(test)]
mod tests {
    use super::popup_failed;

    #[test]
    fn popup_cancel_is_not_failure() {
        assert!(!popup_failed(0, 0));
    }

    #[test]
    fn popup_error_triggers_fallback() {
        assert!(popup_failed(0, 87));
        assert!(!popup_failed(0x7001, 87));
    }
}
