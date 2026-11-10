//! 桌面层接管：隐藏/恢复系统桌面图标、虚拟屏幕与多显示器度量、系统深浅色。

use super::PxRect;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::time::Duration;
use windows::core::{w, Error, BOOL, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, DeleteObject, EnumDisplayMonitors, GetMonitorInfoW, GetWindowRgn,
    MonitorFromPoint, RedrawWindow, SetWindowRgn, HDC, HMONITOR, HRGN, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, NULLREGION, RDW_ALLCHILDREN, RDW_INTERNALPAINT, RDW_INVALIDATE,
    RGN_OR,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForSystem, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, FindWindowW, GetClassNameW, GetForegroundWindow, GetShellWindow,
    GetSystemMetrics, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, PostMessageW, SendMessageW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, GWL_EXSTYLE, GW_HWNDPREV, HWND_NOTOPMOST, HWND_TOP,
    MONITORINFOF_PRIMARY, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOW, SW_SHOWNOACTIVATE,
    WM_COMMAND, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

/// 虚拟屏幕（包含所有显示器的物理像素矩形）。
pub fn virtual_screen() -> PxRect {
    // SAFETY: GetSystemMetrics 为只读查询，无指针参数。
    unsafe {
        PxRect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

/// 所有显示器（含任务栏区域）的物理像素矩形。
pub fn monitors() -> Vec<PxRect> {
    unsafe extern "system" fn cb(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut windows::Win32::Foundation::RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: data 由调用方传入指向 Vec<PxRect> 的有效指针，调用期间有效。
        let out = unsafe { &mut *(data.0 as *mut Vec<PxRect>) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: monitor 由系统回调提供，info 为栈上有效结构。
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            out.push(PxRect {
                x: info.rcMonitor.left,
                y: info.rcMonitor.top,
                w: info.rcMonitor.right - info.rcMonitor.left,
                h: info.rcMonitor.bottom - info.rcMonitor.top,
            });
        }
        TRUE
    }

    let mut out: Vec<PxRect> = Vec::new();
    let data = LPARAM(&mut out as *mut Vec<PxRect> as isize);
    // SAFETY: 回调在调用期间有效，data 指向栈上 Vec。
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), data);
    }
    if out.is_empty() {
        out.push(virtual_screen());
    }
    out
}

/// 判断某个物理像素点所在显示器的矩形。
pub fn monitor_at(x: i32, y: i32) -> PxRect {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: 查询接口，参数为有效结构指针。
    unsafe {
        let hmon = MonitorFromPoint(
            windows::Win32::Foundation::POINT { x, y },
            MONITOR_DEFAULTTONEAREST,
        );
        if GetMonitorInfoW(hmon, &mut info).as_bool() {
            return PxRect {
                x: info.rcMonitor.left,
                y: info.rcMonitor.top,
                w: info.rcMonitor.right - info.rcMonitor.left,
                h: info.rcMonitor.bottom - info.rcMonitor.top,
            };
        }
    }
    virtual_screen()
}

/// 各显示器的工作区（不含任务栏等系统保留区域），物理像素。
pub fn monitor_work_areas() -> Vec<PxRect> {
    unsafe extern "system" fn cb(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: data 由调用方传入指向 Vec<PxRect> 的有效指针，调用期间有效。
        let out = unsafe { &mut *(data.0 as *mut Vec<PxRect>) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: monitor 由系统回调提供，info 为栈上有效结构。
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            let work = info.rcWork;
            out.push(PxRect {
                x: work.left,
                y: work.top,
                w: work.right - work.left,
                h: work.bottom - work.top,
            });
        }
        TRUE
    }

    let mut out: Vec<PxRect> = Vec::new();
    let data = LPARAM(&mut out as *mut Vec<PxRect> as isize);
    // SAFETY: 回调在调用期间有效，data 指向栈上 Vec。
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), data);
    }
    if out.is_empty() {
        out.push(virtual_screen());
    }
    out
}

/// 所有显示器工作区的并集包围盒（单显示器时等于 `rcWork`，即排除任务栏）。
pub fn work_area() -> PxRect {
    let areas = monitor_work_areas();
    let mut left = i32::MAX;
    let mut top = i32::MAX;
    let mut right = i32::MIN;
    let mut bottom = i32::MIN;
    for a in &areas {
        left = left.min(a.x);
        top = top.min(a.y);
        right = right.max(a.right());
        bottom = bottom.max(a.bottom());
    }
    PxRect {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    }
}

/// 鼠标指针所在显示器的工作区（不含任务栏），物理像素。
/// 新建盒子默认停靠"当前屏"，因此以光标位置判定目标显示器。
pub fn cursor_work_area() -> PxRect {
    let mut pt = windows::Win32::Foundation::POINT::default();
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: 只读查询光标位置与显示器信息，结构均为栈上有效对象。
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        if GetMonitorInfoW(hmon, &mut info).as_bool() {
            let work = info.rcWork;
            return PxRect {
                x: work.left,
                y: work.top,
                w: work.right - work.left,
                h: work.bottom - work.top,
            };
        }
    }
    primary_work_area()
}

