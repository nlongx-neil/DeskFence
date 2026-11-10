# DeskKeeper

纯 Rust 实现的 Windows 10/11 x64 桌面小工具，包含两个子系统：

- **桌面整理（Desk）**：在壁纸上绘制半透明圆角"盒子"，把桌面条目按盒子归类；盒子成员是**源文件路径引用**，文件始终留在原位置。
- **剪贴板历史（Clip）**：后台监听剪贴板，SQLite 持久化文本 / 图片 / 文件列表，全局热键唤出/聚焦剪贴板盒，盒内支持搜索、类型筛选、置顶/删除、单击复制与双击粘贴。

> 不使用 Tauri / Electron / WebView2 / HTML+JS / WASM 等任何 web 技术。GUI 由 eframe/egui（glow 原生 GL 后端）绘制，系统能力直接调用 Win32 API（`windows` crate）。

## 功能

### 桌面整理

- 半透明圆角盒子：新建 / 删除 / 重命名、折叠展开、配色与透明度调节、锁定、拖拽移动、**三边与四角缩放**。
- 网格布局展示盒子成员（源文件的真实路径与文件夹），打开 / 右键 / 重命名 / 复制路径 / 拖拽全部基于真实源路径（Shell 图标、自动排列、图标大小 / 间距 / 标签开关、按名称 / 类型 / 修改时间 / 大小排序）。
- **四类盒子**：文件盒、目录盒（虚拟引用分组，文件保持在原位）、剪贴板盒、待办盒（本地记录），外观与交互统一。
- 拖拽归类：桌面图标拖入盒子 = **只记录路径引用**，不移动/复制文件；已归属盒子的条目从桌面网格排除，避免同一文件出现两次；从盒子移出 = 删除引用，文件本来就在原位置。`Ctrl+Z` 只撤销「粘贴文件到桌面」这类显式物理移动。
- 桌面接管：定位 `SHELLDLL_DefView` 并隐藏系统桌面图标层，由叠加层绘制盒子与桌面条目；退出（或异常退出后下次启动）时自动恢复。
- 文件系统同步：`ReadDirectoryChangesW` 监听桌面目录，120ms 防抖后后台扫描；盒子成员按持久化的源路径解析并随桌面变化刷新，不在 UI 线程做遍历。
- 多显示器 / DPI：叠加层覆盖各屏工作区并集；盒子、桌面图标、文字按**所属显示器**的有效 DPI 独立缩放（`GetDpiForMonitor`），图标按该屏物理尺寸栅格化，混合 DPI 下不模糊；显示器增删/分辨率变化时重排并持久化。
- 右键菜单：文件/目录图标默认弹 **Windows 原生上下文菜单**（`IContextMenu/2/3`，含系统项与子菜单），并在分隔线后追加我们自己的项；桌面空白处同样弹 **系统原生桌面菜单**（查看 / 排序方式 / 刷新 / 新建 / 显示设置 / 个性化等），DeskKeeper 动作收进一个 `DeskKeeper` 子菜单（自动整理 / 新建盒子 / 设置 / 重新加载桌面 / 撤销 / 显示或隐藏整理层）。原生菜单在当前环境不可用时自动退回**内置自绘菜单**。
- **「此电脑」系统项**：桌面网格固定显示一个由程序合成的「此电脑」（不写盘、不占成员引用），图标取系统 Shell 命名空间（`FOLDERID_ComputerFolder` / CLSID），双击打开资源管理器「此电脑」，右键走系统菜单；系统项不参与重命名、拖拽、自动归类与盒子成员登记。

#### 盒子能力（已落地）

