#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! DeskKeeper：纯 Rust Windows 桌面整理 + 剪贴板历史。

use deskkeeper::app::DeskApp;
use deskkeeper::config::AppConfig;
use deskkeeper::platform::{desktop, single_instance::SingleInstance};
use deskkeeper::{logging, migration, startup, ui, util, watcher};
use eframe::egui;
use log::LevelFilter;

/// 迁移 CLI：`--migrate-boxes` 真实执行，`--migrate-boxes --dry-run` 只读报告。
///
/// 返回进程退出码：0 成功/仅 dry-run，2 已有实例在运行，3 存在失败条目。
fn run_migration_cli(dry_run: bool) -> i32 {
    let (guard, already_running) = SingleInstance::acquire();
    if already_running {
        let msg =
            "检测到 DeskKeeper 正在运行。迁移涉及真实文件，拒绝并行执行；请先退出应用再重试。";
        log::error!("{msg}");
        println!("{msg}");
        let _ = std::fs::write(util::data_dir().join("migration-report.txt"), msg);
        drop(guard);
        return 2;
    }

    let path = util::config_path();
    let (mut cfg, warning) = AppConfig::load(&path);
    if let Some(w) = &warning {
        log::warn!("{w}");
    }
    let boxes_dir = util::boxes_dir();
    let desktop = util::user_desktop_dir();
    let seeded = migration::seed_legacy_members(&mut cfg, &boxes_dir);
    let plan = migration::plan(&boxes_dir, &desktop, &cfg.boxes);
    let before = watcher::scan_dir(&desktop).len();

    let mut text = migration::format_plan(&plan, &desktop);
    if seeded > 0 {
        text.push_str(&format!(
            "已从旧盒子目录接管 {seeded} 个成员引用（只读，不移动文件）\n"
        ));
    }
    text.push_str(&format!("桌面文件数量：迁移前 {before}\n"));

    let mut code = 0;
    if dry_run {
        text.push_str(&format!(
            "预计迁移后桌面文件数量：{}\n（dry-run 未改动任何文件）\n",
            before + plan.total_files()
        ));
    } else {
        let report = migration::apply(&plan, &mut cfg, &desktop);
        let after = watcher::scan_dir(&desktop).len();
        text.push_str(&migration::format_report(&report));
        text.push_str(&format!("桌面文件数量：迁移前 {before}，迁移后 {after}\n"));
        if let Err(e) = AppConfig::save_now(&path, &cfg) {
            text.push_str(&format!("保存配置失败：{e}\n"));
            code = 3;
        }
        if report.failed() > 0 {
            code = 3;
        }
    }

    println!("{text}");
    log::info!("{text}");
    let _ = std::fs::create_dir_all(util::data_dir());
    if let Err(e) = std::fs::write(util::data_dir().join("migration-report.txt"), &text) {
        log::error!("写入迁移报告失败: {e}");
    }
    drop(guard);
    code
}

/// 从内嵌 ICO 取最大尺寸帧，供窗口图标使用（保证与托盘/exe 图标同源）。
fn window_icon() -> Option<egui::IconData> {
    let ico: &[u8] = include_bytes!("../assets/deskkeeper.ico");
    let count = u16::from_le_bytes([*ico.get(4)?, *ico.get(5)?]) as usize;
    let mut best: Option<(u32, usize, usize)> = None;
    for i in 0..count {
        let base = 6 + i * 16;
        let entry = ico.get(base..base + 16)?;
        let width = if entry[0] == 0 { 256 } else { entry[0] as u32 };
        let size = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
        let offset = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
        if best.is_none_or(|(w, _, _)| width > w) {
            best = Some((width, offset, size));
        }
    }
    let (_, offset, size) = best?;
    let png = ico.get(offset..offset.checked_add(size)?)?;
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    })
}

