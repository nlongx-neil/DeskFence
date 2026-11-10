//! 待办到期时间：周期推进与到期判定。
//!
//! 这里只放**纯函数**（不做 IO、不读时钟），调度与 UI 只负责传入 `now` 与 [`LocalZone`]，
//! 因此周期推进、DST 边界、去重逻辑都能用单测覆盖。

pub mod scheduler;
pub mod time;

use crate::config::{IntervalBase, RepeatRule, TodoItem};
use time::{LocalTime, LocalZone};

/// 单次推进的最大步数（约 55 年按天推进），防止异常数据导致死循环。
const MAX_CATCH_UP_STEPS: usize = 20_000;

/// 一次「应提醒」的计算结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Occurrence {
    /// 当前有效到期点（UTC epoch 秒），可能早于 `now`（应用未运行导致的迟到）。
    pub at: i64,
    /// 实际触发时刻（UTC epoch 秒），已含提前量与「只设日期」的每日提醒时刻。
    pub fire_at: i64,
}

/// 当前有效到期点。
///
/// - 无到期时间 → `None`；
/// - `due_at` 在未来 → 原样返回；
/// - 重复任务已过期 → 按固定相位收敛到不晚于 `now` 的最近一个到期点（不逐条补）。
pub fn current_occurrence(item: &TodoItem, now: i64, zone: &dyn LocalZone) -> Option<i64> {
    let due = item.due_at?;
    if !item.repeat.is_repeating() || due > now {
        return Some(due);
    }
    Some(catch_up(item, due, now, zone))
}

/// 严格晚于 `prev` 的下一个到期点；一次性任务返回 `None`。
pub fn next_occurrence_after(item: &TodoItem, prev: i64, zone: &dyn LocalZone) -> Option<i64> {
    match item.repeat {
        RepeatRule::Once => None,
        RepeatRule::Interval => Some(prev + item.interval_minutes() as i64 * 60),
        // 日历类按本地日历推进（禁止 +86400），DST 由 `LocalZone` 兜底。
        RepeatRule::Daily => Some(zone.local_to_utc(next_local_day(item, prev, zone, false))),
        RepeatRule::Weekdays => Some(zone.local_to_utc(next_local_day(item, prev, zone, true))),
    }
}

/// 日历类重复的下一个本地时刻。
///
/// 优先用配置里的锚点（`local_date` / `local_minute`），这样即使某个到期点落在 DST
/// 跳过的时刻被顺延，后续周期的相位也不会跟着漂移。
fn next_local_day(
    item: &TodoItem,
    prev: i64,
    zone: &dyn LocalZone,
    weekdays_only: bool,
) -> LocalTime {
    let prev_local = zone.utc_to_local(prev);
    let (anchor_day, minute) = match item.local_date {
        Some(day) => (day, item.local_minute),
        None => (prev_local.day, prev_local.minute),
    };
    let mut day = anchor_day.max(prev_local.day) + 1;
    if weekdays_only {
        for _ in 0..7 {
            if (1..=5).contains(&LocalTime::new(day, minute).weekday()) {
                break;
            }
            day += 1;
        }
    }
    LocalTime::new(day, minute)
}

/// 该到期点的实际触发时刻（UTC）。
pub fn fire_at(item: &TodoItem, occurrence: i64, zone: &dyn LocalZone, daily_minute: u16) -> i64 {
    if item.all_day {
        // 只设日期：`due_at` 是当天 00:00 本地时间，提醒时刻取全局「每日提醒时刻」。
        let local = zone.utc_to_local(occurrence);
        zone.local_to_utc(LocalTime::new(local.day, daily_minute))
    } else {
        occurrence - item.lead_min as i64 * 60
    }
}

/// 当前需要触发提醒的实例。
///
/// `None` = 无需提醒：已完成、未到期、该到期点已提醒过、或仍处于 snooze 抑制窗口。
pub fn due_now(
    item: &TodoItem,
    now: i64,
    zone: &dyn LocalZone,
    daily_minute: u16,
) -> Option<Occurrence> {
    if item.done {
        return None;
    }
    if item.snooze_until.is_some_and(|until| now < until) {
        return None;
    }
    let at = current_occurrence(item, now, zone)?;
    if item.last_fired == Some(at) {
        return None;
    }
    let fire_at = fire_at(item, at, zone, daily_minute);
    (fire_at <= now).then_some(Occurrence { at, fire_at })
}

/// 记录一次提醒：写入去重锚点；重复任务按固定相位推进到 `now` 之后。
///
/// `occurrence` 必须来自 [`Occurrence::at`]，重复调用同一到期点时是幂等的。
pub fn mark_fired(item: &mut TodoItem, occurrence: i64, now: i64, zone: &dyn LocalZone) {
    if item.last_fired == Some(occurrence) {
        return;
    }
    item.last_fired = Some(occurrence);
    if !item.repeat.is_repeating() {
        return;
    }
    item.due_at = advance_after(item, occurrence, now, zone);
}

/// 把本地日期/时刻写回到期字段（供快捷录入与编辑复用）。
pub fn set_due_local(item: &mut TodoItem, local: LocalTime, zone: &dyn LocalZone) {
    item.due_at = Some(zone.local_to_utc(local));
    item.local_date = Some(local.day);
    item.local_minute = local.minute;
}

