//! 低级键盘钩子：拦截 `Win+D`，让系统不再切回原生桌面；并在名称文字选区存在时
//! 拦截光标所在名称区域上的 `Ctrl+C`，把复制请求投递给 UI 线程（叠加层不抢焦点，
//! 无法通过普通窗口消息收到键盘事件）。
//!
//! 另外承担空格快速预览的按键链路：仅当 UI 线程把「已选中唯一项、无编辑框、
//! 菜单未打开、叠加层可见」这一组合状态写进 `PREVIEW_ARMED`，且前台仍是系统桌面时，
//! 才吞掉空格的按下/抬起；其余任何情况一律透传。Esc 与预览面板内的 Ctrl+C 同样只在
//! 预览打开且光标位于面板内时拦截。
//!
//! 只在「Win 键按住 + D 键 + 无 Ctrl/Alt/Shift」时吞掉 D 的按下事件，
//! 其余按键（含 D 的抬起、Win+L / Win+G 等组合）全部原样透传。钩子运行在独立线程的
//! 消息循环里，进程退出（或崩溃）时由系统自动回收，不会残留系统级卡键。

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use crate::events::{BgEvent, EventSink};

use windows::core::HSTRING;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_A, VK_C, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_LCONTROL, VK_LEFT, VK_LMENU,
    VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
    VK_SPACE, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, FindWindowW, GetAncestor, GetClassNameW, GetCursorPos, GetForegroundWindow,
    GetLayeredWindowAttributes, GetMessageW, GetShellWindow, GetWindowLongPtrW, IsIconic,
    IsWindowVisible, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WindowFromPoint,
    GA_ROOT, GWL_EXSTYLE, HC_ACTION, KBDLLHOOKSTRUCT, LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA,
    MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WS_EX_LAYERED,
    WS_EX_TRANSPARENT,
};

/// 当前钩子句柄（0 表示未安装）。
static HOOK: AtomicIsize = AtomicIsize::new(0);
/// 钩子线程 ID，用于投递 WM_QUIT。
static THREAD_ID: AtomicU32 = AtomicU32::new(0);
/// Win 键是否按住（由钩子事件维护）。
static WIN_DOWN: AtomicBool = AtomicBool::new(false);

/// 钩子是否处于活动状态（线程退出前置 false）。
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// 当前存在名称文字选区时，选区的屏幕物理矩形（用于限定 Ctrl+C 拦截范围）。
/// `COPY_ARMED` 为 true 表示矩形有效；钩子只会在光标位于该矩形内时吞掉 Ctrl+C，
/// 光标离开后 Ctrl+C 完全透传，不影响其它程序。
static COPY_ARMED: AtomicBool = AtomicBool::new(false);
static COPY_X0: AtomicI32 = AtomicI32::new(0);
static COPY_Y0: AtomicI32 = AtomicI32::new(0);
static COPY_X1: AtomicI32 = AtomicI32::new(0);
static COPY_Y1: AtomicI32 = AtomicI32::new(0);

/// 拦截 Win+D 后把事件投递给 UI 线程的通道。
static SINK: Mutex<Option<EventSink>> = Mutex::new(None);

/// 空格预览是否处于「可以接管空格」的状态（由 UI 线程每帧写入）。
static PREVIEW_ARMED: AtomicBool = AtomicBool::new(false);
/// 叠加层是否允许无条件接管空格（无编辑框 / 无菜单 / 无设置页 / 叠加层可见）。
///
/// 与 [`PREVIEW_ARMED`] 的区别：armed 依赖"已有唯一选中项"，而鼠标点选后的一帧内
/// UI 还没处理点击，armed 必然滞后。允许接管时钩子先把空格转交 UI，由 UI 在
/// 处理完本帧点击后再决定打开/忽略，从根上消除"点选后立刻按空格失效"的竞争。
static SPACE_CAPTURE_ALLOWED: AtomicBool = AtomicBool::new(false);
/// 预览面板是否已打开（决定 Esc / Ctrl+C 是否可拦截）。
static PREVIEW_ACTIVE: AtomicBool = AtomicBool::new(false);
/// 预览面板的屏幕物理矩形（用于限定 Esc / Ctrl+C 的拦截范围）。
static PREVIEW_RECT: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);
/// 空格按下是否已被本次预览吞掉；用于吞掉对应的抬起，并抑制长按产生的重复按下。
static SPACE_CAPTURED: AtomicBool = AtomicBool::new(false);

