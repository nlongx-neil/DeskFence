//! 单实例互斥与唤醒既有实例。

use windows::core::{w, Error};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;

pub struct SingleInstance {
    handle: HANDLE,
}

impl SingleInstance {
    /// 尝试获取单实例锁。返回 `(guard, already_running)`。
    pub fn acquire() -> (Option<Self>, bool) {
        // SAFETY: 创建命名互斥体，无外部指针。
        let handle = unsafe { CreateMutexW(None, true, w!("Local\\DeskKeeper.SingleInstance")) };
        match handle {
            Ok(h) => {
                // SAFETY: 只读查询最后一个错误码。
                let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
                (Some(Self { handle: h }), already)
            }
            Err(e) => {
                log::warn!("创建单实例互斥体失败: {e}");
                (None, false)
            }
        }
    }

    /// 唤醒已运行的实例显示剪贴板盒。
    pub fn signal_existing() -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};
        // SAFETY: 查找本程序隐藏消息窗口并投递自定义消息。
        unsafe {
            let Ok(hwnd) = FindWindowW(w!("DeskKeeper.MsgWindow"), w!("DeskKeeper")) else {
                // 首次实例可能尚未创建消息窗口；由调用方短间隔重试，这里不刷警告。
                log::debug!(
                    "暂未找到已有实例的消息窗口: {}",
                    Error::from_thread().message()
                );
                return false;
            };
            PostMessageW(
                Some(hwnd),
                super::message_window::WM_APP_ACTIVATE,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            )
            .is_ok()
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: 句柄由 CreateMutexW 创建，所有权在本结构。
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}