fn main() {
    // 最早一步：同步写启动日志（含 OS 版本/能力探测）。GUI 进程没有控制台，
    // 之后的任何失败都必须落到这个文件里，否则用户看到的就是「双击毫无反应」。
    startup::init();
    startup::step("logging::init");
    if let Err(e) = logging::init(&util::log_dir(), LevelFilter::Info) {
        startup::note(&format!("文件日志初始化失败（后续仅写 startup.log）: {e}"));
    }
    std::panic::set_hook(Box::new(|info| {
        startup::panic_report(&info.to_string());
        log::error!("未捕获的 panic: {info}");
    }));
    log::info!("DeskKeeper 启动");
    if let Some((major, minor, build)) = startup::os_build() {
        log::info!("系统版本: Windows {major}.{minor} build {build}");
    }

    let code = run();
    // 日志由后台线程落盘，退出前同步一次，避免尾部日志丢失。
    log::logger().flush();
    std::process::exit(code);
}

/// 真正的主流程。返回进程退出码；任何致命错误都会先写启动日志并弹消息框。
fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--migrate-boxes") {
        let dry_run = args.iter().any(|a| a == "--dry-run");
        return run_migration_cli(dry_run);
    }

    startup::step("配置加载");
    let (cfg, warning) = AppConfig::load(&util::config_path());
    log::set_max_level(if startup::diag_enabled() || cfg.general.log_debug {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    });
    if let Some(w) = &warning {
        log::warn!("{w}");
    }

    startup::step("创建数据目录");
    if let Err(e) = std::fs::create_dir_all(util::data_dir()) {
        log::error!("创建数据目录失败: {e}");
        startup::note(&format!("创建数据目录失败: {e}"));
    }
    if let Err(e) = std::fs::create_dir_all(util::thumbs_dir()) {
        log::error!("创建缩略图目录失败: {e}");
    }

    startup::step("单实例互斥");
    let (guard, already_running) = SingleInstance::acquire();
    if already_running {
        // 必须"先确认单实例、再做任何桌面层恢复"：否则第二个进程可能在退出前
        // 把第一个实例正在接管的桌面图标层恢复出来，导致桌面图标闪现/错乱。
        log::info!("检测到已有实例，本进程仅负责唤醒后退出");
        let mut woke = false;
        for attempt in 0..15 {
            if SingleInstance::signal_existing() {
                woke = true;
                log::info!("已向运行中实例发送唤醒请求（第 {} 次尝试）", attempt + 1);
                break;
            }
            // 首个实例可能刚启动、消息窗口尚未创建，短暂重试避免"双击无反应"。
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if !woke {
            // 只有这里仍保持静默的话，用户会以为程序"死了"：弹框告知真实原因。
            log::error!("已检测到运行中实例，但唤醒失败（消息窗口未响应）");
            startup::fatal(
                "单实例唤醒",
                "已检测到 DeskKeeper 正在运行，但无法唤醒它（消息窗口无响应）。\n\
                 可能是旧实例卡死或隐藏消息窗口创建失败：请在任务管理器结束该进程后重试。",
            );
            drop(guard);
            return 1;
        }
        drop(guard);
        return 0;
    }

    startup::step("恢复上次异常退出的桌面图标层");
    // 上次崩溃/强杀后可能遗留隐藏的桌面图标层，仅在拿到单实例锁后恢复。
    desktop::recover_stale_session();

    // 叠加层按显示器工作区定位/定尺（排除任务栏），而不是整屏矩形。
    startup::step("计算叠加层工作区");
    let area = desktop::work_area();
    let scale = desktop::system_scale().max(1.0);
    let size = egui::vec2(area.w as f32 / scale, area.h as f32 / scale);
    let origin = egui::pos2(area.x as f32 / scale, area.y as f32 / scale);
    log::info!(
        "工作区: {}x{} @ ({}, {})，缩放 {scale:.2}",
        area.w,
        area.h,
        area.x,
        area.y
    );
    startup::note(&format!(
        "工作区: {}x{} @ ({}, {})，系统缩放 {scale:.2}",
        area.w, area.h, area.x, area.y
    ));

    startup::step("构建视口参数");
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(desktop::OVERLAY_TITLE)
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_window_level(egui::WindowLevel::AlwaysOnBottom)
        .with_taskbar(false)
        .with_clamp_size_to_monitor_size(false)
        .with_inner_size(size)
        .with_position(origin);
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    } else {
        startup::note("exe 内嵌 ICO 解析失败，窗口图标缺失");
    }

    // 渲染只有一条真正的后端：glow/WGL。GL 上下文由静态导入的 opengl32.dll 提供，
    // 无法在同一进程内替换成软件实现，所以「降级」只能启动同二进制、但同目录放着
    // Mesa llvmpipe opengl32.dll 的 swgl 副本（优先硬件，失败后自动切过去）。
    let choice = renderer_choice();
    if choice == RendererChoice::Software {
        startup::note("DESKKEEPER_RENDERER 指定软件 OpenGL，跳过硬件尝试");
        // 必须先把单实例锁交出去，否则即将启动的软件副本会把自己判成「第二个实例」。
        drop(guard);
        return start_software_copy("按 DESKKEEPER_RENDERER 指定");
    }
    if software_copy_active() {
        startup::note("以软件 OpenGL 副本运行（app-local Mesa opengl32.dll）");
        startup::note("渲染后端: glow(OpenGL)，软件副本(Mesa llvmpipe)");
    } else {
        startup::note("渲染后端: glow(OpenGL)，优先硬件");
    }
    startup::step("渲染后端尝试: glow(OpenGL)");
    log::info!("渲染后端尝试: glow(OpenGL)");

    let attempt_cfg = cfg.clone();
    let attempt_warning = warning.clone();
    let options = native_options(viewport.clone());
    let result = eframe::run_native(
        "DeskKeeper",
        options,
        Box::new(move |cc| {
            log_graphics(cc);
            startup::step("DeskApp::new（托盘/热键/监听/首轮扫描）");
            ui::install_fonts(&cc.egui_ctx);
            let app = DeskApp::new(cc.egui_ctx.clone(), attempt_cfg, attempt_warning, guard);
            startup::step("DeskApp::new 完成");
            Ok(Box::new(app))
        }),
    );
    match result {
        Ok(()) => {
            log::info!("glow(OpenGL) 已正常退出");
            0
        }
        Err(e) => {
            let msg = format!("glow(OpenGL): {e}");
            log::error!("渲染后端失败 {msg}");
            startup::note(&format!("渲染后端失败: {msg}"));

            // 硬件 OpenGL 不可用（虚拟机基本显示适配器 / 远程桌面最常见）：
            // 自动改用 swgl 副本（Mesa llvmpipe 软件光栅化）。
            if software_copy_active() {
                startup::fatal(
                    "软件 OpenGL 降级",
                    &format!(
                        "软件 OpenGL 副本也未能创建上下文：\n{msg}\n\n\
                         请确认 swgl\\opengl32.dll 是 64 位 Mesa(llvmpipe) 构建，\
                         并与副本 exe 放在同一目录；详见 startup.log。"
                    ),
                );
                return 1;
            }
            if choice == RendererChoice::Hardware {
                startup::fatal(
                    "图形后端(glow)",
                    &format!(
                        "硬件 OpenGL 无法创建上下文：\n{msg}\n\n\
                         常见原因：虚拟机/远程桌面只有 Microsoft 基本显示适配器，\
                         没有 OpenGL 3.3；或未安装 VC++ 2015-2022 运行库（VCRUNTIME140.dll）。\n\
                         若本机有 swgl\\ 子目录，请直接运行 swgl\\ 下的副本，详见 startup.log。"
                    ),
                );
                return 1;
            }
            start_software_copy(&msg)
        }
    }
}

