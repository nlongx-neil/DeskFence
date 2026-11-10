//! DeskKeeper 库根：桌面整理与剪贴板历史的可测试核心。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod app;
pub mod clipboard;
pub mod config;
pub mod events;
pub mod fileops;
pub mod logging;
pub mod member_probe;
pub mod migration;
pub mod model;
pub mod platform;
pub mod preview;
pub mod startup;
pub mod ui;
pub mod util;
pub mod watcher;