/// 解析手输的本地日期/时刻：`YYYY-MM-DD` + `HH:MM`；`all_day` 时忽略时刻。
///
/// 只接受真实存在的日期（2 月 31 日这类会被判非法），时刻范围 00:00-23:59。
pub fn parse_local_input(date: &str, time: &str, all_day: bool) -> Option<LocalTime> {
    let mut parts = date.trim().split('-');
    let year: i32 = parts.next()?.trim().parse().ok()?;
    let month: i32 = parts.next()?.trim().parse().ok()?;
    let day: i32 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some()
        || !(1900..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
    {
        return None;
    }
    let minute = if all_day {
        0
    } else {
        let (hour, minute) = time.trim().split_once(':')?;
        let hour: u16 = hour.trim().parse().ok()?;
        let minute: u16 = minute.trim().parse().ok()?;
        if hour > 23 || minute > 59 {
            return None;
        }
        hour * 60 + minute
    };
    // 拒绝 2024-02-31 这类「能被解析但不存在」的日期。
    if time::civil_from_days(time::days_from_civil(year, month, day)) != (year, month, day) {
        return None;
    }
    Some(LocalTime::new(
        time::days_from_civil(year, month, day),
        minute,
    ))
}

/// 单行手输的解析结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedDue {
    /// 解析出的本地时刻。
    pub local: LocalTime,
    /// 输入里是否写了明确时刻（没写时用默认的每日提醒时刻）。
    pub has_time: bool,
}

/// 走查单行输入后需要「已过则顺延」的粒度。
#[derive(Clone, Copy)]
enum Roll {
    None,
    Day,
    Week,
}

/// 解析单行手输的日期时间，容忍常见写法：
///
/// - `2026-11-20 14:30` / `2026/11/20 14:30` / `2026年11月20日 14:30`
/// - `11-20 14:30` / `11月20日`（缺年份取今年，已过则顺延到明年）
/// - `明天 09:00` / `后天` / `下周一 9点` / `周一`（只写日期时用 `default_minute`）
/// - `14:30` / `下午3点`（只有时刻时，今天已过则顺延到明天）
///
/// `Err` 里是可直接展示给用户的简短原因。
pub fn parse_due_text(
    text: &str,
    now: i64,
    zone: &dyn LocalZone,
    default_minute: u16,
) -> Result<ParsedDue, String> {
    let normalized = normalize_due_input(text);
    let input = normalized.trim();
    if input.is_empty() {
        return Err("请输入日期，如 11-20 14:30 或 明天 09:00".into());
    }
    let (date_part, has_time, minute) = match split_trailing_time(input) {
        Some((rest, minute)) => (rest, true, minute),
        None => (input, false, default_minute.min(1_439)),
    };
    let today = zone.utc_to_local(now);
    let (day, mut roll) = parse_date_part(date_part, today)?;
    if has_time && date_part.trim().is_empty() {
        // 「14:30」这类只有时刻的写法：今天已过就落到明天。
        roll = Roll::Day;
    }
    let mut local = LocalTime::new(day, minute);
    let passed = (local.day, local.minute) <= (today.day, today.minute);
    match roll {
        Roll::Day if passed => local = local.offset_minutes(1_440),
        Roll::Week if passed => local = local.offset_minutes(7 * 1_440),
        _ => {}
    }
    Ok(ParsedDue { local, has_time })
}

/// 全角标点/数字归一，省得用户切换输入法后格式对不上。
fn normalize_due_input(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '：' => ':',
            '／' => '/',
            '－' | '—' | '–' | '﹣' => '-',
            '．' => '.',
            '　' => ' ',
            '０'..='９' => char::from_digit(c as u32 - 0xFF10, 10).unwrap_or(c),
            _ => c,
        })
        .collect()
}

/// 从末尾切出时刻部分，剩下的就是日期部分。
fn split_trailing_time(s: &str) -> Option<(&str, u16)> {
    for (start, _) in s.char_indices() {
        let Some(minute) = parse_time_expr(&s[start..]) else {
            continue;
        };
        // 前一字符还是数字/分隔符时说明切在了日期中间（如 2026-11-20 的末尾），放弃。
        let prev = s[..start].chars().next_back();
        if prev
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, ':' | '.' | '-' | '/' | '点' | '时'))
        {
            continue;
        }
        let date_part = s[..start].trim_end();
        let date_part = date_part.trim_end_matches(['T', 't']);
        return Some((date_part.trim_end(), minute));
    }
    None
}

/// 12 小时制的时段前缀。
enum Period {
    Am,
    Noon,
    Pm,
}

fn split_period(s: &str) -> (Option<Period>, &str) {
    for (word, period) in [
        ("凌晨", Period::Am),
        ("早晨", Period::Am),
        ("早上", Period::Am),
        ("上午", Period::Am),
        ("中午", Period::Noon),
        ("下午", Period::Pm),
        ("傍晚", Period::Pm),
        ("晚上", Period::Pm),
        ("晚间", Period::Pm),
    ] {
        if let Some(rest) = s.strip_prefix(word) {
            return (Some(period), rest);
        }
    }
    (None, s)
}