/// 用同二进制、同目录带 Mesa opengl32.dll 的副本启动（`swgl\<exe 名>`）。
///
/// 返回 0 表示子进程已成功拉起（父进程随即退出）；找不到副本文档或拉起失败则返回 1。
fn start_software_copy(reason: &str) -> i32 {
    let Some(exe) = software_copy_exe() else {
        startup::fatal(
            "软件 OpenGL 降级",
            &format!(
                "硬件 OpenGL 不可用（{reason}），且未找到软件降级副本。\n\n\
                 请把 swgl\\ 子目录（与主程序同名的 exe + Mesa 的 opengl32.dll）\
                 放在主程序旁边后重试。详见 startup.log。"
            ),
        );
        return 1;
    };
    startup::note(&format!(
        "降级: 启动软件 OpenGL 副本 {}（原因：{reason}）",
        exe.display()
    ));
    log::info!("启动软件 OpenGL 副本: {}", exe.display());

    let mut cmd = std::process::Command::new(&exe);
    // 子进程走纯 app-local Mesa 路径：清掉可能指向坏 DLL 的 WGL 覆盖变量，
    // 并标记「已是软件副本」，避免再次进入降级分支形成递归。
    cmd.env_remove("GLUTIN_WGL_OPENGL_DLL");
    cmd.env("DESKKEEPER_SOFTWARE_GL", "1");
    match cmd.spawn() {
        Ok(child) => {
            log::info!("软件 OpenGL 副本已启动 pid={}", child.id());
            0
        }
        Err(e) => {
            startup::fatal(
                "软件 OpenGL 降级",
                &format!("无法启动 {}：{e}\n详见 startup.log。", exe.display()),
            );
            1
        }
    }
}

