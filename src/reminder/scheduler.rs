//! 到期提醒调度：消息窗口线程按定时器扫描待办快照，产出 [`crate::events::BgEvent::RemindersDue`]。
//!
//! UI 线程只在配置变化时刷新快照（[`sync_from_config`]）；扫描本身既不持有配置也不
//! 写回状态——去重锚点与周期推进由 UI 线程处理提醒时调用
//! [`crate::reminder::mark_fired`] 完成。

use std::sync::Mutex;
use std::time::Instant;

use crate::config::{
    AppConfig, BoxKind, ReminderConfig, RepeatRule, TodoItem, DEFAULT_DAILY_REMINDER_MINUTE,
};
use crate::reminder::{self, time::SystemZone};

/// 快照中的一条待办（带盒子上下文，便于气泡文案与点击定位）。
#[derive(Clone, Debug, PartialEq)]
pub struct ReminderCandidate {
    pub box_id: String,
    pub box_name: String,
    pub item: TodoItem,
}

/// 一条已到期提醒（交给 UI 线程）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueItem {
    pub box_id: String,
    pub box_name: String,
    pub todo_id: String,
    pub text: String,
    /// 当前有效到期点（UTC epoch 秒），可能早于当前时间。
    pub due_at: i64,
}

static SNAPSHOT: Mutex<Vec<ReminderCandidate>> = Mutex::new(Vec::new());
static SETTINGS: Mutex<ReminderConfig> = Mutex::new(ReminderConfig {
    enabled: true,
    daily_minute: DEFAULT_DAILY_REMINDER_MINUTE,
    lead_min: 0,
    snooze_min: 10,
    balloon: true,
});
static LAST_TICK: Mutex<Option<(Instant, i64)>> = Mutex::new(None);

/// 当前生效的提醒设置（快照副本）。
pub fn settings() -> ReminderConfig {
    lock(&SETTINGS).clone()
}

/// 当前墙上时间（UTC epoch 秒）。
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 用当前配置刷新调度快照；必要时通知消息窗口重新装载定时器粒度。
pub fn sync_from_config(cfg: &AppConfig) {
    *lock(&SETTINGS) = cfg.reminder.clone();
    let mut items = Vec::new();
    for b in &cfg.boxes {
        if b.kind != BoxKind::Todo {
            continue;
        }
        for t in &b.todos {
            if t.done || t.due_at.is_none() {
                continue;
            }
            items.push(ReminderCandidate {
                box_id: b.id.clone(),
                box_name: b.name.clone(),
                item: t.clone(),
            });
        }
    }
    let changed = {
        let mut guard = lock(&SNAPSHOT);
        let changed = *guard != items;
        *guard = items;
        changed
    };
    if changed {
        crate::platform::message_window::refresh_reminder_timer();
    }
}

/// 定时器粒度：存在短于 15 分钟的固定间隔任务时用 30s，否则 60s。
pub fn tick_interval_ms() -> u32 {
    let guard = lock(&SNAPSHOT);
    let has_short = guard
        .iter()
        .any(|c| c.item.repeat == RepeatRule::Interval && c.item.interval_minutes() < 15);
    if has_short {
        30_000
    } else {
        60_000
    }
}

/// 扫描当前到期项（按到期时刻升序）。
pub fn scan(now: i64) -> Vec<DueItem> {
    let cfg = settings();
    if !cfg.enabled {
        return Vec::new();
    }
    let zone = SystemZone;
    let guard = lock(&SNAPSHOT);
    let mut due: Vec<DueItem> = guard
        .iter()
        .filter_map(|c| {
            let occ = reminder::due_now(&c.item, now, &zone, cfg.daily_minute)?;
            Some(DueItem {
                box_id: c.box_id.clone(),
                box_name: c.box_name.clone(),
                todo_id: c.item.id.clone(),
                text: c.item.text.clone(),
                due_at: occ.at,
            })
        })
        .collect();
    due.sort_by_key(|d| d.due_at);
    due
}

/// 时钟跳变判定：墙上时钟增量比单调时钟增量多出 90s 以上，视为睡眠或手改时间。
pub fn clock_jumped(mono_secs: i64, prev_wall: i64, wall: i64) -> bool {
    (wall - prev_wall) - mono_secs > 90
}