/// 主显示器的工作区（不含任务栏），物理像素。
pub fn primary_work_area() -> PxRect {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: 查询接口，参数为有效结构指针。
    unsafe {
        let hmon = MonitorFromPoint(
            windows::Win32::Foundation::POINT { x: 0, y: 0 },
            MONITOR_DEFAULTTONEAREST,
        );
        if GetMonitorInfoW(hmon, &mut info).as_bool() {
            let work = info.rcWork;
            return PxRect {
                x: work.left,
                y: work.top,
                w: work.right - work.left,
                h: work.bottom - work.top,
            };
        }
    }
    work_area()
}

/// 单块显示器的信息：工作区（物理像素）、有效 DPI 缩放、是否主屏。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonitorInfo {
    pub work: PxRect,
    pub scale: f32,
    pub primary: bool,
}

/// 枚举所有显示器，并读取每块屏幕各自的有效 DPI 缩放（非系统全局缩放）。
pub fn monitor_infos() -> Vec<MonitorInfo> {
    unsafe extern "system" fn cb(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: data 由调用方传入指向 Vec<MonitorInfo> 的有效指针，调用期间有效。
        let out = unsafe { &mut *(data.0 as *mut Vec<MonitorInfo>) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: monitor 由系统回调提供，info 为栈上有效结构。
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            let (mut dx, mut dy) = (0u32, 0u32);
            // SAFETY: 只读查询；失败时回落系统缩放。
            let scale = if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) }
                .is_ok()
                && dx > 0
            {
                dx as f32 / 96.0
            } else {
                system_scale().max(1.0)
            };
            let work = info.rcWork;
            out.push(MonitorInfo {
                work: PxRect {
                    x: work.left,
                    y: work.top,
                    w: work.right - work.left,
                    h: work.bottom - work.top,
                },
                scale: scale.max(1.0),
                primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
            });
        }
        TRUE
    }

    let mut out: Vec<MonitorInfo> = Vec::new();
    let data = LPARAM(&mut out as *mut Vec<MonitorInfo> as isize);
    // SAFETY: 回调在调用期间有效，data 指向栈上 Vec。
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), data);
    }
    if out.is_empty() {
        out.push(MonitorInfo {
            work: work_area(),
            scale: system_scale().max(1.0),
            primary: true,
        });
    }
    out
}

/// 物理点所在显示器的工作区；用于把盒子/图标归属到某块屏幕。
pub fn monitor_at_info(monitors: &[MonitorInfo], x: f32, y: f32) -> Option<MonitorInfo> {
    let inside = |m: &&MonitorInfo| {
        x >= m.work.x as f32
            && x <= m.work.right() as f32
            && y >= m.work.y as f32
            && y <= m.work.bottom() as f32
    };
    if let Some(m) = monitors.iter().find(inside) {
        return Some(*m);
    }
    // 点落在显示器缝隙（L 形工作区）时取最近中心，避免缩放回落到错误值。
    monitors.iter().copied().min_by(|a, b| {
        let da = (a.work.x as f32 + a.work.w as f32 * 0.5 - x).powi(2)
            + (a.work.y as f32 + a.work.h as f32 * 0.5 - y).powi(2);
        let db = (b.work.x as f32 + b.work.w as f32 * 0.5 - x).powi(2)
            + (b.work.y as f32 + b.work.h as f32 * 0.5 - y).powi(2);
        da.total_cmp(&db)
    })
}

/// 物理点所在的显示器缩放；找不到时返回 1.0。
pub fn scale_at(monitors: &[MonitorInfo], x: f32, y: f32) -> f32 {
    monitor_at_info(monitors, x, y)
        .map(|m| m.scale)
        .unwrap_or(1.0)
}

/// 主显示器缩放。
pub fn primary_scale(monitors: &[MonitorInfo]) -> f32 {
    monitors
        .iter()
        .find(|m| m.primary)
        .map(|m| m.scale)
        .unwrap_or_else(|| system_scale().max(1.0))
}

/// 把叠加层窗口的可见区域裁剪为"各显示器工作区的并集"，从而绝不覆盖任务栏。
///
/// 单一窗口矩形无法表示 L 形工作区（多显示器且任务栏只在某块屏上），
/// 因此这里用 `SetWindowRgn`（区域支持多个矩形）。裁剪对 topmost 状态同样生效，
/// `Win+D` 置顶期间任务栏也不会被盖住。
///
/// 返回 `false` 表示窗口尚未移动到预期工作区原点（例如显示器切换后位置命令
/// 还未生效），调用方应稍后重试；此时不会写入基于旧原点的错误区域。
pub fn apply_work_region() -> bool {
    let Some(hwnd) = overlay_hwnd() else {
        return false;
    };
    let expected = work_area();
    let mut rect = RECT::default();
    // SAFETY: hwnd 来自本进程窗口查询，rect 为栈上有效结构。
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return false;
    }
    if (rect.left - expected.x).abs() > 2 || (rect.top - expected.y).abs() > 2 {
        return false;
    }
    let areas = monitor_work_areas();
    let mut region: Option<HRGN> = None;
    for area in &areas {
        // 窗口无边框，客户区原点即窗口矩形左上角；区域坐标需换算到客户区。
        // SAFETY: CreateRectRgn 为纯创建调用，参数为普通整数。
        let r = unsafe {
            CreateRectRgn(
                area.x - rect.left,
                area.y - rect.top,
                area.right() - rect.left,
                area.bottom() - rect.top,
            )
        };
        if r.is_invalid() {
            continue;
        }
        match region {
            // SAFETY: 两个区域句柄均有效；合并后释放临时区域。
            Some(dst) => {
                let _ = unsafe { CombineRgn(Some(dst), Some(dst), Some(r), RGN_OR) };
                let _ = unsafe { DeleteObject(r.into()) };
            }
            None => region = Some(r),
        }
    }
    let Some(region) = region else {
        log::warn!("无法为叠加层创建任何工作区区域: {}", Error::from_thread());
        return false;
    };
    // SAFETY: hwnd 为本进程窗口；成功后区域所有权移交系统，失败时由本函数释放。
    let ok = unsafe { SetWindowRgn(hwnd, Some(region), true) } != 0;
    if ok {
        log::info!(
            "叠加层已按工作区裁剪（排除任务栏），共 {} 块显示器工作区",
            areas.len()
        );
    } else {
        let _ = unsafe { DeleteObject(region.into()) };
        log::warn!("按工作区裁剪叠加层失败: {}", Error::from_thread());
    }
    ok
}