/// 盒子多选键盘是否可接管（UI 线程每帧写入：叠加层可见、无编辑框、无菜单、
/// 已聚焦某个展开的盒子）。只有该状态为 true 且前台仍是桌面时，方向键 /
/// Ctrl+A / Esc 才会被吞掉并转交 UI 线程。
static NAV_ARMED: AtomicBool = AtomicBool::new(false);

/// 桌面多选的键盘是否可接管：叠加层可见、无编辑框、无菜单时置 true。
/// 盒子有焦点时 `NAV_ARMED` 优先；两者都为 false 时按键完全透传。
static DESK_SELECT_ARMED: AtomicBool = AtomicBool::new(false);

/// D 键虚拟键码。
const VK_D: u32 = 0x44;

/// 安装 Win+D 拦截钩子。返回是否安装成功（失败仅记录日志）。
pub fn install(sink: EventSink) -> bool {
    if ACTIVE.load(Ordering::Relaxed) {
        return true;
    }
    if let Ok(mut slot) = SINK.lock() {
        *slot = Some(sink);
    }
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("win-d-hook".into())
        .spawn(move || hook_thread(tx));
    if let Err(e) = spawned {
        log::warn!("启动 Win+D 钩子线程失败: {e}");
        return false;
    }
    match rx.recv_timeout(Duration::from_millis(1500)) {
        Ok(ok) => ok,
        Err(e) => {
            log::warn!("等待 Win+D 钩子安装结果超时: {e}");
            false
        }
    }
}

