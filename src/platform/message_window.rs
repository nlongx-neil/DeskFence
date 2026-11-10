//! 隐藏消息窗口线程：托盘图标、全局热键、剪贴板监听、显示器变化、第二实例唤醒。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP,
    NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetIconInfo, GetMessageW, LoadIconW, LoadImageW, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetTimer,
    TrackPopupMenu, TranslateMessage, CS_HREDRAW, CS_VREDRAW, HICON, ICONINFO, IDI_APPLICATION,
    IMAGE_ICON, LR_LOADFROMFILE, MF_SEPARATOR, MF_STRING, MSG, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_CREATE, WM_DESTROY, WM_DISPLAYCHANGE,
    WM_DRAWITEM, WM_HOTKEY, WM_INITMENUPOPUP, WM_LBUTTONUP, WM_MEASUREITEM, WM_MENUCHAR, WM_NULL,
    WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_POPUP,
};

use crate::events::{BgEvent, EventSink, TrayCommand};

pub const WM_APP_TRAY: u32 = WM_APP + 1;
pub const WM_APP_ACTIVATE: u32 = WM_APP + 2;
pub const WM_APP_RELOAD_HOTKEY: u32 = WM_APP + 3;
/// 重新装载到期提醒定时器（UI 线程刷新调度快照后投递）。
/// 注意避开 `shell_menu` 已占用的 `WM_APP + 4/5`。
pub const WM_APP_REFRESH_TIMER: u32 = WM_APP + 16;

/// 到期提醒定时器 id。
const TIMER_REMINDER: usize = 1;
/// 电源状态变化（`WM_POWERBROADCAST`）。
const WM_POWERBROADCAST: u32 = 0x0218;
const PBT_APMRESUMESUSPEND: usize = 0x0007;
const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;

const HOTKEY_ID: i32 = 1;
const ID_SHOW_CLIPBOARD: usize = 1001;
const ID_TOGGLE_OVERLAY: usize = 1002;
const ID_TOGGLE_PAUSE: usize = 1003;
const ID_NEW_BOX: usize = 1004;
const ID_SETTINGS: usize = 1005;
const ID_EXIT: usize = 1006;
const ID_AUTO_GROUP: usize = 1007;
const ID_RESTORE_AUTO: usize = 1008;
const ID_ENABLE_BOARD: usize = 1009;

/// 气泡被用户点击（`WM_USER + 5`，避开 `WM_APP_TRAY` 的鼠标消息区间）。
const NIN_BALLOONUSERCLICK: u32 = 0x0405;

static OVERLAY_VISIBLE: AtomicBool = AtomicBool::new(true);
static PAUSED: AtomicBool = AtomicBool::new(false);
/// 最近一次到期气泡对应的盒子，供点击回跳。
static LAST_BALLOON_BOX: Mutex<Option<String>> = Mutex::new(None);
/// 首次装载定时器时先跑一个短周期，相当于启动时补一次到期提醒。
static REMINDER_TIMER_FIRST: AtomicBool = AtomicBool::new(true);

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct HotkeySpec {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub key: String,
}

impl HotkeySpec {
    pub fn from_config(text: &str) -> Option<Self> {
        let (ctrl, alt, shift, win, key) = crate::config::parse_hotkey(text)?;
        Self::vk_of(&key)?;
        Some(Self {
            ctrl,
            alt,
            shift,
            win,
            key,
        })
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.win {
            parts.push("Win");
        }
        parts.push(self.key.as_str());
        parts.join("+")
    }

    fn vk_of(key: &str) -> Option<u32> {
        let k = key.to_ascii_uppercase();
        let mapping = match k.as_str() {
            "ENTER" | "RETURN" => 0x0D,
            "TAB" => 0x09,
            "ESC" | "ESCAPE" => 0x1B,
            "SPACE" => 0x20,
            "UP" => 0x26,
            "DOWN" => 0x28,
            "LEFT" => 0x25,
            "RIGHT" => 0x27,
            "HOME" => 0x24,
            "END" => 0x23,
            "PAGEUP" => 0x21,
            "PAGEDOWN" => 0x22,
            "INSERT" => 0x2D,
            "DELETE" | "DEL" => 0x2E,
            _ => {
                if k.len() == 1 {
                    let c = k.chars().next()?;
                    if !c.is_ascii_alphanumeric() {
                        return None;
                    }
                    c as u32
                } else {
                    let num: u32 = k.strip_prefix('F')?.parse().ok()?;
                    if !(1..=24).contains(&num) {
                        return None;
                    }
                    0x70 + (num - 1)
                }
            }
        };
        Some(mapping)
    }

