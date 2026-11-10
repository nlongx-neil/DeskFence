//! 设置页：外观、桌面整理、剪贴板、通用与快捷键说明。

use egui::RichText;

use crate::app::UiAction;
use crate::config::{AppConfig, BoxKind, GroupTarget, OccupiedDropMode, SortMode, ThemeMode};
use crate::model;
use crate::ui::theme::Palette;

fn section(ui: &mut egui::Ui, pal: &Palette, title: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).size(14.0).strong().color(pal.accent));
    ui.add_space(2.0);
    let line_y = ui.cursor().top();
    ui.painter().hline(
        ui.max_rect().x_range(),
        line_y,
        egui::Stroke::new(1.0, pal.border),
    );
    ui.add_space(4.0);
}

fn hint(ui: &mut egui::Ui, pal: &Palette, text: &str) {
    ui.label(RichText::new(text).size(11.5).color(pal.text_dim));
}

/// 渲染设置页；直接修改 `cfg`，调用方负责节流保存与副作用。
/// `icon_slider_held` 由图标大小滑杆的按住状态写入，用于推迟图标纹理重解码。
pub(crate) fn show(
    ui: &mut egui::Ui,
    cfg: &mut AppConfig,
    pal: &Palette,
    actions: &mut Vec<UiAction>,
    icon_slider_held: &mut bool,
) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            section(ui, pal, "外观");
            ui.horizontal(|ui| {
                ui.label("主题");
                egui::ComboBox::from_id_salt("theme_combo")
                    .selected_text(cfg.theme.label())
                    .show_ui(ui, |ui| {
                        for mode in ThemeMode::ALL {
                            ui.selectable_value(&mut cfg.theme, mode, mode.label());
                        }
                    });
                ui.add_space(18.0);
                ui.checkbox(&mut cfg.desktop.animations, "启用平滑动画");
            });
            hint(
                ui,
                pal,
                "浅色/深色/跟随系统；动画克制，不影响滚动与拖拽性能。",
            );

            section(ui, pal, "桌面整理");
            ui.horizontal(|ui| {
                let resp = ui.add(
                    egui::Slider::new(&mut cfg.desktop.icon_size, 24..=96)
                        .step_by(8.0)
                        .text("图标大小"),
                );
                // 拖动途中只重排、不解码；松开后由 app 的防抖逻辑解码最终尺寸。
                *icon_slider_held = resp.is_pointer_button_down_on() || resp.dragged();
                ui.add_space(12.0);
                ui.add(egui::Slider::new(&mut cfg.desktop.spacing, 4..=48).text("网格间距"));
            });
            ui.horizontal(|ui| {
                ui.add(
                    egui::Slider::new(&mut cfg.desktop.label_gap, 0.0..=16.0)
                        .step_by(1.0)
                        .text("图标与文字间距"),
                );
                ui.label(
                    egui::RichText::new("逻辑点，随屏幕缩放")
                        .small()
                        .color(pal.text_dim),
                );
            });
            ui.horizontal(|ui| {
                ui.label("新建盒子默认尺寸");
                ui.add(
                    egui::Slider::new(&mut cfg.desktop.box_defaults.w, 240.0..=800.0)
                        .step_by(10.0)
                        .text("宽"),
                );
                ui.add(
                    egui::Slider::new(&mut cfg.desktop.box_defaults.h, 160.0..=700.0)
                        .step_by(10.0)
                        .text("高"),
                );
            });
            ui.horizontal(|ui| {
                ui.label("新建盒子默认配色");
                ui.color_edit_button_srgb(&mut cfg.desktop.box_defaults.color);
            });
            ui.horizontal(|ui| {
                ui.label("新建盒子默认视图");
                egui::ComboBox::from_id_salt("default_box_view")
                    .selected_text(cfg.desktop.default_box_view.label())
                    .show_ui(ui, |ui| {
                        for view in crate::config::BoxView::ALL {
                            ui.selectable_value(
                                &mut cfg.desktop.default_box_view,
                                view,
                                view.label(),
                            );
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.add(
                    egui::Slider::new(&mut cfg.desktop.box_opacity, 10..=235)
                        .text("盒子背景不透明度"),
                );
                ui.label(
                    egui::RichText::new("仅影响背景填充")
                        .small()
                        .color(pal.text_dim),
                );
            });
            ui.label(
                egui::RichText::new("拖动即时应用到所有盒子并持久化；图标、文字与边框不受影响。")
                    .small()
                    .color(pal.text_dim),
            );
            ui.horizontal(|ui| {
                ui.checkbox(&mut cfg.desktop.show_labels, "显示文件名");
                ui.add_space(12.0);
                ui.checkbox(&mut cfg.desktop.hide_system_icons, "接管桌面（隐藏系统图标）");
            });
            ui.horizontal(|ui| {
                ui.label("桌面图标排序");
                egui::ComboBox::from_id_salt("desk_sort")
                    .selected_text(cfg.desktop.sort.label())
                    .show_ui(ui, |ui| {
                        for mode in SortMode::ALL {
                            ui.selectable_value(&mut cfg.desktop.sort, mode, mode.label());
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.label("拖到已占格");
                for mode in [OccupiedDropMode::Swap, OccupiedDropMode::NewBox] {
                    ui.radio_value(&mut cfg.desktop.drop_on_occupied, mode, mode.label());
                }
            });
            hint(
                ui,
                pal,
                "交换：两项互换格子；生成新盒子：在落点新建盒子并收纳两项。",
            );

            // 自动归类：新图标按类型进入文件/目录盒子；可逐大类指定去向。
            section(ui, pal, "自动归类");
            ui.horizontal(|ui| {
                ui.checkbox(&mut cfg.desktop.auto_group, "新桌面图标自动归类");
                ui.add_space(12.0);
                if ui.button("立即整理现有图标").clicked() {
                    actions.push(UiAction::AutoGroupAll);
                }
                ui.add_space(12.0);
                if ui.button("新建剪贴板盒").clicked() {
                    actions.push(UiAction::NewClipBox);
                }
                ui.add_space(12.0);
                if ui.button("新建待办盒（可多开）").clicked() {
                    actions.push(UiAction::NewTodoBox);
                }
            });
            hint(
                ui,
                pal,
                "首次启动会创建「文件盒子」「目录盒子」；文件盒子内按类型分组连续排列。",
            );
            let has_files = cfg.boxes.iter().any(|b| b.kind == BoxKind::Files);
            let has_dirs = cfg.boxes.iter().any(|b| b.kind == BoxKind::Dirs);
            if !has_files || !has_dirs {
                hint(ui, pal, "缺少分类盒子：请重新开启自动归类或手动新建后重启。");
            }
            if cfg.desktop.category_targets.len() < model::CATEGORY_COUNT {
                cfg.desktop
                    .category_targets
                    .resize(model::CATEGORY_COUNT, GroupTarget::Auto);
            }
            egui::Grid::new("auto_group_rules")
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    for i in 0..model::CATEGORY_COUNT {
                        ui.label(model::CATEGORY_LABELS[i]);
                        let target = cfg.desktop.category_targets.get_mut(i).unwrap();
                        egui::ComboBox::from_id_salt(("group_target", i))
                            .selected_text(target.label())
                            .width(96.0)
                            .show_ui(ui, |ui| {
                                for t in GroupTarget::ALL {
                                    ui.selectable_value(target, t, t.label());
                                }
                            });
                        ui.end_row();
                    }
                });

            section(ui, pal, "已关闭的盒子");
            let closed: Vec<(usize, String, BoxKind)> = cfg
                .boxes
                .iter()
                .enumerate()
                .filter(|(_, b)| b.closed)
                .map(|(i, b)| (i, b.name.clone(), b.kind))
                .collect();
            if closed.is_empty() {
                hint(ui, pal, "没有已关闭的盒子；盒子菜单中的「关闭」只隐藏并完整保留内容。");
            }
            for (idx, name, kind) in closed {
                let kind_label = match kind {
                    BoxKind::Files => "文件盒",
                    BoxKind::Dirs => "目录盒",
                    BoxKind::Clipboard => "剪贴板盒",
                    BoxKind::Todo => "待办盒",
                    BoxKind::Normal => "普通盒",
                };
                ui.horizontal(|ui| {
                    ui.label(format!("{name}（{kind_label}）"));
                    if ui.button("重新打开").clicked() {
                        actions.push(UiAction::ReopenBox(idx));
                    }
                });
            }

            ui.horizontal(|ui| {
                ui.checkbox(&mut cfg.desktop.snap_enabled, "盒子拖动磁吸");
                ui.add_space(12.0);
                ui.add(
                    egui::Slider::new(&mut cfg.desktop.snap_threshold, 0..=40)
                        .text("磁吸阈值 (px)"),
                );
            });
            hint(
                ui,
                pal,
                "拖动盒子靠近其它盒子或屏幕工作区边缘时自动对齐，0 表示关闭吸附。",
            );
            hint(
                ui,
                pal,
                "接管桌面后，系统图标层会被隐藏，退出时自动恢复。若系统为第三方桌面环境可能无法接管。",
            );

            section(ui, pal, "剪贴板历史");
            ui.horizontal(|ui| {
                ui.checkbox(&mut cfg.clipboard.enabled, "记录剪贴板内容");
                ui.add_space(12.0);
                ui.checkbox(&mut cfg.clipboard.auto_paste, "选中后自动粘贴（Ctrl+V）");
            });
            ui.horizontal(|ui| {
                ui.add(
                    egui::Slider::new(&mut cfg.clipboard.max_items, 20..=2000)
                        .text("最大记录条数")
                        .logarithmic(true),
                );
            });
            ui.horizontal(|ui| {
                let mut mb = (cfg.clipboard.max_bytes / (1024 * 1024)) as u32;
                if ui
                    .add(egui::Slider::new(&mut mb, 32..=8192).text("总容量上限 (MB)"))
                    .changed()
                {
                    cfg.clipboard.max_bytes = mb as u64 * 1024 * 1024;
                }
            });
            ui.horizontal(|ui| {
                let mut max_chars = cfg.clipboard.max_text_len;
                if ui
                    .add(
                        egui::Slider::new(&mut max_chars, 1_000..=500_000)
                            .text("忽略超过该长度的文本")
                            .logarithmic(true),
                    )
                    .changed()
                {
                    cfg.clipboard.max_text_len = max_chars;
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut cfg.clipboard.capture_images, "记录图片");
                ui.add_space(12.0);
                ui.checkbox(&mut cfg.clipboard.capture_files, "记录文件列表");
            });

            ui.horizontal(|ui| {
                ui.checkbox(
                    &mut cfg.clipboard.ignore_password_windows,
                    "忽略密码管理类应用",
                );
            });
            ui.horizontal(|ui| {
                ui.label("忽略的进程名");
                let mut procs = cfg.clipboard.ignored_processes.join(", ");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut procs)
                            .desired_width(260.0)
                            .hint_text("如 keepass.exe, 1password.exe"),
                    )
                    .changed()
                {
                    cfg.clipboard.ignored_processes = procs
                        .split(',')
                        .map(|s| s.trim().to_lowercase())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            });
            ui.horizontal(|ui| {
                ui.label("唤出热键");
                ui.add(
                    egui::TextEdit::singleline(&mut cfg.clipboard.hotkey)
                        .desired_width(150.0)
                        .hint_text("Ctrl+Shift+V"),
                );
                if crate::config::parse_hotkey(&cfg.clipboard.hotkey).is_none() {
                    ui.label(RichText::new("格式无效").color(egui::Color32::from_rgb(214, 92, 92)));
                }
            });
            hint(
                ui,
                pal,
                "热键需包含至少一个修饰键与一个字母/数字/F1-F24，修改后立即生效（可能被其他程序占用）。",
            );
            hint(
                ui,
                pal,
                "搜索、类型筛选、置顶、删除、双击粘贴、暂停、清空均可在剪贴板盒内操作。",
            );
            ui.horizontal(|ui| {
                if ui.button("清空历史").clicked() {
                    actions.push(UiAction::ClearClipboard);
                }
                hint(ui, pal, "清空全部历史（含置顶项），不可恢复。");
            });

            section(ui, pal, "通用");
            ui.horizontal(|ui| {
                ui.checkbox(&mut cfg.general.autostart, "开机自动启动");
                ui.add_space(12.0);
                ui.checkbox(&mut cfg.general.show_overlay, "显示桌面整理层");
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut cfg.general.log_debug, "输出调试日志");
            });
            hint(ui, pal, &format!("配置文件：{}", crate::util::config_path().display()));
            hint(ui, pal, &format!("数据目录：{}", crate::util::data_dir().display()));
            hint(ui, pal, &format!("日志目录：{}", crate::util::log_dir().display()));

            section(ui, pal, "快捷键");
            egui::CollapsingHeader::new("展开查看全部快捷键")
                .default_open(true)
                .show(ui, |ui| {
                    let rows = [
                        ("Ctrl+Shift+V", "唤出/恢复剪贴板盒（关闭状态会自动重开）"),
                        ("盒子内左键", "复制该条记录回剪贴板"),
                        ("盒子内双击", "粘贴该条记录到前台窗口"),
                        ("顶 / 删", "置顶或删除单条记录"),
                        ("Ctrl+Z", "撤销粘贴到桌面的文件移动"),
                        ("双击图标", "打开文件/文件夹"),
                        ("右键图标", "打开、剪切、复制、删除、属性、移出盒子"),
                        ("右键盒子", "重命名、配色、折叠、锁定、排序、删除"),
                        ("双击盒子标题", "重命名盒子"),
                        ("托盘左键", "显示剪贴板盒"),
                        ("托盘右键", "显示/隐藏整理层、暂停记录、设置、新建盒子、退出"),
                    ];
                    egui::Grid::new("hotkeys_grid")
                        .num_columns(2)
                        .spacing([18.0, 6.0])
                        .show(ui, |ui| {
                            for (key, desc) in rows {
                                ui.label(RichText::new(key).strong().color(pal.text));
                                ui.label(RichText::new(desc).color(pal.text_dim));
                                ui.end_row();
                            }
                        });
                });
        });
}
