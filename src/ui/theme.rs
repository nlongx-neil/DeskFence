//! 深浅色主题与全局控件风格。

use egui::{Color32, CornerRadius, Stroke};

use crate::config::ThemeMode;
use crate::platform::desktop;

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    pub window_bg: Color32,
    pub card_bg: Color32,
    pub card_hover: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub accent: Color32,
    pub border: Color32,
    pub selection: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            dark: true,
            window_bg: Color32::from_rgb(26, 28, 34),
            card_bg: Color32::from_rgb(34, 37, 45),
            card_hover: Color32::from_rgb(46, 50, 60),
            text: Color32::from_rgb(232, 235, 242),
            text_dim: Color32::from_rgb(150, 156, 170),
            accent: Color32::from_rgb(96, 146, 226),
            border: Color32::from_rgb(58, 62, 74),
            selection: Color32::from_rgb(52, 86, 140),
        }
    } else {
        Palette {
            dark: false,
            window_bg: Color32::from_rgb(244, 246, 250),
            card_bg: Color32::from_rgb(255, 255, 255),
            card_hover: Color32::from_rgb(236, 240, 248),
            text: Color32::from_rgb(30, 34, 42),
            text_dim: Color32::from_rgb(110, 116, 130),
            accent: Color32::from_rgb(52, 108, 196),
            border: Color32::from_rgb(210, 214, 224),
            selection: Color32::from_rgb(210, 224, 246),
        }
    }
}

pub fn is_dark(theme: ThemeMode) -> bool {
    match theme {
        ThemeMode::Dark => true,
        ThemeMode::Light => false,
        ThemeMode::Auto => desktop::system_is_dark(),
    }
}

pub fn palette_for(theme: ThemeMode) -> Palette {
    palette(is_dark(theme))
}

/// 行内编辑框（盒子标题、图标名称）浮在深色半透明底上：统一白字、白色光标、
/// 深蓝选中底 + 白色描边，保证深浅主题下选中文字都清晰可见。
pub fn apply_inline_edit_visuals(ui: &mut egui::Ui) {
    let v = ui.visuals_mut();
    v.text_cursor.stroke = Stroke::new(2.0, Color32::WHITE);
    v.selection.bg_fill = Color32::from_rgb(38, 92, 176);
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
}

/// 弹窗统一圆角（窗口/浮层）；菜单略小一档，保持层级差异但不割裂。
pub const POPUP_RADIUS: u8 = 12;
pub const MENU_RADIUS: u8 = 10;

/// 统一弹窗底板：深色半透明 / 浅色实底 + 1px 边框 + 柔和投影 + 16px 内边距。
/// 半透明盒子上弹出时也不会与盒底色混在一起（底板始终比盒子更不透明、更亮）。
pub fn popup_frame(pal: &Palette, dark: bool) -> egui::Frame {
    let fill = if dark {
        Color32::from_rgba_unmultiplied(30, 33, 41, 252)
    } else {
        Color32::from_rgb(252, 253, 255)
    };
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(CornerRadius::same(POPUP_RADIUS))
        .inner_margin(egui::Margin::same(16))
        .shadow(egui::epaint::Shadow {
            offset: [0, 10],
            blur: 28,
            spread: 0,
            color: Color32::from_black_alpha(if dark { 150 } else { 60 }),
        })
}

/// 右键/下拉菜单底板：比弹窗更紧凑，投影更轻。
pub fn menu_frame(pal: &Palette, dark: bool) -> egui::Frame {
    let fill = if dark {
        Color32::from_rgba_unmultiplied(28, 31, 38, 250)
    } else {
        Color32::from_rgb(252, 253, 255)
    };
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(CornerRadius::same(MENU_RADIUS))
        .inner_margin(egui::Margin::symmetric(8, 8))
        .shadow(egui::epaint::Shadow {
            offset: [0, 6],
            blur: 18,
            spread: 0,
            color: Color32::from_black_alpha(if dark { 120 } else { 45 }),
        })
}

/// 弹窗标题（15.5 / 强 / 主文字色）。
pub fn popup_title(text: impl Into<String>, pal: &Palette) -> egui::RichText {
    egui::RichText::new(text)
        .size(15.5)
        .strong()
        .color(pal.text)
}

/// 弹窗正文（12.5 / 主文字色，行距由调用方控制）。
pub fn popup_body(text: impl Into<String>, pal: &Palette) -> egui::RichText {
    egui::RichText::new(text).size(12.5).color(pal.text)
}