    fn modifiers(&self) -> HOT_KEY_MODIFIERS {
        let mut m = HOT_KEY_MODIFIERS(0);
        if self.ctrl {
            m |= MOD_CONTROL;
        }
        if self.alt {
            m |= MOD_ALT;
        }
        if self.shift {
            m |= MOD_SHIFT;
        }
        if self.win {
            m |= MOD_WIN;
        }
        m
    }
}

struct Ctx {
    hwnd: HWND,
    tray_added: bool,
    hotkey: Option<HotkeySpec>,
    taskbar_created: u32,
}

/// `WM_CLIPBOARDUPDATE`（AddClipboardFormatListener 通知）
const WM_CLIPBOARDUPDATE: u32 = 0x031D;

// SAFETY: Ctx 只在消息线程内部使用；HWND 为进程内系统句柄。
unsafe impl Send for Ctx {}

static CTX: Mutex<Option<Ctx>> = Mutex::new(None);
static NEXT_HOTKEY: Mutex<Option<Option<HotkeySpec>>> = Mutex::new(None);
static SINK: Mutex<Option<EventSink>> = Mutex::new(None);

/// 启动消息窗口线程。`hotkey` 为初始全局热键。
pub fn spawn(sink: EventSink, hotkey: Option<HotkeySpec>) -> bool {
    if let Ok(mut slot) = SINK.lock() {
        *slot = Some(sink.clone());
    }
    std::thread::Builder::new()
        .name("msg-window".into())
        .spawn(move || run_message_loop(sink, hotkey))
        .map(|_| true)
        .unwrap_or_else(|e| {
            log::error!("启动消息窗口线程失败: {e}");
            false
        })
}

fn run_message_loop(_sink: EventSink, hotkey: Option<HotkeySpec>) {
    // 本线程负责托盘菜单与原生文件右键菜单。若线程为 DPI-unaware，系统会把
    // 物理坐标按缩放比虚拟化，导致菜单弹在点击点之外（实机 150% 下偏到 2/3 处）。
    // eframe 只在自己的线程设置感知级别，因此这里必须显式设置；用运行期探测，
    // 低版本系统缺失该 API 时降级而不是让进程加载失败。
    super::enable_thread_dpi_awareness_v2();
    // SAFETY: 注册窗口类并创建隐藏消息窗口，类名/实例句柄均有效。
    unsafe {
        let Ok(hinstance) = GetModuleHandleW(None) else {
            log::error!(
                "GetModuleHandleW 失败: {}",
                windows::core::Error::from_thread()
            );
            return;
        };
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: w!("DeskKeeper.MsgWindow"),
            ..Default::default()
        };
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            log::error!(
                "RegisterClassW 失败: {}",
                windows::core::Error::from_thread()
            );
        }
        let hwnd = match CreateWindowExW(
            Default::default(),
            w!("DeskKeeper.MsgWindow"),
            w!("DeskKeeper"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                log::error!("创建消息窗口失败: {e}");
                return;
            }
        };
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));

        if let Ok(mut slot) = CTX.lock() {
            *slot = Some(Ctx {
                hwnd,
                tray_added: false,
                hotkey: None,
                taskbar_created,
            });
        }

        add_tray(hwnd);
        apply_hotkey(hotkey);
        arm_reminder_timer(hwnd);
        let _ = windows::Win32::System::DataExchange::AddClipboardFormatListener(hwnd);

        let mut msg = MSG::default();
        loop {
            let res = GetMessageW(&mut msg, None, 0, 0);
            if res.0 == 0 || res.0 == -1 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    log::info!("消息窗口线程退出");
}

