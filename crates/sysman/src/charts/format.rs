use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

use crate::data::prefs::prefs;

const BYTE_UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];

/// The unit every temperature is shown in; readings stay in °C everywhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TempUnit {
    #[default]
    Celsius,
    Fahrenheit,
}

/// 0 until read from the preferences, then 1 + the unit's index.
static TEMP_UNIT: AtomicU8 = AtomicU8::new(0);

impl TempUnit {
    pub const ALL: [TempUnit; 2] = [Self::Celsius, Self::Fahrenheit];
    const KEY: &str = "display.temperature";

    pub fn label(self) -> &'static str {
        match self {
            Self::Celsius => "°C",
            Self::Fahrenheit => "°F",
        }
    }

    /// The chosen unit, read from the preferences once.
    pub fn current() -> Self {
        match TEMP_UNIT.load(Ordering::Relaxed) {
            1 => Self::Celsius,
            2 => Self::Fahrenheit,
            // tests format in °C and never read the user's preferences
            _ if cfg!(test) => Self::Celsius,
            _ => {
                let unit: Self = prefs().lock().ok().and_then(|p| p.get(Self::KEY)).unwrap_or_default();
                unit.remember();
                unit
            }
        }
    }

    /// Makes `self` the unit shown from the next redraw on, and stores it.
    pub fn choose(self) {
        self.remember();
        if let Ok(mut p) = prefs().lock() {
            p.set(Self::KEY, &self);
        }
    }

    /// Forgets the cached unit, after the preferences file was changed by hand.
    pub fn reload() {
        TEMP_UNIT.store(0, Ordering::Relaxed);
    }

    fn remember(self) {
        TEMP_UNIT.store(self as u8 + 1, Ordering::Relaxed);
    }

    pub fn convert(self, celsius: f64) -> f64 {
        match self {
            Self::Celsius => celsius,
            Self::Fahrenheit => celsius * 9. / 5. + 32.,
        }
    }

    pub fn format(self, celsius: f64, decimals: usize) -> String {
        format!("{:.decimals$}{}", self.convert(celsius), self.label())
    }
}

/// A °C reading in the chosen unit: `63.8°C` or `146.8°F`.
pub fn temperature(celsius: f64, decimals: usize) -> String {
    TempUnit::current().format(celsius, decimals)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Percent,
    /// A 0–100 score shown without a sign, as on the pressure axis.
    Index,
    Bytes,
    BytesPerSec,
    Celsius,
    Millis,
    /// Operations, requests: whole numbers, one decimal below 10.
    Count,
    Watts,
}

impl Unit {
    pub fn format(self, v: f64) -> String {
        match self {
            Self::Percent => format!("{v:.0}%"),
            Self::Index => format!("{v:.0}"),
            Self::Bytes => bytes(v),
            Self::BytesPerSec => format!("{}/s", bytes(v)),
            Self::Celsius => temperature(v, 1),
            Self::Millis => format!("{v:.1} ms"),
            Self::Count if v.abs() < 10. && v.fract() != 0. => format!("{v:.1}"),
            Self::Count => format!("{v:.0}"),
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

/// 1024-based, labelled `0 bytes`, `95.4 MB`.
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
    fn bytes_use_1024_units_with_word_labels() {
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
        assert_eq!(Unit::Millis.format(0.44), "0.4 ms");
        assert_eq!(Unit::Count.format(0.4), "0.4", "a queue-depth axis keeps its fractions");
        assert_eq!(Unit::Count.format(8245.4), "8245");
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
    fn fahrenheit_converts_the_reading_not_just_the_label() {
        let f = TempUnit::Fahrenheit;
        assert_eq!(f.format(100., 0), "212°F");
        assert_eq!(f.format(0., 1), "32.0°F");
        assert_eq!(f.format(-40., 0), "-40°F", "the scales cross at -40");
        assert_eq!(f.format(63.84, 1), "146.9°F");
        assert_eq!(TempUnit::Celsius.format(63.84, 1), "63.8°C");
    }

    #[test]
    fn the_temperature_unit_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        crate::data::prefs::Prefs::load(path.clone()).set(TempUnit::KEY, &TempUnit::Fahrenheit);
        let p = crate::data::prefs::Prefs::load(path);
        assert_eq!(p.get::<TempUnit>(TempUnit::KEY), Some(TempUnit::Fahrenheit));
    }

    #[test]
    fn durations_keep_two_units() {
        assert_eq!(duration(13 * 3600 + 12 * 60 + 5), "13h 12m");
        assert_eq!(duration(2 * 86400 + 4 * 3600), "2d 4h");
        assert_eq!(duration(59), "0m");
    }
}
