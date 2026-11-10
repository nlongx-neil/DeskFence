//! 空格快速预览：把选中文件的内容抽取到内存，供叠加层里的无边框预览面板显示。
//!
//! 全部解析都在后台线程完成（文本编码识别、Office ZIP+XML 抽文字、目录列举），
//! UI 线程只做绘制；图片/PDF 不在这里解码，而是复用 `IShellItemImageFactory` 缩略图。
//!
//! 高亮刻意不用 syntect：syntect 会引入整套语法/主题数据（体积与启动成本都很高），
//! 这里用逐行轻量分词，只区分注释/字符串/数字/关键字，满足快速预览的定位。

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use crate::events::{BgEvent, EventSink};

/// 文本预览读取上限：超过后只读前 4 MB，并提示已截断。
pub const TEXT_LIMIT: u64 = 4 * 1024 * 1024;
/// 单次预览最多保留的字符数（Office 抽出的文字同样受限）。
pub const MAX_CHARS: usize = 600_000;

/// 预览类型（决定面板正文的呈现方式）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewKind {
    Text,
    Code,
    Office,
    Image,
    Pdf,
    Folder,
    Other,
}

impl PreviewKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "文本",
            Self::Code => "代码",
            Self::Office => "Office",
            Self::Image => "图片",
            Self::Pdf => "PDF",
            Self::Folder => "文件夹",
            Self::Other => "二进制",
        }
    }
}

/// 轻量高亮语言族。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Rust,
    CLike,
    Python,
    Shell,
    HashComment,
    Json,
    Plain,
}

/// 高亮片段类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    Str,
    Comment,
    Number,
}

/// 后台抽取完成的一份预览数据（图片/PDF 的 `text` 为 `None`）。
#[derive(Clone, Debug)]
pub struct PreviewData {
    pub path: PathBuf,
    pub kind: PreviewKind,
    pub text: Option<String>,
    pub lang: Lang,
    pub encoding: String,
    pub truncated: bool,
    pub total_bytes: u64,
    pub elapsed_ms: f64,
    pub note: Option<String>,
}

/// 预览请求（只带路径，元数据由工作线程自行读取）。
#[derive(Clone, Debug)]
pub struct PreviewRequest {
    pub path: PathBuf,
}

/// 启动预览工作线程，返回请求发送端；结果通过 `BgEvent::PreviewReady` 回到 UI 线程。
pub fn spawn_worker() -> (Sender<PreviewRequest>, Receiver<PreviewData>) {
    let (req_tx, req_rx) = mpsc::channel::<PreviewRequest>();
    let (data_tx, data_rx) = mpsc::channel::<PreviewData>();
    std::thread::Builder::new()
        .name("preview-worker".into())
        .spawn(move || {
            while let Ok(req) = req_rx.recv() {
                let data = build(&req.path);
                log::info!(
                    "[preview] {} 类型={} 编码={} {} 字节 截断={} 文本={} 字 耗时 {:.1} ms{}",
                    req.path.display(),
                    data.kind.label(),
                    data.encoding,
                    data.total_bytes,
                    data.truncated,
                    data.text.as_ref().map(|t| t.chars().count()).unwrap_or(0),
                    data.elapsed_ms,
                    data.note
                        .as_ref()
                        .map(|n| format!("，{n}"))
                        .unwrap_or_default()
                );
                if data_tx.send(data).is_err() {
                    break;
                }
            }
        })
        .ok();
    (req_tx, data_rx)
}

/// 把工作线程的结果桥接到 egui 事件通道（与图标线程同一套模式）。
pub fn spawn_bridge(sink: EventSink, rx: Receiver<PreviewData>) {
    std::thread::Builder::new()
        .name("preview-bridge".into())
        .spawn(move || {
            while let Ok(data) = rx.recv() {
                sink.emit(BgEvent::PreviewReady(data));
            }
        })
        .ok();
}

