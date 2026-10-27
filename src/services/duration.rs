use chrono::{DateTime, Utc};
use regex::Regex;
use std::sync::LazyLock;

pub const MAX_MUTE_MS: i64 = 30 * 24 * 60 * 60 * 1000;
static PART: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s*([dhms])").unwrap());

fn unit_ms(unit: &str) -> i64 {
    match unit {
        "d" => 86_400_000,
        "h" => 3_600_000,
        "m" => 60_000,
        _ => 1_000,
    }
}

/// Parses `1d 4h 30m`-style durations into milliseconds, capped at 30 days.
pub fn parse_duration(input: &str) -> Option<i64> {
    let normalized = input.trim().to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    let parts: Vec<_> = PART.captures_iter(&normalized).collect();
    let joined: String = parts.iter().map(|c| c[0].to_string()).collect::<String>();
    let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    if parts.is_empty() || strip(&joined) != strip(&normalized) {
        return None;
    }
    let mut total: i64 = 0;
    for part in parts {
        let value: i64 = part[1].parse().ok()?;
        total = total.checked_add(value.checked_mul(unit_ms(&part[2]))?)?;
        if total > MAX_MUTE_MS {
            return None;
        }
    }
    (total > 0).then_some(total)
}

pub fn human_duration(mut ms: i64) -> String {
    let mut parts = Vec::new();
    for (name, divisor) in [("day", 86_400_000), ("hour", 3_600_000), ("minute", 60_000), ("second", 1_000)] {
        let n = ms / divisor;
        ms %= divisor;
        if n != 0 {
            parts.push(format!("{n} {name}{}", if n == 1 { "" } else { "s" }));
        }
    }
    parts.truncate(3);
    if parts.is_empty() { "0 seconds".into() } else { parts.join(" ") }
}

pub fn discord_timestamp_seconds(date: DateTime<Utc>) -> i64 {
    date.timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats() {
        assert_eq!(parse_duration("1d 4h 30m"), Some(86_400_000 + 4 * 3_600_000 + 30 * 60_000));
        assert_eq!(parse_duration("10S"), Some(10_000));
        assert_eq!(parse_duration("31d"), None);
        assert_eq!(parse_duration("5x"), None);
        assert_eq!(parse_duration("1h foo"), None);
        assert_eq!(parse_duration("0m"), None);
        assert_eq!(human_duration(90_061_000), "1 day 1 hour 1 minute");
        assert_eq!(human_duration(0), "0 seconds");
    }
}