| 能力 | 行为 |
| --- | --- |
| 按类型分组与连续排序 | 文件盒按扩展名族（文档 / 表格 / 演示 / PDF / 图片 / 压缩包 / 其他）连续分组，组内沿用所选排序；目录盒与普通盒按所选排序 |
| 类型 tab | 文件盒与普通盒标题栏下方有「全部 / 文档 / 表格 / 演示 / PDF / 图片 / 压缩包 / 其他」8 个 tab，显示各分类数量；**只筛选显示**，不移动文件、不改变排序；选中项每盒独立持久化，切换后滚动回顶部 |
| 名称行内重命名 | 双击名称进入编辑（图标主体双击仍是打开）；`Enter` 提交、`Esc` 取消、点击别处提交；文件/目录调用真实 `fs::rename`，失败 toast 给出原因；待办/剪贴板条目只改本地记录 |
| 名称选中复制 | 名称区按下拖动可选中文本，`Ctrl+C` 复制；与图标拖拽、双击打开互不冲突 |
| 三边四角缩放 | 左 / 右 / 下三边与四角均可拖拽改尺寸（命中带 6 逻辑点，随 DPI 缩放）；对边固定，受最小尺寸与工作区约束；收起态不参与缩放 |
| 收起高度联动 | 收起时高度压到标题栏并显示「已折叠 N 项」；展开 / 收起共用同一个 `h`，展开高度单独持久化，重启后再次展开仍是收起前高度 |
| 几何持久化 | `x/y/w/h` 以**虚拟屏幕物理像素**保存；重启并重新接管后逐像素一致（含小数物理坐标的往返回归测试） |
| 锁图标 | 标题栏自绘闭合 / 打开锁图标，点击切换；锁定后禁用拖动与缩放，带 hover 提示 |
| 系统右键菜单 | 原生菜单尾部追加：重命名 / 复制路径 / 添加到剪贴板盒 / 移出盒子（放回桌面）/ 在盒子内移动；选中系统项走 `InvokeCommand` |
| 盒子重命名 | 双击**盒子标题栏**进入行内编辑（右键菜单也有「重命名」）；进入时全选原标题，`Enter` 提交、`Esc` 取消、**失焦提交**；空名 / 纯空白 / 超过 40 字符会被拒绝并保留编辑态（toast 提示）；**允许重名**（toast 提示，不静默覆盖，避免自动后缀导致用户找不到原本的名字）；标题超过 24 字符显示省略号，悬停可见全名；改名立即写入配置、重启保持，四类盒子均生效，只改显示名**绝不触发文件操作** |
| 关闭 / 删除 / 解散 | **关闭**＝仅隐藏，数据保留、可从托盘或设置重开；**删除待办盒**＝删除盒子与全部待办事项（二次确认，不涉及文件），与剪贴板/文件盒无关；**解散**＝删除盒子与成员引用，真实文件仍在原位置并立即回到桌面网格（二次确认，文案注明「不会删除文件」） |
| 外观设置 | 全局盒子背景不透明度、图标尺寸 / 间距、标签显隐、按屏 DPI 缩放等设置项 |


#### 原生桌面语义（零副作用实测）

- **运行中**（`hide_system_icons = true`，默认）：叠加层用 `ShowWindow(SW_HIDE)` 隐藏 `SHELLDLL_DefView`，原生桌面图标层不显示；盒子里的条目与桌面图标全部由叠加层绘制，**同一个条目不会同时出现在盒子和原生桌面**（不存在"重复两份"）。叠加层使用自己的网格，不读取、也不改写系统保存的图标位置。
- **隐藏整理层**（右键空白 → 隐藏整理层 / 托盘）：恢复 `DefView`，原生图标按系统保存的位置显示，盒子消失。
- **退出**：恢复 `DefView` 并删除 `session.state`；实测「隐藏态」与「退出后」截图逐像素一致（3840×2160，差异像素 0），`DefView` 句柄不变（`132250` / `0x2049A`），图标计数 89 不变。
- **只读保证（实测）**：一轮「启动 → 运行 → 隐藏整理层 → 退出」前后，
  - 桌面目录 88 项的 名称 / 大小 / mtime 与目录 mtime 完全不变；
  - 图标布局数据库 `HKCU\Software\Microsoft\Windows\Shell\Bags\1\Desktop` 的注册表导出 SHA256 不变；
  - 壁纸注册表（`Control Panel\Desktop` 的 `WallPaper` / `WallpaperStyle`）不变；
  - 不写壁纸、不写图标布局库、不写 `Desktop` 目录（显式文件操作除外，见下表）。
