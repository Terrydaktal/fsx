use chrono::{DateTime, Datelike, Local};

pub fn format_time_display(timestamp: i64, now_year: i32, now_timestamp: i64) -> String {
    let datetime: DateTime<Local> = DateTime::from_timestamp(timestamp, 0)
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap())
        .with_timezone(&Local);
    if now_year == datetime.year() && (now_timestamp - datetime.timestamp()).abs() < 15_552_000 {
        datetime.format("%e %b %H:%M").to_string()
    } else {
        datetime.format("%e %b  %Y").to_string()
    }
}

pub fn format_size_compact(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut unit = 0usize;
    let mut value = bytes as f64;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{}B", bytes)
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

pub fn format_size_compact_3(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut unit = 0usize;
    let mut value = bytes as f64;
    while (value >= 1024.0 || (unit == 0 && value > 99_999.0)) && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    for decimals in (0usize..=3).rev() {
        let factor = 10f64.powi(decimals as i32);
        let truncated = (value * factor).floor() / factor;
        let candidate = format!("{truncated:.*}{}", decimals, UNITS[unit]);
        if candidate.len() <= 6 {
            return candidate;
        }
    }
    if unit < UNITS.len() - 1 {
        return format_size_compact_3(bytes / 1024);
    }
    "99999T".to_string()
}

pub fn format_size_iec(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0usize;
    let mut value = bytes as f64;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

pub fn format_count(value: u64) -> String {
    let raw = value.to_string();
    let mut output = String::with_capacity(raw.len() + raw.len() / 3);
    for (index, byte) in raw.bytes().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            output.push(',');
        }
        output.push(byte as char);
    }
    output.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    #[test]
    fn display_time_uses_ls_recent_and_old_layouts() {
        let now = Local.with_ymd_and_hms(2026, 8, 9, 12, 0, 0).unwrap();
        let recent = now - Duration::days(1);
        let old = now - Duration::days(200);

        assert_eq!(
            format_time_display(recent.timestamp(), now.year(), now.timestamp()),
            recent.format("%e %b %H:%M").to_string()
        );
        assert_eq!(
            format_time_display(old.timestamp(), now.year(), now.timestamp()),
            old.format("%e %b  %Y").to_string()
        );
    }
}
