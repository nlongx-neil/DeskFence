//! 剪贴板历史存储：SQLite（元数据）+ 磁盘 PNG（图片/缩略图）。
//! 所有数据库操作在独立线程执行，避免阻塞 UI。

pub mod service;
pub mod store;

pub use store::{ClipRecord, NewClip, Store, StoreCmd};