- **所有会写盘/写注册表的位置**：

  | 位置 | 时机 |
  | --- | --- |
  | `%LOCALAPPDATA%\DeskKeeper\config.toml` | 配置变化，400ms 节流 + 临时文件原子替换 |
  | `%LOCALAPPDATA%\DeskKeeper\logs\deskkeeper.log*` | 滚动日志 |
  | `%LOCALAPPDATA%\DeskKeeper\clipboard.db`、`thumbs\` | 剪贴板记录与图片缩略图 |
  | `%LOCALAPPDATA%\DeskKeeper\session.state` | 接管桌面时写入；退出 / 恢复时删除 |
  | `%LOCALAPPDATA%\DeskKeeper\boxes\<id>\` | **旧版遗留目录**：仅在迁移旧数据时读取；新盒子不再创建目录，迁移后删除空目录 |
  | 桌面目录 | 用户显式操作：粘贴到桌面、删除到回收站、撤销；盒子收纳/移出与自动归类**不产生任何文件移动** |
  | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` | 仅在设置中开启开机自启时 |
  | `SHELLDLL_DefView` 显隐 | 仅 `ShowWindow` 调用，不改注册表、不写文件 |

- **虚拟引用**：盒子成员只保存源路径（`[[boxes]].members`），拖入 / 移出 / 删除盒子 / 自动归类都只读写这份引用，**不会移动或复制用户文件**。删除盒子不会删除文件；已归属条目从桌面网格排除（磁盘上仍在桌面）。源文件被外部移动/删除后，该引用在盒子内标记为失效并在启动时给出可见提示，不会静默丢弃配置。

### 剪贴板历史

- `AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE` 监听；支持文本、图片（DIB → PNG 缩略图落盘）、文件列表（`CF_HDROP`）。
- SQLite（`rusqlite` `bundled`）存储；相同内容去重只保留最新一条；条数与总容量上限，超限自动清理最旧记录。
- 全局热键 `Ctrl+Shift+V` 显示/聚焦剪贴板盒（关闭状态自动恢复；可在设置中修改，避开系统 Win+V）。盒内支持搜索、按类型筛选、暂停/继续、清空、置顶、删除、单击复制、双击粘贴到前台窗口。
- 粘贴回填：写回文本 / 图片 / 文件列表到剪贴板，可选 `SendInput` 模拟 `Ctrl+V`。
- 隐私：忽略超长文本、忽略密码类来源窗口、一键清空历史。

### 系统集成

- 托盘图标菜单：显示/隐藏整理层、显示剪贴板盒、暂停记录、设置、新建盒子、**自动整理**、退出。
- **自动整理（一次性）**：入口在桌面 `DeskKeeper` 子菜单与托盘菜单；把当前桌面上所有尚未归属盒子的图标按类型写入文件盒 / 目录盒的成员引用（**绝不移动或复制文件**），完成后 toast 报告「已整理 N 项」，设置页原有的持续自动归类开关保持不变。
- 单实例（命名互斥体，第二次启动唤醒已有实例）、开机自启（注册表 Run 项）、TOML 配置、滚动日志文件（2MB × 3）、内嵌多尺寸托盘图标（`assets/deskkeeper.ico`）。
- 深浅色主题跟随系统、圆角 / 阴影 / 抗锯齿、克制的过渡动画。

## 架构

```
src/
  main.rs                 启动流程：日志 → 配置 → 数据目录 → 崩溃残留恢复 → 单实例 → 视口创建
  lib.rs                  库根（业务逻辑可单测）
  app.rs                  overlay 应用主体：状态机、事件轮询、绘制、拖拽、撤销、动作分发
  config.rs               AppConfig 模型、TOML 读写、400ms 节流保存线程
  events.rs              跨线程事件（BgEvent / TrayCommand）与 EventSink（mpsc + request_repaint）
  logging.rs              滚动文件日志实现
  model.rs                条目元数据、网格布局纯计算、排序（单测覆盖）
  fileops.rs              移动 / 复制策略、撤销栈（单测覆盖）
  watcher.rs              ReadDirectoryChangesW 监听线程（防抖、UTF-16 事件解析）
  clipboard/
    store.rs              SQLite 存储线程：建表、去重、限额清理、查询
    service.rs            剪贴板采样 / 过滤 / 入队，回填粘贴
    mod.rs                对外导出
  platform/
    desktop.rs            虚拟屏幕 / DPI、DefView 隐藏与恢复、会话残留标记、叠加层层级校准
    show_desktop.rs       Win+D 自实现：最小化 / 还原普通顶层窗口（钩子拦截后调用）
    key_hook.rs           低级键盘钩子：拦截 Win+D、名称选区 Ctrl+C
    icons.rs              Shell 图标提取（后台线程 + 纹理缓存）与图片缩略图
    message_window.rs     隐藏消息窗口：托盘、全局热键、剪贴板通知、单实例唤醒、原生菜单消息泵
    shell_menu.rs         Windows 原生右键菜单（IContextMenu/2/3 + 追加项 + 降级信号）
    shell.rs              打开 / 定位 / 回收站删除 / 属性
    clipboard_native.rs   CF_UNICODETEXT / CF_DIB / CF_HDROP 读写
    input.rs              SendInput 模拟粘贴、前台窗口记录
    autostart.rs          注册表 Run 项
    single_instance.rs    命名互斥体
  ui/
    mod.rs                中文字体、纹理缓存、圆角卡片、文本适配
    settings.rs           设置页（含「已关闭的盒子」重开入口）
    theme.rs              深浅色调色板与 egui 样式
  util.rs                 路径与通用小工具
```

