//! Shell 图标提取与图片缩略图生成（全部在后台线程完成）。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use windows::core::HSTRING;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDIBits, GetObjectW,
    SelectObject, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ,
};
use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SHGetFileInfoW, SHGetImageList,
    SHGetStockIconInfo, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SMALLICON,
    SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHGSI_ICON, SHGSI_LARGEICON, SHGSI_SMALLICON,
    SHGSI_SYSICONINDEX, SHIL_EXTRALARGE, SHIL_JUMBO, SHIL_LARGE, SHIL_SMALL, SHSTOCKICONID,
    SHSTOCKICONINFO, SIID_DESKTOPPC, SIID_RECYCLER, SIID_RECYCLERFULL, SIIGBF,
};

use crate::model::SystemItem;
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, DrawIconEx, DI_NORMAL, HICON};

#[derive(Clone, Debug)]
pub enum IconTask {
    /// 提取文件/文件夹的 Shell 图标
    Shell {
        key: String,
        path: PathBuf,
        is_dir: bool,
        /// 目标栅格尺寸（物理像素），按 DPI 请求可避免放大模糊
        size: u32,
        /// 程序合成的系统项（此电脑/回收站）：用 `SHGetStockIconInfo` 取系统真实图标。
        system: Option<SystemItem>,
    },
    /// 生成图片缩略图（用于剪贴板图片列表）
    Thumbnail {
        key: String,
        path: PathBuf,
        max_size: u32,
    },
    /// 系统缩略图（IShellItemImageFactory）：图片/PDF/Office 等，走系统缩略图缓存
    ShellThumb {
        key: String,
        path: PathBuf,
        /// 目标边长（物理像素）
        size: u32,
    },
}

#[derive(Clone, Debug, Default)]
pub struct IconResponse {
    pub key: String,
    pub width: u32,
    pub height: u32,
    /// RGBA8；空表示加载失败
    pub rgba: Vec<u8>,
}

pub struct IconService {
    pub tx: Sender<IconTask>,
}

/// 启动后台图标线程，返回任务发送端与结果接收端。
pub fn spawn_worker() -> (Sender<IconTask>, Receiver<IconResponse>) {
    let (task_tx, task_rx) = mpsc::channel::<IconTask>();
    let (res_tx, res_rx) = mpsc::channel::<IconResponse>();
    std::thread::Builder::new()
        .name("icon-worker".into())
        .spawn(move || {
            // SHGetImageList 返回 COM 接口，工作线程需初始化 COM 套间。
            // SAFETY: 仅初始化本线程套间，不传递外部指针。
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                );
            }
            for task in task_rx {
                let resp = match task {
                    IconTask::Shell {
                        key,
                        path,
                        is_dir,
                        size,
                        system,
                    } => match shell_icon(&path, is_dir, size, system) {
                        Some((w, h, rgba)) => IconResponse {
                            key,
                            width: w,
                            height: h,
                            rgba,
                        },
                        None => IconResponse {
                            key,
                            ..Default::default()
                        },
                    },
                    IconTask::Thumbnail {
                        key,
                        path,
                        max_size,
                    } => match thumbnail(&path, max_size) {
                        Some((w, h, rgba)) => IconResponse {
                            key,
                            width: w,
                            height: h,
                            rgba,
                        },
                        None => IconResponse {
                            key,
                            ..Default::default()
                        },
                    },
                    IconTask::ShellThumb { key, path, size } => match shell_thumb(&path, size) {
                        Some((w, h, rgba)) => IconResponse {
                            key,
                            width: w,
                            height: h,
                            rgba,
                        },
                        None => IconResponse {
                            key,
                            ..Default::default()
                        },
                    },
                };
                if res_tx.send(resp).is_err() {
                    break;
                }
            }
        })
        .ok();
    (task_tx, res_rx)
}

