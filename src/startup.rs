//! 启动早期诊断：在创建任何窗口/线程之前同步落盘启动日志，并在致命失败时弹消息框。
//!
//! 背景：本程序是 `windows_subsystem = "windows"` 的 GUI 进程，`main` 返回 `Err` 时
//! Rust 只把错误写到 stderr，而 GUI 进程没有控制台，用户看到的就是「双击毫无反应」。
//! 因此启动链路的关键节点必须写文件；致命错误额外弹一个消息框，把失败原因和日志
//! 路径直接告诉用户。
//!
//! 日志顺序：`init()` → 各 `step()`（诊断构建/`DESKKEEPER_DIAG=1` 时逐条）→
//! `fatal()`（写文件 + 弹框）。

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows::core::{HSTRING, PCSTR};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONERROR, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
};

const MAX_BOOT_LOG: u64 = 256 * 1024;

static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// 诊断构建（`--features diag`）或环境变量 `DESKKEEPER_DIAG=1` 时输出逐步追踪。
pub fn diag_enabled() -> bool {
    cfg!(feature = "diag") || std::env::var_os("DESKKEEPER_DIAG").is_some()
}

/// `RtlGetVersion` 的 OSVERSIONINFOW（只取前 5 个字段 + CSD 文本，避免额外 feature）。
#[repr(C)]
struct OsVersionInfoW {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    csd: [u16; 128],
}

type RtlGetVersionFn = unsafe extern "system" fn(*mut OsVersionInfoW) -> i32;

fn os_version() -> Option<(u32, u32, u32)> {
    // SAFETY: 仅查询 ntdll 导出；返回的函数指针按 RtlGetVersion 签名调用。
    unsafe {
        let ntdll = GetModuleHandleW(&HSTRING::from("ntdll.dll")).ok()?;
        let name = b"RtlGetVersion\0";
        let proc = GetProcAddress(ntdll, PCSTR::from_raw(name.as_ptr()))?;
        let f: RtlGetVersionFn = std::mem::transmute(proc);
        let mut info = OsVersionInfoW {
            size: std::mem::size_of::<OsVersionInfoW>() as u32,
            major: 0,
            minor: 0,
            build: 0,
            platform: 0,
            csd: [0; 128],
        };
        if f(&mut info) == 0 {
            Some((info.major, info.minor, info.build))
        } else {
            None
        }
    }
}

/// 运行期探测某 DLL 是否导出指定函数（不依赖静态导入表）。
fn has_proc(dll: &str, func: &str) -> bool {
    let mut name: Vec<u8> = func.as_bytes().to_vec();
    name.push(0);
    // SAFETY: GetModuleHandleW 只加载已存在的模块；GetProcAddress 只做符号查询。
    unsafe {
        let Ok(module) = GetModuleHandleW(&HSTRING::from(dll)) else {
            return false;
        };
        GetProcAddress(module, PCSTR::from_raw(name.as_ptr())).is_some()
    }
}

/// 供 `main` 在日志里记录系统版本。
pub fn os_build() -> Option<(u32, u32, u32)> {
    os_version()
}

fn write_line(path: &std::path::Path, line: &str) {
    let stamp = crate::util::format_datetime(crate::util::now_secs());
    let text = format!("[{stamp}] {line}\n");
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    match opened {
        Ok(mut f) => {
            if f.write_all(text.as_bytes()).is_err() {
                eprintln!("{text}");
            }
        }
        Err(_) => eprintln!("{text}"),
    }
}

fn log_path() -> Option<&'static PathBuf> {
    LOG_PATH.get().and_then(|p| p.as_ref())
}