fn tray_tip() -> [u16; 128] {
    let mut tip = [0u16; 128];
    let text = "DeskKeeper · 桌面整理与剪贴板";
    for (i, ch) in text.encode_utf16().take(127).enumerate() {
        tip[i] = ch;
    }
    tip
}

/// 内嵌托盘图标（多尺寸 PNG 帧 ICO，由 `assets/deskkeeper.ico` 生成）。
const TRAY_ICON_BYTES: &[u8] = include_bytes!("../../assets/deskkeeper.ico");

/// 把内嵌图标落到数据目录，供 `LoadImageW(LR_LOADFROMFILE)` 读取。
/// 文件内容一致时不重写，避免每次启动都产生磁盘写。
fn tray_icon_path() -> Option<std::path::PathBuf> {
    let path = crate::util::data_dir().join("deskkeeper.ico");
    let same = std::fs::read(&path)
        .map(|old| old == TRAY_ICON_BYTES)
        .unwrap_or(false);
    if !same {
        if let Err(e) = std::fs::write(&path, TRAY_ICON_BYTES) {
            log::warn!("写入托盘图标文件失败: {e}");
            return None;
        }
    }
    Some(path)
}

fn load_tray_icon() -> Option<HICON> {
    use std::os::windows::ffi::OsStrExt;
    let path = tray_icon_path()?;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: wide 以 NUL 结尾且调用期间有效；返回的 HICON 由进程持有。
    match unsafe {
        LoadImageW(
            None,
            PCWSTR(wide.as_ptr()),
            IMAGE_ICON,
            32,
            32,
            LR_LOADFROMFILE,
        )
    } {
        Ok(handle) => {
            let hicon = HICON(handle.0);
            if hicon.0.is_null() {
                log::warn!("内嵌托盘图标返回空句柄，回退系统图标");
                return None;
            }
            // 句柄可用性自检：GetIconInfo 能取到彩色位图才认为图标真正加载成功。
            let mut info = ICONINFO::default();
            match unsafe { GetIconInfo(hicon, &mut info) } {
                Ok(()) if !info.hbmColor.is_invalid() => {
                    log::info!(
                        "托盘图标已加载：assets/deskkeeper.ico（HICON={hicon:?}，fIcon={}，彩色位图有效）",
                        info.fIcon.as_bool()
                    );
                    Some(hicon)
                }
                Ok(()) => {
                    log::warn!("托盘图标缺少彩色位图，回退系统图标");
                    None
                }
                Err(e) => {
                    log::warn!("托盘图标 GetIconInfo 校验失败: {e}，回退系统图标");
                    None
                }
            }
        }
        Err(e) => {
            log::warn!("加载内嵌托盘图标失败: {e}，回退系统图标");
            None
        }
    }
}

fn add_tray(hwnd: HWND) {
    // SAFETY: 优先使用项目内嵌图标，失败时回退系统通用图标。
    let hicon = load_tray_icon().or_else(|| unsafe { LoadIconW(None, IDI_APPLICATION).ok() });
    let Some(hicon) = hicon else {
        log::warn!("加载托盘图标失败");
        return;
    };
    unsafe {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_APP_TRAY,
            hIcon: hicon,
            szTip: tray_tip(),
            ..Default::default()
        };
        if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            if let Ok(mut guard) = CTX.lock() {
                if let Some(ctx) = guard.as_mut() {
                    ctx.tray_added = true;
                }
            }
            // 向外壳查询图标实际位置，证明图标已注册而非只是加载了句柄。
            let ident = NOTIFYICONIDENTIFIER {
                cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
                hWnd: hwnd,
                uID: 1,
                guidItem: windows::core::GUID::zeroed(),
            };
            match Shell_NotifyIconGetRect(&ident) {
                Ok(r) => log::info!(
                    "托盘图标已注册到通知区域：位置 {},{} {}x{}",
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top
                ),
                Err(e) => log::info!("托盘图标已注册（当前在折叠区，查询位置: {e}）"),
            }
        } else {
            log::warn!("添加托盘图标失败: {}", windows::core::Error::from_thread());
        }
        let _ = &mut nid;
    }
}