fn shell_icon(
    path: &Path,
    is_dir: bool,
    size: u32,
    system: Option<SystemItem>,
) -> Option<(u32, u32, Vec<u8>)> {
    // 程序合成的系统项（此电脑/回收站）：直接按 StockIcon 取系统真实图标，
    // 不依赖 Shell 命名空间解析（本机上 `SHGetFileInfoW` 对 CLSID 取不到可用索引，
    // 会退回「首字 + 色块」占位图）。
    if let Some(item) = system {
        return stock_icon(item, size);
    }
    // Shell 命名空间路径（如其它 `::{CLSID}`）：不能按真实文件解析，
    // 直接交给 SHGetFileInfoW 解析 CLSID，取系统图标（含各尺寸图像列表）。
    if path.to_string_lossy().starts_with("::") {
        return namespace_shell_icon(path, size);
    }
    let hpath = HSTRING::from(path.as_os_str());
    let attrs = if is_dir {
        windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
    } else {
        windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL
    };
    let target = size.clamp(16, 256);

    // 先从系统图像列表取"不小于目标尺寸"的原生帧（Jumbo 256 / ExtraLarge 48 / Large 32），
    // 再高质量缩小到目标物理像素。此前只调用 SHGetFileInfoW 的 LARGEICON（32px），
    // 在 150% DPI 下被 DrawIconEx 放大到 48px 以上，这是图标发糊的根因。
    let mut sys = SHFILEINFOW::default();
    // SAFETY: sys 为有效栈上结构；路径在调用期间有效。
    let ok = unsafe {
        SHGetFileInfoW(
            &hpath,
            attrs,
            Some(&mut sys),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES,
        )
    };
    if ok != 0 {
        for (kind, source) in image_list_candidates(target) {
            if let Some(out) = icon_from_image_list(sys.iIcon, *kind, *source, target) {
                log::debug!(
                    "Shell 图标：{} 从图像列表 {source}px → {target}px",
                    path.display()
                );
                return Some(out);
            }
        }
    }

    // 图像列表不可用时的兜底：沿用 SHGFI_ICON（32/16px），至少保证有图标。
    legacy_shell_icon(&hpath, attrs, target)
}