/// 卸载钩子：唤醒钩子线程，由线程自身调用 `UnhookWindowsHookEx`。
pub fn uninstall() {
    COPY_ARMED.store(false, Ordering::Relaxed);
    PREVIEW_ARMED.store(false, Ordering::Relaxed);
    SPACE_CAPTURE_ALLOWED.store(false, Ordering::Relaxed);
    PREVIEW_ACTIVE.store(false, Ordering::Relaxed);
    SPACE_CAPTURED.store(false, Ordering::Relaxed);
    NAV_ARMED.store(false, Ordering::Relaxed);
    DESK_SELECT_ARMED.store(false, Ordering::Relaxed);
    if let Ok(mut rect) = PREVIEW_RECT.lock() {
        *rect = None;
    }
    let tid = THREAD_ID.load(Ordering::Relaxed);
    if tid == 0 {
        return;
    }
    // SAFETY: 向本进程创建的钩子线程投递退出消息，不涉及其它窗口。
    unsafe {
        let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
    }
    // 稍等线程清理，最多 300ms；失败也不阻塞退出（进程终止时系统会移除钩子）。
    for _ in 0..30 {
        if !ACTIVE.load(Ordering::Relaxed) && HOOK.load(Ordering::Relaxed) == 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    log::warn!("Win+D 钩子线程未在 300ms 内退出，交由系统随进程回收");
}

/// 设置名称文字选区的屏幕物理矩形；`None` 表示当前没有选区，Ctrl+C 完全透传。
///
/// 先写坐标再置 armed，避免钩子线程读到半更新状态；清除时先置 false。
pub fn set_copy_region(rect: Option<(i32, i32, i32, i32)>) {
    match rect {
        Some((x0, y0, x1, y1)) => {
            COPY_X0.store(x0, Ordering::Relaxed);
            COPY_Y0.store(y0, Ordering::Relaxed);
            COPY_X1.store(x1, Ordering::Relaxed);
            COPY_Y1.store(y1, Ordering::Relaxed);
            COPY_ARMED.store(true, Ordering::Relaxed);
        }
        None => COPY_ARMED.store(false, Ordering::Relaxed),
    }
}

/// 由 UI 线程每帧写入：当前是否允许空格打开/关闭快速预览。
pub fn set_preview_armed(armed: bool) {
    PREVIEW_ARMED.store(armed, Ordering::Relaxed);
}

/// 由 UI 线程每帧写入：叠加层是否允许无条件接管空格（与是否已有选中项无关）。
pub fn set_space_capture_allowed(allowed: bool) {
    SPACE_CAPTURE_ALLOWED.store(allowed, Ordering::Relaxed);
}

/// 由 UI 线程每帧写入：是否允许接管盒子多选的方向键 / Ctrl+A / Esc。
pub fn set_nav_armed(armed: bool) {
    NAV_ARMED.store(armed, Ordering::Relaxed);
}

/// 由 UI 线程每帧写入：桌面多选是否允许接管 Ctrl+A / Esc（盒子无焦点时生效）。
pub fn set_desk_select_armed(armed: bool) {
    DESK_SELECT_ARMED.store(armed, Ordering::Relaxed);
}

/// 由 UI 线程每帧写入：预览面板是否已打开。
pub fn set_preview_active(active: bool) {
    PREVIEW_ACTIVE.store(active, Ordering::Relaxed);
    if !active {
        // 预览关闭后立刻解除空格标记，避免下一次空格被误判为「重复按下」。
        SPACE_CAPTURED.store(false, Ordering::Relaxed);
    }
}

/// 由 UI 线程每帧写入：预览面板的屏幕物理矩形，用于限定 Esc / Ctrl+C。
pub fn set_preview_rect(rect: Option<(i32, i32, i32, i32)>) {
    if let Ok(mut slot) = PREVIEW_RECT.lock() {
        *slot = rect;
    }
}

fn hook_thread(tx: mpsc::Sender<bool>) {
    // SAFETY: 取本进程模块句柄；失败时以 null 句柄安装低级钩子。
    let module = unsafe { GetModuleHandleW(None) }.ok();
    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(hook_proc),
            module.map(|m| HINSTANCE(m.0)),
            0,
        )
    };
    let hook = match hook {
        Ok(h) => h,
        Err(e) => {
            log::warn!("安装 Win+D 键盘钩子失败: {e}");
            let _ = tx.send(false);
            return;
        }
    };
    HOOK.store(hook.0 as isize, Ordering::Relaxed);
    // SAFETY: 获取当前钩子线程 ID，仅用于卸载时投递 WM_QUIT。
    THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    ACTIVE.store(true, Ordering::Relaxed);
    log::info!("Win+D 拦截钩子已安装（WH_KEYBOARD_LL）");
    let _ = tx.send(true);

    // 钩子线程必须保持消息循环，否则系统不会派发低级键盘事件。
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg 为栈上有效结构；本线程无窗口，仅等待消息。
        let res = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if res.0 == 0 || res.0 == -1 {
            break;
        }
    }

    ACTIVE.store(false, Ordering::Relaxed);
    // SAFETY: hook 由本线程安装且仍然有效，卸载一次即可。
    let _ = unsafe { UnhookWindowsHookEx(hook) };
    HOOK.store(0, Ordering::Relaxed);
    THREAD_ID.store(0, Ordering::Relaxed);
    log::info!("Win+D 拦截钩子已卸载");
}

