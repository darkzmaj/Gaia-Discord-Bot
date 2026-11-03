use chrono::{DateTime, LocalResult, NaiveDate, TimeZone, Utc};
use regex::Regex;
use std::sync::LazyLock;

static INPUT: LazyLock<Regex> = LazyLock::new(|| {
    // ASCII digits only: `\d` is Unicode-aware and would accept digits `str::parse` rejects.
    Regex::new(r"^(?:([0-9]{4})-([0-9]{2})-([0-9]{2})|([0-9]{2})\.([0-9]{2})\.([0-9]{4}))\s+([0-9]{2}):([0-9]{2})$").unwrap()
});

#[derive(Debug)]
pub struct Schedule {
    pub utc: DateTime<Utc>,
    pub display: String,
}

/// en-GB `dateStyle: long, timeStyle: short`, e.g. `27 October 2026 at 14:05`.
pub fn long_display(date: DateTime<Utc>) -> String {
    date.with_timezone(&crate::config::timezone()).format("%-d %B %Y at %H:%M").to_string()
}

pub fn parse_schedule(input: &str, now: DateTime<Utc>) -> Result<Schedule, String> {
    let tz = crate::config::timezone();
    let format = format!("Use `YYYY-MM-DD HH:mm` or `DD.MM.YYYY HH:mm` in {}.", tz.name());
    let c = INPUT.captures(input.trim()).ok_or_else(|| format.clone())?;
    let num = |a: usize, b: usize| -> Result<u32, String> {
        c.get(a).or(c.get(b)).and_then(|m| m.as_str().parse().ok()).ok_or_else(|| format.clone())
    };
    let (year, month, day) = (num(1, 6)? as i32, num(2, 5)?, num(3, 4)?);
    let (hour, minute) = (num(7, 7)?, num(8, 8)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return Err("The date or time is invalid.".into());
    }
    let wall = NaiveDate::from_ymd_opt(year, month, day)
        .ok_or("The date is invalid.")?
        .and_hms_opt(hour, minute, 0)
        .unwrap();
    let utc = match tz.from_local_datetime(&wall) {
        LocalResult::None => return Err("This local time does not exist because of the DST change.".into()),
        LocalResult::Ambiguous(..) => {
            return Err("This local time is ambiguous because of the DST change; choose another time.".into())
        }
        LocalResult::Single(t) => t.with_timezone(&Utc),
    };
    if utc <= now {
        return Err("Scheduled time must be in the future.".into());
    }
    if (utc - now).num_milliseconds() > 366 * 86_400_000 {
        return Err("Scheduled time must be within one year.".into());
    }
    Ok(Schedule { utc, display: long_display(utc) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_formats_and_dst_gaps() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let a = parse_schedule("2026-07-01 12:00", now).unwrap();
        assert_eq!(a.utc, Utc.with_ymd_and_hms(2026, 7, 1, 10, 0, 0).unwrap());
        assert_eq!(parse_schedule("01.07.2026 12:00", now).unwrap().utc, a.utc);
        assert!(parse_schedule("2026-03-29 02:30", now).unwrap_err().contains("does not exist"));
        assert!(parse_schedule("2026-10-25 02:30", now).unwrap_err().contains("ambiguous"));
        assert_eq!(parse_schedule("2026-02-30 10:00", now).unwrap_err(), "The date is invalid.");
        assert!(parse_schedule("2025-07-01 12:00", now).unwrap_err().contains("future"));
    }

    #[test]
    fn non_ascii_digits_are_a_format_error_not_a_panic() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let err = parse_schedule("٢٠٢٦-٠٧-٠١ ١٢:٠٠", now).unwrap_err();
        assert!(err.starts_with("Use `YYYY-MM-DD HH:mm`"), "{err}");
    }
}