/// 解析 Shell 命名空间路径（`::{CLSID}`）并取系统图标。
///
/// `SHGetFileInfoW` 对以 `::` 开头的路径会走 Shell 命名空间解析，因此无需
/// `SHGFI_USEFILEATTRIBUTES`；拿到系统图标索引后再从对应尺寸的图像列表取原生帧。
fn namespace_shell_icon(path: &Path, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    let hpath = HSTRING::from(path.as_os_str());
    let target = size.clamp(16, 256);
    let mut sys = SHFILEINFOW::default();
    // SAFETY: sys 为有效栈上结构；命名空间路径在调用期间有效。
    let ok = unsafe {
        SHGetFileInfoW(
            &hpath,
            windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
            Some(&mut sys),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX,
        )
    };
    if ok != 0 {
        for (kind, source) in image_list_candidates(target) {
            if let Some(out) = icon_from_image_list(sys.iIcon, *kind, *source, target) {
                return Some(out);
            }
        }
    }
    // 图像列表不可用时退回 SHGFI_ICON（16/32px 帧）。
    let mut info = SHFILEINFOW::default();
    let flags = SHGFI_ICON
        | if target <= 24 {
            SHGFI_SMALLICON
        } else {
            SHGFI_LARGEICON
        };
    // SAFETY: info 为有效栈上结构；路径在调用期间有效。
    let ok = unsafe {
        SHGetFileInfoW(
            &hpath,
            windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };
    if ok == 0 || info.hIcon.is_invalid() {
        return None;
    }
    // SAFETY: hIcon 来自 SHGetFileInfoW，需要 DestroyIcon 释放。
    let out = unsafe { hicon_to_rgba(info.hIcon, target) };
    // SAFETY: 同上。
    unsafe {
        let _ = DestroyIcon(info.hIcon);
    }
    out
}

/// 程序合成系统项的 StockIcon 编号：此电脑 / 回收站（空、非空两种状态）。
fn stock_icon_id(item: SystemItem) -> SHSTOCKICONID {
    match item {
        SystemItem::Computer => SIID_DESKTOPPC,
        SystemItem::RecycleBin { full: true } => SIID_RECYCLERFULL,
        SystemItem::RecycleBin { full: false } => SIID_RECYCLER,
    }
}

/// 取系统 StockIcon 的原生图标帧，并走与文件图标相同的「最大可用档 + alpha 归一化」管线。
///
/// 优先 `SHGSI_SYSICONINDEX` 拿到系统图像列表索引（可取到 256px Jumbo 帧），
/// 失败时退回 `SHGSI_ICON` 的 HICON（16/32px 原生帧）再等比缩放到目标尺寸。
fn stock_icon(item: SystemItem, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    let siid = stock_icon_id(item);
    let target = size.clamp(16, 256);
    let mut info = SHSTOCKICONINFO {
        cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: info 为有效栈上结构，cbSize 已按当前架构填充。
    let index = unsafe { SHGetStockIconInfo(siid, SHGSI_SYSICONINDEX, &mut info) }
        .ok()
        .map(|_| info.iSysImageIndex);
    if let Some(index) = index {
        if index >= 0 {
            for (kind, source) in image_list_candidates(target) {
                if let Some(out) = icon_from_image_list(index, *kind, *source, target) {
                    log::debug!(
                        "系统图标 {item:?} 从图像列表 {source}px → {target}px（索引 {index}）"
                    );
                    return Some(out);
                }
            }
        }
    }

    // 图像列表不可用时退回 StockIcon 直接给出的 HICON（原生 16/32px）。
    let mut info = SHSTOCKICONINFO {
        cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32,
        ..Default::default()
    };
    let flags = SHGSI_ICON
        | if target <= 24 {
            SHGSI_SMALLICON
        } else {
            SHGSI_LARGEICON
        };
    // SAFETY: info 为有效栈上结构；HICON 由系统返回，稍后用 DestroyIcon 释放。
    if unsafe { SHGetStockIconInfo(siid, flags, &mut info) }.is_err() || info.hIcon.is_invalid() {
        log::warn!("系统图标 {item:?} 取 StockIcon 失败");
        return None;
    }
    let native = if target <= 24 { 16 } else { 32 };
    // SAFETY: hIcon 来自 SHGetStockIconInfo，绘制后立即销毁。
    let frame = unsafe { hicon_to_rgba(info.hIcon, native) };
    // SAFETY: 同上。
    unsafe {
        let _ = DestroyIcon(info.hIcon);
    }
    let (w, h, rgba) = frame?;
    normalize_shell_frame(w, h, &rgba, target)
}

/// 目标尺寸对应的系统图像列表候选（先大后小，失败时逐级回退）。
fn image_list_candidates(target: u32) -> &'static [(u32, u32)] {
    if target <= 16 {
        &[(SHIL_SMALL, 16), (SHIL_LARGE, 32), (SHIL_EXTRALARGE, 48)]
    } else if target <= 32 {
        &[(SHIL_LARGE, 32), (SHIL_EXTRALARGE, 48), (SHIL_JUMBO, 256)]
    } else if target <= 48 {
        &[(SHIL_EXTRALARGE, 48), (SHIL_JUMBO, 256), (SHIL_LARGE, 32)]
    } else {
        &[(SHIL_JUMBO, 256), (SHIL_EXTRALARGE, 48), (SHIL_LARGE, 32)]
    }
}

fn icon_from_image_list(
    index: i32,
    kind: u32,
    source: u32,
    target: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    // SAFETY: SHGetImageList 返回系统持有的图像列表接口；GetIcon 生成的 HICON 由本函数销毁。
    unsafe {
        let list: IImageList = SHGetImageList(kind as i32).ok()?;
        if index < 0 {
            return None;
        }
        let hicon = list.GetIcon(index, ILD_TRANSPARENT.0).ok()?;
        if hicon.is_invalid() {
            return None;
        }
        let native = hicon_to_rgba(hicon, source);
        let _ = DestroyIcon(hicon);
        let (w, h, rgba) = native?;
        normalize_shell_frame(w, h, &rgba, target)
    }
}

/// 非透明内容的包围盒（alpha > 0）；整帧全透明时返回 None。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AlphaBounds {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

fn alpha_bounds(width: u32, height: u32, rgba: &[u8]) -> Option<AlphaBounds> {
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut found = false;
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            if rgba[i + 3] > 0 {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                found = true;
            }
        }
    }
    found.then(|| AlphaBounds {
        x: min_x,
        y: min_y,
        w: max_x - min_x + 1,
        h: max_y - min_y + 1,
    })
}