/// 把叠加层窗口区域裁成空（0×0）：视觉上等同隐藏、不拦截鼠标，但窗口仍处于
/// 可见状态，能继续接收重绘与托盘事件。
///
/// 不能用 `ViewportCommand::Visible(false)`：窗口一旦真正隐藏，winit 往往不再派发
/// 重绘，托盘里的"显示叠加层 / 退出"事件就再也处理不到，表现为点了没反应。
pub fn set_empty_region() -> bool {
    let Some(hwnd) = overlay_hwnd() else {
        return false;
    };
    // SAFETY: CreateRectRgn 为纯创建调用；空区域使窗口不可见且不参与命中测试。
    let region = unsafe { CreateRectRgn(0, 0, 0, 0) };
    if region.is_invalid() {
        log::warn!("创建空区域失败: {}", Error::from_thread());
        return false;
    }
    // SAFETY: hwnd 为本进程窗口；成功后区域所有权移交系统，失败时由本函数释放。
    let ok = unsafe { SetWindowRgn(hwnd, Some(region), true) } != 0;
    if ok {
        log::info!("叠加层已隐藏（区域裁空，窗口仍保留以接收托盘事件）");
    } else {
        let _ = unsafe { DeleteObject(region.into()) };
        log::warn!("清空叠加层区域失败: {}", Error::from_thread());
    }
    ok
}

/// 系统 DPI 缩放比例（1.0 = 96 DPI）。
pub fn system_scale() -> f32 {
    // SAFETY: 只读查询。
    let dpi = unsafe { GetDpiForSystem() };
    if dpi == 0 {
        1.0
    } else {
        dpi as f32 / 96.0
    }
}

/// 读取系统是否使用深色主题（注册表 AppsUseLightTheme == 0）。
pub fn system_is_dark() -> bool {
    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let subkey = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let name = w!("AppsUseLightTheme");
    // SAFETY: 输出缓冲区为有效栈上内存，长度正确。
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey,
            name,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    ok.is_ok() && value == 0
}

/// 找到承载桌面图标的 `SHELLDLL_DefView` 窗口。
fn find_def_view() -> Option<HWND> {
    unsafe {
        // 1) Progman 直接子窗口（经典布局）
        let progman = FindWindowW(w!("Progman"), PCWSTR::null()).ok()?;
        if let Ok(def) = FindWindowExW(Some(progman), None, w!("SHELLDLL_DefView"), PCWSTR::null())
        {
            return Some(def);
        }

        // 2) 遍历所有顶层窗口，查找 WorkerW / Progman 下的 DefView
        struct FindCtx {
            found: Option<HWND>,
        }
        unsafe extern "system" fn cb(hwnd: HWND, data: LPARAM) -> BOOL {
            // SAFETY: data 指向 FindCtx，调用期间有效。
            let ctx = unsafe { &mut *(data.0 as *mut FindCtx) };
            let mut buf = [0u16; 128];
            // SAFETY: buf 长度正确，hwnd 由系统提供。
            let n = unsafe { GetClassNameW(hwnd, &mut buf) };
            if n <= 0 {
                return TRUE;
            }
            let class = String::from_utf16_lossy(&buf[..n as usize]);
            if class == "Progman" || class == "WorkerW" {
                // SAFETY: hwnd 有效。
                if let Ok(def) = unsafe {
                    FindWindowExW(Some(hwnd), None, w!("SHELLDLL_DefView"), PCWSTR::null())
                } {
                    ctx.found = Some(def);
                    return BOOL(0);
                }
            }
            TRUE
        }

        let mut ctx = FindCtx { found: None };
        let data = LPARAM(&mut ctx as *mut FindCtx as isize);
        let _ = EnumWindows(Some(cb), data);
        if ctx.found.is_some() {
            return ctx.found;
        }

        // 3) 退化：全局搜索 DefView 顶层窗口
        FindWindowW(w!("SHELLDLL_DefView"), PCWSTR::null()).ok()
    }
}

/// 叠加层窗口标题（`main.rs` 创建视口时使用，层级调整时按标题查找窗口）。
pub const OVERLAY_TITLE: &str = "DeskKeeper 桌面整理层";

/// 「未找到叠加层窗口」是否已记录过（避免 100ms 巡检刷屏）。
static OVERLAY_MISSING_LOGGED: AtomicBool = AtomicBool::new(false);

/// 是否正处于「临时接收键盘输入」状态（待办/重命名行内编辑）。
///
/// 叠加层默认 `WS_EX_NOACTIVATE`，键盘消息到不了 egui；编辑开始时临时清除该样式并
/// 主动置前台，结束后恢复，让普通点击回到「不抢焦点」。
static TEXT_INPUT_ACTIVE: AtomicBool = AtomicBool::new(false);