/// 低级键盘钩子过程：只拦截 Win+D，其余一律透传。
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: HC_ACTION 时 lparam 指向有效的 KBDLLHOOKSTRUCT。
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let msg = wparam.0 as u32;
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        let vk = kb.vkCode;
        if vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32 {
            if down {
                WIN_DOWN.store(true, Ordering::Relaxed);
            } else if up {
                WIN_DOWN.store(false, Ordering::Relaxed);
            }
        } else if vk == VK_D && down {
            // 只吞掉 D 的按下：系统看不到 Win+D 组合，不会进入显示桌面；
            // 抬起事件照常放行，让 shell 认为 Win 键与其它键组合过，避免单按 Win
            // 弹出开始菜单（把抬起也吞掉会触发该副作用）。
            // 不特殊放行 injected：自动化发送的 Win+D 同样应被拦截。
            if !modifiers_down() && win_down() {
                log::info!("已拦截 Win+D（改为自实现显示桌面：最小化/还原普通窗口）");
                // 不在钩子线程里做窗口枚举（可能较慢且会拖住键盘链）：
                // 只投递事件，由 UI 线程执行真正的最小化/还原。
                if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                    sink.emit(BgEvent::ToggleDesktop);
                }
                return LRESULT(1);
            }
        } else if vk == VK_SPACE.0 as u32 {
            // 空格快速预览：只有 UI 线程声明「有唯一选中项、无编辑框、无菜单、叠加层可见」，
            // 且前台仍是系统桌面（用户没有切到别的应用）时才接管。
            if down {
                let armed = PREVIEW_ARMED.load(Ordering::Relaxed);
                let allowed = SPACE_CAPTURE_ALLOWED.load(Ordering::Relaxed);
                let desktop_fg = desktop_is_foreground();
                let mods = modifiers_down();
                let win = win_down();
                // 每次按下都留一条可判定证据：armed / 是否允许接管 / 前台探测 / 修饰键。
                log::info!(
                    "[preview-key] 空格按下 armed={armed} allowed={allowed} desktop_fg={desktop_fg} mods={mods} win={win} captured={} {}",
                    SPACE_CAPTURED.load(Ordering::Relaxed),
                    foreground_diagnostics()
                );
                // 只要叠加层允许接管（不在编辑/菜单/设置），就先转交 UI。
                // UI 会在处理完本帧点击后再决定打开或忽略，因此不再依赖滞后的 armed。
                if allowed && !mods && !win && desktop_fg {
                    // 长按方块键会持续派发按下事件：只处理第一次，避免反复开关预览。
                    if !SPACE_CAPTURED.swap(true, Ordering::Relaxed) {
                        log::info!("已拦截空格（打开/关闭快速预览）");
                        if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                            sink.emit(BgEvent::TogglePreview);
                        }
                    } else {
                        log::info!("[preview-key] 空格重复按下（长按），已吞掉但不再触发");
                    }
                    return LRESULT(1);
                }
                // 未接管：把确切原因写进日志，而不是只留下一个“偶发失败”的现象。
                let reason = if !allowed {
                    "allowed=false（正在编辑/菜单或设置打开/叠加层不可见）"
                } else if mods {
                    "有 Ctrl/Alt/Shift 修饰键"
                } else if win {
                    "Win 键按下"
                } else {
                    "前台窗口不是桌面"
                };
                log::info!("[preview-key] 空格未接管：{reason}");
            } else if up && SPACE_CAPTURED.swap(false, Ordering::Relaxed) {
                // 按下被吞掉后，抬起也必须吞掉，否则前台窗口会收到一次“无按下”的抬起。
                return LRESULT(1);
            }
        } else if vk == VK_ESCAPE.0 as u32 && down {
            if PREVIEW_ACTIVE.load(Ordering::Relaxed)
                && (cursor_in_preview() || desktop_is_foreground())
            {
                log::info!("已拦截 Esc（关闭快速预览）");
                if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                    sink.emit(BgEvent::ClosePreview);
                }
                return LRESULT(1);
            }
            // 无预览时 Esc = 清空多选（盒子优先，其次桌面）。
            if (NAV_ARMED.load(Ordering::Relaxed) || DESK_SELECT_ARMED.load(Ordering::Relaxed))
                && !alt_down()
                && !win_down()
                && desktop_is_foreground()
            {
                log::debug!("已拦截 Esc（清空多选）");
                if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                    sink.emit(BgEvent::ClearBoxSelection);
                }
                return LRESULT(1);
            }
        } else if vk == VK_A.0 as u32 && down {
            // Ctrl+A：盒子有焦点时选中盒子条目；否则全选桌面图标。
            let armed =
                NAV_ARMED.load(Ordering::Relaxed) || DESK_SELECT_ARMED.load(Ordering::Relaxed);
            if armed
                && ctrl_down()
                && !alt_down()
                && !shift_down()
                && !win_down()
                && desktop_is_foreground()
            {
                let box_focus = NAV_ARMED.load(Ordering::Relaxed);
                log::debug!(
                    "已拦截 Ctrl+A（{}）",
                    if box_focus {
                        "全选盒子条目"
                    } else {
                        "全选桌面图标"
                    }
                );
                if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                    sink.emit(if box_focus {
                        BgEvent::SelectAllBox
                    } else {
                        BgEvent::SelectAllDesktop
                    });
                }
                return LRESULT(1);
            }
        } else if down
            && (vk == VK_LEFT.0 as u32
                || vk == VK_RIGHT.0 as u32
                || vk == VK_UP.0 as u32
                || vk == VK_DOWN.0 as u32)
        {
            // 方向键：只在「已聚焦盒子 + 前台仍是桌面」时接管；Shift 表示扩展选区。
            if NAV_ARMED.load(Ordering::Relaxed)
                && !alt_down()
                && !win_down()
                && desktop_is_foreground()
            {
                let dir = if vk == VK_LEFT.0 as u32 {
                    crate::model::NavDir::Left
                } else if vk == VK_RIGHT.0 as u32 {
                    crate::model::NavDir::Right
                } else if vk == VK_UP.0 as u32 {
                    crate::model::NavDir::Up
                } else {
                    crate::model::NavDir::Down
                };
                let extend = shift_down();
                log::debug!(
                    "已拦截方向键 {:?}（extend={extend} shift={} ctrl={}）",
                    dir,
                    shift_down(),
                    ctrl_down()
                );
                if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                    sink.emit(BgEvent::NavKey { dir, extend });
                }
                return LRESULT(1);
            }
        } else if vk == VK_C.0 as u32 && down {
            // 预览面板内的 Ctrl+C 优先：把整份预览文本写入系统剪贴板。
            if PREVIEW_ACTIVE.load(Ordering::Relaxed)
                && cursor_in_preview()
                && ctrl_down()
                && !alt_or_shift_down()
                && !win_down()
            {
                log::info!("已拦截预览面板上的 Ctrl+C（转交 UI 线程复制全文）");
                if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                    sink.emit(BgEvent::CopyPreviewText);
                }
                return LRESULT(1);
            }
            // 名称文字选区存在、且光标仍停在选区内时才吞 Ctrl+C：
            // 叠加层带 WS_EX_NOACTIVATE，普通键盘消息到不了 egui，只能从这里转交。
            if ctrl_down() && !alt_or_shift_down() && !win_down() && copy_region_hit_cursor() {
                log::info!("已拦截名称选区上的 Ctrl+C（转交 UI 线程复制）");
                if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                    sink.emit(BgEvent::CopySelection);
                }
                return LRESULT(1);
            }
        }
    }
    // SAFETY: 交还给钩子链上的下一个处理者。
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 光标是否落在预览面板的屏幕物理矩形内。
fn cursor_in_preview() -> bool {
    let Some((x0, y0, x1, y1)) = PREVIEW_RECT.lock().ok().and_then(|g| *g) else {
        return false;
    };
    let mut p = POINT::default();
    // SAFETY: p 为栈上有效结构，仅查询光标位置。
    if unsafe { GetCursorPos(&mut p) }.is_err() {
        return false;
    }
    p.x >= x0 && p.x <= x1 && p.y >= y0 && p.y <= y1
}

