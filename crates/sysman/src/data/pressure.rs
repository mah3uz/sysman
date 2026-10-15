/// `pressure.memory.some.avg10` (%) to index, piecewise linear between these knots.
const KNOTS: [(f64, f64); 4] = [(0., 0.), (5., 34.), (20., 67.), (50., 100.)];
pub const WARNING: f64 = 34.;
pub const CRITICAL: f64 = 67.;
const SWAP_IN_FLOOR_BPS: f64 = 1024. * 1024.;
/// The kernel reports swap-in in pages; every architecture we run on uses 4 KiB pages.
pub const PAGE_BYTES: f64 = 4096.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Normal,
    Warning,
    Critical,
}

/// Memory pressure index 0–100: PSI stall share, raised to Warning while the system is swapping in.
pub fn index(some_avg10: f64, swap_in_bps: f64) -> f64 {
    let pct = some_avg10.max(0.);
    let mut value = 100.;
    for w in KNOTS.windows(2) {
        let ((x0, y0), (x1, y1)) = (w[0], w[1]);
        if pct <= x1 {
            value = y0 + (pct - x0) / (x1 - x0) * (y1 - y0);
            break;
        }
    }
    if swap_in_bps > SWAP_IN_FLOOR_BPS {
        value = value.max(WARNING);
    }
    value
}

pub fn band(index: f64) -> Band {
    if index >= CRITICAL {
        Band::Critical
    } else if index >= WARNING {
        Band::Warning
    } else {
        Band::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knots_map_to_the_band_edges() {
        assert_eq!(index(0., 0.), 0.);
        assert_eq!(index(5., 0.), 34., "5% stalled is where Warning starts");
        assert_eq!(index(20., 0.), 67., "20% stalled is where Critical starts");
        assert_eq!(index(50., 0.), 100.);
        assert_eq!(index(90., 0.), 100., "clamped above the last knot");
        assert_eq!(index(-1., 0.), 0.);
    }

    #[test]
    fn between_knots_is_linear_and_rising() {
        assert!((index(2.5, 0.) - 17.).abs() < 1e-9);
        assert!((index(12.5, 0.) - 50.5).abs() < 1e-9);
        let mut last = -1.;
        for i in 0..=600 {
            let v = index(f64::from(i) / 10., 0.);
            assert!(v >= last, "the index never falls as stalls rise");
            last = v;
        }
    }

    #[test]
    fn swapping_in_raises_a_calm_reading_to_warning_but_never_lowers_one() {
        let swapping = 2. * 1024. * 1024.;
        assert_eq!(index(0., swapping), WARNING);
        assert_eq!(index(20., swapping), 67.);
        assert_eq!(index(0., 512. * 1024.), 0., "light swap-in is not pressure");
    }

    #[test]
    fn bands_split_at_34_and_67() {
        assert_eq!(band(33.9), Band::Normal);
        assert_eq!(band(34.), Band::Warning);
        assert_eq!(band(66.9), Band::Warning);
        assert_eq!(band(67.), Band::Critical);
    }
}