/// 记录一次 tick；返回 true 表示本次需要按「补提醒」口径处理（首次或时钟跳变）。
pub fn note_tick(wall: i64) -> bool {
    let now = Instant::now();
    let mut guard = lock(&LAST_TICK);
    let jumped = match *guard {
        Some((prev, prev_wall)) => clock_jumped(prev.elapsed().as_secs() as i64, prev_wall, wall),
        None => true,
    };
    *guard = Some((now, wall));
    jumped
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BoxConfig, RepeatRule};

    /// 快照与 tick 状态是进程级全局，测试必须串行。
    static SERIAL: Mutex<()> = Mutex::new(());

    fn todo_box(items: Vec<TodoItem>) -> BoxConfig {
        BoxConfig {
            id: "box-1".into(),
            name: "工作".into(),
            kind: BoxKind::Todo,
            todos: items,
            ..BoxConfig::default()
        }
    }

    #[test]
    fn scan_skips_done_and_undated_items() {
        let _serial = lock(&SERIAL);
        let now = now_secs();
        let cfg = AppConfig {
            boxes: vec![todo_box(vec![
                TodoItem {
                    id: "a".into(),
                    text: "已到期".into(),
                    due_at: Some(now - 60),
                    ..TodoItem::default()
                },
                TodoItem {
                    id: "b".into(),
                    text: "已完成".into(),
                    done: true,
                    due_at: Some(now - 60),
                    ..TodoItem::default()
                },
                TodoItem {
                    id: "c".into(),
                    text: "无到期".into(),
                    ..TodoItem::default()
                },
                TodoItem {
                    id: "d".into(),
                    text: "未来".into(),
                    due_at: Some(now + 3_600),
                    ..TodoItem::default()
                },
            ])],
            ..AppConfig::default()
        };
        sync_from_config(&cfg);
        let due = scan(now);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].todo_id, "a");
        assert_eq!(due[0].box_name, "工作");
        sync_from_config(&AppConfig::default());
    }

    #[test]
    fn last_fired_dedupes_repeated_scan() {
        let _serial = lock(&SERIAL);
        let now = now_secs();
        let cfg = AppConfig {
            boxes: vec![todo_box(vec![TodoItem {
                id: "a".into(),
                text: "巡检".into(),
                due_at: Some(now - 60),
                last_fired: Some(now - 60),
                ..TodoItem::default()
            }])],
            ..AppConfig::default()
        };
        sync_from_config(&cfg);
        assert!(scan(now).is_empty());
        sync_from_config(&AppConfig::default());
    }

    #[test]
    fn short_interval_shortens_tick() {
        let _serial = lock(&SERIAL);
        let cfg = AppConfig {
            boxes: vec![todo_box(vec![TodoItem {
                id: "a".into(),
                text: "频繁".into(),
                due_at: Some(1),
                repeat: RepeatRule::Interval,
                interval_min: 5,
                ..TodoItem::default()
            }])],
            ..AppConfig::default()
        };
        sync_from_config(&cfg);
        assert_eq!(tick_interval_ms(), 30_000);
        sync_from_config(&AppConfig::default());
        assert_eq!(tick_interval_ms(), 60_000);
    }

    #[test]
    fn clock_jump_detection() {
        // 正常 60s tick：单调与墙上同步。
        assert!(!clock_jumped(60, 1_000, 1_060));
        // 睡眠 1 小时后：单调只走了 10s。
        assert!(clock_jumped(10, 1_000, 4_610));
        // 手动把表调快 2 分钟。
        assert!(clock_jumped(60, 1_000, 1_180));
        // 手动调慢不视为跳变（自然等待下一次 tick）。
        assert!(!clock_jumped(60, 1_000, 940));
    }

    #[test]
    fn note_tick_flags_first_call_and_jump() {
        let _serial = lock(&SERIAL);
        // 首次调用必然要求补提醒；随后同一时刻再次调用不触发。
        assert!(note_tick(10_000));
        assert!(!note_tick(10_001));
    }
}
