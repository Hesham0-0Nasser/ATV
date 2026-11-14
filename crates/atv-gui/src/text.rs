//! Turning numbers into the short, plain text the screens show.

/// `1234567` becomes `1,234,567`.
pub fn group_digits(number: u64) -> String {
    let digits = number.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// `1 file`, `2 files`, `1 entry`, `2 entries`.
pub fn plural(count: u64, noun: &str) -> String {
    let word = if count == 1 {
        noun.to_string()
    } else if let Some(stem) = noun.strip_suffix('y') {
        format!("{stem}ies")
    } else {
        format!("{noun}s")
    };
    format!("{} {word}", group_digits(count))
}

/// A size with a unit a person would use: `812 B`, `7.80 MB`, `1.07 GB`.
pub fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    match unit {
        0 => format!("{count} B"),
        _ if value >= 100.0 => format!("{value:.0} {}", UNITS[unit]),
        _ if value >= 10.0 => format!("{value:.1} {}", UNITS[unit]),
        _ => format!("{value:.2} {}", UNITS[unit]),
    }
}

/// A clock reading: `0:12`, `2:08`, `1:02:05`. Under a second shows tenths.
pub fn clock(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "—".to_string();
    }
    if seconds < 1.0 {
        return format!("{seconds:.1} s");
    }
    let total = seconds.round() as u64;
    let (hours, minutes, secs) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

/// A rough length in words, for estimates: `under a second`, `about 12 s`,
/// `about 2 min`, `about 1 h 5 min`.
pub fn about(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "—".to_string();
    }
    if seconds < 1.0 {
        return "under a second".to_string();
    }
    let total = seconds.round() as u64;
    if total < 60 {
        format!("about {total} s")
    } else if total < 3600 {
        format!("about {} min", (total + 30) / 60)
    } else {
        let minutes = (total + 30) / 60;
        format!("about {} h {} min", minutes / 60, minutes % 60)
    }
}

/// Bytes per second as `7.80 MB/s`.
pub fn speed(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return "—".to_string();
    }
    format!("{}/s", bytes(bytes_per_second as u64))
}

/// A whole percentage: `0%` … `100%`.
pub fn percent(fraction: f32) -> String {
    format!("{:.0}%", (fraction.clamp(0.0, 1.0) * 100.0).floor())
}

/// The last few lines of a long message, for small print under an error.
pub fn tail_lines(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_words() {
        assert_eq!(group_digits(1_234_567), "1,234,567");
        assert_eq!(group_digits(999), "999");
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(1200, "file"), "1,200 files");
        assert_eq!(plural(2, "entry"), "2 entries");
    }

    #[test]
    fn sizes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_500), "1.50 KB");
        assert_eq!(bytes(31_100_000), "31.1 MB");
        assert_eq!(bytes(1_073_741_824), "1.07 GB");
        assert_eq!(speed(7_800_000.0), "7.80 MB/s");
        assert_eq!(speed(0.0), "—");
    }

    #[test]
    fn times() {
        assert_eq!(clock(0.1), "0.1 s");
        assert_eq!(clock(128.0), "2:08");
        assert_eq!(clock(3725.0), "1:02:05");
        assert_eq!(clock(f64::NAN), "—");
        assert_eq!(about(0.5), "under a second");
        assert_eq!(about(12.4), "about 12 s");
        assert_eq!(about(138.0), "about 2 min");
        assert_eq!(about(3900.0), "about 1 h 5 min");
    }

    #[test]
    fn percentages_never_round_up_to_done() {
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(0.996), "99%");
        assert_eq!(percent(1.0), "100%");
        assert_eq!(percent(1.7), "100%");
    }

    #[test]
    fn tails() {
        assert_eq!(tail_lines("a\n\nb\nc\nd\n", 2), "c\nd");
        assert_eq!(tail_lines("one", 3), "one");
    }
}
