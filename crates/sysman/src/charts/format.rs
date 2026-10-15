const BYTE_UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Percent,
    /// A 0–100 score shown without a sign, as on the pressure axis.
    Index,
    Bytes,
    BytesPerSec,
    Celsius,
    Watts,
}

impl Unit {
    pub fn format(self, v: f64) -> String {
        match self {
            Self::Percent => format!("{v:.0}%"),
            Self::Index => format!("{v:.0}"),
            Self::Bytes => bytes(v),
            Self::BytesPerSec => format!("{}/s", bytes(v)),
            Self::Celsius => format!("{v:.1}°C"),
            Self::Watts if v.abs() >= 10. => format!("{v:.0} W"),
            Self::Watts => format!("{v:.1} W"),
        }
    }

    /// Ticks are chosen in this many base units, so byte axes step in whole MB or GB.
    pub fn tick_scale(self, max: f64) -> f64 {
        match self {
            Self::Bytes | Self::BytesPerSec => byte_scale(max),
            _ => 1.,
        }
    }
}

/// 1024-based, labelled as the reference does: `0 bytes`, `95.4 MB`.
pub fn bytes(v: f64) -> String {
    if v.abs() < 1023.95 {
        return format!("{v:.0} bytes");
    }
    let mut x = v / 1024.;
    let mut unit = 0;
    while x.abs() >= 1023.95 && unit < BYTE_UNITS.len() - 1 {
        x /= 1024.;
        unit += 1;
    }
    format!("{x:.1} {}", BYTE_UNITS[unit])
}

fn byte_scale(max: f64) -> f64 {
    let mut scale = 1.;
    while max.abs() / scale >= 1024. && scale < 1024f64.powi(5) {
        scale *= 1024.;
    }
    scale
}

/// `13h 12m`, `2d 4h`, `45m`.
pub fn duration(seconds: u64) -> String {
    let (d, h, m) = (seconds / 86400, seconds / 3600 % 24, seconds / 60 % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_read_like_the_reference() {
        assert_eq!(bytes(0.), "0 bytes");
        assert_eq!(bytes(512.), "512 bytes");
        assert_eq!(bytes(1536.), "1.5 KB");
        assert_eq!(bytes(95.4 * 1024. * 1024.), "95.4 MB");
        assert_eq!(bytes(1023.99 * 1024.), "1.0 MB", "never 1024.0 KB");
        assert_eq!(bytes(125.5 * 1024f64.powi(3)), "125.5 GB");
    }

    #[test]
    fn units_carry_their_suffix() {
        assert_eq!(Unit::Percent.format(84.6), "85%");
        assert_eq!(Unit::Index.format(34.), "34");
        assert_eq!(Unit::BytesPerSec.format(164. * 1024.), "164.0 KB/s");
        assert_eq!(Unit::BytesPerSec.format(0.), "0 bytes/s");
        assert_eq!(Unit::Celsius.format(63.84), "63.8°C");
        assert_eq!(Unit::Watts.format(33.66), "34 W");
        assert_eq!(Unit::Watts.format(3.66), "3.7 W", "small iGPU draws keep a decimal");
    }

    #[test]
    fn byte_axes_tick_in_the_unit_of_their_maximum() {
        assert_eq!(Unit::Bytes.tick_scale(500.), 1.);
        assert_eq!(Unit::Bytes.tick_scale(18. * 1024f64.powi(3)), 1024f64.powi(3));
        assert_eq!(Unit::Percent.tick_scale(1e9), 1.);
    }

    #[test]
    fn durations_keep_two_units() {
        assert_eq!(duration(13 * 3600 + 12 * 60 + 5), "13h 12m");
        assert_eq!(duration(2 * 86400 + 4 * 3600), "2d 4h");
        assert_eq!(duration(59), "0m");
    }
}