/// 弹一次托盘气泡（`NIM_MODIFY + NIF_INFO`）。
///
/// 返回 false 表示气泡通道不可用（托盘未注册/系统关闭通知），调用方可回退到盒内红点。
/// 气泡由 explorer 绘制，不抢焦点，也不受叠加层 `WS_EX_NOACTIVATE` 影响。
pub fn notify_due(title: &str, body: &str, box_id: &str) -> bool {
    let Some(hwnd) = hwnd() else {
        return false;
    };
    if let Ok(mut guard) = LAST_BALLOON_BOX.lock() {
        *guard = Some(box_id.to_string());
    }
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_INFO,
        szInfoTitle: wide_fixed(title),
        szInfo: wide_fixed(body),
        dwInfoFlags: NIIF_INFO,
        ..Default::default()
    };
    // SAFETY: 托盘图标由本进程注册，`hwnd` 仍有效；Shell 调用线程无关。
    let ok = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) }.as_bool();
    if ok {
        log::info!("已通过托盘气泡提醒：{title} / {body}");
    } else {
        log::warn!("托盘气泡发送失败: {}", windows::core::Error::from_thread());
    }
    ok
}

fn wide_fixed<const N: usize>(text: &str) -> [u16; N] {
    let mut buf = [0u16; N];
    for (slot, unit) in buf.iter_mut().zip(text.encode_utf16()) {
        *slot = unit;
    }
    buf
}

fn remove_tray(hwnd: HWND) {
    // SAFETY: 删除托盘图标。
    unsafe {
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            ..Default::default()
        };
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

fn apply_hotkey(hotkey: Option<HotkeySpec>) {
    let hwnd = {
        let guard = match CTX.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        match guard.as_ref() {
            Some(ctx) => ctx.hwnd,
            None => return,
        }
    };
    // SAFETY: 注册/注销全局热键，句柄有效。
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
        if let Some(hk) = hotkey.as_ref() {
            let vk = HotkeySpec::vk_of(&hk.key);
            if let Some(vk) = vk {
                let ok = RegisterHotKey(Some(hwnd), HOTKEY_ID, hk.modifiers(), vk);
                match ok {
                    Ok(()) => log::info!("全局热键已注册: {}", hk.describe()),
                    Err(e) => log::warn!("全局热键注册失败（可能被占用）: {} ({e})", hk.describe()),
                }
            }
        }
    }
    if let Ok(mut guard) = CTX.lock() {
        if let Some(ctx) = guard.as_mut() {
            ctx.hotkey = hotkey;
        }
    }
}

/// 装载/重装到期提醒定时器（粒度由调度器按是否存在短周期任务决定）。
fn arm_reminder_timer(hwnd: HWND) {
    let interval = if REMINDER_TIMER_FIRST.swap(false, Ordering::Relaxed) {
        2_000
    } else {
        crate::reminder::scheduler::tick_interval_ms()
    };
    // SAFETY: 句柄属于本线程创建的隐藏窗口，随窗口销毁自动回收。
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_REMINDER, interval, None);
    }
}

/// UI 线程请求按新的粒度重装定时器（快照变化后调用）。
pub fn refresh_reminder_timer() {
    let Some(hwnd) = hwnd() else {
        return;
    };
    // SAFETY: 只向消息窗口投递命令，定时器本身仍在消息线程上操作。
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP_REFRESH_TIMER, WPARAM(0), LPARAM(0));
    }
}

/// 扫描一次到期提醒并把结果交给 UI 线程（展示与周期推进都在 UI 线程完成）。
fn run_reminder_scan() {
    let now = crate::reminder::scheduler::now_secs();
    // 首次 tick 或时钟跳变（睡眠/改表）时同样走这次扫描，等价于「启动补提醒」。
    let _ = crate::reminder::scheduler::note_tick(now);
    let due = crate::reminder::scheduler::scan(now);
    if due.is_empty() {
        return;
    }
    log::info!(
        "到期提醒：{} 项（最近：{} · {}）",
        due.len(),
        due[0].box_name,
        due[0].text
    );
    if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
        sink.emit(BgEvent::RemindersDue(due));
    }
}