fn force_foreground(hwnd: HWND) {
    // SAFETY: hwnd 为本进程或系统窗口；SetForegroundWindow 失败时用 AttachThreadInput 兜底。
    if unsafe { SetForegroundWindow(hwnd) }.as_bool() {
        // 仅置前台还不够：winit 只有在收到 WM_SETFOCUS 后才把键盘事件交给 egui。
        unsafe {
            let _ = SetFocus(Some(hwnd));
        }
        return;
    }
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() {
        return;
    }
    let fg_tid = unsafe { GetWindowThreadProcessId(fg, None) };
    let my_tid = unsafe { GetCurrentThreadId() };
    if fg_tid == 0 || fg_tid == my_tid {
        return;
    }
    unsafe {
        let _ = AttachThreadInput(my_tid, fg_tid, true);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        let _ = AttachThreadInput(my_tid, fg_tid, false);
    }
}

/// 进入行内编辑：临时允许叠加层激活并置前台，使 egui 能收到键盘事件。
pub fn begin_text_input() -> bool {
    if TEXT_INPUT_ACTIVE.swap(true, Ordering::SeqCst) {
        return true;
    }
    let Some(overlay) = overlay_hwnd() else {
        TEXT_INPUT_ACTIVE.store(false, Ordering::SeqCst);
        return false;
    };
    // SAFETY: overlay 为本进程窗口，仅临时修改扩展样式。
    unsafe {
        let current = GetWindowLongPtrW(overlay, GWL_EXSTYLE);
        let want = current & !(WS_EX_NOACTIVATE.0 as isize);
        if want != current {
            SetWindowLongPtrW(overlay, GWL_EXSTYLE, want);
        }
    }
    force_foreground(overlay);
    true
}

/// 结束行内编辑：恢复 `WS_EX_NOACTIVATE`，并把前台交还桌面。
pub fn end_text_input() {
    if !TEXT_INPUT_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let Some(overlay) = overlay_hwnd() else {
        return;
    };
    // SAFETY: overlay 为本进程窗口，恢复不抢焦点样式。
    unsafe {
        let current = GetWindowLongPtrW(overlay, GWL_EXSTYLE);
        SetWindowLongPtrW(overlay, GWL_EXSTYLE, current | WS_EX_NOACTIVATE.0 as isize);
    }
    if let Some(defview) = find_def_view() {
        force_foreground(defview);
    } else {
        // SAFETY: 桌面 shell 窗口由系统创建，句柄可能为空。
        let shell = unsafe { GetShellWindow() };
        if !shell.0.is_null() {
            force_foreground(shell);
        }
    }
}

/// 校正叠加层窗口扩展样式：不占任务栏/Alt+Tab（`WS_EX_TOOLWINDOW`），点击不抢焦点
/// （`WS_EX_NOACTIVATE`）。`eframe` 的 `with_taskbar(false)` 在 Windows 上并不保证
/// 清除 `WS_EX_APPWINDOW`，实测仍会出现任务栏按钮，因此这里运行时兜底。
pub fn ensure_overlay_styles() -> bool {
    // 行内编辑期间不能把 NOACTIVATE 加回去，否则正在输入的文本框会立刻失去键盘。
    if TEXT_INPUT_ACTIVE.load(Ordering::SeqCst) {
        return true;
    }
    let Some(overlay) = overlay_hwnd() else {
        // 100ms 巡检会反复调用，这里只记一次，避免刷屏。
        if !OVERLAY_MISSING_LOGGED.swap(true, Ordering::Relaxed) {
            log::warn!(
                "未找到叠加层窗口（标题 \"{OVERLAY_TITLE}\"），跳过样式校正: {}",
                Error::from_thread()
            );
        }
        return false;
    };
    OVERLAY_MISSING_LOGGED.store(false, Ordering::Relaxed);
    // SAFETY: 只读查询扩展样式。
    let current = unsafe { GetWindowLongPtrW(overlay, GWL_EXSTYLE) };
    let add = (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) as isize;
    let clear = WS_EX_APPWINDOW.0 as isize;
    let want = (current | add) & !clear;
    if want != current {
        // SAFETY: overlay 为本进程窗口，修改样式不会重建窗口。
        unsafe {
            SetWindowLongPtrW(overlay, GWL_EXSTYLE, want);
        }
        log::info!("叠加层窗口样式已校正：工具窗口 / 不抢焦点 / 不进任务栏（ex=0x{want:X}）");
    }
    true
}

/// 桌面宿主窗口的最小面积（物理像素²）。系统里存在大量 135x37 大小的隐藏
/// `WorkerW` 辅助窗口（缩略图/输入法之类），它们位于 z 序最顶层，不能当作壁纸宿主。
const DESKTOP_HOST_MIN_AREA: i64 = 300_000;

