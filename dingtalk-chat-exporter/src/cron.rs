// 最小 cron 引擎（自研，无第三方依赖）
//
// 支持 5 段标准语法：分 时 日 月 周
// 每段支持：`*`、`*/N`、`a`、`a-b`、`a,b,c`、`a-b/N` 及其组合
// 周字段：0-6（0=周日），也接受 SUN-SAT 别名；兼容 7=周日
// 所有时间均按北京时间语义解释（与项目其余部分一致）。
//
// 日/周字段的组合语义采用 Vixie cron 规则：
// 两者都受限（非 `*`）时取并集，任一为 `*` 时以另一个为准。

use crate::date::{parse_dws_datetime, ymd_to_epoch_days};

const WEEKDAY_NAMES: [(&str, u32); 7] = [
    ("SUN", 0),
    ("MON", 1),
    ("TUE", 2),
    ("WED", 3),
    ("THU", 4),
    ("FRI", 5),
    ("SAT", 6),
];

/// 字段取值位集合（用 u64/u128 足够覆盖 0-59 范围）
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FieldSet {
    bits: u128,
    /// 该字段是否为 `*`（用于日/周并集语义判断）
    wildcard: bool,
}

impl FieldSet {
    fn contains(&self, value: u32) -> bool {
        self.bits & (1u128 << value) != 0
    }

    /// >= from 的最小匹配值（同周期内）
    fn next_from(&self, from: u32) -> Option<u32> {
        let shifted = self.bits >> from;
        if shifted == 0 {
            return None;
        }
        Some(from + shifted.trailing_zeros())
    }

    fn first(&self) -> Option<u32> {
        if self.bits == 0 {
            None
        } else {
            Some(self.bits.trailing_zeros())
        }
    }
}

fn parse_field(
    field: &str,
    min: u32,
    max: u32,
    allow_weekday_names: bool,
) -> Result<FieldSet, String> {
    let mut bits: u128 = 0;
    let mut wildcard = false;
    if field.trim().is_empty() {
        return Err("cron 字段不能为空".into());
    }
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("cron 字段包含空项: {field}"));
        }
        let (range_part, step_part) = match part.split_once('/') {
            Some((range, step)) => (range, Some(step)),
            None => (part, None),
        };
        let step: u32 = match step_part {
            Some(step) => {
                let value: u32 = step.parse().map_err(|_| format!("cron 步长无效: {part}"))?;
                if value == 0 {
                    return Err(format!("cron 步长不能为 0: {part}"));
                }
                value
            }
            None => 1,
        };
        let (start, end) = if range_part == "*" {
            wildcard = true;
            (min, max)
        } else if let Some((low, high)) = range_part.split_once('-') {
            (
                parse_value(low, min, max, allow_weekday_names)?,
                parse_value(high, min, max, allow_weekday_names)?,
            )
        } else {
            let value = parse_value(range_part, min, max, allow_weekday_names)?;
            if step_part.is_some() {
                // "a/N" 语义：从 a 开始到字段上限，每 N 取一个
                (value, max)
            } else {
                (value, value)
            }
        };
        if start > end {
            return Err(format!("cron 范围起点大于终点: {part}"));
        }
        let mut value = start;
        while value <= end {
            bits |= 1u128 << value;
            value += step;
        }
    }
    if bits == 0 {
        return Err(format!("cron 字段无有效取值: {field}"));
    }
    Ok(FieldSet { bits, wildcard })
}

fn parse_value(text: &str, min: u32, max: u32, allow_weekday_names: bool) -> Result<u32, String> {
    let text = text.trim();
    if allow_weekday_names {
        let upper = text.to_ascii_uppercase();
        if let Some((_, value)) = WEEKDAY_NAMES.iter().find(|(name, _)| *name == upper) {
            return Ok(*value);
        }
    }
    let value: u32 = text
        .parse()
        .map_err(|_| format!("cron 字段取值无效: {text}"))?;
    // 周字段兼容 7=周日
    let value = if allow_weekday_names && max == 6 && value == 7 {
        0
    } else {
        value
    };
    if value < min || value > max {
        return Err(format!("cron 字段取值越界: {text}（允许 {min}-{max}）"));
    }
    Ok(value)
}

