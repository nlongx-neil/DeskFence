//! Win32 平台能力封装。

pub mod autostart;
pub mod clipboard_native;
pub mod desktop;
pub mod icons;
pub mod input;
pub mod key_hook;
pub mod message_window;
pub mod path_state;
pub mod recycle;
pub mod shell;
pub mod shell_menu;
pub mod show_desktop;
pub mod single_instance;

use windows::core::{BOOL, HSTRING, PCSTR};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

/// 运行期把当前线程设为 Per-Monitor V2 DPI 感知。
///
/// 该函数（Windows 10 1703+）若写成静态导入，会进入 PE 导入表：在更低版本系统上
/// 进程会在加载阶段直接失败（连日志都写不出来）。因此这里用 `GetProcAddress` 探测，
/// 缺失时只记录并降级（线程沿用进程的 DPI 感知级别），不影响启动。
pub fn enable_thread_dpi_awareness_v2() -> bool {
    type SetThreadDpiAwarenessContextFn = unsafe extern "system" fn(DPI_AWARENESS_CONTEXT) -> BOOL;
    let mut name = b"SetThreadDpiAwarenessContext\0".to_vec();
    // SAFETY: 只查询已加载模块的导出函数；函数指针按官方签名调用。
    unsafe {
        let Ok(user32) = GetModuleHandleW(&HSTRING::from("user32.dll")) else {
            log::warn!(
                "获取 user32.dll 失败，消息线程沿用进程 DPI 感知: {}",
                windows::core::Error::from_thread()
            );
            return false;
        };
        let Some(proc) = GetProcAddress(user32, PCSTR::from_raw(name.as_mut_ptr())) else {
            log::warn!(
                "系统未导出 SetThreadDpiAwarenessContext（低于 Windows 10 1703），消息线程沿用进程 DPI 感知"
            );
            return false;
        };
        let set: SetThreadDpiAwarenessContextFn = std::mem::transmute(proc);
        if set(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).as_bool() {
            log::info!("消息线程 DPI 感知已设为 Per-Monitor V2（运行期探测）");
            true
        } else {
            log::warn!(
                "SetThreadDpiAwarenessContext 调用失败: {}",
                windows::core::Error::from_thread()
            );
            false
        }
    }
}

/// 物理像素矩形（虚拟屏幕坐标系）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PxRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl PxRect {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
}
