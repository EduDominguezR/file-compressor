//! Formateo legible de tamaños, velocidades y duraciones.

use std::time::Duration;

/// 1.23 GB, 456.7 MB, 12.0 KB, 512 B (base 1024).
pub fn human_size(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K {
        format!("{bytes} B")
    } else if b < K * K {
        format!("{:.1} KB", b / K)
    } else if b < K * K * K {
        format!("{:.1} MB", b / (K * K))
    } else if b < K * K * K * K {
        format!("{:.2} GB", b / (K * K * K))
    } else {
        format!("{:.2} TB", b / (K * K * K * K))
    }
}

pub fn human_rate(bytes_per_sec: f64) -> String {
    format!("{}/s", human_size(bytes_per_sec.max(0.0) as u64))
}

pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h} h {m:02} min {sec:02} s")
    } else if m > 0 {
        format!("{m} min {sec:02} s")
    } else {
        format!("{sec} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size((456.7 * 1024.0 * 1024.0) as u64), "456.7 MB");
        assert_eq!(
            human_size((1.23 * 1024.0 * 1024.0 * 1024.0) as u64),
            "1.23 GB"
        );
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(Duration::from_secs(5)), "5 s");
        assert_eq!(human_duration(Duration::from_secs(125)), "2 min 05 s");
        assert_eq!(human_duration(Duration::from_secs(3725)), "1 h 02 min 05 s");
    }
}
