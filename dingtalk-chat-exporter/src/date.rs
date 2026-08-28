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

pub(crate) fn normalize_datetime(value: &str) -> Option<String> {
    let value = value.trim();
    if value.len() >= 19 {
        let mut candidate = value.get(..19)?.to_string();
        if candidate.as_bytes().get(10) == Some(&b'T') {
            candidate.replace_range(10..11, " ");
        }
        if parse_dws_datetime(&candidate).is_some() {
            return Some(candidate);
        }
    }
    if value.len() == 10 {
        let candidate = format!("{value} 00:00:00");
        if parse_dws_datetime(&candidate).is_some() {
            return Some(candidate);
        }
    }
    None
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
        2 => 28,
        _ => 31,
    }
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
    fn normalizes_group_creation_dates() {
        assert_eq!(
            normalize_datetime("2026-07-26T12:34:56+08:00").as_deref(),
            Some("2026-07-26 12:34:56")
        );
        assert_eq!(
            normalize_datetime("2026-07-26").as_deref(),
            Some("2026-07-26 00:00:00")
        );
        assert!(normalize_datetime("2026-02-29").is_none());
        assert!(normalize_datetime("not-a-date").is_none());
    }
}
