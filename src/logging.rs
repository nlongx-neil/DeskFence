//! 轻量滚动文件日志：单文件超过阈值后轮转，保留若干历史文件。
//! 写入在独立线程完成（调用线程只做格式化 + 入队），避免 UI/钩子线程被同步磁盘 IO 阻塞。
//! 日志写入失败时退化为 stderr，绝不 panic。

use log::{LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::time::Duration;

const MAX_BYTES: u64 = 2 * 1024 * 1024;
const KEEP_FILES: usize = 3;
/// 队列容量：满时丢弃并退化为 stderr，保证调用线程永不阻塞。
const QUEUE_LINES: usize = 1024;
/// `flush` 等待写入线程应答的上限。
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

enum Msg {
    Line(String),
    Flush(SyncSender<()>),
}

struct Inner {
    file: Option<File>,
    path: PathBuf,
    size: u64,
}

impl Inner {
    fn new(path: PathBuf) -> Self {
        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        Self { file, path, size }
    }

    fn rotate(&mut self) {
        // deskkeeper.log -> .1 -> .2 -> .3
        for i in (1..KEEP_FILES).rev() {
            let from = self.path.with_extension(format!("log.{i}"));
            let to = self.path.with_extension(format!("log.{}", i + 1));
            if from.exists() {
                let _ = fs::rename(&from, &to);
            }
        }
        let first = self.path.with_extension("log.1");
        if self.path.exists() {
            let _ = fs::rename(&self.path, &first);
        }
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
        self.size = 0;
    }

    fn write_line(&mut self, line: &str) {
        if self.size + line.len() as u64 > MAX_BYTES {
            self.rotate();
        }
        if let Some(file) = self.file.as_mut() {
            if file.write_all(line.as_bytes()).is_ok() {
                self.size += line.len() as u64;
                return;
            }
        }
        // 落盘失败：退化到 stderr，保证日志不静默丢失
        eprintln!("{line}");
    }

    fn flush(&mut self) {
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
        }
    }
}

fn worker(mut inner: Inner, rx: Receiver<Msg>) {
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Line(line) => inner.write_line(&line),
            Msg::Flush(ack) => {
                inner.flush();
                let _ = ack.send(());
            }
        }
    }
    inner.flush();
}

pub struct FileLogger {
    tx: Option<SyncSender<Msg>>,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "[{}] {:<5} {}: {}\n",
            crate::util::format_datetime(crate::util::now_secs()),
            record.level(),
            record.target(),
            record.args()
        );
        match &self.tx {
            Some(tx) => match tx.try_send(Msg::Line(line)) {
                Ok(()) => {}
                // 队列满/写入线程已退出：退化为 stderr，绝不阻塞调用线程
                Err(
                    TrySendError::Full(Msg::Line(l)) | TrySendError::Disconnected(Msg::Line(l)),
                ) => eprintln!("{l}"),
                Err(_) => {}
            },
            None => eprintln!("{line}"),
        }
    }

    fn flush(&self) {
        let Some(tx) = &self.tx else { return };
        let (ack, wait) = sync_channel(1);
        if tx.send(Msg::Flush(ack)).is_ok() {
            let _ = wait.recv_timeout(FLUSH_TIMEOUT);
        }
    }
}

/// 初始化全局 logger（写入线程后台运行）。
pub fn init(dir: &Path, level: LevelFilter) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;
    let inner = Inner::new(dir.join("deskkeeper.log"));
    let (tx, rx) = sync_channel::<Msg>(QUEUE_LINES);
    let tx = match std::thread::Builder::new()
        .name("log-writer".to_string())
        .spawn(move || worker(inner, rx))
    {
        Ok(_) => Some(tx),
        Err(e) => {
            // 线程起不来时退化为直接写 stderr，日志不丢但不再落盘
            eprintln!("启动日志写入线程失败: {e}");
            None
        }
    };
    let logger = FileLogger { tx };
    log::set_boxed_logger(Box::new(logger)).map_err(|e| anyhow::anyhow!("设置日志器失败: {e}"))?;
    log::set_max_level(level);
    Ok(())
}