/// 启动第一行调用：选定可写的启动日志路径并写入环境信息。
pub fn init() {
    let candidates = [
        Some(crate::util::log_dir().join("startup.log")),
        std::env::var_os("TEMP").map(|t| PathBuf::from(t).join("DeskKeeper-startup.log")),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("deskkeeper-startup.log"))),
    ];
    let mut chosen: Option<PathBuf> = None;
    for path in candidates.into_iter().flatten() {
        if let Some(dir) = path.parent() {
            if std::fs::create_dir_all(dir).is_err() && !dir.exists() {
                continue;
            }
        }
        if std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .is_ok()
        {
            chosen = Some(path);
            break;
        }
    }
    let _ = LOG_PATH.set(chosen);
    let Some(path) = log_path() else { return };
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > MAX_BOOT_LOG {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }

    write_line(path, "========== DeskKeeper 启动 ==========");
    match os_version() {
        Some((major, minor, build)) => {
            write_line(
                path,
                &format!("系统: Windows {major}.{minor} build {build}"),
            );
            if (major, minor, build) < (10, 0, 17763) {
                write_line(
                    path,
                    "警告: 系统版本低于 1809（build 17763），部分能力（PMv2 DPI/通知区域 API）可能不可用",
                );
            }
        }
        None => write_line(path, "系统: 无法通过 RtlGetVersion 读取版本号"),
    }
    write_line(
        path,
        &format!(
            "进程: {} pid={} 架构={} 诊断构建={}",
            std::env::consts::OS,
            std::process::id(),
            std::env::consts::ARCH,
            diag_enabled()
        ),
    );
    write_line(
        path,
        &format!(
            "exe: {}",
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|e| format!("<无法获取: {e}>"))
        ),
    );
    write_line(
        path,
        &format!("数据目录: {}", crate::util::data_dir().display()),
    );
    // 运行期能力矩阵：这些函数是否真的能被解析，比静态导入表更能反映目标机能力。
    for (dll, func) in [
        ("user32.dll", "SetThreadDpiAwarenessContext"),
        ("user32.dll", "SetProcessDpiAwarenessContext"),
        ("user32.dll", "GetDpiForWindow"),
        ("user32.dll", "AdjustWindowRectExForDpi"),
        ("shcore.dll", "GetDpiForMonitor"),
        ("shcore.dll", "SetProcessDpiAwareness"),
        ("dwmapi.dll", "DwmSetWindowAttribute"),
        ("shell32.dll", "Shell_NotifyIconGetRect"),
        ("shell32.dll", "SHGetStockIconInfo"),
    ] {
        write_line(
            path,
            &format!(
                "能力 {dll}!{func}={}",
                if has_proc(dll, func) { "有" } else { "无" }
            ),
        );
    }
}

/// 记录一个启动阶段。仅在诊断构建或 `DESKKEEPER_DIAG=1` 时写入。
pub fn step(stage: &str) {
    if !diag_enabled() {
        return;
    }
    if let Some(path) = log_path() {
        write_line(path, &format!("阶段: {stage}"));
    }
}

/// 记录一条非致命但需要留痕的信息（始终写入启动日志）。
pub fn note(message: &str) {
    if let Some(path) = log_path() {
        write_line(path, message);
    }
}

/// 记录 panic（由 `main` 的 panic hook 调用）；诊断构建额外弹消息框。
pub fn panic_report(info: &str) {
    note(&format!("PANIC: {info}"));
    if diag_enabled() {
        alert(
            "DeskKeeper 异常退出",
            &format!("{info}\n\n启动日志：{}", log_path_text()),
        );
    }
}

/// 致命启动失败：写启动日志 + 同步刷主日志 + 弹消息框，然后由调用方 `exit`。
pub fn fatal(stage: &str, reason: &str) {
    note(&format!("FATAL [{stage}] {reason}"));
    log::logger().flush();
    let hint = log_path_text();
    alert(
        "DeskKeeper 启动失败",
        &format!("阶段：{stage}\n原因：{reason}\n\n启动日志：{hint}"),
    );
}

fn log_path_text() -> String {
    log_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<未能创建启动日志，请检查 %LOCALAPPDATA% 权限>".to_string())
}

/// 弹出消息框（阻塞直到用户确认）。GUI 进程没有控制台，这是唯一可见的失败反馈。
pub fn alert(title: &str, text: &str) {
    let title = HSTRING::from(title);
    let text = HSTRING::from(text);
    // SAFETY: 两个字符串在调用期间有效；无父窗口，图标/置顶样式保证用户能看到。
    unsafe {
        MessageBoxW(
            None,
            &text,
            &title,
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// 弹出启动警告（黄色感叹号），用于"非致命但用户应知道"的场景。
pub fn warn_box(title: &str, text: &str) {
    let title = HSTRING::from(title);
    let text = HSTRING::from(text);
    // SAFETY: 同上。
    unsafe {
        MessageBoxW(
            None,
            &text,
            &title,
            MB_OK | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}