/// 已解析的 cron 表达式
#[derive(Clone, Debug)]
pub struct Cron {
    pub minutes: FieldSet,
    pub hours: FieldSet,
    pub days: FieldSet,
    pub months: FieldSet,
    pub weekdays: FieldSet,
    /// 原始表达式（展示用）
    source: String,
}

impl Cron {
    /// 解析 5 段 cron 表达式
    pub fn parse(expression: &str) -> Result<Cron, String> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron 表达式必须为 5 段（分 时 日 月 周），实际 {} 段: {expression}",
                fields.len()
            ));
        }
        Ok(Cron {
            minutes: parse_field(fields[0], 0, 59, false)
                .map_err(|e| format!("{e}（分钟字段）"))?,
            hours: parse_field(fields[1], 0, 23, false).map_err(|e| format!("{e}（小时字段）"))?,
            days: parse_field(fields[2], 1, 31, false).map_err(|e| format!("{e}（日字段）"))?,
            months: parse_field(fields[3], 1, 12, false).map_err(|e| format!("{e}（月字段）"))?,
            weekdays: parse_field(fields[4], 0, 6, true).map_err(|e| format!("{e}（周字段）"))?,
            source: expression.trim().to_string(),
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// 日字段是否匹配（Vixie cron 日/周并集语义）
    fn day_matches(&self, year: i32, month: u32, day: u32) -> bool {
        let epoch_day = ymd_to_epoch_days(year, month, day);
        // 1970-01-01 是周四(4)；epoch_day 可为负，rem_euclid 保证 0-6
        let weekday = ((epoch_day + 4).rem_euclid(7)) as u32;
        let day_limited = !self.days.wildcard;
        let week_limited = !self.weekdays.wildcard;
        match (day_limited, week_limited) {
            (true, true) => self.days.contains(day) || self.weekdays.contains(weekday),
            (true, false) => self.days.contains(day),
            (false, true) => self.weekdays.contains(weekday),
            (false, false) => true,
        }
    }

    fn days_in_month(year: i32, month: u32) -> u32 {
        match month {
            4 | 6 | 9 | 11 => 30,
            2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
            2 => 28,
            _ => 31,
        }
    }

    /// 计算严格晚于 `after`（"yyyy-MM-dd HH:mm:ss"，北京时间）的下一个触发时刻。
    /// 秒恒为 00。四年内无解返回 None（如 2 月 30 日）。
    pub fn next_after(&self, after: &str) -> Option<String> {
        let (mut year, mut month, day, hour, minute, _second) = parse_dws_datetime(after)?;
        // 从 after 的下一分钟开始搜索
        let mut minute_cursor = minute + 1;
        let mut hour_cursor = hour;
        if minute_cursor > 59 {
            minute_cursor = 0;
            hour_cursor += 1;
        }
        let mut day_cursor = day;
        if hour_cursor > 23 {
            hour_cursor = 0;
            day_cursor += 1;
        }

        let limit_year = year + 4;
        while year <= limit_year {
            if !self.months.contains(month) {
                // 前进到下个月 1 日 00:00
                month += 1;
                if month > 12 {
                    month = 1;
                    year += 1;
                }
                day_cursor = 1;
                hour_cursor = 0;
                minute_cursor = 0;
                continue;
            }
            let month_days = Self::days_in_month(year, month);
            if day_cursor > month_days {
                month += 1;
                if month > 12 {
                    month = 1;
                    year += 1;
                }
                day_cursor = 1;
                hour_cursor = 0;
                minute_cursor = 0;
                continue;
            }
            if !self.day_matches(year, month, day_cursor) {
                day_cursor += 1;
                hour_cursor = 0;
                minute_cursor = 0;
                continue;
            }
            let Some(matched_hour) = self.hours.next_from(hour_cursor) else {
                day_cursor += 1;
                hour_cursor = 0;
                minute_cursor = 0;
                continue;
            };
            if matched_hour != hour_cursor {
                minute_cursor = 0;
            }
            hour_cursor = matched_hour;
            let Some(matched_minute) = self.minutes.next_from(minute_cursor) else {
                hour_cursor += 1;
                minute_cursor = 0;
                continue;
            };
            return Some(format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:00",
                year, month, day_cursor, hour_cursor, matched_minute
            ));
        }
        None
    }

    /// 从 after 开始的接下来 n 次触发时间（用于界面预览）
    pub fn upcoming(&self, after: &str, count: usize) -> Vec<String> {
        let mut result = Vec::with_capacity(count);
        let mut cursor = after.to_string();
        for _ in 0..count {
            match self.next_after(&cursor) {
                Some(next) => {
                    cursor = next.clone();
                    result.push(next);
                }
                None => break,
            }
        }
        result
    }
}