/// z 序中最高（最靠近顶层）的**可见**桌面宿主窗口：`Progman` 或 `WorkerW`。
///
/// `EnumWindows` 按 z 序自上而下枚举，因此第一个命中的大尺寸可见桌面宿主即最高者。
/// 必须用"最高的宿主"作参照：有的机器壁纸由 `WorkerW` 绘制且它位于 `Progman` 之上，
/// 只插到 `Progman` 之上仍会被壁纸窗口盖住。
fn top_desktop_host() -> Option<HWND> {
    #[derive(Default)]
    struct Ctx {
        found: Option<HWND>,
    }

    unsafe extern "system" fn cb(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: data 指向 Ctx，调用期间有效。
        let ctx = unsafe { &mut *(data.0 as *mut Ctx) };
        // 隐藏的 WorkerW 不是壁纸宿主；若误选它们，锚点会落在 topmost 窗口上，
        // 叠加层会被插到普通窗口之上（实测会压住资源管理器/记事本）。
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return TRUE;
        }
        let mut buf = [0u16; 64];
        // SAFETY: buf 长度正确，hwnd 由系统提供。
        let n = unsafe { GetClassNameW(hwnd, &mut buf) };
        if n <= 0 {
            return TRUE;
        }
        let class = String::from_utf16_lossy(&buf[..n as usize]);
        if class != "Progman" && class != "WorkerW" {
            return TRUE;
        }
        let mut rect = RECT::default();
        // SAFETY: rect 为栈上有效结构，hwnd 由系统提供。
        if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
            return TRUE;
        }
        let area = (rect.right - rect.left) as i64 * (rect.bottom - rect.top) as i64;
        if area < DESKTOP_HOST_MIN_AREA {
            return TRUE;
        }
        ctx.found = Some(hwnd);
        BOOL(0)
    }

    let mut ctx = Ctx::default();
    let data = LPARAM(&mut ctx as *mut Ctx as isize);
    // SAFETY: 回调签名符合 EnumWindows 约定，data 在调用期间有效。
    let _ = unsafe { EnumWindows(Some(cb), data) };
    ctx.found
}

/// 层级调整失败的累计次数与上次记录时间（秒）。
/// 守护线程约每 100ms 巡检一次，失败时若每次都 `warn!` 会在几小时内写满日志；
/// 这里只在首次失败、以及距上次记录 ≥60s 时再记一条，恢复时补一条 info。
static PLACE_FAILS: AtomicU32 = AtomicU32::new(0);
static PLACE_FAIL_LOGGED_SECS: AtomicU64 = AtomicU64::new(0);