线程边界：**UI 线程只做 egui 绘制与交互**；图标提取、目录监听、SQLite、图片解码、文件移动、粘贴模拟、配置写盘、消息窗口（托盘/热键）全部在后台线程，通过 `mpsc` 回传事件并 `request_repaint` 唤醒 UI。

性能要点（已落实在代码中）：

- 绘制期间只收集 `UiAction`，帧末统一处理；不每帧重建场景。
- 图标纹理、缩略图、字体与调色板均缓存复用，绘制循环内不新建资源。
- 配置保存 400ms 节流合并、紧凑 TOML、临时文件 + 原子替换；写盘失败写日志，不静默吞掉。
- 启动先出窗口，图标与历史记录后台加载；大图缩略图在后台线程生成。

### 托盘图标资源

`assets/deskkeeper.ico` 不是外部素材：由 `assets/gen_icon.ps1` 使用 System.Drawing 纯矢量绘制——蓝靛对角渐变圆角方块（主色 #A0C5FF → #3745C6）内排布 2×2 圆角模块，三块浅色整理格（#FAFDFF → #DBE8FF）加右下角一块琥珀强调格（#FFCE68 → #FFA23E，代表剪贴板 / 收集），带柔和外投影与外发光，16px 下模块仍约 4.4px、间隙约 1.25px，浅色与深色任务栏均可辨识，图标不含任何文字。所有档位先在 1024×1024 渲染，再逐级高质量降采样，打包 16 / 20 / 24 / 32 / 40 / 48 / 64 / 96 / 128 / 256 十种尺寸的 32bpp PNG 帧（256 档目录项以 0 表示）。运行时通过 `include_bytes!` 内嵌进二进制，首次启动写入数据目录，再用 `LoadImageW(LR_LOADFROMFILE)` 生成 `HICON`；加载失败自动回退系统通用图标。重新生成：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File assets/gen_icon.ps1
```

窗口图标与托盘图标都从同一个 ICO 读取（窗口取最大帧解码为 `egui::IconData`，托盘走 `LoadImageW`）；`build.rs` 还会用 GNU `windres` 把该 ICO 作为 ID 1 图标资源链接进 exe，因此资源管理器里的 exe 属性也显示新图标。若 PATH 中没有 `windres`（MinGW/binutils 自带），构建仍会成功，仅 exe 文件图标缺失（可用 `WINDRES` 环境变量指定路径）。

## 构建与运行

前置：Windows 10/11 x64、Rust stable（`rust-version = 1.89`）、MSVC 链接器（Visual Studio Build Tools 的 "使用 C++ 的桌面开发"）。无需 Node、CMake 或其他 C++ 构建步骤（SQLite 由 `rusqlite` 的 `bundled` 特性随 crate 编译）。

```powershell
cd D:\work\Projects\rusts\deskkeeper
cargo build --release
.\target\release\deskkeeper.exe
```

首次运行会自动创建：

- 配置：`%LOCALAPPDATA%\DeskKeeper\config.toml`
- 数据库：`%LOCALAPPDATA%\DeskKeeper\clipboard.db`
- 图片缩略图：`%LOCALAPPDATA%\DeskKeeper\thumbs\`
- 旧版盒子遗留目录：`%LOCALAPPDATA%\DeskKeeper\boxes\<盒子 id>\`（迁移后可删除；新盒子不再创建）
- 日志：`%LOCALAPPDATA%\DeskKeeper\logs\deskkeeper.log`
- 接管标记：`%LOCALAPPDATA%\DeskKeeper\session.state`（存在表示上次异常退出时桌面图标仍被隐藏，下次启动会恢复）

质量校验：

```powershell
cargo fmt --all -- --check
cargo test
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release
```

## 快捷键

| 操作 | 快捷键 |
| --- | --- |
| 显示 / 聚焦剪贴板盒 | `Ctrl+Shift+V`（可在设置页修改；关闭状态自动恢复） |
| 搜索记录 | 剪贴板盒顶部输入框 |
| 按类型筛选 | 剪贴板盒顶部「全部 / 文本 / 图片 / 文件」 |
| 复制记录到剪贴板 | 单击记录行 |
| 粘贴记录到前台窗口 | 双击记录行 |
| 置顶 / 删除单条记录 | 记录行右侧「顶」「删」按钮 |
| 暂停 / 恢复记录 | 剪贴板盒顶部「暂停/继续」或托盘菜单 |
| 清空历史 | 剪贴板盒顶部「清空」或设置页 |
| 撤销最近一次文件移动 | `Ctrl+Z`（整理层聚焦时） |

## 配置文件示例

```toml
version = 1
theme = "auto"            # auto | light | dark