/// `swgl\<当前 exe 文件名>`；不存在则返回 None。
fn software_copy_exe() -> Option<std::path::PathBuf> {
    if software_copy_active() {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join("swgl").join(exe.file_name()?);
    path.is_file().then_some(path)
}

fn software_copy_active() -> bool {
    matches!(
        std::env::var("DESKKEEPER_SOFTWARE_GL").ok().as_deref(),
        Some("1")
    )
}

/// `DESKKEEPER_RENDERER` 开关：`glow|hw` = 只跑硬件且不降级（排查用）；
/// `swgl|mesa|software` = 直接跑软件副本；缺省 = 硬件优先、失败自动降级。
fn renderer_choice() -> RendererChoice {
    // 已处在软件副本里：只跑本目录的 glow，不再触发降级（否则会自我递归）。
    if software_copy_active() {
        return RendererChoice::Hardware;
    }
    let value = std::env::var("DESKKEEPER_RENDERER")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase());
    match value.as_deref() {
        Some("glow") | Some("hw") | Some("hardware") => RendererChoice::Hardware,
        Some("swgl") | Some("mesa") | Some("software") => RendererChoice::Software,
        _ => RendererChoice::Auto,
    }
}

fn native_options(viewport: egui::ViewportBuilder) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    }
}

/// 记录实际使用的 OpenGL 厂商/渲染器/版本：这是判断「硬件 GL 还是 Mesa 软件降级」的直接证据。
fn log_graphics(cc: &eframe::CreationContext<'_>) {
    let Some(gl) = &cc.gl else { return };
    use eframe::glow::HasContext as _;
    // SAFETY: 上下文在创建回调内保证当前有效。
    let info = unsafe {
        format!(
            "vendor={} | renderer={} | version={}",
            gl.get_parameter_string(eframe::glow::VENDOR),
            gl.get_parameter_string(eframe::glow::RENDERER),
            gl.get_parameter_string(eframe::glow::VERSION)
        )
    };
    log::info!("OpenGL: {info}");
    startup::note(&format!("OpenGL: {info}"));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RendererChoice {
    Auto,
    Hardware,
    Software,
}