/// 窗口是否被 DWM 隐形（典型场景：位于其它虚拟桌面上的窗口仍可见/前台，但用户看不到）。
fn window_is_cloaked(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    let mut cloaked = 0u32;
    // SAFETY: cloaked 为栈上有效 u32，属性长度与类型一致。
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
    };
    ok.is_ok() && cloaked != 0
}

/// 窗口是否“点了会穿透 / 完全透明”——这类窗口在屏幕上不构成真实遮挡，
/// 典型来源：输入法候选、提示气泡、WPF/XAML popup 占位层。
fn window_is_click_through(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    // SAFETY: 只读查询窗口扩展样式与分层属性。
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TRANSPARENT.0 != 0 {
            return true;
        }
        if ex & WS_EX_LAYERED.0 != 0 {
            let mut alpha = 0u8;
            let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
            if GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), Some(&mut flags)).is_ok() {
                return flags.0 & LWA_ALPHA.0 != 0 && alpha == 0;
            }
        }
    }
    false
}

/// 叠加层窗口句柄（不存在时返回 None）。
fn overlay_window() -> Option<HWND> {
    // SAFETY: 只读查找窗口。
    unsafe {
        FindWindowW(
            None,
            &HSTRING::from(crate::platform::desktop::OVERLAY_TITLE),
        )
        .ok()
    }
}

