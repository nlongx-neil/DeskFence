//! `Win+D` 自实现：拦截系统“显示桌面/peek”，改为最小化本进程以外的普通顶层窗口。
//!
//! 系统原生 `Win+D` 会把原生桌面（连同系统图标层与壁纸窗口）临时抬到普通窗口之上，
//! 导致本软件的叠加层被盖住。这里保留钩子对 `Win+D` 的拦截，由本软件自己完成
//! “显示桌面”的语义：把所有普通顶层窗口最小化，于是桌面层（本软件的叠加层）露出，
//! 再按一次则把先前最小化的窗口按原顺序还原。原生桌面图标层全程不参与。
//!
//! 状态只记录我们主动最小化的窗口句柄；期间用户新开的窗口不会被还原逻辑触碰。

use std::sync::Mutex;

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowThreadProcessId,
    IsIconic, IsWindow, IsWindowVisible, SetForegroundWindow, ShowWindow, GWL_EXSTYLE, SW_RESTORE,
    SW_SHOWMINNOACTIVE, WS_EX_TOOLWINDOW,
};

/// 一次“显示桌面”保存的窗口清单。
#[derive(Debug, Default)]
struct Tracked {
    /// 按枚举顺序（自上而下）记录被我们最小化的窗口。
    windows: Vec<isize>,
    /// 操作前的前台窗口，还原时尽量恢复到它。
    foreground: isize,
}

static STATE: Mutex<Option<Tracked>> = Mutex::new(None);

/// 参与最小化的窗口描述（纯数据，便于单测过滤规则）。
#[derive(Clone, Debug, Default)]
pub struct WindowInfo {
    pub pid: u32,
    pub visible: bool,
    pub iconic: bool,
    pub cloaked: bool,
    pub tool_window: bool,
    pub class: String,
}

/// 系统 shell / 桌面 / 输入法 / 菜单自身的窗口类名：既不能最小化，也不该出现在清单里。
const SHELL_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "TaskListThumbnailWnd",
    "DV2ControlHost",
    "XamlExplorerHostIslandWindow",
    "MultitaskingViewFrame",
    "ApplicationManager_DesktopShellWindow",
    "Windows.UI.Core.CoreWindow",
    "IME",
    "MSCTFIME UI",
    "#32768",
    "#32769",
];

/// 判断某个顶层窗口是否应被“显示桌面”最小化。
///
/// 过滤原则：只处理**可见、未最小化、非本进程、非工具窗口、未被 DWM 隐形、非 shell/输入法
/// 自身**的窗口。这里不再强制要求 `WS_MINIMIZEBOX`：现代无边框应用（Electron / WinUI）
/// 常常自绘标题栏并去掉该样式，但仍然可以被 `ShowWindow` 最小化；若按样式过滤会漏掉它们。
/// 代价是独占全屏游戏也会被最小化（可能触发显示模式切换），与原生 Win+D 的行为一致。
pub fn should_minimize(info: &WindowInfo, own_pid: u32) -> bool {
    info.visible
        && !info.iconic
        && info.pid != own_pid
        && !info.tool_window
        && !info.cloaked
        && !SHELL_CLASSES.contains(&info.class.as_str())
}

/// 在“显示桌面”与“还原窗口”之间切换（仅在 UI 线程调用）。
pub fn toggle() {
    let mut guard = match STATE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(tracked) = guard.take() {
        restore(&tracked);
        return;
    }

    let mut collected: Vec<isize> = Vec::new();
    // SAFETY: 回调只把指针还原为 Vec 引用并写入，调用期间有效。
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut collected as *mut _ as isize));
    }
    if collected.is_empty() {
        log::info!("Win+D（自实现）：当前没有可最小化的窗口，保持本软件桌面不变");
        return;
    }
    // SAFETY: 只读查询前台窗口。
    let foreground = unsafe { GetForegroundWindow() };
    log::info!(
        "Win+D（自实现）：最小化 {} 个普通窗口，原生桌面图标层保持隐藏",
        collected.len()
    );
    for h in &collected {
        let hwnd = HWND(*h as *mut core::ffi::c_void);
        // SAFETY: 句柄来自本次枚举；SW_SHOWMINNOACTIVE 最小化且不激活。
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
        }
    }
    *guard = Some(Tracked {
        windows: collected,
        foreground: foreground.0 as isize,
    });
}