fn show_tray_menu(hwnd: HWND) {
    // SAFETY: 构建并弹出原生托盘菜单。
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            log::warn!("创建托盘菜单失败: {}", windows::core::Error::from_thread());
            return;
        };
        let overlay_label = if OVERLAY_VISIBLE.load(Ordering::Relaxed) {
            "隐藏桌面整理层"
        } else {
            "显示桌面整理层"
        };
        let pause_label = if PAUSED.load(Ordering::Relaxed) {
            "恢复剪贴板记录"
        } else {
            "暂停剪贴板记录"
        };
        let _ = AppendMenuW(menu, MF_STRING, ID_SHOW_CLIPBOARD, w!("显示剪贴板盒"));
        let overlay_label = windows::core::HSTRING::from(overlay_label);
        let pause_label = windows::core::HSTRING::from(pause_label);
        let _ = AppendMenuW(menu, MF_STRING, ID_TOGGLE_OVERLAY, &overlay_label);
        let _ = AppendMenuW(menu, MF_STRING, ID_TOGGLE_PAUSE, &pause_label);
        let _ = AppendMenuW(menu, MF_STRING, ID_NEW_BOX, w!("新建盒子"));
        let _ = AppendMenuW(menu, MF_STRING, ID_AUTO_GROUP, w!("自动整理"));
        let _ = AppendMenuW(menu, MF_STRING, ID_RESTORE_AUTO, w!("恢复自动排列"));
        let _ = AppendMenuW(menu, MF_STRING, ID_ENABLE_BOARD, w!("启用棋盘摆放"));
        let _ = AppendMenuW(menu, MF_STRING, ID_SETTINGS, w!("设置"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("退出 DeskKeeper"));

        let mut pt = windows::Win32::Foundation::POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        // TPM_RETURNCMD 不会投递 WM_COMMAND，这里直接处理返回值
        if cmd.0 != 0 {
            handle_menu_command(cmd.0 as usize);
        }
    }
}