/// 光标命中的最顶层根窗口（光标不可用时返回空句柄）。
fn cursor_root_window() -> HWND {
    let mut p = POINT::default();
    // SAFETY: p 为栈上有效结构，只读查询。
    unsafe {
        if GetCursorPos(&mut p).is_err() {
            return HWND::default();
        }
        let h = WindowFromPoint(p);
        if h.0.is_null() {
            HWND::default()
        } else {
            GetAncestor(h, GA_ROOT)
        }
    }
}

/// 空格/Esc/导航键的接管判定：不依赖被我们隐藏的 DefView，而是看
/// 「叠加层可见 + 光标落在叠加层/桌面宿主上 + 光标点没有更高的可见窗口遮挡」。
///
/// 叠加层带 `WS_EX_NOACTIVATE`，点击它不会改变前台窗口，因此不能用前台句柄作为
/// 唯一依据（用户可能停留在其它虚拟桌面留下的 Chrome 等“隐形前台”上）。
/// 一旦光标点被真实可见的外应用窗口覆盖，仍必须完全透传，保护对方输入。
fn desktop_is_foreground() -> bool {
    let Some(overlay) = overlay_window() else {
        return false;
    };
    // 叠加层自身不可见时绝不接管空格。
    // SAFETY: 只读查询窗口状态。
    if !unsafe { IsWindowVisible(overlay) }.as_bool() {
        return false;
    }
    let hit = cursor_root_window();
    let hit_class = window_class(hit);
    let hit_cloaked = window_is_cloaked(hit);
    let hit_click_through = window_is_click_through(hit);
    let hit_is_overlay = !hit.0.is_null() && hit == unsafe { GetAncestor(overlay, GA_ROOT) };
    let hit_is_host =
        hit_class.eq_ignore_ascii_case("Progman") || hit_class.eq_ignore_ascii_case("WorkerW");
    if hit_is_overlay || hit_is_host {
        // 光标就在桌面上，且 WindowFromPoint 没返回更高的窗口：安全接管。
        return true;
    }
    // 光标点被其它窗口覆盖，但该窗口属于其它虚拟桌面/已最小化/不可见/点击穿透：
    // 都不构成遮挡。
    // SAFETY: 只读查询窗口状态。
    if !hit.0.is_null() {
        let hit_visible = unsafe { IsWindowVisible(hit) }.as_bool();
        let hit_iconic = unsafe { IsIconic(hit) }.as_bool();
        if hit_cloaked || !hit_visible || hit_iconic || hit_click_through {
            return true;
        }
        // 可见、非最小化的外应用窗口确实盖住了光标点：不抢空格。
        return false;
    }
    // 光标不可用（极少数远程/锁屏场景）：退回前台判定。
    // SAFETY: 只读查询前台窗口。
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() || fg == overlay || fg == unsafe { GetShellWindow() } {
        return true;
    }
    // SAFETY: 只读查询窗口状态。
    if !unsafe { IsWindowVisible(fg) }.as_bool()
        || unsafe { IsIconic(fg) }.as_bool()
        || window_is_cloaked(fg)
    {
        return true;
    }
    let fg_class = window_class(fg);
    fg_class.eq_ignore_ascii_case("Progman") || fg_class.eq_ignore_ascii_case("WorkerW")
}

/// 判定过程的可观测快照（每次空格按下写日志用）。
fn foreground_diagnostics() -> String {
    let overlay = overlay_window();
    let overlay_showing = overlay
        .map(|h| unsafe { IsWindowVisible(h) }.as_bool())
        .unwrap_or(false);
    let hit = cursor_root_window();
    let hit_class = window_class(hit);
    let hit_cloaked = window_is_cloaked(hit);
    let hit_click_through = window_is_click_through(hit);
    let cursor_overlay = overlay
        .map(|o| !hit.0.is_null() && hit == unsafe { GetAncestor(o, GA_ROOT) })
        .unwrap_or(false);
    // SAFETY: 只读查询前台窗口与状态。
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return format!(
                "fg=<null> overlay_showing={overlay_showing} cursor_class={hit_class} cursor_cloaked={hit_cloaked} cursor_transparent={hit_click_through} cursor_overlay={cursor_overlay}"
            );
        }
        let class = window_class(fg);
        let visible = IsWindowVisible(fg).as_bool();
        let iconic = IsIconic(fg).as_bool();
        let cloaked = window_is_cloaked(fg);
        let shell = fg == GetShellWindow();
        format!(
            "fg_class={class} fg_visible={visible} fg_iconic={iconic} fg_cloaked={cloaked} shell={shell} overlay_showing={overlay_showing} cursor_class={hit_class} cursor_cloaked={hit_cloaked} cursor_transparent={hit_click_through} cursor_overlay={cursor_overlay}"
        )
    }
}