fn restore(tracked: &Tracked) {
    let mut restored = 0usize;
    for h in &tracked.windows {
        let hwnd = HWND(*h as *mut core::ffi::c_void);
        // 只还原仍存在、且仍处于最小化的窗口：用户期间手动恢复过的窗口保持现状，
        // 新开的窗口不在清单里，不会被影响。
        // SAFETY: 只读查询 + SW_RESTORE 还原。
        unsafe {
            if IsWindow(Some(hwnd)).as_bool() && IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                restored += 1;
            }
        }
    }
    let fg = HWND(tracked.foreground as *mut core::ffi::c_void);
    // SAFETY: 句柄为操作前的前台窗口；失效时跳过。
    unsafe {
        if !fg.0.is_null() && IsWindow(Some(fg)).as_bool() {
            let _ = SetForegroundWindow(fg);
        }
    }
    log::info!(
        "Win+D（自实现）：还原 {restored}/{} 个窗口，恢复前台 {:?}",
        tracked.windows.len(),
        tracked.foreground
    );
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam 指向调用方栈上的 Vec<isize>，回调同步执行。
    let out = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
    let info = build_info(hwnd);
    let own_pid = std::process::id();
    if should_minimize(&info, own_pid) {
        out.push(hwnd.0 as isize);
    }
    BOOL(1)
}

fn build_info(hwnd: HWND) -> WindowInfo {
    let mut pid = 0u32;
    // SAFETY: 只读查询。
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    let mut class_buf = [0u16; 256];
    // SAFETY: 缓冲区大小正确，返回拷贝的 UTF-16 单元数。
    let class_len = unsafe { GetClassNameW(hwnd, &mut class_buf) };
    let class = String::from_utf16_lossy(&class_buf[..class_len.max(0) as usize]);
    // SAFETY: 只读查询窗口样式与 DWM 属性。
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
    let mut cloaked = 0u32;
    // SAFETY: DWMWA_CLOAKED 写入 u32；调用失败按未隐形处理。
    let _ = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    WindowInfo {
        pid,
        // SAFETY: 只读查询。
        visible: unsafe { IsWindowVisible(hwnd).as_bool() },
        iconic: unsafe { IsIconic(hwnd).as_bool() },
        cloaked: cloaked != 0,
        tool_window: ex_style & WS_EX_TOOLWINDOW.0 != 0,
        class,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> WindowInfo {
        WindowInfo {
            pid: 4242,
            visible: true,
            iconic: false,
            cloaked: false,
            tool_window: false,
            class: "Notepad".into(),
        }
    }

    #[test]
    fn captures_normal_visible_window() {
        assert!(should_minimize(&base(), 100));
    }

    #[test]
    fn skips_own_process() {
        let w = base();
        assert!(!should_minimize(&w, 4242));
    }

    #[test]
    fn skips_invisible_iconic_cloaked_and_tool_windows() {
        let mut w = base();
        w.visible = false;
        assert!(!should_minimize(&w, 100));
        let mut w = base();
        w.iconic = true;
        assert!(!should_minimize(&w, 100));
        let mut w = base();
        w.cloaked = true;
        assert!(!should_minimize(&w, 100));
        let mut w = base();
        w.tool_window = true;
        assert!(!should_minimize(&w, 100));
    }

    #[test]
    fn skips_shell_and_ime_windows() {
        for class in SHELL_CLASSES {
            let mut w = base();
            w.class = (*class).into();
            assert!(
                !should_minimize(&w, 100),
                "shell/IME 类 {class} 不应被最小化"
            );
        }
    }

    #[test]
    fn captures_frameless_app_without_minimize_box() {
        // Electron/WinUI 常见：无边框、自绘标题栏、没有 WS_MINIMIZEBOX，仍应参与显示桌面。
        let w = WindowInfo {
            class: "Chrome_WidgetWin_1".into(),
            ..base()
        };
        assert!(should_minimize(&w, 100));
    }

    #[test]
    fn state_is_empty_by_default() {
        let guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
        assert!(guard.is_none());
    }
}