[desktop]
icon_size = 48
spacing = 18
show_labels = true
hide_system_icons = true

[clipboard]
enabled = true
max_items = 500
max_bytes = 52428800
hotkey = "Ctrl+Shift+V"
max_text_len = 20000
auto_paste = true

[general]
autostart = false
show_overlay = true
```

`[[boxes]]` 段由程序维护：`id`、`name`、`kind`（`files` / `dirs` / `clipboard` / `todo` / `normal`）、`members`（盒子成员：源文件绝对路径引用，旧配置缺省为空）、`x`、`y`、`w`、`h`（虚拟屏幕物理像素）、`color = [r,g,b]`、`alpha`、`collapsed`、`locked`、`sort`、`file_filter`（类型 tab 的选中项）、`todos`（待办盒条目）。折叠态与展开态共用同一个 `h`（展开高度），折叠时只按标题栏高度绘制。

## 已知限制

- **旧版盒子目录迁移**：旧配置把文件真实搬进 `%LOCALAPPDATA%\DeskKeeper\boxes\<id>\`。升级后这些目录只作为遗留数据，可用 `deskkeeper --migrate-boxes --dry-run` 生成搬回桌面清单（逐项 SHA256、同名冲突改 ` (2)`），确认无误后再执行 `deskkeeper --migrate-boxes`（须先退出应用）。迁移在同盘 `rename`、跨盘 `复制 → SHA256 校验 → 删除源`；任何校验失败都会保留源文件。迁移完成后更新成员引用并删除空目录。启动时会先接管遗留目录内容为引用，因此迁移前也能正常显示。
- **原生右键菜单的子菜单展开 / 属性对话框未自动截图**：本轮自动化输入被系统钩子限制（`SetCursorPos` 返回失败、`keybd_event` 无法到达系统菜单 `#32768`），只能验证菜单弹出、追加项渲染与 `InvokeCommand` 执行路径；「打开方式 / 发送到」子菜单与「属性」弹窗需人工点检。
- **换分辨率 / 改 DPI 后的几何持久化未实测**：几何只以物理像素存取、绘制时按 `x/scale` 换算，已由单测覆盖物理坐标往返；真机改分辨率会重排原生桌面图标，违反「零副作用」约束，且当前配置没有缩放覆盖项，因此本轮只做了单屏 150% 的实测。
- **Win+D 为开关式自实现**：低级键盘钩子吞掉 `Win+D`，第一次按下最小化所有普通顶层窗口（排除本进程、shell、工具窗口、DWM cloaked 窗口），叠加层保持在桌面之上；再按一次按保存清单还原。原生桌面图标层全程保持隐藏。若钩子安装失败，`ensure_overlay_layer` 兜底为「保持叠加层可见、不显示系统图标」，不回退到系统 peek。
- 系统桌面图标层采用隐藏 `SHELLDLL_DefView` 的方式接管；若第三方桌面软件也重排了该窗口结构，可能需要在设置中关闭"接管桌面"。
- **窗口清屏色必须是全透明**：`eframe` 默认 `clear_color` 为 `rgba(12,12,12,180)`，在透明窗口上会形成整屏深灰遮罩，鼠标移动（`egui-winit` 对 `CursorMoved` 无条件请求重绘）时表现为闪烁。`DeskApp::clear_color` 现固定返回 `[0,0,0,0]`，请勿改回。
- **上游重绘限制**：`egui-winit` 对每次鼠标移动固定返回 `repaint`，eframe 未提供事件过滤钩子，无法在不动框架的前提下做到"移动鼠标完全不产生一帧"。当前折中是让这些帧与上一帧逐像素一致（全透明清底），视觉上无闪烁；未使用防抖/节流。
- 叠加层尺寸取**显示器工作区**（`rcWork`，排除任务栏），多显示器时用 `SetWindowRgn` 裁成各工作区并集；`Win+D` 自实现期间同样不覆盖任务栏。
- 「粘贴文件到桌面」这类显式物理移动跨盘时按"复制 → 校验 → 删除源"，磁盘空间不足会保留源文件并提示失败；盒子收纳/移出不再触发任何物理移动。
- 剪贴板富文本（HTML）当前只做记录与文本回填，不还原 HTML 格式。
- 首次启动时 Shell 图标提取依赖于当前会话的桌面；在无交互桌面的服务会话中图标可能为空。
- **原生菜单不可用时退回内置菜单**：消息窗口不存在、请求入队 / `PostMessageW` 失败，或消息窗口线程里 `ILCreateFromPathW` / `SHBindToParent` / `GetUIObjectOf` / `CreatePopupMenu` / `QueryContextMenu` / `TrackPopupMenuEx` 失败时，UI 线程会在原点击位置弹出内置自绘菜单（打开 / 重命名 / 在资源管理器中定位 / 复制路径 / 添加到剪贴板盒 / 移出盒子 / 在盒子内移动）。内置菜单是功能子集，不含系统项（属性、发送到、打开方式等）。