fn handle_menu_command(id: usize) {
    let sink = SINK.lock().ok().and_then(|g| g.clone());
    let Some(sink) = sink else {
        return;
    };
    match id {
        ID_SHOW_CLIPBOARD => sink.emit(BgEvent::Tray(TrayCommand::ShowClipboardBox)),
        ID_TOGGLE_OVERLAY => sink.emit(BgEvent::Tray(TrayCommand::ToggleOverlay)),
        ID_TOGGLE_PAUSE => sink.emit(BgEvent::Tray(TrayCommand::TogglePause)),
        ID_NEW_BOX => sink.emit(BgEvent::Tray(TrayCommand::NewBox)),
        ID_AUTO_GROUP => sink.emit(BgEvent::Tray(TrayCommand::AutoGroup)),
        ID_RESTORE_AUTO => sink.emit(BgEvent::Tray(TrayCommand::RestoreAutoArrange)),
        ID_ENABLE_BOARD => sink.emit(BgEvent::Tray(TrayCommand::EnableBoardPlacement)),
        ID_SETTINGS => sink.emit(BgEvent::Tray(TrayCommand::Settings)),
        ID_EXIT => sink.emit(BgEvent::Tray(TrayCommand::Exit)),
        _ => log::debug!("未知托盘命令 {id}"),
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: 从窗口用户数据读取自身上下文；仅在消息线程访问。
    match msg {
        WM_APP_TRAY => {
            let event = (lparam.0 as u32) & 0xFFFF;
            if event == WM_LBUTTONUP {
                // 点击托盘时前台是任务栏/资源管理器，不能作为自动粘贴目标
                if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                    sink.emit(BgEvent::ShowClipboardBox);
                }
            } else if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                show_tray_menu(hwnd);
            } else if event == NIN_BALLOONUSERCLICK {
                // 点击气泡：唤出气泡对应的待办盒（叠加层被隐藏时同样恢复）。
                let box_id = LAST_BALLOON_BOX.lock().ok().and_then(|g| g.clone());
                if let (Some(sink), Some(box_id)) =
                    (SINK.lock().ok().and_then(|g| g.clone()), box_id)
                {
                    sink.emit(BgEvent::ReminderActivated(box_id));
                }
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
            // 热键只负责显示/聚焦剪贴板盒；双击盒子条目时再取当前前台窗口粘贴。
            log::info!(
                "热键[t={}] 消息线程收到 WM_HOTKEY",
                crate::platform::shell_menu::now_ms()
            );
            if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                sink.emit(BgEvent::ShowClipboardBox);
            }
            LRESULT(0)
        }
        WM_APP_ACTIVATE => {
            // 来自"第二次启动"的唤醒：即使叠加层此前被隐藏，也要恢复显示剪贴板盒，
            // 让用户能看到明确的唤醒反馈。
            if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                log::info!("收到唤醒请求（第二个进程/外部消息），显示叠加层与剪贴板盒");
                sink.emit(BgEvent::Activate);
            }
            LRESULT(0)
        }
        WM_CLIPBOARDUPDATE => {
            if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                sink.emit(BgEvent::ClipboardChanged);
            }
            LRESULT(0)
        }
        WM_APP_RELOAD_HOTKEY => {
            let spec = NEXT_HOTKEY.lock().ok().and_then(|mut g| g.take()).flatten();
            apply_hotkey(spec);
            LRESULT(0)
        }
        WM_APP_REFRESH_TIMER => {
            arm_reminder_timer(hwnd);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_REMINDER => {
            run_reminder_scan();
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            // 睡眠/休眠恢复后立刻补一次扫描，不等下一个定时器周期。
            if wparam.0 == PBT_APMRESUMEAUTOMATIC || wparam.0 == PBT_APMRESUMESUSPEND {
                log::info!("系统从睡眠/休眠恢复，立即补扫到期提醒");
                run_reminder_scan();
            }
            LRESULT(0)
        }
        crate::platform::shell_menu::WM_APP_SHELL_MENU => {
            let nested = crate::platform::shell_menu::in_modal();
            let req = crate::platform::shell_menu::take_request();
            log::info!(
                "菜单[t={}] 消息线程收到 WM_APP_SHELL_MENU 请求存在={} 模态中={}",
                crate::platform::shell_menu::now_ms(),
                req.is_some(),
                nested
            );
            if nested {
                // 模态循环内的重入消息：丢弃请求，并且不能释放外层菜单的占用标记。
                log::info!("菜单：丢弃模态循环内的嵌套文件菜单请求");
            } else if let Some(req) = req {
                if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                    if let Some((req, reason)) = crate::platform::shell_menu::show(req, hwnd, &sink)
                    {
                        log::warn!("原生右键菜单不可用（{reason}），退回内置菜单");
                        sink.emit(BgEvent::ShellMenuFallback(req));
                    }
                }
            } else {
                crate::platform::shell_menu::release_owned();
            }
            LRESULT(0)
        }
        crate::platform::shell_menu::WM_APP_DESKTOP_MENU => {
            let nested = crate::platform::shell_menu::in_modal();
            let req = crate::platform::shell_menu::take_desktop_request();
            log::info!(
                "菜单[t={}] 消息线程收到 WM_APP_DESKTOP_MENU 请求存在={} 模态中={}",
                crate::platform::shell_menu::now_ms(),
                req.is_some(),
                nested
            );
            if nested {
                // 模态循环内的重入消息：丢弃请求，并且不能释放外层菜单的占用标记。
                log::info!("菜单：丢弃模态循环内的嵌套桌面菜单请求");
            } else if let Some((x, y)) = req {
                if crate::platform::shell_menu::desktop_fallback_forced() {
                    // 测试开关：通过真实的降级事件路径，验证 UI 线程的坐标换算。
                    log::warn!("桌面菜单：测试开关强制降级");
                    if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                        sink.emit(BgEvent::DesktopMenuFallback {
                            x,
                            y,
                            reason: crate::platform::shell_menu::FALLBACK_FORCED_REASON.to_string(),
                        });
                    }
                    crate::platform::shell_menu::release_owned();
                } else if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                    if let Some(reason) =
                        crate::platform::shell_menu::show_desktop(x, y, hwnd, &sink)
                    {
                        log::warn!("桌面原生菜单不可用（{reason}），退回内置菜单");
                        sink.emit(BgEvent::DesktopMenuFallback {
                            x,
                            y,
                            reason: reason.to_string(),
                        });
                    }
                }
            } else {
                crate::platform::shell_menu::release_owned();
            }
            LRESULT(0)
        }
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR => {
            if let Some(result) = crate::platform::shell_menu::forward_menu_msg(msg, wparam, lparam)
            {
                result
            } else {
                // SAFETY: 无活动 Shell 菜单时交给默认窗口过程。
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            handle_menu_command(id);
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                sink.emit(BgEvent::DisplayChanged);
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            // 系统设置变化（主题、环境变量等）不重排窗口，只刷新主题，避免无谓的
            // 窗口尺寸命令与配置写盘。
            if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                sink.emit(BgEvent::SystemThemeChanged);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: 销毁窗口并退出消息循环。
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_tray(hwnd);
            // SAFETY: 注销热键与剪贴板监听。
            unsafe {
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
                let _ = windows::Win32::System::DataExchange::RemoveClipboardFormatListener(hwnd);
            }
            // SAFETY: 结束消息循环。
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_CREATE => LRESULT(0),
        _ => {
            // 检查任务栏重建消息
            let taskbar_msg = CTX
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|c| c.taskbar_created))
                .unwrap_or(0);
            if taskbar_msg != 0 && msg == taskbar_msg {
                log::info!("检测到资源管理器重启，重建托盘图标");
                add_tray(hwnd);
                if let Some(sink) = SINK.lock().ok().and_then(|g| g.clone()) {
                    sink.emit(BgEvent::DisplayChanged);
                }
                return LRESULT(0);
            }
            // SAFETY: 交给默认窗口过程处理。
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

/// 运行时更新全局热键（UI 线程调用）。
pub fn update_hotkey(spec: Option<HotkeySpec>) -> bool {
    if let Ok(mut g) = NEXT_HOTKEY.lock() {
        *g = Some(spec);
    }
    let hwnd = CTX
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|c| c.hwnd.0 as isize))
        .unwrap_or(0);
    if hwnd == 0 {
        return false;
    }
    let hwnd_ref = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: 向消息窗口投递重新注册命令。
    unsafe { PostMessageW(Some(hwnd_ref), WM_APP_RELOAD_HOTKEY, WPARAM(0), LPARAM(0)).is_ok() }
}

pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::Relaxed);
}

/// 供其它平台模块（原生右键菜单）向消息窗口线程投递任务。
pub fn hwnd() -> Option<HWND> {
    CTX.lock().ok().and_then(|g| g.as_ref().map(|c| c.hwnd))
}

pub fn set_overlay_visible(visible: bool) {
    OVERLAY_VISIBLE.store(visible, Ordering::Relaxed);
}

/// 请求退出程序（销毁消息窗口触发消息循环结束）。
pub fn request_quit() {
    // SAFETY: 查找并关闭消息窗口。
    unsafe {
        if let Ok(hwnd) = windows::Win32::UI::WindowsAndMessaging::FindWindowW(
            w!("DeskKeeper.MsgWindow"),
            w!("DeskKeeper"),
        ) {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TRAY_ICON_BYTES;

    /// 校验内嵌 ICO 头与帧表，防止资源被误替换成空文件/错误格式。
    #[test]
    fn tray_icon_bytes_are_valid_ico() {
        let b = TRAY_ICON_BYTES;
        assert!(b.len() > 6, "ICO 过短");
        assert_eq!(&b[0..4], &[0, 0, 1, 0], "ICO 头标记不正确");
        let count = u16::from_le_bytes([b[4], b[5]]) as usize;
        assert!(count >= 1, "没有图标帧");
        assert!(b.len() >= 6 + count * 16, "帧表不完整");
        for i in 0..count {
            let off = 6 + i * 16;
            let w = if b[off] == 0 { 256 } else { b[off] as usize };
            let bytes =
                u32::from_le_bytes([b[off + 8], b[off + 9], b[off + 10], b[off + 11]]) as usize;
            let pos =
                u32::from_le_bytes([b[off + 12], b[off + 13], b[off + 14], b[off + 15]]) as usize;
            assert!((16..=256).contains(&w), "帧 {i} 尺寸异常: {w}");
            assert!(pos + bytes <= b.len(), "帧 {i} 数据越界");
            assert_eq!(&b[pos + 1..pos + 4], b"PNG", "帧 {i} 非 PNG 压缩");
        }
    }
}