/// 统一无边框弹窗入口（确认框、配色框等所有浮层都走这里，避免各自复制尺寸参数）。
///
/// 根因备注：egui `Window` 的默认高度是 420，而内容底部按钮行用
/// `Layout::right_to_left(Align::Center)` 会把按钮在“可用高度 - 内容高度”里垂直居中。
/// `Resize` 又会把这个被撑高的内容尺寸写回下一帧的可用高度，形成
/// “内容 + 剩余/2”的正反馈，最终弹窗高度接近内容的两倍，按钮上下出现大块空白。
/// 这里把默认高度设为 0，让高度由内容撑开；按钮行再固定 30px 行高（见 [`popup_buttons`]）。
pub fn popup_window(id: egui::Id, title: &str, pal: &Palette, dark: bool) -> egui::Window<'static> {
    egui::Window::new(title.to_owned())
        .id(id)
        .title_bar(false)
        .frame(popup_frame(pal, dark))
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -60.0))
        .default_size(egui::vec2(360.0, 0.0))
        .min_width(320.0)
        .collapsible(false)
        .resizable(false)
}

/// 弹窗底部按钮行：固定 30px 行高 + 右对齐。
///
/// 不要直接用 `ui.with_layout(right_to_left, Align::Center)`：它会占满剩余高度并
/// 把按钮上下摊开。固定行高后按钮紧贴内容底部，上下只保留调用方显式添加的间距。
pub fn popup_buttons<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), 30.0),
        egui::Layout::right_to_left(egui::Align::Center),
        add,
    )
    .inner
}

/// 辅助说明（11.5 / 次文字色）。
pub fn popup_hint(text: impl Into<String>, pal: &Palette) -> egui::RichText {
    egui::RichText::new(text).size(11.5).color(pal.text_dim)
}

pub const DANGER: Color32 = Color32::from_rgb(203, 64, 64);

/// 主按钮：accent 实底、白字；危险操作用警示色实底。
pub fn primary_button(
    ui: &mut egui::Ui,
    text: &str,
    danger: bool,
    pal: &Palette,
) -> egui::Response {
    let fill = if danger { DANGER } else { pal.accent };
    ui.add(
        egui::Button::new(
            egui::RichText::new(text)
                .size(12.5)
                .strong()
                .color(Color32::WHITE),
        )
        .fill(fill)
        .stroke(Stroke::new(1.0, fill.gamma_multiply(0.85)))
        .min_size(egui::vec2(88.0, 30.0)),
    )
}

/// 次按钮：透明底 + 细边框，hover 时用卡片色。
pub fn ghost_button(ui: &mut egui::Ui, text: &str, pal: &Palette) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(12.5).color(pal.text))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, pal.border))
            .min_size(egui::vec2(80.0, 30.0)),
    )
}

/// 菜单里的危险项：默认红字，hover 仍走卡片底色。
pub fn danger_menu_text(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).size(12.5).color(DANGER)
}

/// 应用全局视觉风格（对根视口与设置窗口视口共享的 egui Context 生效）。
pub fn apply(ctx: &egui::Context, dark: bool) {
    let p = palette(dark);
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = p.window_bg;
    visuals.window_fill = p.window_bg;
    visuals.extreme_bg_color = if dark {
        Color32::from_rgb(18, 20, 25)
    } else {
        Color32::from_rgb(255, 255, 255)
    };
    visuals.faint_bg_color = p.card_bg;
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(10);
    visuals.window_stroke = Stroke::new(1.0, p.border);
    visuals.widgets.noninteractive.bg_fill = p.card_bg;
    visuals.widgets.inactive.bg_fill = p.card_bg;
    visuals.widgets.inactive.weak_bg_fill = p.card_bg;
    visuals.widgets.hovered.bg_fill = p.card_hover;
    visuals.widgets.hovered.weak_bg_fill = p.card_hover;
    visuals.widgets.active.bg_fill = p.selection;
    visuals.widgets.active.weak_bg_fill = p.selection;
    visuals.widgets.inactive.corner_radius = CornerRadius::same(8);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(8);
    visuals.widgets.active.corner_radius = CornerRadius::same(8);
    visuals.widgets.open.corner_radius = CornerRadius::same(8);
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(8);
    visuals.widgets.inactive.expansion = 0.0;
    visuals.widgets.hovered.expansion = 1.0;
    visuals.widgets.active.expansion = 1.0;
    visuals.selection.bg_fill = p.selection;
    visuals.selection.stroke = Stroke::new(1.0, p.accent);
    visuals.override_text_color = Some(p.text);
    visuals.hyperlink_color = p.accent;
    ctx.set_visuals(visuals);

    ctx.all_styles_mut(|style| {
        style.animation_time = 0.12;
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        style.spacing.interact_size.y = 26.0;
        style.spacing.scroll.bar_width = 8.0;
    });
}