/// 解析 `14:30` / `9点` / `9点30分` / `下午3点`；看不懂就返回 `None`，不猜。
fn parse_time_expr(token: &str) -> Option<u16> {
    let token = token.trim();
    let (period, rest) = split_period(token);
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let has_marker = rest.contains(':') || rest.contains('点') || rest.contains('时');
    // 没有时段前缀又没有任何标记时，纯数字可能只是日期的一部分（如 11-20 的 20），不当作时刻。
    if period.is_none() && !has_marker {
        return None;
    }
    let (hour_text, minute_text) = if let Some((h, m)) = rest.split_once(':') {
        (h, Some(m))
    } else if let Some(idx) = rest.find(['点', '时']) {
        let (h, m) = rest.split_at(idx);
        (h, Some(m.trim_start_matches(['点', '时'])))
    } else {
        (rest, None)
    };
    let hour_text = hour_text.trim();
    if hour_text.is_empty() || !hour_text.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let hour: u32 = hour_text.parse().ok()?;
    let minute: u32 = match minute_text {
        None => 0,
        Some(text) => {
            let text = text.trim().trim_end_matches('分').trim();
            if text.is_empty() {
                0
            } else if text.chars().all(|c| c.is_ascii_digit()) {
                text.parse().ok()?
            } else {
                return None;
            }
        }
    };
    if minute > 59 {
        return None;
    }
    let hour = match period {
        Some(Period::Am) if hour == 12 => 0,
        Some(Period::Noon) if hour < 11 => hour + 12,
        Some(Period::Pm) if hour < 12 => hour + 12,
        _ => hour,
    };
    if hour > 23 {
        return None;
    }
    Some((hour * 60 + minute) as u16)
}

fn parse_date_part(part: &str, today: LocalTime) -> Result<(i32, Roll), String> {
    let part = part.trim();
    if part.is_empty() {
        return Ok((today.day, Roll::None));
    }
    if let Some(offset) = day_word_offset(part) {
        return Ok((today.day + offset, Roll::None));
    }
    if let Some(found) = weekday_target(part, today) {
        return Ok(found);
    }
    parse_numeric_date(part, today)
}

/// `今天/明天/后天/大后天`。
fn day_word_offset(s: &str) -> Option<i32> {
    [
        ("今天", 0),
        ("今日", 0),
        ("明天", 1),
        ("明日", 1),
        ("后天", 2),
        ("大后天", 3),
    ]
    .iter()
    .find_map(|(word, offset)| (*word == s).then_some(*offset))
}

/// `周一` / `下周一` / `本周日` 这类写法。
fn weekday_target(s: &str, today: LocalTime) -> Option<(i32, Roll)> {
    let (next_week, rest) = if let Some(r) = s.strip_prefix("下个") {
        (Some(true), r)
    } else if let Some(r) = s.strip_prefix("下") {
        (Some(true), r)
    } else if let Some(r) = s.strip_prefix("这个") {
        (Some(false), r)
    } else if let Some(r) = s.strip_prefix("本") {
        (Some(false), r)
    } else if let Some(r) = s.strip_prefix("这") {
        (Some(false), r)
    } else {
        (None, s)
    };
    let rest = rest
        .strip_prefix("星期")
        .or_else(|| rest.strip_prefix("礼拜"))
        .or_else(|| rest.strip_prefix("周"))?;
    let mut chars = rest.chars();
    let target = match chars.next()? {
        '一' => 1,
        '二' => 2,
        '三' => 3,
        '四' => 4,
        '五' => 5,
        '六' => 6,
        '日' | '天' => 7,
        _ => return None,
    };
    if chars.next().is_some() {
        return None;
    }
    let today_index = (today.weekday() as i32 + 6) % 7 + 1; // 周一 = 1 … 周日 = 7
    let monday = today.day - (today_index - 1);
    let day = match next_week {
        Some(true) => monday + 7 + target - 1,
        Some(false) | None => {
            let mut day = monday + target - 1;
            if day < today.day {
                day += 7;
            }
            day
        }
    };
    // 不带「本周/下周」时可能落到今天，时刻已过就顺延一周。
    let roll = if next_week.is_none() {
        Roll::Week
    } else {
        Roll::None
    };
    Some((day, roll))
}