/// 抽取一个路径的预览数据（同步执行，供工作线程与测试调用）。
pub fn build(path: &Path) -> PreviewData {
    let started = std::time::Instant::now();
    let meta = std::fs::metadata(path);
    let total_bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if meta.as_ref().is_ok_and(|m| m.is_dir()) {
        let (text, note) = folder_listing(path);
        return finish(
            path,
            PreviewKind::Folder,
            Some(text),
            Lang::Plain,
            "目录".into(),
            false,
            total_bytes,
            started,
            Some(note),
        );
    }
    if path.to_string_lossy().starts_with("::") {
        return finish(
            path,
            PreviewKind::Other,
            None,
            Lang::Plain,
            "-".into(),
            false,
            0,
            started,
            Some("系统虚拟项目不支持内容预览".into()),
        );
    }

    if is_image(&ext) {
        return finish(
            path,
            PreviewKind::Image,
            None,
            Lang::Plain,
            "-".into(),
            false,
            total_bytes,
            started,
            Some("图片由系统缩略图工厂绘制".into()),
        );
    }
    if ext == "pdf" {
        return finish(
            path,
            PreviewKind::Pdf,
            None,
            Lang::Plain,
            "-".into(),
            false,
            total_bytes,
            started,
            Some("PDF 首页由系统缩略图工厂渲染".into()),
        );
    }
    if matches!(ext.as_str(), "docx" | "xlsx" | "pptx") {
        let kind = PreviewKind::Office;
        match office_text(path, &ext) {
            Ok((text, note)) => {
                let (text, clipped) = clamp_chars(text, MAX_CHARS);
                return finish(
                    path,
                    kind,
                    Some(text),
                    Lang::Plain,
                    "ZIP+XML".into(),
                    clipped,
                    total_bytes,
                    started,
                    Some(note),
                );
            }
            Err(e) => {
                return finish(
                    path,
                    kind,
                    None,
                    Lang::Plain,
                    "-".into(),
                    false,
                    total_bytes,
                    started,
                    Some(format!("Office 内容抽取失败：{e}（可双击用 Office 打开）")),
                );
            }
        }
    }

    let lang = lang_for(&ext, &path.to_string_lossy());
    let code = matches!(
        lang,
        Lang::Rust | Lang::CLike | Lang::Python | Lang::Shell | Lang::Json
    );
    let kind = if code {
        PreviewKind::Code
    } else {
        PreviewKind::Text
    };
    match read_text(path, total_bytes) {
        Ok((text, encoding, truncated)) => {
            let (text, clipped) = clamp_chars(text, MAX_CHARS);
            let note = if !encoding_is_utf8(&encoding) && lang != Lang::Plain {
                Some("非 UTF-8 源码按原始编码预览".into())
            } else {
                None
            };
            finish(
                path,
                kind,
                Some(text),
                lang,
                encoding,
                truncated || clipped,
                total_bytes,
                started,
                note,
            )
        }
        Err(e) => finish(
            path,
            PreviewKind::Other,
            None,
            Lang::Plain,
            "-".into(),
            false,
            total_bytes,
            started,
            Some(format!("无法按文本读取：{e}")),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn finish(
    path: &Path,
    kind: PreviewKind,
    text: Option<String>,
    lang: Lang,
    encoding: String,
    truncated: bool,
    total_bytes: u64,
    started: std::time::Instant,
    note: Option<String>,
) -> PreviewData {
    PreviewData {
        path: path.to_path_buf(),
        kind,
        text,
        lang,
        encoding,
        truncated,
        total_bytes,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        note,
    }
}

fn encoding_is_utf8(label: &str) -> bool {
    label.starts_with("UTF-8")
}

/// 读取文本：BOM → UTF-8 → GBK 兜底；超过 4 MB 只读前缀。
fn read_text(path: &Path, total_bytes: u64) -> Result<(String, String, bool), String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    if total_bytes > TEXT_LIMIT {
        let mut head = (&mut f).take(TEXT_LIMIT);
        head.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    } else {
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    }
    let (text, encoding) = decode_text(&buf);
    let binary = text_chars(&text)
        .take(4096)
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\r' || *c == '\t')
        .count()
        * 100
        / text_chars(&text).take(4096).count().max(1)
        < 70;
    if binary && total_bytes <= TEXT_LIMIT {
        return Err("内容更像二进制（可打印字符不足 70%）".into());
    }
    Ok((text, encoding, total_bytes > TEXT_LIMIT))
}

fn text_chars(s: &str) -> impl Iterator<Item = char> + '_ {
    s.chars()
}

/// 编码识别：UTF-8/UTF-16 BOM → 严格 UTF-8 → GBK 兜底。
pub fn decode_text(bytes: &[u8]) -> (String, String) {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return (
            String::from_utf8_lossy(rest).into_owned(),
            "UTF-8 (BOM)".into(),
        );
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return (decode_utf16(rest, true), "UTF-16LE".into());
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return (decode_utf16(rest, false), "UTF-16BE".into());
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return (s.to_owned(), "UTF-8".into());
    }
    let (cow, _, had_errors) = encoding_rs::GBK.decode(bytes);
    (
        cow.into_owned(),
        if had_errors {
            "GBK (部分不可解)"
        } else {
            "GBK"
        }
        .into(),
    )
}

