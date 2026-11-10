//! 键盘模拟与前台窗口辅助。

use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
    VK_CONTROL, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
};

/// 若当前前台窗口属于其他进程则返回其句柄（供消息窗口线程使用）。
pub fn foreground_if_other() -> WindowRef {
    WindowRef::foreground_if_other()
}

/// 前台窗口句柄（以 isize 形式跨线程携带）。
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowRef(pub isize);

impl WindowRef {
    pub fn foreground() -> Self {
        // SAFETY: 只读查询当前前台窗口。
        Self(unsafe { GetForegroundWindow() }.0 as isize)
    }

    pub fn is_valid(&self) -> bool {
        self.0 != 0
    }

    /// 若前台窗口属于其他进程则返回其句柄，否则返回空引用。
    pub fn foreground_if_other() -> Self {
        // SAFETY: 只读查询前台窗口与其进程 ID。
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_invalid() {
                return Self(0);
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 || pid == GetCurrentProcessId() {
                Self(0)
            } else {
                Self(hwnd.0 as isize)
            }
        }
    }

    /// 把焦点还给之前的窗口（失败时忽略，系统可能限制前台切换）。
    pub fn restore(&self) {
        if self.0 == 0 {
            return;
        }
        // SAFETY: 句柄来自系统查询，可能已失效；SetForegroundWindow 失败无害。
        unsafe {
            let _ = SetForegroundWindow(windows::Win32::Foundation::HWND(
                self.0 as *mut core::ffi::c_void,
            ));
        }
    }
}

fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 模拟按下并释放 Ctrl+V。
pub fn send_ctrl_v() -> bool {
    let inputs = [
        key(VK_CONTROL, false),
        key(VK_V, false),
        key(VK_V, true),
        key(VK_CONTROL, true),
    ];
    // SAFETY: inputs 为有效数组，长度与 cbSize 一致。
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        log::warn!(
            "SendInput 仅发送 {sent}/{} 个事件（可能被 UIPI 拦截）",
            inputs.len()
        );
    }
    sent as usize == inputs.len()
}
