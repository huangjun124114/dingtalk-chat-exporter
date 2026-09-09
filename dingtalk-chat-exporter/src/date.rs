/// 中国标准时间偏移（UTC+8），与 dws::current_time_str() 保持一致
pub(crate) const CHINA_STANDARD_TIME_OFFSET_SECS: u64 = 8 * 3600;

pub(crate) fn parse_dws_datetime(value: &str) -> Option<(i32, u32, u32, u32, u32, u32)> {
    let bytes = value.as_bytes();
    if bytes.len() != 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b' '
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| !matches!(index, 4 | 7 | 10 | 13 | 16) && !byte.is_ascii_digit())
    {
        return None;
    }

    let parsed = (
        value.get(0..4)?.parse().ok()?,
        value.get(5..7)?.parse().ok()?,
        value.get(8..10)?.parse().ok()?,
        value.get(11..13)?.parse().ok()?,
        value.get(14..16)?.parse().ok()?,
        value.get(17..19)?.parse().ok()?,
    );
    if !(1..=12).contains(&parsed.1)
        || parsed.2 == 0
        || parsed.2 > days_in_month(parsed.0, parsed.1)
        || parsed.3 > 23
        || parsed.4 > 59
        || parsed.5 > 59
    {
        return None;
    }
    Some(parsed)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
        2 => 28,
        _ => 31,
    }
}

/// 从日期时间字符串中提取年月,用于按月分文件。
/// 输入 "2026-01-15 10:30:00" 返回 Some("202601")。
pub(crate) fn extract_year_month(datetime: &str) -> Option<String> {
    let (year, month, _day, _hour, _minute, _second) = parse_dws_datetime(datetime)?;
    Some(format!("{year:04}{month:02}"))
}

pub(crate) fn epoch_days_to_ymd(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year as i32, month as u32, day as u32)
}

/// epoch_days_to_ymd 的逆运算：公历日期 → Unix 纪元天数。
pub(crate) fn ymd_to_epoch_days(year: i32, month: u32, day: u32) -> i64 {
    let y = if month <= 2 {
        i64::from(year) - 1
    } else {
        i64::from(year)
    };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let year_of_era = y - era * 400;
    let m = i64::from(month);
    let day_of_year = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// 校验 "yyyy-MM-dd HH:mm:ss" 字符串是否合法（cron / 调度时间校验用）。
pub(crate) fn is_valid_datetime(value: &str) -> bool {
    parse_dws_datetime(value).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dws_datetime_requires_the_documented_format() {
        assert!(parse_dws_datetime("2026-07-26 12:34:56").is_some());
        assert!(parse_dws_datetime("2026-07-26T12:34:56").is_none());
        assert!(parse_dws_datetime("2026-02-29 12:34:56").is_none());
        assert!(parse_dws_datetime("2024-02-29 12:34:56").is_some());
    }

    #[test]
    fn ymd_epoch_days_round_trip() {
        // 已知锚点：1970-01-01 = 0，2000-03-01 = 11017
        assert_eq!(ymd_to_epoch_days(1970, 1, 1), 0);
        assert_eq!(ymd_to_epoch_days(2000, 3, 1), 11_017);
        // 双向互逆（覆盖闰年与世纪边界）
        for days in [-719_162_i64, -1, 0, 11_017, 18_262, 20_454, 20_819] {
            let (year, month, day) = epoch_days_to_ymd(days);
            assert_eq!(ymd_to_epoch_days(year, month, day), days, "days={days}");
        }
    }

    #[test]
    fn is_valid_datetime_rejects_bad_input() {
        assert!(is_valid_datetime("2026-09-09 15:47:00"));
        assert!(!is_valid_datetime("bad"));
        assert!(!is_valid_datetime("2026-13-09 15:47:00"));
    }
}