fn decode_utf16(bytes: &[u8], little: bool) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            if little {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// 按字符数截断（保留在 UTF-8 字符边界上），返回（文本，是否截断）。
fn clamp_chars(text: String, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        return (text, false);
    }
    let cut: String = text.chars().take(max).collect();
    (cut, true)
}

fn is_image(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "tif" | "tiff" | "avif" | "heic"
    )
}

/// 扩展名/文件名 → 高亮语言族。
pub fn lang_for(ext: &str, name: &str) -> Lang {
    match ext {
        "rs" => Lang::Rust,
        "c" | "h" | "cc" | "cpp" | "hpp" | "cs" | "java" | "kt" | "go" | "js" | "mjs" | "cjs"
        | "ts" | "tsx" | "jsx" | "swift" | "scss" | "css" | "php" => Lang::CLike,
        "py" => Lang::Python,
        "sh" | "bash" | "zsh" | "ps1" | "bat" | "cmd" => Lang::Shell,
        "toml" | "ini" | "cfg" | "conf" | "yaml" | "yml" => Lang::HashComment,
        "json" => Lang::Json,
        _ => {
            let lower = name.to_lowercase();
            if lower.ends_with("dockerfile") || lower.ends_with("makefile") {
                Lang::HashComment
            } else {
                Lang::Plain
            }
        }
    }
}

/// 逐行轻量分词：只返回注释/字符串/数字/关键字的片段（字节区间）。
///
/// 刻意不跨行维护块注释状态：快速预览只需要“扫一眼”，逐行无状态分词更简单、
/// 更便宜，出错也只影响颜色不影响文本本身。
pub fn highlight_line(line: &str, lang: Lang) -> Vec<(std::ops::Range<usize>, TokenKind)> {
    let mut spans = Vec::new();
    if lang == Lang::Plain {
        return spans;
    }
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let (start, c) = chars[i];
        if let Some(prefix) = line_comment(lang) {
            if line[start..].starts_with(prefix) {
                spans.push((start..line.len(), TokenKind::Comment));
                break;
            }
        }
        if c == '/' && line[start..].starts_with("/*") {
            let end = line[start + 2..]
                .find("*/")
                .map(|p| start + 2 + p + 2)
                .unwrap_or(line.len());
            spans.push((start..end, TokenKind::Comment));
            i = chars.partition_point(|(off, _)| *off < end);
            continue;
        }
        if c == '"' || c == '\'' || (lang == Lang::CLike && c == '`') {
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j].1 == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j].1 == c {
                    j += 1;
                    break;
                }
                j += 1;
            }
            let end = chars.get(j).map(|(off, _)| *off).unwrap_or(line.len());
            spans.push((start..end, TokenKind::Str));
            i = j;
            continue;
        }
        if c.is_ascii_digit() {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].1.is_ascii_alphanumeric() || chars[j].1 == '.') {
                j += 1;
            }
            let end = chars.get(j).map(|(off, _)| *off).unwrap_or(line.len());
            spans.push((start..end, TokenKind::Number));
            i = j;
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].1.is_alphanumeric() || chars[j].1 == '_') {
                j += 1;
            }
            let end = chars.get(j).map(|(off, _)| *off).unwrap_or(line.len());
            if keywords(lang).iter().any(|k| *k == &line[start..end]) {
                spans.push((start..end, TokenKind::Keyword));
            }
            i = j;
            continue;
        }
        i += 1;
    }
    spans
}

fn line_comment(lang: Lang) -> Option<&'static str> {
    match lang {
        Lang::Rust | Lang::CLike | Lang::Json => Some("//"),
        Lang::Python | Lang::Shell | Lang::HashComment => Some("#"),
        Lang::Plain => None,
    }
}