/// 把 "HH:mm" 拆成 (hour, minute)，非法返回 None
pub fn parse_hhmm(value: &str) -> Option<(u32, u32)> {
    let (hour, minute) = value.split_once(':')?;
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some((hour, minute))
}

/// 计算某日期是星期几（0=周日），供界面描述使用
pub fn weekday_of(datetime: &str) -> Option<u32> {
    let (year, month, day, _, _, _) = parse_dws_datetime(datetime)?;
    let epoch_day = ymd_to_epoch_days(year, month, day);
    Some(((epoch_day + 4).rem_euclid(7)) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_invalid_expressions() {
        assert!(Cron::parse("* * * *").is_err()); // 4 段
        assert!(Cron::parse("* * * * * *").is_err()); // 6 段
        assert!(Cron::parse("60 * * * *").is_err()); // 分钟越界
        assert!(Cron::parse("* 24 * * *").is_err()); // 小时越界
        assert!(Cron::parse("* * 0 * *").is_err()); // 日最小 1
        assert!(Cron::parse("* * * 13 *").is_err()); // 月越界
        assert!(Cron::parse("* * * * 8").is_err()); // 周越界
        assert!(Cron::parse("*/0 * * * *").is_err()); // 步长 0
        assert!(Cron::parse("5-1 * * * *").is_err()); // 范围反向
        assert!(Cron::parse("abc * * * *").is_err());
        assert!(Cron::parse("").is_err());
    }

    #[test]
    fn parse_accepts_valid_expressions() {
        assert!(Cron::parse("*/5 * * * *").is_ok());
        assert!(Cron::parse("0 */2 * * *").is_ok());
        assert!(Cron::parse("30 2 * * *").is_ok());
        assert!(Cron::parse("0 9 1,15 * *").is_ok());
        assert!(Cron::parse("0 9 * * MON,WED,FRI").is_ok());
        assert!(Cron::parse("0 9 * * 1-5").is_ok());
        assert!(Cron::parse("0 0 1 1 *").is_ok());
        assert!(Cron::parse("10-20/5 * * * *").is_ok());
        assert!(Cron::parse("* * * * 7").is_ok()); // 7=周日
    }

    #[test]
    fn every_minute_advances_one_minute() {
        let cron = Cron::parse("* * * * *").unwrap();
        assert_eq!(
            cron.next_after("2026-09-09 15:47:37"),
            Some("2026-09-09 15:48:00".into())
        );
    }

    #[test]
    fn daily_at_time_rolls_over_to_next_day() {
        let cron = Cron::parse("30 2 * * *").unwrap();
        assert_eq!(
            cron.next_after("2026-09-09 01:00:00"),
            Some("2026-09-09 02:30:00".into())
        );
        assert_eq!(
            cron.next_after("2026-09-09 02:30:00"),
            Some("2026-09-10 02:30:00".into())
        );
        assert_eq!(
            cron.next_after("2026-09-09 23:59:59"),
            Some("2026-09-10 02:30:00".into())
        );
    }

    #[test]
    fn hourly_step_crosses_day_boundary() {
        let cron = Cron::parse("0 */6 * * *").unwrap();
        assert_eq!(
            cron.next_after("2026-09-09 19:00:00"),
            Some("2026-09-10 00:00:00".into())
        );
    }

    #[test]
    fn minute_step_works() {
        let cron = Cron::parse("*/15 * * * *").unwrap();
        assert_eq!(
            cron.next_after("2026-09-09 15:47:00"),
            Some("2026-09-09 16:00:00".into())
        );
        assert_eq!(
            cron.next_after("2026-09-09 15:45:00"),
            Some("2026-09-09 16:00:00".into())
        );
    }

    #[test]
    fn weekly_matches_weekday() {
        // 2026-09-09 是周三
        let cron = Cron::parse("0 9 * * MON").unwrap();
        assert_eq!(
            cron.next_after("2026-09-09 10:00:00"),
            Some("2026-09-14 09:00:00".into())
        );
        let cron = Cron::parse("0 9 * * WED").unwrap();
        assert_eq!(
            cron.next_after("2026-09-09 08:00:00"),
            Some("2026-09-09 09:00:00".into())
        );
        assert_eq!(
            cron.next_after("2026-09-09 09:00:00"),
            Some("2026-09-16 09:00:00".into())
        );
    }

    #[test]
    fn monthly_matches_day_and_handles_short_months() {
        let cron = Cron::parse("0 8 31 * *").unwrap();
        // 9 月只有 30 天 → 跳到 10-31
        assert_eq!(
            cron.next_after("2026-09-09 00:00:00"),
            Some("2026-10-31 08:00:00".into())
        );
    }

    #[test]
    fn leap_day_only_matches_leap_years() {
        let cron = Cron::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            cron.next_after("2026-01-01 00:00:00"),
            Some("2028-02-29 00:00:00".into())
        );
    }

    #[test]
    fn impossible_date_returns_none() {
        let cron = Cron::parse("0 0 30 2 *").unwrap(); // 2 月 30 日
        assert_eq!(cron.next_after("2026-01-01 00:00:00"), None);
    }

    #[test]
    fn day_and_week_restriction_union_semantics() {
        // Vixie cron：日=1 或 周一，两者并集
        let cron = Cron::parse("0 0 1 * MON").unwrap();
        // 2026-09-02(周三) 之后 → 09-07(周一)
        assert_eq!(
            cron.next_after("2026-09-02 00:00:00"),
            Some("2026-09-07 00:00:00".into())
        );
        // 09-07(周一) 之后 → 09-14(周一)
        assert_eq!(
            cron.next_after("2026-09-07 00:00:00"),
            Some("2026-09-14 00:00:00".into())
        );
        // 09-28(周一) 之后 → 10-01(周四, 日=1)
        assert_eq!(
            cron.next_after("2026-09-28 00:00:00"),
            Some("2026-10-01 00:00:00".into())
        );
    }

    #[test]
    fn upcoming_returns_requested_count() {
        let cron = Cron::parse("0 9 * * *").unwrap();
        let runs = cron.upcoming("2026-09-09 10:00:00", 3);
        assert_eq!(
            runs,
            vec![
                "2026-09-10 09:00:00",
                "2026-09-11 09:00:00",
                "2026-09-12 09:00:00"
            ]
        );
    }

    #[test]
    fn year_boundary_crossing() {
        let cron = Cron::parse("0 0 1 1 *").unwrap();
        assert_eq!(
            cron.next_after("2026-12-31 23:59:59"),
            Some("2027-01-01 00:00:00".into())
        );
    }

    #[test]
    fn weekday_of_known_dates() {
        assert_eq!(weekday_of("1970-01-01 00:00:00"), Some(4)); // 周四
        assert_eq!(weekday_of("2026-09-09 00:00:00"), Some(3)); // 周三
        assert_eq!(weekday_of("2026-09-13 00:00:00"), Some(0)); // 周日
    }

    #[test]
    fn parse_hhmm_validates_range() {
        assert_eq!(parse_hhmm("02:30"), Some((2, 30)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("bad"), None);
    }
}