/// 把叠加层窗口放到"原生桌面之上、所有普通窗口之下"。
///
/// `SetWindowPos` 的 `hWndInsertAfter` 表示"排在该窗口之后（z 序更低）"，因此不能直接
/// 传桌面宿主（那样叠加层会落到壁纸之下，实测盒子完全不可见）。正确做法是取桌面宿主
/// 在 z 序中紧邻的上一个窗口作锚点，让叠加层正好贴在宿主之上、其余普通窗口仍在其上。
/// 若宿主上方只剩 topmost 窗口，则退化为插到非 topmost 层最顶（topmost 窗口天然在
/// 非 topmost 层之上，因此仍不会压住普通窗口）。
pub fn place_overlay_above_desktop() -> bool {
    // SAFETY: 按唯一标题查找本进程的顶层窗口，失败时返回 None。
    let Ok(overlay) = (unsafe { FindWindowW(None, &HSTRING::from(OVERLAY_TITLE)) }) else {
        return false;
    };
    ensure_overlay_styles();
    let Some(host) = top_desktop_host() else {
        return false;
    };
    // SAFETY: 只读查询 z 序中宿主窗口的上一个窗口。
    let anchor = unsafe { GetWindow(host, GW_HWNDPREV) }
        .ok()
        .filter(|w| !w.0.is_null());
    let insert_after = match anchor {
        // 叠加层已经紧贴宿主之上，无需重复调整（避免每轮轮询抖动）。
        Some(w) if w == overlay => return true,
        // 锚点必须是普通层级窗口，否则 SetWindowPos 可能把叠加层一并变成 topmost。
        Some(w) if !is_topmost(w) => w,
        _ => HWND_TOP,
    };
    // SAFETY: overlay 为本进程窗口；insert_after 为已存在的目标窗口；
    // SWP_NOACTIVATE 保证不抢焦点，NOMOVE/NOSIZE 保持位置与尺寸不变。
    let ok = unsafe {
        SetWindowPos(
            overlay,
            Some(insert_after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
    if ok.is_err() {
        let fails = PLACE_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
        let now = crate::util::now_secs();
        let last = PLACE_FAIL_LOGGED_SECS.load(Ordering::Relaxed);
        // 首次失败 + 之后每 60s 最多一条，避免巡检循环刷屏（日志写入虽已异步，仍不应制造噪声）
        if fails == 1 || now.saturating_sub(last) >= 60 {
            PLACE_FAIL_LOGGED_SECS.store(now, Ordering::Relaxed);
            log::warn!(
                "调整叠加层层级失败（累计 {fails} 次）: {}",
                windows::core::Error::from_thread()
            );
        }
        return false;
    }
    let recovered = PLACE_FAILS.swap(0, Ordering::Relaxed);
    if recovered > 0 {
        log::info!("叠加层层级已恢复（此前连续失败 {recovered} 次）");
    }
    log::debug!("叠加层已重插：位于桌面宿主之上、普通窗口之下");
    true
}

/// 是否处于"接管桌面"状态（隐藏系统图标）。守护线程只在接管状态下维护层级。
static TAKEOVER: AtomicBool = AtomicBool::new(false);
/// 上一次轮询时是否处于"显示桌面"状态，用于只在状态切换时写日志。
static PEEK_ACTIVE: AtomicBool = AtomicBool::new(false);
/// 进入/退出显示桌面后需要补发的强制重绘次数。
/// 叠加层长时间被全屏窗口遮挡时，系统会停止投递重绘；Win+D 重新露出窗口时
/// DWM 可能只显示一张空表面，这里连发几帧覆盖 peek 动画过程。
static PEEK_REPAINT: AtomicU8 = AtomicU8::new(0);

/// 请求叠加层立即重绘一帧（等价于 winit `Window::request_redraw`）。
fn force_repaint(hwnd: HWND) {
    // SAFETY: hwnd 为本进程的叠加层窗口；只触发 WM_PAINT，不改变窗口状态。
    unsafe {
        let _ = RedrawWindow(
            Some(hwnd),
            None,
            None,
            RDW_INVALIDATE | RDW_INTERNALPAINT | RDW_ALLCHILDREN,
        );
    }
}
/// 层级守护轮询间隔（毫秒）。只做几个只读查询，开销可忽略。
const LAYER_POLL_MS: u64 = 100;

/// 由 App 在启用/停用接管时更新。
pub fn set_takeover_enabled(enabled: bool) {
    TAKEOVER.store(enabled, Ordering::Relaxed);
    if !enabled {
        PEEK_ACTIVE.store(false, Ordering::Relaxed);
    }
}

/// 叠加层窗口的区域是否被裁成空（`set_empty_region` 的"隐藏"状态）。
fn overlay_region_empty(hwnd: HWND) -> bool {
    // SAFETY: CreateRectRgn 为纯创建调用；随后只读查询区域类型并释放。
    unsafe {
        let rgn = CreateRectRgn(0, 0, 0, 0);
        if rgn.is_invalid() {
            return false;
        }
        let kind = GetWindowRgn(hwnd, rgn);
        let _ = DeleteObject(rgn.into());
        kind == NULLREGION
    }
}

/// 叠加层此刻是否真正对用户可见：窗口可见、未最小化、区域未被裁空。
fn overlay_showing(hwnd: HWND) -> bool {
    // SAFETY: 均为只读查询。
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let iconic = unsafe { IsIconic(hwnd) }.as_bool();
    visible && !iconic && !overlay_region_empty(hwnd)
}

fn overlay_hwnd() -> Option<HWND> {
    // SAFETY: 按唯一标题查找本进程窗口，失败返回 Err。
    unsafe { FindWindowW(None, &HSTRING::from(OVERLAY_TITLE)).ok() }
}

fn is_topmost(hwnd: HWND) -> bool {
    // SAFETY: 只读查询窗口扩展样式。
    unsafe { (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize) != 0 }
}

/// 强制叠加层脱离 topmost：任何路径都不允许它压到普通窗口之上。
///
/// 若发现 topmost 位（系统行为或外部工具设置），立即清除并重新插回桌面宿主之后，
/// 保证叠加层始终位于「原生桌面之上、普通窗口之下」。
fn enforce_not_topmost(hwnd: HWND) -> bool {
    if !is_topmost(hwnd) {
        return false;
    }
    // SAFETY: hwnd 为本进程叠加层窗口；HWND_NOTOPMOST 只改层级，不动位置与尺寸。
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
    log::warn!("叠加层被置为 topmost，已强制恢复普通层级");
    place_overlay_above_desktop();
    true
}

/// 隐藏所有可见的桌面图标层（`Progman` / `WorkerW` 下的 `SHELLDLL_DefView`）。
///
/// 系统在"显示桌面"时可能重建 `WorkerW`，直接缓存旧句柄会失效，因此这里每次
/// 都重新枚举，逐个隐藏。
fn ensure_icons_hidden() {
    #[derive(Default)]
    struct Ctx {
        hidden: bool,
    }

    unsafe extern "system" fn cb(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: data 指向 Ctx，调用期间有效。
        let ctx = unsafe { &mut *(data.0 as *mut Ctx) };
        let mut buf = [0u16; 128];
        // SAFETY: buf 长度正确，hwnd 由系统提供。
        let n = unsafe { GetClassNameW(hwnd, &mut buf) };
        if n <= 0 {
            return TRUE;
        }
        let class = String::from_utf16_lossy(&buf[..n as usize]);
        if class != "Progman" && class != "WorkerW" {
            return TRUE;
        }
        // SAFETY: hwnd 来自系统；查找与显示/隐藏调用均合法。
        unsafe {
            if let Ok(def) = FindWindowExW(Some(hwnd), None, w!("SHELLDLL_DefView"), PCWSTR::null())
            {
                if IsWindowVisible(def).as_bool() {
                    let _ = ShowWindow(def, SW_HIDE);
                    ctx.hidden = true;
                }
            }
        }
        TRUE
    }

    let mut ctx = Ctx::default();
    let data = LPARAM(&mut ctx as *mut Ctx as isize);
    // SAFETY: 回调签名与 EnumWindows 约定一致，data 在调用期间有效。
    let _ = unsafe { EnumWindows(Some(cb), data) };
    if ctx.hidden {
        log::info!("桌面图标层被系统重新显示，已再次隐藏");
        // 隐藏后强制重绘桌面，让图标立即消失
        refresh_desktop();
    }
}

/// 强制显示桌面图标层（退出时兜底：守护线程可能在接管期间重新隐藏过图标层，
/// 需要保证退出一定恢复）。
pub fn show_icon_layer() {
    let Some(def) = find_def_view() else {
        return;
    };
    // SAFETY: def 来自系统枚举；SW_SHOW 合法。
    unsafe {
        let _ = ShowWindow(def, SW_SHOW);
    }
}

/// 维护叠加层层级，解决两类系统行为造成的遮挡：
///
/// 1. 按 `Win+D` 显示桌面时，系统会把桌面窗口临时抬到普通窗口之上，叠加层被遮住；
/// 2. 显示桌面后系统会把被隐藏的桌面图标层重新显示出来。
///
/// 检测方式是"前台窗口是否为 shell 桌面窗口"（实测 `Win+D` 时前台窗口正是 `Progman`）。
/// Win+D 正常由 `key_hook` 拦截；这里只作为兜底：绝不显示系统图标层，也不使用临时
/// 置顶（避免残留后压住普通窗口），而是把叠加层重新插回桌面宿主之后并强制重绘。
pub fn ensure_overlay_layer() {
    if !TAKEOVER.load(Ordering::Relaxed) {
        return;
    }
    let Some(overlay) = overlay_hwnd() else {
        return;
    };
    // 部分系统在显示桌面/切换顶层时会重新应用创建时的窗口样式，导致
    // WS_EX_APPWINDOW 回归、点击抢焦点；每次轮询都兜底校正（通常无变化，开销可忽略）。
    ensure_overlay_styles();
    // 每轮巡检都确保叠加层不是 topmost：即使被外部工具或系统行为置顶，
    // 也会立刻退回普通层级，避免压住普通应用窗口。
    enforce_not_topmost(overlay);
    // 每轮巡检都把叠加层插回桌面宿主之上。启动瞬间桌面结构可能尚未稳定
    // （此时锚点落在 topmost 窗口上，叠加层会暂落到普通窗口顶部），或系统重排 z 序，
    // 下一次巡检（约 100ms）内都会自动回到「原生桌面之上、普通窗口之下」。
    place_overlay_above_desktop();
    // SAFETY: 只读查询。
    if !unsafe { IsWindowVisible(overlay) }.as_bool() {
        return;
    }
    // SAFETY: 只读查询 shell 桌面窗口。
    let shell = unsafe { GetShellWindow() };
    if shell.0.is_null() {
        return;
    }
    let topmost = is_topmost(overlay);
    // SAFETY: 只读查询前台窗口。
    let foreground = unsafe { GetForegroundWindow() };
    // 桌面显示期间：前台是 shell 桌面窗口。叠加层带 WS_EX_NOACTIVATE，点击不会抢焦点，
    // 因此不需要额外判断"前台是否为叠加层自身"。
    let desktop_shown = foreground == shell;
    let was_peek = PEEK_ACTIVE.swap(desktop_shown, Ordering::Relaxed);

    if desktop_shown {
        // Win+D 已由低级键盘钩子拦截；若仍进入显示桌面（钩子安装失败或被其它
        // 方式触发），绝不显示系统桌面图标层：保持本软件桌面，把叠加层重新插回
        // 桌面宿主之后并补发重绘。不做临时置顶，避免压住普通应用窗口。
        if overlay_showing(overlay) {
            ensure_icons_hidden();
            place_overlay_above_desktop();
            PEEK_REPAINT.store(3, Ordering::Relaxed);
        }
        if !was_peek {
            log::info!("进入显示桌面（Win+D 未被拦截）：保持本软件桌面，不显示系统图标");
        }
    } else if was_peek || topmost {
        // SAFETY: 同上，恢复普通层级。
        let _ = unsafe {
            SetWindowPos(
                overlay,
                Some(HWND_NOTOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        };
        place_overlay_above_desktop();
        // 退出显示桌面同样需要重新呈现，否则可能停留在 peek 期间的空表面。
        PEEK_REPAINT.store(3, Ordering::Relaxed);
        if !overlay_showing(overlay) {
            // 个别桌面/显卡组合会在 peek 中把叠加层最小化或裁空，这里做一次无焦点恢复。
            // SAFETY: overlay 为本进程窗口；SW_SHOWNOACTIVATE 恢复显示且不抢焦点。
            let _ = unsafe { ShowWindow(overlay, SW_SHOWNOACTIVATE) };
            let _ = apply_work_region();
        }
        if overlay_showing(overlay) {
            // 叠加层可见时重新压制系统图标，避免退出显示桌面后两层图标重叠。
            ensure_icons_hidden();
        }
        if was_peek {
            log::info!("退出显示桌面：恢复接管桌面图标，叠加层恢复桌面层级");
        }
    }

    // 补发强制重绘：Win+D 露出被遮挡的叠加层时，避免 DWM 显示空表面。
    let pending = PEEK_REPAINT.load(Ordering::Relaxed);
    if pending > 0 {
        force_repaint(overlay);
        PEEK_REPAINT.store(pending.saturating_sub(1), Ordering::Relaxed);
    }
}

/// 启动桌面层守护线程：周期检查层级与图标可见性。
pub fn spawn_layer_keeper() {
    let spawned = std::thread::Builder::new()
        .name("layer-keeper".into())
        .spawn(|| loop {
            std::thread::sleep(Duration::from_millis(LAYER_POLL_MS));
            ensure_overlay_layer();
        });
    if let Err(e) = spawned {
        log::warn!("启动桌面层守护线程失败: {e}");
    }
}

/// 桌面图标层接管句柄（用于恢复）。
#[derive(Debug, Default)]
pub struct DesktopLayer {
    def_view: Option<HWND>,
    hidden_by_command: bool,
    applied: bool,
}

// SAFETY: HWND 是系统句柄（isize），可跨线程传递；DesktopLayer 只在 UI 线程操作。
unsafe impl Send for DesktopLayer {}

impl DesktopLayer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 隐藏系统桌面图标。返回是否成功（失败时记录原因，不 panic）。
    pub fn hide(&mut self) -> bool {
        if self.applied {
            return true;
        }
        if let Some(def) = find_def_view() {
            // SAFETY: def 为有效 HWND，SW_HIDE 为合法命令。
            let vis = unsafe { IsWindowVisible(def) }.as_bool();
            // SAFETY: 同上。
            unsafe {
                let _ = ShowWindow(def, SW_HIDE);
            }
            self.def_view = Some(def);
            self.applied = true;
            log::info!("已隐藏桌面图标层 (DefView={def:?}, 之前可见={vis})");
            return true;
        }

        // 回退方案：向 Progman 发送 0x7402 命令切换图标显示
        // SAFETY: 仅向已知系统窗口发送只读命令消息。
        unsafe {
            if let Ok(progman) = FindWindowW(w!("Progman"), PCWSTR::null()) {
                let _ = SendMessageW(
                    progman,
                    WM_COMMAND,
                    Some(windows::Win32::Foundation::WPARAM(0x7402)),
                    None,
                );
                self.hidden_by_command = true;
                self.applied = true;
                log::warn!("未找到 DefView，改用 Progman 0x7402 命令隐藏桌面图标");
                return true;
            }
        }
        log::warn!("未能定位桌面图标层，跳过隐藏（可能为虚拟桌面/第三方桌面环境）");
        false
    }

    /// 恢复系统桌面图标（退出/崩溃恢复时调用）。
    pub fn restore(&mut self) {
        if !self.applied {
            return;
        }
        if let Some(def) = self.def_view.take() {
            // SAFETY: 句柄来自系统查询，SW_SHOW 合法。
            unsafe {
                let _ = ShowWindow(def, SW_SHOW);
            }
            log::info!("已恢复桌面图标层");
        } else if self.hidden_by_command {
            // SAFETY: 同上，再次发送命令切回可见状态。
            unsafe {
                if let Ok(progman) = FindWindowW(w!("Progman"), PCWSTR::null()) {
                    let _ = SendMessageW(
                        progman,
                        WM_COMMAND,
                        Some(windows::Win32::Foundation::WPARAM(0x7402)),
                        None,
                    );
                }
            }
            log::info!("已通过 0x7402 命令恢复桌面图标");
        }
        self.applied = false;
        self.hidden_by_command = false;
    }
}

impl Drop for DesktopLayer {
    fn drop(&mut self) {
        self.restore();
    }
}

/// 记录/读取"接管中"标记文件，用于崩溃后下次启动恢复桌面图标。
pub fn write_session_flag(hidden: bool) {
    let path = crate::util::state_path();
    let res = if hidden {
        std::fs::write(&path, b"hidden")
    } else {
        std::fs::remove_file(&path).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        })
    };
    if let Err(e) = res {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("写入桌面接管标记失败: {e}");
        }
    }
}

pub fn session_was_hidden() -> bool {
    crate::util::state_path().exists()
}

/// 尝试恢复上次异常退出时隐藏的桌面图标（启动早期调用）。
pub fn recover_stale_session() {
    if !session_was_hidden() {
        return;
    }
    log::warn!("检测到上次异常退出，尝试恢复桌面图标层");
    let mut layer = DesktopLayer::new();
    if layer.hide() {
        layer.restore();
    }
    write_session_flag(false);
}

/// 重绘/刷新桌面（隐藏或恢复图标后让壁纸重绘）。
pub fn refresh_desktop() {
    // SAFETY: 仅向已知窗口发送重绘消息。
    unsafe {
        if let Ok(progman) = FindWindowW(w!("Progman"), PCWSTR::null()) {
            let _ = PostMessageW(
                Some(progman),
                windows::Win32::UI::WindowsAndMessaging::WM_SETTINGCHANGE,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(x: i32, y: i32, w: i32, h: i32) -> PxRect {
        PxRect { x, y, w, h }
    }

    fn mon(x: i32, y: i32, w: i32, h: i32, scale: f32, primary: bool) -> MonitorInfo {
        MonitorInfo {
            work: px(x, y, w, h),
            scale,
            primary,
        }
    }

    #[test]
    fn scale_at_picks_owning_monitor() {
        let mons = vec![
            mon(0, 0, 3840, 2088, 1.5, true),
            mon(3840, 0, 1920, 1080, 1.0, false),
        ];
        assert_eq!(scale_at(&mons, 100.0, 100.0), 1.5);
        assert_eq!(scale_at(&mons, 3900.0, 100.0), 1.0);
        // 缝隙处取最近中心，而不是随意回落到 1.0
        assert_eq!(scale_at(&mons, 3000.0, 2000.0), 1.5);
        assert_eq!(primary_scale(&mons), 1.5);
    }

    #[test]
    fn scale_at_falls_back_when_empty() {
        assert_eq!(scale_at(&[], 0.0, 0.0), 1.0);
        assert_eq!(primary_scale(&[]), system_scale().max(1.0));
    }
}