fn parse_numeric_date(part: &str, today: LocalTime) -> Result<(i32, Roll), String> {
    let cleaned: String = part
        .chars()
        .map(|c| match c {
            '年' | '月' => '-',
            '日' | '号' | ' ' => '-',
            _ => c,
        })
        .collect();
    let groups: Vec<&str> = cleaned
        .split(['-', '/', '.'])
        .filter(|g| !g.trim().is_empty())
        .collect();
    let number = |text: &str| -> Option<i32> {
        let text = text.trim();
        (!text.is_empty() && text.chars().all(|c| c.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    match groups.len() {
        3 => {
            let (Some(year), Some(month), Some(day)) =
                (number(groups[0]), number(groups[1]), number(groups[2]))
            else {
                return Err(unrecognized(part));
            };
            if !(1900..=9999).contains(&year) {
                return Err(format!("年份需在 1900-9999 之间（输入 {year}）"));
            }
            if !(1..=12).contains(&month) {
                return Err(format!("月份需在 1-12 之间（输入 {month}）"));
            }
            if !valid_date(year, month, day) {
                return Err(format!("{year} 年 {month} 月没有 {day} 日"));
            }
            Ok((time::days_from_civil(year, month, day), Roll::None))
        }
        2 => {
            let (Some(month), Some(day)) = (number(groups[0]), number(groups[1])) else {
                return Err(unrecognized(part));
            };
            if !(1..=12).contains(&month) {
                return Err(format!("月份需在 1-12 之间（输入 {month}）"));
            }
            let (mut year, _, _) = time::civil_from_days(today.day);
            if !valid_date(year, month, day) {
                return Err(format!("{month} 月没有 {day} 日"));
            }
            let mut days = time::days_from_civil(year, month, day);
            if days < today.day {
                year += 1;
                if !valid_date(year, month, day) {
                    return Err(format!("{year} 年 {month} 月没有 {day} 日"));
                }
                days = time::days_from_civil(year, month, day);
            }
            Ok((days, Roll::None))
        }
        1 => {
            let text = groups[0].trim();
            if text.len() == 8 && text.chars().all(|c| c.is_ascii_digit()) {
                let year: i32 = text[..4].parse().map_err(|_| unrecognized(part))?;
                let month: i32 = text[4..6].parse().map_err(|_| unrecognized(part))?;
                let day: i32 = text[6..].parse().map_err(|_| unrecognized(part))?;
                if !valid_date(year, month, day) {
                    return Err(format!("{year} 年 {month} 月没有 {day} 日"));
                }
                return Ok((time::days_from_civil(year, month, day), Roll::None));
            }
            let Some(day) = number(text) else {
                return Err(unrecognized(part));
            };
            if !(1..=31).contains(&day) {
                return Err(format!("日期需在 1-31 之间（输入 {day}）"));
            }
            let (year, month, today_day) = time::civil_from_days(today.day);
            let (mut year, mut month) = (year, month);
            if day < today_day || !valid_date(year, month, day) {
                month += 1;
                if month > 12 {
                    month = 1;
                    year += 1;
                }
            }
            if !valid_date(year, month, day) {
                return Err(format!("{year} 年 {month} 月没有 {day} 日"));
            }
            Ok((time::days_from_civil(year, month, day), Roll::None))
        }
        _ => Err(unrecognized(part)),
    }
}

fn valid_date(year: i32, month: i32, day: i32) -> bool {
    (1900..=9999).contains(&year)
        && (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && time::civil_from_days(time::days_from_civil(year, month, day)) == (year, month, day)
}

fn unrecognized(part: &str) -> String {
    format!("无法识别「{part}」，可试试 11-20 14:30 或 明天 09:00")
}

/// 下一个指定星期几（0 = 周日）的本地时刻；用于「下周一 09:00」这类快捷项。
pub fn next_weekday_local(now: i64, weekday: u8, minute: u16, zone: &dyn LocalZone) -> LocalTime {
    let today = zone.utc_to_local(now);
    let mut day = today.day + 1;
    for _ in 0..7 {
        if LocalTime::new(day, minute).weekday() == weekday {
            break;
        }
        day += 1;
    }
    LocalTime::new(day, minute)
}

/// 盒内展示用的到期时刻：普通任务就是到期点；「只设日期」换算到每日提醒时刻。
pub fn display_due(
    item: &TodoItem,
    now: i64,
    zone: &dyn LocalZone,
    daily_minute: u16,
) -> Option<i64> {
    let at = current_occurrence(item, now, zone)?;
    if !item.all_day {
        return Some(at);
    }
    let local = zone.utc_to_local(at);
    Some(zone.local_to_utc(LocalTime::new(local.day, daily_minute)))
}

/// 是否已到期（含 snooze 抑制窗口的判断），用于盒内红点与标题计数。
pub fn is_overdue(item: &TodoItem, now: i64, zone: &dyn LocalZone, daily_minute: u16) -> bool {
    if item.done || item.snooze_until.is_some_and(|until| now < until) {
        return false;
    }
    display_due(item, now, zone, daily_minute).is_some_and(|due| due <= now)
}

/// 盒内到期文案：`25 分钟后` / `已逾期 3 小时`。
pub fn relative_label(now: i64, target: i64) -> String {
    let diff = target - now;
    let abs = diff.abs();
    if abs < 60 {
        return if diff >= 0 {
            "即将到期".into()
        } else {
            "刚刚到期".into()
        };
    }
    let (value, unit) = if abs < 3_600 {
        (abs / 60, "分钟")
    } else if abs < 86_400 {
        (abs / 3_600, "小时")
    } else {
        (abs / 86_400, "天")
    };
    if diff > 0 {
        format!("{value} {unit}后")
    } else {
        format!("已逾期 {value} {unit}")
    }
}

/// 周期角标：`每天` / `工作日` / `每 2h` / `每 30m`；一次性任务无角标。
pub fn repeat_badge(item: &TodoItem) -> Option<String> {
    match item.repeat {
        RepeatRule::Once => None,
        RepeatRule::Daily => Some("每天".into()),
        RepeatRule::Weekdays => Some("工作日".into()),
        RepeatRule::Interval => {
            let minutes = item.interval_minutes();
            Some(if minutes.is_multiple_of(60) {
                format!("每 {}h", minutes / 60)
            } else {
                format!("每 {minutes}m")
            })
        }
    }
}

/// 从 `from` 起推进到严格晚于 `now` 的第一个到期点。
fn advance_after(item: &TodoItem, from: i64, now: i64, zone: &dyn LocalZone) -> Option<i64> {
    if item.repeat == RepeatRule::Interval {
        let period = item.interval_minutes() as i64 * 60;
        if item.interval_base == IntervalBase::CompletionTime {
            return Some(now + period);
        }
        let steps = (now - from).max(0) / period + 1;
        return Some(from + steps * period);
    }
    let mut cur = from;
    for _ in 0..MAX_CATCH_UP_STEPS {
        match next_occurrence_after(item, cur, zone) {
            Some(next) if next <= now => cur = next,
            other => return other,
        }
    }
    None
}

/// 从 `from` 起收敛到不晚于 `now` 的最近一个到期点。
fn catch_up(item: &TodoItem, from: i64, now: i64, zone: &dyn LocalZone) -> i64 {
    if item.repeat == RepeatRule::Interval {
        let period = item.interval_minutes() as i64 * 60;
        if from >= now {
            return from;
        }
        return from + (now - from) / period * period;
    }
    let mut cur = from;
    for _ in 0..MAX_CATCH_UP_STEPS {
        match next_occurrence_after(item, cur, zone) {
            Some(next) if next <= now => cur = next,
            _ => break,
        }
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::time::{civil_from_days, days_from_civil, LocalTime, LocalZone};
    use super::*;
    use crate::config::{IntervalBase, RepeatRule, TodoItem};

    /// 固定偏移时区（分钟），无 DST。
    struct FixedZone(i32);

    impl LocalZone for FixedZone {
        fn local_to_utc(&self, local: LocalTime) -> i64 {
            local.day as i64 * 86_400 + local.minute as i64 * 60 - self.0 as i64 * 60
        }

        fn utc_to_local(&self, utc: i64) -> LocalTime {
            let secs = utc + self.0 as i64 * 60;
            LocalTime {
                day: secs.div_euclid(86_400) as i32,
                minute: (secs.rem_euclid(86_400) / 60) as u16,
            }
        }
    }

    /// 伪造 DST 时区：平时 +01:00，夏季 +02:00。
    struct DstZone {
        start: i64,
        end: i64,
    }

    impl LocalZone for DstZone {
        fn local_to_utc(&self, local: LocalTime) -> i64 {
            let wall = local.day as i64 * 86_400 + local.minute as i64 * 60;
            let mut best: Option<i64> = None;
            for off in [3_600i64, 7_200] {
                let ts = wall - off;
                if self.utc_to_local(ts) == local {
                    best = Some(best.map_or(ts, |b| b.min(ts)));
                }
            }
            // 跳过时刻：按跳变前的偏移解释，效果是顺延到跳变之后。
            best.unwrap_or(wall - 3_600)
        }

        fn utc_to_local(&self, utc: i64) -> LocalTime {
            let off = if utc >= self.start && utc < self.end {
                7_200
            } else {
                3_600
            };
            let secs = utc + off;
            LocalTime {
                day: secs.div_euclid(86_400) as i32,
                minute: (secs.rem_euclid(86_400) / 60) as u16,
            }
        }
    }

    fn dst_zone() -> DstZone {
        DstZone {
            start: days_from_civil(2024, 3, 31) as i64 * 86_400 + 3_600,
            end: days_from_civil(2024, 10, 27) as i64 * 86_400 + 3_600,
        }
    }

    fn at(zone: &dyn LocalZone, y: i32, m: i32, d: i32, minute: u16) -> i64 {
        zone.local_to_utc(LocalTime::new(days_from_civil(y, m, d), minute))
    }

    fn item_at(due: i64, repeat: RepeatRule) -> TodoItem {
        TodoItem {
            id: "t".into(),
            text: "测试".into(),
            due_at: Some(due),
            repeat,
            ..TodoItem::default()
        }
    }

    #[test]
    fn civil_days_roundtrip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2024, 1, 1), 19_723);
        assert_eq!(civil_from_days(19_813), (2024, 3, 31));
        for days in [-1, 0, 19_723, 19_813, 20_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "day {days}");
        }
    }

    #[test]
    fn interval_keeps_fixed_phase_after_late_fire() {
        let zone = FixedZone(480);
        let t0 = at(&zone, 2024, 6, 1, 9 * 60);
        let mut item = item_at(t0, RepeatRule::Interval);
        item.interval_min = 30;
        let now = t0 + 50 * 60;
        assert_eq!(current_occurrence(&item, now, &zone), Some(t0 + 30 * 60));
        mark_fired(&mut item, t0 + 30 * 60, now, &zone);
        assert_eq!(item.last_fired, Some(t0 + 30 * 60));
        // 按到期时刻固定相位：下一次仍是整点 10:00，而不是 10:50 + 30 分钟。
        assert_eq!(item.due_at, Some(t0 + 60 * 60));
    }

    #[test]
    fn interval_below_minimum_converges_to_five_minutes() {
        let zone = FixedZone(480);
        let mut item = item_at(1_000, RepeatRule::Interval);
        item.interval_min = 1;
        assert_eq!(item.interval_minutes(), 5);
        assert_eq!(
            next_occurrence_after(&item, 1_000, &zone),
            Some(1_000 + 300)
        );
    }

    #[test]
    fn interval_catches_up_to_latest_slot_after_long_sleep() {
        let zone = FixedZone(480);
        let t0 = at(&zone, 2024, 6, 1, 9 * 60);
        let mut item = item_at(t0, RepeatRule::Interval);
        item.interval_min = 5;
        let now = t0 + 365 * 86_400 + 137;
        let occ = current_occurrence(&item, now, &zone).unwrap();
        assert_eq!((occ - t0) % 300, 0);
        assert!(occ <= now && now - occ < 300);
        mark_fired(&mut item, occ, now, &zone);
        let next = item.due_at.unwrap();
        assert!(next > now);
        assert_eq!((next - t0) % 300, 0);
    }

    #[test]
    fn interval_completion_base_restarts_from_now() {
        let zone = FixedZone(480);
        let t0 = at(&zone, 2024, 6, 1, 9 * 60);
        let mut item = item_at(t0, RepeatRule::Interval);
        item.interval_min = 120;
        item.interval_base = IntervalBase::CompletionTime;
        let now = t0 + 40 * 60;
        mark_fired(&mut item, t0, now, &zone);
        assert_eq!(item.due_at, Some(now + 120 * 60));
    }

    #[test]
    fn daily_advances_one_local_day() {
        let zone = FixedZone(480);
        let t0 = at(&zone, 2024, 6, 1, 9 * 60);
        let mut item = item_at(t0, RepeatRule::Daily);
        item.local_date = Some(days_from_civil(2024, 6, 1));
        item.local_minute = 9 * 60;
        assert_eq!(
            next_occurrence_after(&item, t0, &zone),
            Some(at(&zone, 2024, 6, 2, 9 * 60))
        );
    }

    #[test]
    fn weekdays_skip_weekend() {
        let zone = FixedZone(480);
        // 2024-06-07 是周五，2024-06-08 是周六。
        let friday = at(&zone, 2024, 6, 7, 9 * 60);
        let mut item = item_at(friday, RepeatRule::Weekdays);
        item.local_date = Some(days_from_civil(2024, 6, 7));
        item.local_minute = 9 * 60;
        assert_eq!(
            next_occurrence_after(&item, friday, &zone),
            Some(at(&zone, 2024, 6, 10, 9 * 60))
        );
        let saturday = at(&zone, 2024, 6, 8, 9 * 60);
        item.local_date = Some(days_from_civil(2024, 6, 8));
        assert_eq!(
            next_occurrence_after(&item, saturday, &zone),
            Some(at(&zone, 2024, 6, 10, 9 * 60))
        );
    }

    #[test]
    fn dst_spring_gap_shifts_forward_without_drifting() {
        let zone = dst_zone();
        let due = at(&zone, 2024, 3, 30, 150);
        let mut item = item_at(due, RepeatRule::Daily);
        item.local_date = Some(days_from_civil(2024, 3, 30));
        item.local_minute = 150;

        let gap = next_occurrence_after(&item, due, &zone).unwrap();
        let local = zone.utc_to_local(gap);
        assert_eq!(local.day, days_from_civil(2024, 3, 31));
        // 02:30 当天不存在，顺延到跳变之后的有效时刻。
        assert!(local.minute >= 180, "跳过的时刻应被顺延，得到 {local:?}");

        // 次日恢复到 02:30，不被前一天的顺延带偏。
        let next = next_occurrence_after(&item, gap, &zone).unwrap();
        assert_eq!(
            zone.utc_to_local(next),
            LocalTime::new(days_from_civil(2024, 4, 1), 150)
        );
    }

    #[test]
    fn dst_fall_duplicate_takes_first_occurrence_once() {
        let zone = dst_zone();
        let due = at(&zone, 2024, 10, 26, 150);
        let mut item = item_at(due, RepeatRule::Daily);
        item.local_date = Some(days_from_civil(2024, 10, 26));
        item.local_minute = 150;

        let ambiguous = next_occurrence_after(&item, due, &zone).unwrap();
        let local = zone.utc_to_local(ambiguous);
        assert_eq!(local, LocalTime::new(days_from_civil(2024, 10, 27), 150));
        // 第一个（夏令时）实例：UTC 00:30；第二个标准时实例是 UTC 01:30。
        assert_eq!(
            ambiguous,
            days_from_civil(2024, 10, 27) as i64 * 86_400 + 1_800
        );

        // 同一本地时刻不会在次日再补一次。
        let next = next_occurrence_after(&item, ambiguous, &zone).unwrap();
        assert_eq!(
            zone.utc_to_local(next),
            LocalTime::new(days_from_civil(2024, 10, 28), 150)
        );
    }

    #[test]
    fn snooze_suppresses_only_current_window() {
        let zone = FixedZone(480);
        let due = at(&zone, 2024, 6, 1, 10 * 60);
        let mut item = item_at(due, RepeatRule::Once);
        let now = due + 60;
        assert!(due_now(&item, now, &zone, 540).is_some());
        item.snooze_until = Some(now + 600);
        assert!(due_now(&item, now, &zone, 540).is_none());
        assert!(due_now(&item, now + 600, &zone, 540).is_some());
        // snooze 不改变到期点。
        assert_eq!(item.due_at, Some(due));
    }

    #[test]
    fn single_shot_fires_once() {
        let zone = FixedZone(480);
        let due = at(&zone, 2024, 6, 1, 10 * 60);
        let mut item = item_at(due, RepeatRule::Once);
        let now = due + 30;
        mark_fired(&mut item, due, now, &zone);
        assert_eq!(item.due_at, Some(due));
        assert_eq!(item.last_fired, Some(due));
        assert!(due_now(&item, now + 600, &zone, 540).is_none());
    }

    #[test]
    fn completed_item_never_reminds() {
        let zone = FixedZone(480);
        let due = at(&zone, 2024, 6, 1, 10 * 60);
        let mut item = item_at(due, RepeatRule::Daily);
        item.done = true;
        assert!(due_now(&item, due + 60, &zone, 540).is_none());
    }

    #[test]
    fn lead_min_fires_early() {
        let zone = FixedZone(480);
        let due = at(&zone, 2024, 6, 1, 10 * 60);
        let mut item = item_at(due, RepeatRule::Once);
        item.lead_min = 15;
        assert!(due_now(&item, due - 16 * 60, &zone, 540).is_none());
        assert!(due_now(&item, due - 15 * 60, &zone, 540).is_some());
    }

    #[test]
    fn all_day_uses_daily_reminder_minute() {
        let zone = FixedZone(480);
        let midnight = at(&zone, 2024, 6, 2, 0);
        let mut item = item_at(midnight, RepeatRule::Once);
        item.all_day = true;
        item.local_date = Some(days_from_civil(2024, 6, 2));
        // 09:00 之前不提醒，09:00 开始提醒。
        assert!(due_now(&item, at(&zone, 2024, 6, 2, 8 * 60), &zone, 540).is_none());
        assert!(due_now(&item, at(&zone, 2024, 6, 2, 9 * 60), &zone, 540).is_some());
        assert_eq!(
            fire_at(&item, midnight, &zone, 540),
            at(&zone, 2024, 6, 2, 9 * 60)
        );
    }

    #[test]
    fn parse_local_input_validates_date_and_time() {
        assert_eq!(
            parse_local_input("2024-06-01", "18:30", false),
            Some(LocalTime::new(days_from_civil(2024, 6, 1), 18 * 60 + 30))
        );
        assert_eq!(
            parse_local_input(" 2024-06-01 ", "09:00", true),
            Some(LocalTime::new(days_from_civil(2024, 6, 1), 0))
        );
        // 非法：不存在的日期、越界时刻、格式错误、负数。
        assert_eq!(parse_local_input("2024-02-31", "09:00", false), None);
        assert_eq!(parse_local_input("2024-13-01", "09:00", false), None);
        assert_eq!(parse_local_input("2024-06-01", "24:00", false), None);
        assert_eq!(parse_local_input("2024-06-01", "9", false), None);
        assert_eq!(parse_local_input("2024/06/01", "09:00", false), None);
        assert_eq!(parse_local_input("-2024-06-01", "09:00", false), None);
        assert_eq!(parse_local_input("2024-06-01", "-1:00", false), None);
    }

    #[test]
    fn parse_due_text_accepts_common_shapes() {
        let zone = FixedZone(480);
        // 2026-11-11 是周三，本地 15:20。
        let now = at(&zone, 2026, 11, 11, 15 * 60 + 20);
        let expect = |text: &str, y: i32, m: i32, d: i32, minute: u16, has_time: bool| {
            let got = parse_due_text(text, now, &zone, 540)
                .unwrap_or_else(|e| panic!("{text} 应能解析：{e}"));
            assert_eq!(
                got.local,
                LocalTime::new(days_from_civil(y, m, d), minute),
                "{text}"
            );
            assert_eq!(got.has_time, has_time, "{text}");
        };
        expect("2026-11-20 14:30", 2026, 11, 20, 14 * 60 + 30, true);
        expect("2026/11/20 14:30", 2026, 11, 20, 14 * 60 + 30, true);
        expect("2026年11月20日 14:30", 2026, 11, 20, 14 * 60 + 30, true);
        expect("2026-11-20T14:30", 2026, 11, 20, 14 * 60 + 30, true);
        expect("11-20 14:30", 2026, 11, 20, 14 * 60 + 30, true);
        expect("11月20日", 2026, 11, 20, 540, false);
        expect("明天 09:00", 2026, 11, 12, 540, true);
        expect("明天", 2026, 11, 12, 540, false);
        expect("后天", 2026, 11, 13, 540, false);
        expect("下周一 9点", 2026, 11, 16, 540, true);
        expect("周一 9点", 2026, 11, 16, 540, true);
        // 只有时刻：今天已过 → 明天。
        expect("14:30", 2026, 11, 12, 14 * 60 + 30, true);
        expect("09:00", 2026, 11, 12, 540, true);
        expect("下午3点", 2026, 11, 12, 15 * 60, true);
        // 只写日号：本月还没到就用本月，已过就落到下个月。
        expect("20", 2026, 11, 20, 540, false);
        expect("5", 2026, 12, 5, 540, false);
        // 全角标点与数字同样接受。
        expect("１１－２０ １４：３０", 2026, 11, 20, 14 * 60 + 30, true);
    }

    #[test]
    fn parse_due_text_rolls_month_day_and_reports_reason() {
        let zone = FixedZone(480);
        let now = at(&zone, 2026, 11, 11, 15 * 60 + 20);
        // 缺年份且已过 → 顺延到明年。
        let got = parse_due_text("01-05 09:00", now, &zone, 540).expect("应能解析");
        assert_eq!(got.local, LocalTime::new(days_from_civil(2027, 1, 5), 540));
        // 非法输入给出可展示的原因，而不是静默丢弃。
        let err = parse_due_text("2026-02-31 09:00", now, &zone, 540).unwrap_err();
        assert!(err.contains("2 月"), "{err}");
        assert!(parse_due_text("25:00", now, &zone, 540).is_err());
        assert!(parse_due_text("下周九 9点", now, &zone, 540).is_err());
        assert!(parse_due_text("   ", now, &zone, 540).is_err());
    }

    #[test]
    fn next_weekday_local_targets_next_week() {
        let zone = FixedZone(480);
        // 2024-06-07 周五 → 下周一 06-10。
        let now = at(&zone, 2024, 6, 7, 10 * 60);
        assert_eq!(
            next_weekday_local(now, 1, 540, &zone),
            LocalTime::new(days_from_civil(2024, 6, 10), 540)
        );
        // 周日 → 次日即周一。
        let sunday = at(&zone, 2024, 6, 9, 10 * 60);
        assert_eq!(
            next_weekday_local(sunday, 1, 540, &zone),
            LocalTime::new(days_from_civil(2024, 6, 10), 540)
        );
    }

    #[test]
    fn snooze_after_fire_rearms_the_same_reminder() {
        let zone = FixedZone(480);
        let due = at(&zone, 2024, 6, 1, 10 * 60);
        let mut item = item_at(due, RepeatRule::Once);
        mark_fired(&mut item, due, due + 30, &zone);
        assert!(due_now(&item, due + 60, &zone, 540).is_none());
        // 稍后提醒：清去重标记 + 设抑制窗口，到期点不变。
        item.last_fired = None;
        item.snooze_until = Some(due + 600);
        assert!(due_now(&item, due + 60, &zone, 540).is_none());
        assert!(due_now(&item, due + 600, &zone, 540).is_some());
        assert_eq!(item.due_at, Some(due));
    }

    #[test]
    fn overdue_respects_done_and_snooze() {
        let zone = FixedZone(480);
        let due = at(&zone, 2024, 6, 1, 10 * 60);
        let mut item = item_at(due, RepeatRule::Once);
        assert!(is_overdue(&item, due + 60, &zone, 540));
        item.snooze_until = Some(due + 600);
        assert!(!is_overdue(&item, due + 60, &zone, 540));
        assert!(is_overdue(&item, due + 600, &zone, 540));
        item.snooze_until = None;
        item.done = true;
        assert!(!is_overdue(&item, due + 60, &zone, 540));
    }

    #[test]
    fn all_day_overdue_waits_for_daily_minute() {
        let zone = FixedZone(480);
        let midnight = at(&zone, 2024, 6, 2, 0);
        let mut item = item_at(midnight, RepeatRule::Once);
        item.all_day = true;
        assert!(!is_overdue(
            &item,
            at(&zone, 2024, 6, 2, 8 * 60),
            &zone,
            540
        ));
        assert!(is_overdue(&item, at(&zone, 2024, 6, 2, 9 * 60), &zone, 540));
        assert_eq!(
            display_due(&item, at(&zone, 2024, 6, 2, 9 * 60), &zone, 540),
            Some(at(&zone, 2024, 6, 2, 9 * 60))
        );
    }

    #[test]
    fn relative_label_buckets() {
        let now = 1_000_000;
        assert_eq!(relative_label(now, now + 30), "即将到期");
        assert_eq!(relative_label(now, now - 30), "刚刚到期");
        assert_eq!(relative_label(now, now + 25 * 60), "25 分钟后");
        assert_eq!(relative_label(now, now + 2 * 3_600), "2 小时后");
        assert_eq!(relative_label(now, now + 3 * 86_400), "3 天后");
        assert_eq!(relative_label(now, now - 90 * 60), "已逾期 1 小时");
        assert_eq!(relative_label(now, now - 3 * 86_400), "已逾期 3 天");
    }

    #[test]
    fn repeat_badges() {
        let mut item = TodoItem::default();
        assert_eq!(repeat_badge(&item), None);
        item.repeat = RepeatRule::Daily;
        assert_eq!(repeat_badge(&item).as_deref(), Some("每天"));
        item.repeat = RepeatRule::Weekdays;
        assert_eq!(repeat_badge(&item).as_deref(), Some("工作日"));
        item.repeat = RepeatRule::Interval;
        item.interval_min = 120;
        assert_eq!(repeat_badge(&item).as_deref(), Some("每 2h"));
        item.interval_min = 30;
        assert_eq!(repeat_badge(&item).as_deref(), Some("每 30m"));
    }

    #[test]
    fn set_due_local_keeps_utc_and_anchor_in_sync() {
        let zone = FixedZone(480);
        let mut item = TodoItem::default();
        let local = LocalTime::new(days_from_civil(2024, 7, 1), 18 * 60);
        set_due_local(&mut item, local, &zone);
        assert_eq!(item.due_at, Some(zone.local_to_utc(local)));
        assert_eq!(item.local_date, Some(days_from_civil(2024, 7, 1)));
        assert_eq!(item.local_minute, 18 * 60);
        assert_eq!(zone.utc_to_local(item.due_at.unwrap()), local);
    }
}