/// 系统图像列表返回的帧可能带大片透明边距（个别第三方图标的不透明内容只占画面几个百分点），
/// 直接整帧缩放会让图标视觉尺寸明显偏小。这里先按 alpha 包围盒裁掉透明边距，
/// 再等比缩放到目标方块内并居中（不拉伸宽高比）。
///
/// 内容本身已铺满整帧时与原来的整帧缩放完全等价，因此多数系统图标观感不变。
fn normalize_shell_frame(
    width: u32,
    height: u32,
    rgba: &[u8],
    target: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    let Some(bounds) = alpha_bounds(width, height, rgba) else {
        return resize_rgba(width, height, rgba, target, target);
    };
    if bounds.w >= width && bounds.h >= height {
        return resize_rgba(width, height, rgba, target, target);
    }
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if rgba.len() < expected {
        return None;
    }
    // 裁出非透明内容区域。
    let mut cropped = Vec::with_capacity((bounds.w * bounds.h * 4) as usize);
    for y in bounds.y..bounds.y + bounds.h {
        let start = (((y * width) + bounds.x) * 4) as usize;
        let end = start + (bounds.w * 4) as usize;
        cropped.extend_from_slice(&rgba[start..end]);
    }
    // 等比缩放到目标方块：长边撑满，短边按原比例，居中留透明边。
    let scale = target as f32 / bounds.w.max(bounds.h) as f32;
    let dst_w = ((bounds.w as f32 * scale).round() as u32).clamp(1, target);
    let dst_h = ((bounds.h as f32 * scale).round() as u32).clamp(1, target);
    let (dw, dh, scaled) = resize_rgba(bounds.w, bounds.h, &cropped, dst_w, dst_h)?;
    if dw == target && dh == target {
        return Some((dw, dh, scaled));
    }
    let mut out = vec![0u8; (target * target * 4) as usize];
    let ox = (target - dw) / 2;
    let oy = (target - dh) / 2;
    for y in 0..dh {
        let src = (y * dw * 4) as usize;
        let dst = (((y + oy) * target + ox) * 4) as usize;
        let len = (dw * 4) as usize;
        out[dst..dst + len].copy_from_slice(&scaled[src..src + len]);
    }
    Some((target, target, out))
}

fn legacy_shell_icon(
    hpath: &HSTRING,
    attrs: windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES,
    size: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    let mut info = SHFILEINFOW::default();
    let mut flags = SHGFI_ICON | SHGFI_USEFILEATTRIBUTES;
    flags |= if size <= 24 {
        SHGFI_SMALLICON
    } else {
        SHGFI_LARGEICON
    };
    // SAFETY: info 为有效栈上结构，路径 HSTRING 在调用期间有效。
    let ok = unsafe {
        SHGetFileInfoW(
            hpath,
            attrs,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };
    if ok == 0 || info.hIcon.is_invalid() {
        return None;
    }
    // SAFETY: hIcon 来自 SHGetFileInfoW，调用后销毁。
    let out = unsafe { hicon_to_rgba(info.hIcon, size) };
    // SAFETY: 图标句柄由本函数创建，需要 DestroyIcon 释放。
    unsafe {
        let _ = DestroyIcon(info.hIcon);
    }
    out
}

/// 高质量缩放 RGBA 图像（先预乘 alpha，避免透明边缘出现黑边/彩边）。
fn resize_rgba(
    width: u32,
    height: u32,
    rgba: &[u8],
    dst_w: u32,
    dst_h: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    if width == dst_w && height == dst_h {
        return Some((width, height, rgba.to_vec()));
    }
    let mut img = image::RgbaImage::from_raw(width, height, rgba.to_vec())?;
    for px in img.pixels_mut() {
        let a = u32::from(px[3]);
        for c in &mut px.0[..3] {
            *c = ((u32::from(*c) * a) / 255) as u8;
        }
    }
    let scaled = image::imageops::resize(&img, dst_w, dst_h, image::imageops::FilterType::Lanczos3);
    let mut raw = scaled.into_raw();
    for px in raw.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        if a > 0 {
            for c in &mut px[..3] {
                *c = (u32::from(*c) * 255).checked_div(a).unwrap_or(0).min(255) as u8;
            }
        }
    }
    Some((dst_w, dst_h, raw))
}