/// 读取窗口类名（读不到返回空串）。
fn window_class(hwnd: windows::Win32::Foundation::HWND) -> String {
    let mut buf = [0u16; 64];
    // SAFETY: 只读查询窗口类名。
    unsafe {
        let n = GetClassNameW(hwnd, &mut buf) as usize;
        if n == 0 {
            String::new()
        } else {
            String::from_utf16_lossy(&buf[..n])
        }
    }
}

/// Ctrl 是否按住。
fn ctrl_down() -> bool {
    let keys = [VK_CONTROL.0, VK_LCONTROL.0, VK_RCONTROL.0];
    // SAFETY: 只读查询按键状态。
    unsafe { keys.iter().any(|vk| GetAsyncKeyState(*vk as i32) < 0) }
}

/// Alt 是否按住。
fn alt_down() -> bool {
    let keys = [VK_MENU.0, VK_LMENU.0, VK_RMENU.0];
    // SAFETY: 只读查询按键状态。
    unsafe { keys.iter().any(|vk| GetAsyncKeyState(*vk as i32) < 0) }
}

/// Shift 是否按住。
fn shift_down() -> bool {
    let keys = [VK_SHIFT.0, VK_LSHIFT.0, VK_RSHIFT.0];
    // SAFETY: 只读查询按键状态。
    unsafe { keys.iter().any(|vk| GetAsyncKeyState(*vk as i32) < 0) }
}

/// Alt / Shift 是否按住（组合键时放行，避免误伤）。
fn alt_or_shift_down() -> bool {
    let keys = [
        VK_MENU.0,
        VK_LMENU.0,
        VK_RMENU.0,
        VK_SHIFT.0,
        VK_LSHIFT.0,
        VK_RSHIFT.0,
    ];
    // SAFETY: 只读查询按键状态。
    unsafe { keys.iter().any(|vk| GetAsyncKeyState(*vk as i32) < 0) }
}

/// 光标是否落在当前名称选区矩形内（屏幕物理像素）。
fn copy_region_hit_cursor() -> bool {
    if !COPY_ARMED.load(Ordering::Relaxed) {
        return false;
    }
    let mut p = POINT::default();
    // SAFETY: p 为栈上有效结构，仅查询光标位置。
    if unsafe { GetCursorPos(&mut p) }.is_err() {
        return false;
    }
    let (x0, y0, x1, y1) = (
        COPY_X0.load(Ordering::Relaxed),
        COPY_Y0.load(Ordering::Relaxed),
        COPY_X1.load(Ordering::Relaxed),
        COPY_Y1.load(Ordering::Relaxed),
    );
    p.x >= x0 && p.x <= x1 && p.y >= y0 && p.y <= y1
}

/// Win 键是否按住：钩子内维护的状态，并用真实按键状态复核。
fn win_down() -> bool {
    // SAFETY: 只读查询按键状态。
    let async_down =
        unsafe { GetAsyncKeyState(VK_LWIN.0 as i32) < 0 || GetAsyncKeyState(VK_RWIN.0 as i32) < 0 };
    WIN_DOWN.load(Ordering::Relaxed) || async_down
}

/// Ctrl / Alt / Shift 是否任一按住（任一按住就放行，避免误伤其它组合键）。
fn modifiers_down() -> bool {
    let keys = [
        VK_CONTROL.0,
        VK_LCONTROL.0,
        VK_RCONTROL.0,
        VK_MENU.0,
        VK_LMENU.0,
        VK_RMENU.0,
        VK_SHIFT.0,
        VK_LSHIFT.0,
        VK_RSHIFT.0,
    ];
    // SAFETY: 只读查询按键状态。
    unsafe { keys.iter().any(|vk| GetAsyncKeyState(*vk as i32) < 0) }
}