fn keywords(lang: Lang) -> &'static [&'static str] {
    const RUST: &[&str] = &[
        "fn", "let", "mut", "pub", "use", "mod", "impl", "struct", "enum", "trait", "match", "if",
        "else", "for", "while", "loop", "return", "self", "crate", "const", "static", "type",
        "where", "async", "await", "move", "ref", "dyn", "unsafe", "true", "false",
    ];
    const CLIKE: &[&str] = &[
        "function",
        "const",
        "let",
        "var",
        "class",
        "new",
        "return",
        "if",
        "else",
        "for",
        "while",
        "switch",
        "case",
        "break",
        "continue",
        "try",
        "catch",
        "throw",
        "public",
        "private",
        "static",
        "void",
        "int",
        "float",
        "double",
        "bool",
        "true",
        "false",
        "null",
        "undefined",
        "import",
        "export",
        "from",
        "extends",
        "interface",
        "type",
        "async",
        "await",
        "def",
        "package",
        "struct",
        "func",
        "go",
        "defer",
        "chan",
        "nil",
    ];
    const PY: &[&str] = &[
        "def", "class", "import", "from", "return", "if", "elif", "else", "for", "while", "try",
        "except", "finally", "with", "as", "lambda", "yield", "pass", "break", "continue", "True",
        "False", "None", "and", "or", "not", "in", "is", "global", "nonlocal",
    ];
    const SH: &[&str] = &[
        "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "case", "esac",
        "function", "return", "export", "local", "echo", "set", "unset", "exit",
    ];
    match lang {
        Lang::Rust => RUST,
        Lang::CLike => CLIKE,
        Lang::Python => PY,
        Lang::Shell => SH,
        Lang::Json | Lang::HashComment | Lang::Plain => &[],
    }
}

/// 目录预览：列出前 80 个条目名称，避免大目录拖慢预览。
fn folder_listing(path: &Path) -> (String, String) {
    let mut names: Vec<String> = Vec::new();
    let mut total = 0usize;
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            total += 1;
            if names.len() < 80 {
                let meta = e.metadata().ok();
                let dir = meta.as_ref().is_some_and(|m| m.is_dir());
                names.push(format!(
                    "{}{}",
                    e.file_name().to_string_lossy(),
                    if dir { "\\" } else { "" }
                ));
            }
        }
    }
    names.sort_by_key(|s| s.to_lowercase());
    let mut text = names.join("\n");
    if total > names.len() {
        text.push_str(&format!(
            "\n…（共 {total} 项，仅列出前 {} 项）",
            names.len()
        ));
    }
    (text, format!("共 {total} 项"))
}

/// Office（docx/xlsx/pptx）内容抽取：ZIP 解包 + 极简 XML 文本节点提取。
///
/// 不依赖 Office/COM，失败时由调用方降级为“可双击用 Office 打开”。
fn office_text(path: &Path, ext: &str) -> Result<(String, String), String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut ar = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    match ext {
        "docx" => {
            let xml = read_entry(&mut ar, "word/document.xml")?;
            let text = extract_tags(&xml, "w:t", "w:p", "w:tab");
            Ok((text, "正文 word/document.xml".into()))
        }
        "pptx" => {
            let names: Vec<String> = ar.file_names().map(str::to_string).collect();
            let mut slides: Vec<(u32, String)> = Vec::new();
            for name in names {
                if let Some(n) = slide_index(&name) {
                    slides.push((n, name));
                }
            }
            slides.sort_by_key(|(n, _)| *n);
            if slides.is_empty() {
                return Err("压缩包里没有幻灯片".into());
            }
            let mut out = String::new();
            for (n, name) in &slides {
                let xml = read_entry(&mut ar, name)?;
                out.push_str(&format!("—— 第 {} 页 ——\n", n + 1));
                out.push_str(&extract_tags(&xml, "a:t", "a:p", ""));
                out.push('\n');
            }
            Ok((out, format!("{} 页幻灯片", slides.len())))
        }
        "xlsx" => {
            let shared = read_entry_opt(&mut ar, "xl/sharedStrings.xml")
                .map(|xml| shared_strings(&xml))
                .unwrap_or_default();
            let mut sheets: Vec<(u32, String)> = Vec::new();
            let names: Vec<String> = ar.file_names().map(str::to_string).collect();
            for name in names {
                if let Some(n) = sheet_index(&name) {
                    sheets.push((n, name));
                }
            }
            sheets.sort_by_key(|(n, _)| *n);
            let Some((_, name)) = sheets.first() else {
                return Err("压缩包里没有工作表".into());
            };
            let xml = read_entry(&mut ar, name)?;
            let (text, rows) = sheet_text(&xml, &shared);
            Ok((
                text,
                format!("首个工作表，{rows} 行；共 {} 表", sheets.len()),
            ))
        }
        _ => Err("不支持的 Office 类型".into()),
    }
}