/// 把 HICON 绘制到 32 位 DIB 并读出 RGBA。
unsafe fn hicon_to_rgba(hicon: HICON, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: bmi 为有效结构，bits 输出指针有效。
    let hbmp = unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
    if bits.is_null() {
        // SAFETY: 刚创建的位图需要释放。
        unsafe {
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
        }
        return None;
    }
    // SAFETY: 创建内存 DC。
    let hdc: HDC = unsafe { CreateCompatibleDC(None) };
    if hdc.is_invalid() {
        // SAFETY: 释放位图。
        unsafe {
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
        }
        return None;
    }
    // SAFETY: SelectObject 返回旧对象，需要恢复。
    let old = unsafe { SelectObject(hdc, HGDIOBJ(hbmp.0)) };
    // SAFETY: 绘制图标到 DIB，透明背景保留 alpha。
    let drawn = unsafe {
        DrawIconEx(
            hdc,
            0,
            0,
            hicon,
            size as i32,
            size as i32,
            0,
            None,
            DI_NORMAL,
        )
    };
    let count = (size * size) as usize;
    // 必须在 DeleteObject 之前拷贝像素：DIB 的位图内存随位图对象一起释放，
    // 先释放再读会访问已释放内存。
    // SAFETY: DIB 内存由系统分配，在 DeleteObject 调用前有效，大小为 size*size*4。
    let mut rgba = unsafe {
        let raw = std::slice::from_raw_parts(bits as *const u8, count * 4);
        raw.to_vec()
    };
    // SAFETY: 恢复并释放 GDI 资源。
    unsafe {
        if !old.is_invalid() {
            let _ = SelectObject(hdc, old);
        }
        let _ = DeleteDC(hdc);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
    }
    if drawn.is_err() {
        return None;
    }
    let mut any_alpha = false;
    for px in rgba.as_chunks_mut::<4>().0 {
        // DIB 是 BGRA 排列，转成 RGBA
        px.swap(0, 2);
        if px[3] != 0 {
            any_alpha = true;
        }
    }
    if !any_alpha {
        for px in rgba.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }
    Some((size, size, rgba))
}

fn thumbnail(path: &Path, max_size: u32) -> Option<(u32, u32, Vec<u8>)> {
    let img = image::open(path).ok()?;
    let thumb = img.thumbnail(max_size.max(16), max_size.max(16)).to_rgba8();
    let (w, h) = (thumb.width(), thumb.height());
    Some((w, h, thumb.into_raw()))
}

/// 系统缩略图：IShellItemImageFactory（走系统缩略图缓存，支持图片/PDF/Office 等）。
fn shell_thumb(path: &Path, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    let size = size.clamp(16, 512);
    let name = HSTRING::from(path.to_string_lossy().as_ref());
    let factory: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(&name, None) }.ok()?;
    let hbmp = unsafe {
        factory.GetImage(
            windows::Win32::Foundation::SIZE {
                cx: size as i32,
                cy: size as i32,
            },
            SIIGBF(0),
        )
    }
    .ok()?;
    unsafe { hbitmap_rgba(hbmp) }
}

/// HBITMAP → RGBA（顶行优先），失败返回 None；始终释放位图。
unsafe fn hbitmap_rgba(hbmp: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    let mut bm = BITMAP::default();
    let got = GetObjectW(
        HGDIOBJ(hbmp.0),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut _ as *mut std::ffi::c_void),
    );
    if got == 0 || bm.bmWidth <= 0 || bm.bmHeight == 0 {
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        return None;
    }
    let (w, h) = (bm.bmWidth as u32, bm.bmHeight.unsigned_abs());
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bm.bmWidth,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = vec![0u8; (w as usize) * (h as usize) * 4];
    let hdc = CreateCompatibleDC(None);
    let old = SelectObject(hdc, HGDIOBJ(hbmp.0));
    let lines = GetDIBits(
        hdc,
        hbmp,
        0,
        h,
        Some(bits.as_mut_ptr().cast()),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    SelectObject(hdc, old);
    let _ = DeleteDC(hdc);
    let _ = DeleteObject(HGDIOBJ(hbmp.0));
    if lines == 0 {
        return None;
    }
    for px in bits.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    Some((w, h, bits))
}

