//! 本地墙上时间 ↔ UTC 换算。
//!
//! 时区规则（含 DST）一律交给系统 API，不在本 crate 里维护规则表；逻辑层只依赖
//! [`LocalZone`]，因此单测可以注入固定偏移或伪造 DST 的时区来覆盖边界。

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{
    FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime,
    TzSpecificLocalTimeToSystemTime,
};

/// 本地墙上时间：自 1970-01-01 起的天数 + 当天分钟（0-1439）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalTime {
    pub day: i32,
    pub minute: u16,
}

impl LocalTime {
    pub fn new(day: i32, minute: u16) -> Self {
        Self { day, minute }
    }

    /// 平移若干分钟后得到的本地时间（分钟可跨天）。
    pub fn offset_minutes(self, minutes: i64) -> Self {
        let total = self.day as i64 * 1440 + self.minute as i64 + minutes;
        Self {
            day: total.div_euclid(1440) as i32,
            minute: total.rem_euclid(1440) as u16,
        }
    }

    /// 星期几：0 = 周日 … 6 = 周六。
    pub fn weekday(self) -> u8 {
        (self.day + 4).rem_euclid(7) as u8
    }
}

/// 本地时间换算接口。
pub trait LocalZone {
    fn local_to_utc(&self, local: LocalTime) -> i64;
    fn utc_to_local(&self, utc: i64) -> LocalTime;
}

/// 公历日期 → 自 1970-01-01 起的天数（Howard Hinnant 算法）。
pub fn days_from_civil(year: i32, month: i32, day: i32) -> i32 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 自 1970-01-01 起的天数 → 公历 (年, 月, 日)。
pub fn civil_from_days(days: i32) -> (i32, i32, i32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 1601-01-01 到 1970-01-01 的秒数。
const FILETIME_UNIX_EPOCH_OFFSET_SECS: i64 = 11_644_473_600;

fn filetime_to_secs(ft: FILETIME) -> i64 {
    let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    (ticks / 10_000_000) as i64 - FILETIME_UNIX_EPOCH_OFFSET_SECS
}

fn secs_to_filetime(secs: i64) -> FILETIME {
    let ticks = (secs + FILETIME_UNIX_EPOCH_OFFSET_SECS) * 10_000_000;
    FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    }
}

fn local_to_system(local: LocalTime) -> SYSTEMTIME {
    let (year, month, day) = civil_from_days(local.day);
    SYSTEMTIME {
        wYear: year as u16,
        wMonth: month as u16,
        wDayOfWeek: 0,
        wDay: day as u16,
        wHour: local.minute / 60,
        wMinute: local.minute % 60,
        wSecond: 0,
        wMilliseconds: 0,
    }
}

fn system_to_local(st: SYSTEMTIME) -> LocalTime {
    LocalTime {
        day: days_from_civil(st.wYear as i32, st.wMonth as i32, st.wDay as i32),
        minute: st.wHour * 60 + st.wMinute,
    }
}

fn fallback_utc(local: LocalTime) -> i64 {
    local.day as i64 * 86_400 + local.minute as i64 * 60
}

/// 系统当前时区。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemZone;

impl LocalZone for SystemZone {
    fn local_to_utc(&self, local: LocalTime) -> i64 {
        let wall = local_to_system(local);
        let mut utc = SYSTEMTIME::default();
        let ts = unsafe {
            if TzSpecificLocalTimeToSystemTime(None, &wall, &mut utc).is_err() {
                fallback_utc(local)
            } else {
                let mut ft = FILETIME::default();
                if SystemTimeToFileTime(&utc, &mut ft).is_err() {
                    fallback_utc(local)
                } else {
                    filetime_to_secs(ft)
                }
            }
        };
        // 秋季 DST 结束时同一本地时刻会出现两次：系统按标准时（较晚）解释，
        // 这里回算更早的候选；若仍落在同一本地时刻，取更早的一次。
        for back in [7_200i64, 3_600, 1_800] {
            let earlier = ts - back;
            if self.utc_to_local(earlier) == local {
                return earlier;
            }
        }
        ts
    }

    fn utc_to_local(&self, utc: i64) -> LocalTime {
        let ft = secs_to_filetime(utc);
        let mut utc_st = SYSTEMTIME::default();
        let mut local_st = SYSTEMTIME::default();
        unsafe {
            if FileTimeToSystemTime(&ft, &mut utc_st).is_err()
                || SystemTimeToTzSpecificLocalTime(None, &utc_st, &mut local_st).is_err()
            {
                return LocalTime {
                    day: utc.div_euclid(86_400) as i32,
                    minute: utc.rem_euclid(86_400) as u16 / 60,
                };
            }
        }
        system_to_local(local_st)
    }
}