## 验证记录（本机实测）

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过（退出码 0） |
| `cargo test` | 通过：`83 passed; 0 failed`（布局、排序、序列化、移动策略、去重、混合 DPI 排版、重命名校验、类型 tab、几何往返、原生菜单降级判定、托盘 ICO 校验） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过（无告警输出，退出码 0） |
| `cargo build --release` | 通过（产出 `target\release\deskkeeper.exe`） |

运行验证（本机真实执行）：

- 启动后日志确认：`已隐藏桌面图标层 (DefView=HWND(0x2049a))`、`全局热键已注册: Ctrl+Shift+V`。
- 桌面截图确认盒子（圆角半透明、标题、控制按钮、Shell 图标）正确绘制，见 [docs/screenshot-desktop.png](docs/screenshot-desktop.png)（为叠加层窗口自身渲染截图）。
- 用 `Ctrl+Shift+V` 实际唤出剪贴板盒，分别复制文本 / 图片 / 文件列表后，盒内出现对应条目；搜索、类型筛选、置顶/删除、双击粘贴均可用。
- 发送 `WM_CLOSE` 优雅退出后日志确认：`已恢复桌面图标层`、`DeskKeeper 已退出`，接管标记文件被清除。
- `Win+D` 两态实测（release 构建）：`Win+D` 不再交给系统，由低级键盘钩子拦截后调用 `show_desktop::toggle()`。日志确认第一次按下输出 `Win+D（自实现）：最小化 N 个普通窗口，原生桌面图标层保持隐藏`，再按一次输出 `Win+D（自实现）：还原 x/y 个窗口`；叠加层全程保持在桌面之上，系统图标层不显示（实现见 `src/platform/show_desktop.rs`）。
- 壁纸零改动实测：启动前/退出后 `HKCU\Control Panel\Desktop\Wallpaper`、`WallpaperStyle`、`Explorer\Wallpapers\BackgroundType`、`Control Panel\Colors\Background` 全部一致，壁纸文件 SHA256 `52BE575D…C5963` 未变；叠加层显示 vs 隐藏时同一壁纸区域像素逐字节相同，桌面像素 `(20,20,20)` 不变。
- 鼠标移动闪烁实测：连续移动鼠标并逐帧截屏 20 帧，壁纸区域（800×700）SHA256 全部相同（唯一值 1 个）；日志显示空闲时 2–6 帧/秒、移动时 20–93 帧/秒，但帧内容不再含遮罩。
- 任务栏实测：叠加层矩形 `0,0,2560,1392`，任务栏 `0,1392,2560,1440`（工作区日志 `叠加层已按工作区裁剪（排除任务栏），共 1 块显示器工作区`）；对任务栏三个坐标点做 `WindowFromPoint` 均返回 `Shell_TrayWnd / MSTaskSwWClass / TrayNotifyWnd`，未被叠加层覆盖。
- 折叠箭头改为 GB2312 自带字形 `▲/▼` 后，日志中不再出现 `No font has "▾"` 缺字告警。
- **降级路径真机验证**：设置 `DESKKEEPER_FORCE_FALLBACK_MENU=1`（或在 `%LOCALAPPDATA%\DeskKeeper` 下创建 `force_fallback_menu` 标记文件）启动，强制 `shell_menu::request` 返回失败。右键文件图标后日志输出 `原生右键菜单不可用（测试开关…），退回内置菜单`，内置菜单在原点击位置弹出（见 [docs/screenshot-fallback-menu.png](docs/screenshot-fallback-menu.png)）；点击「重命名」后名称区进入行内编辑（移开指针后编辑框仍在，见 [docs/screenshot-fallback-rename.png](docs/screenshot-fallback-rename.png)），点击「复制路径」后剪贴板内容为源文件的真实桌面路径。该开关仅用于验证，正常启动不设置。