/// 供 UI 复用：把 RGBA 数据转为 egui 图像。
pub fn rgba_to_color_image(width: u32, height: u32, rgba: &[u8]) -> Option<egui::ColorImage> {
    if width == 0 || height == 0 {
        return None;
    }
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if rgba.len() < expected {
        return None;
    }
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        &rgba[..expected],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_image_requires_matching_buffer() {
        assert!(rgba_to_color_image(0, 0, &[]).is_none());
        assert!(rgba_to_color_image(2, 2, &[0u8; 8]).is_none());
        assert!(rgba_to_color_image(2, 2, &[0u8; 16]).is_some());
    }

    #[test]
    fn resize_uses_requested_size_and_preserves_alpha() {
        let mut rgba = vec![0u8; 64 * 64 * 4];
        for px in rgba.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[200, 40, 40, 255]);
        }
        let (w, h, out) = resize_rgba(64, 64, &rgba, 48, 48).unwrap();
        assert_eq!((w, h), (48, 48));
        assert_eq!(out.len(), 48 * 48 * 4);
        assert!(out.as_chunks::<4>().0.iter().all(|px| px[3] == 255));
    }

    #[test]
    fn image_list_prefers_large_source_for_big_targets() {
        assert_eq!(image_list_candidates(16)[0], (SHIL_SMALL, 16));
        assert_eq!(image_list_candidates(32)[0], (SHIL_LARGE, 32));
        assert_eq!(image_list_candidates(48)[0], (SHIL_EXTRALARGE, 48));
        assert_eq!(image_list_candidates(64)[0], (SHIL_JUMBO, 256));
    }

    /// 构造 `w×h` 的透明帧，并在 `rect` 区域填充不透明像素。
    fn frame_with_content(w: u32, h: u32, rect: (u32, u32, u32, u32)) -> Vec<u8> {
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let (rx, ry, rw, rh) = rect;
        for y in ry..(ry + rh).min(h) {
            for x in rx..(rx + rw).min(w) {
                let i = ((y * w + x) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&[200, 120, 40, 255]);
            }
        }
        rgba
    }

    #[test]
    fn normalize_crops_transparent_margins_and_centers_content() {
        // 64×64 帧里内容只有 20×10（四周都是透明边距）。
        let rgba = frame_with_content(64, 64, (2, 3, 20, 10));
        let (w, h, out) = normalize_shell_frame(64, 64, &rgba, 32).unwrap();
        assert_eq!((w, h), (32, 32));
        let bounds = alpha_bounds(w, h, &out).unwrap();
        // 长边撑满 32，短边等比 16，上下各留 8px 透明。
        assert_eq!((bounds.w, bounds.h), (32, 16));
        assert_eq!((bounds.x, bounds.y), (0, 8));
    }

    #[test]
    fn normalize_keeps_aspect_for_portrait_content() {
        let rgba = frame_with_content(64, 64, (40, 0, 10, 20));
        let (w, h, out) = normalize_shell_frame(64, 64, &rgba, 32).unwrap();
        assert_eq!((w, h), (32, 32));
        let bounds = alpha_bounds(w, h, &out).unwrap();
        assert_eq!((bounds.w, bounds.h), (16, 32));
        assert_eq!((bounds.x, bounds.y), (8, 0));
    }

    #[test]
    fn normalize_full_frame_matches_plain_resize() {
        let mut rgba = vec![0u8; 64 * 64 * 4];
        for px in rgba.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[10, 20, 30, 255]);
        }
        let plain = resize_rgba(64, 64, &rgba, 32, 32).unwrap();
        let normalized = normalize_shell_frame(64, 64, &rgba, 32).unwrap();
        assert_eq!(plain, normalized);
        let bounds = alpha_bounds(32, 32, &normalized.2).unwrap();
        assert_eq!((bounds.w, bounds.h), (32, 32));
    }

    #[test]
    fn normalize_handles_fully_transparent_frame() {
        let rgba = vec![0u8; 64 * 64 * 4];
        let (w, h, out) = normalize_shell_frame(64, 64, &rgba, 32).unwrap();
        assert_eq!((w, h), (32, 32));
        assert!(out.as_chunks::<4>().0.iter().all(|px| px[3] == 0));
    }

    #[test]
    fn thumbnail_decode_png() {
        let dir = std::env::temp_dir().join("deskkeeper_icon_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.png");
        let img = image::RgbaImage::from_pixel(64, 48, image::Rgba([10, 20, 30, 255]));
        img.save(&path).unwrap();
        let (w, h, rgba) = thumbnail(&path, 16).unwrap();
        assert!(w <= 16 && h <= 16);
        assert_eq!(rgba.len() as u32, w * h * 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
