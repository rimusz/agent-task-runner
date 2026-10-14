use anyhow::{bail, Context, Result};
use std::time::Duration;

/// Parse a compact duration such as `30s`, `10m`, `90m`, `4h`.
pub fn parse_duration(raw: &str) -> Result<Duration> {
    let s = raw.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    if let Some(num) = s.strip_suffix("ms") {
        let n: u64 = num
            .trim()
            .parse()
            .with_context(|| format!("duration {raw}"))?;
        return Ok(Duration::from_millis(n));
    }
    let (num, mult) = match s.as_bytes().last() {
        Some(b's') => (&s[..s.len() - 1], 1u64),
        Some(b'm') => (&s[..s.len() - 1], 60),
        Some(b'h') => (&s[..s.len() - 1], 3600),
        Some(b'd') => (&s[..s.len() - 1], 86400),
        _ => bail!("duration {raw:?} must end in s, m, h, or d"),
    };
    let n: u64 = num
        .trim()
        .parse()
        .with_context(|| format!("duration {raw}"))?;
    Ok(Duration::from_secs(n.saturating_mul(mult)))
}

/// Parse a size such as `64`, `5k`, `5m`, `1g` (base 1024).
pub fn parse_bytes(raw: &str) -> Result<u64> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        bail!("empty size");
    }
    let (num, mult) = if let Some(n) = s.strip_suffix("kb") {
        (n, 1024u64)
    } else if let Some(n) = s.strip_suffix("mb") {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("gb") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('k') {
        (n, 1024)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('g') {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('b') {
        (n, 1)
    } else {
        (s.as_str(), 1)
    };
    let n: u64 = num.trim().parse().with_context(|| format!("size {raw}"))?;
    Ok(n.saturating_mul(mult))
}

pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs % 86400 == 0 && secs >= 86400 {
        format!("{}d", secs / 86400)
    } else if secs % 3600 == 0 && secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs % 60 == 0 && secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("90m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("4h").unwrap(), Duration::from_secs(4 * 3600));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
    }

    #[test]
    fn parses_byte_sizes() {
        assert_eq!(parse_bytes("64").unwrap(), 64);
        assert_eq!(parse_bytes("5k").unwrap(), 5 * 1024);
        assert_eq!(parse_bytes("5m").unwrap(), 5 * 1024 * 1024);
        assert_eq!(parse_bytes("1gb").unwrap(), 1024 * 1024 * 1024);
    }
}