未能自动化验证、需人工点检的部分：原生菜单子菜单展开与「属性」系统对话框（自动化键鼠注入被系统钩子限制，详见「已知限制」）、真实混合 DPI 双屏与换分辨率（本机仅单屏 150%，几何由单测覆盖）、托盘菜单点击、真实粘贴回填（双击记录行自动 `Ctrl+V`）。

已知取舍：叠加层是**一个跨屏窗口**，盒子的屏幕位置 / 尺寸仍以物理像素持久化（避免改分辨率后漂移）；其标题、按钮、图标按所在屏 DPI 缩放。设置窗口按窗口 `pixels_per_point` 渲染，在非窗口 DPI 的屏幕上尺寸可能略有偏差。

## 人工验收清单

1. 运行 `target\release\deskkeeper.exe`，确认系统桌面图标消失、盒子上出现。
2. 右键桌面空白处 → 新建盒子；拖动标题栏移动、拖左/右/下边与四角缩放、点击折叠箭头、调色与点击锁图标切换锁定。
3. 把桌面文件拖进盒子，确认文件**仍留在桌面目录**（只记录路径引用），并已从桌面网格排除；从盒子移出后桌面重新显示。
4. 在桌面目录里新建/删除文件，确认桌面与盒子内容在 1 秒内刷新。
5. 文件盒/普通盒点击类型 tab（全部 / PDF / 图片等），确认只改变显示、文件位置不变；重启后 tab 仍保持；双击名称重命名并按 `Enter`，确认磁盘文件名同步变化。
6. 按 `Ctrl+Shift+V` 显示剪贴板盒（若此前关闭会自动恢复）；复制一段文本、一张图片、几个文件；确认三种类型都出现、搜索与类型筛选可用、行内「顶」「删」生效。
7. 双击一条记录，确认内容被粘贴到当前前台窗口；单击则只复制回剪贴板。
8. 关闭剪贴板盒/待办盒（标题栏 ×），确认盒子从桌面消失但内容、几何保留；在设置页「已关闭的盒子」点「重新打开」，确认恢复后与关闭前一致，且重启后仍记得关闭状态。
9. 新建多个待办盒，分别添加条目、调节几何与折叠，确认互不影响且重启后各自保持。
10. 右键文件图标确认系统菜单（含追加项）；在「已知限制」列出的环境下改看内置菜单是否可用。托盘图标右键，逐项验证菜单动作；设置页修改热键、主题、开机自启后重启确认生效。
11. 直接结束进程后重新启动，确认桌面图标被恢复（未残留隐藏状态）。
12. 按 `Win+D`，确认普通窗口被最小化、盒子与壁纸仍可见；再按一次，确认先前窗口按原顺序还原，原生桌面图标层始终未出现。
13. 确认任务栏始终可见可点（开始菜单、任务栏按钮、时钟），包括 `Win+D` 自实现期间；确认鼠标快速划过桌面时壁纸不闪烁、不被染暗。