fn slide_index(name: &str) -> Option<u32> {
    let name = name.replace('\\', "/");
    let rest = name.strip_prefix("ppt/slides/slide")?;
    let num = rest.strip_suffix(".xml")?;
    num.parse().ok()
}

fn sheet_index(name: &str) -> Option<u32> {
    let name = name.replace('\\', "/");
    let rest = name.strip_prefix("xl/worksheets/sheet")?;
    let num = rest.strip_suffix(".xml")?;
    num.parse().ok()
}

fn read_entry(ar: &mut zip::ZipArchive<File>, name: &str) -> Result<String, String> {
    // ZIP 规范用正斜杠；部分打包工具（含测试夹具）会写成反斜杠，两侧统一后再匹配。
    let want = name.replace('\\', "/");
    let raw = ar
        .file_names()
        .find(|n| n.replace('\\', "/") == want)
        .map(str::to_string)
        .ok_or_else(|| format!("缺少 {name}"))?;
    let mut f = ar.by_name(&raw).map_err(|e| e.to_string())?;
    let mut buf = String::new();
    f.read_to_string(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

fn read_entry_opt(ar: &mut zip::ZipArchive<File>, name: &str) -> Option<String> {
    read_entry(ar, name).ok()
}

/// 提取指定文本标签的内容；遇到段落结束标签补换行，制表标签补 \t。
fn extract_tags(xml: &str, text_tag: &str, para_tag: &str, tab_tag: &str) -> String {
    let mut out = String::new();
    let mut pos = 0usize;
    while let Some(rel) = xml[pos..].find('<') {
        let lt = pos + rel;
        let Some(gt_rel) = xml[lt..].find('>') else {
            break;
        };
        let gt = lt + gt_rel;
        let raw = &xml[lt + 1..gt];
        let self_closing = raw.ends_with('/');
        let closing = raw.starts_with('/');
        let name = raw
            .trim_end_matches('/')
            .trim_start_matches('/')
            .split([' ', '\n', '\r', '\t'])
            .next()
            .unwrap_or("");
        pos = gt + 1;
        if !tab_tag.is_empty() && name == tab_tag && self_closing {
            out.push('\t');
        } else if name == para_tag && closing {
            out.push('\n');
        } else if name == text_tag && !self_closing && !closing {
            let end_tag = format!("</{text_tag}>");
            if let Some(rel_end) = xml[pos..].find(&end_tag) {
                let content = &xml[pos..pos + rel_end];
                out.push_str(&decode_entities(content));
                pos += rel_end + end_tag.len();
            }
        }
    }
    out.trim_start_matches('\n').to_string()
}

/// xlsx 共享字符串表：(<si> 段落内的全部 <t> 文本)。
fn shared_strings(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = xml[pos..].find("<si") {
        let start = pos + rel;
        let Some(end_rel) = xml[start..].find("</si>") else {
            break;
        };
        let end = start + end_rel;
        out.push(extract_tags(&xml[start..end], "t", "", ""));
        pos = end + 5;
    }
    out
}

/// 首个工作表：按行拼单元格，值之间用 \t，行之间用 \n。
fn sheet_text(xml: &str, shared: &[String]) -> (String, usize) {
    let mut out = String::new();
    let mut rows = 0usize;
    let mut pos = 0usize;
    while let Some(rel) = xml[pos..].find("<row") {
        let row_start = pos + rel;
        // 行必须成对出现；缺 </row> 时截到下一个 <row。
        let end = xml[row_start..]
            .find("</row>")
            .map(|p| row_start + p)
            .or_else(|| xml[row_start + 4..].find("<row").map(|p| row_start + 4 + p))
            .unwrap_or(xml.len());
        let row = &xml[row_start..end];
        let mut cells: Vec<String> = Vec::new();
        let mut cpos = 0usize;
        while let Some(crel) = row[cpos..].find("<c") {
            let cstart = cpos + crel;
            let Some(cend_rel) = row[cstart..].find("</c>") else {
                break;
            };
            let cend = cstart + cend_rel;
            let cell = &row[cstart..cend];
            let kind = if cell.contains("t=\"s\"") {
                "s"
            } else if cell.contains("t=\"inlineStr\"") || cell.contains("<is") {
                "inline"
            } else {
                "n"
            };
            let value = if kind == "inline" {
                extract_tags(cell, "t", "", "")
            } else {
                between(cell, "<v>", "</v>").unwrap_or_default()
            };
            let value = if kind == "s" {
                value
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| shared.get(i).cloned())
                    .unwrap_or_default()
            } else {
                decode_entities(&value)
            };
            cells.push(value);
            cpos = cend + 4;
        }
        if cells.iter().any(|c| !c.is_empty()) {
            out.push_str(&cells.join("\t"));
            out.push('\n');
            rows += 1;
        }
        pos = end.saturating_add(1);
        if rows > 5000 {
            break;
        }
    }
    (out, rows)
}

fn between(s: &str, open: &str, close: &str) -> Option<String> {
    let start = s.find(open)? + open.len();
    let end = s[start..].find(close)? + start;
    Some(s[start..end].to_string())
}

/// 最小 XML 实体解码（Office 文本里的 &amp; 等）。
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let ent = &rest[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&rest[..=semi]),
        }
        rest = &rest[semi + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_utf8_bom_and_plain() {
        let (t, e) = decode_text(b"\xEF\xBB\xBFhello");
        assert_eq!(t, "hello");
        assert_eq!(e, "UTF-8 (BOM)");
        let (t, e) = decode_text("中文".as_bytes());
        assert_eq!(t, "中文");
        assert_eq!(e, "UTF-8");
    }

    #[test]
    fn decode_utf16le_and_gbk() {
        let (t, e) = decode_text(b"\xFF\xFEa\x00b\x00");
        assert_eq!(t, "ab");
        assert_eq!(e, "UTF-16LE");
        // “中文”的 GBK 编码
        let (t, e) = decode_text(&[0xD6, 0xD0, 0xCE, 0xC4]);
        assert_eq!(t, "中文");
        assert!(e.starts_with("GBK"));
    }

    #[test]
    fn clamp_keeps_char_boundary() {
        let (t, cut) = clamp_chars("中文abc".into(), 3);
        assert_eq!(t, "中文a");
        assert!(cut);
    }

    #[test]
    fn docx_tag_extraction() {
        let xml = r#"<w:body><w:p><w:r><w:t>你好</w:t></w:r><w:r><w:t xml:space="preserve"> &amp; world</w:t></w:r></w:p><w:p><w:t>第二段</w:t></w:p></w:body>"#;
        let text = extract_tags(xml, "w:t", "w:p", "w:tab");
        assert_eq!(text, "你好 & world\n第二段\n");
    }

    #[test]
    fn pptx_tag_extraction() {
        let xml = r#"<a:p><a:r><a:t>标题</a:t></a:r></a:p><a:p><a:t>正文</a:t></a:p>"#;
        let text = extract_tags(xml, "a:t", "a:p", "");
        assert_eq!(text, "标题\n正文\n");
    }

    #[test]
    fn xlsx_shared_and_cells() {
        let shared = shared_strings(
            r#"<sst><si><t>名称</t></si><si><r><t>值</t></r><r><t>一</t></r></si></sst>"#,
        );
        assert_eq!(shared, vec!["名称".to_string(), "值一".to_string()]);
        let sheet = r#"<sheetData><row><c t="s"><v>0</v></c><c t="s"><v>1</v></c></row><row><c><v>42</v></c></row></sheetData>"#;
        let (text, rows) = sheet_text(sheet, &shared);
        assert_eq!(text, "名称\t值一\n42\n");
        assert_eq!(rows, 2);
    }

    #[test]
    fn entity_decoding() {
        assert_eq!(decode_entities("a&amp;b&lt;c&#65;"), "a&b<cA");
    }

    #[test]
    fn highlight_marks_comments_strings_keywords() {
        let spans = highlight_line("let s = \"x\"; // hi", Lang::Rust);
        let kinds: Vec<TokenKind> = spans.iter().map(|(_, k)| *k).collect();
        assert!(kinds.contains(&TokenKind::Keyword));
        assert!(kinds.contains(&TokenKind::Str));
        assert!(kinds.contains(&TokenKind::Comment));
        let text = "let s = \"x\"; // hi";
        let (r, _) = spans
            .iter()
            .find(|(_, k)| *k == TokenKind::Comment)
            .unwrap();
        assert_eq!(&text[r.clone()], "// hi");
    }

    #[test]
    fn language_by_extension() {
        assert_eq!(lang_for("rs", "a.rs"), Lang::Rust);
        assert_eq!(lang_for("ts", "a.ts"), Lang::CLike);
        assert_eq!(lang_for("toml", "Cargo.toml"), Lang::HashComment);
        assert_eq!(lang_for("", "README"), Lang::Plain);
    }
}
