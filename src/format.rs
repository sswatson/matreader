//! Formatting helpers for values, sizes and dimensions.

use crate::model::{Class, Scalar};

/// User-adjustable options for how numeric values are rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayOpts {
    /// Significant digits shown for floating point values.
    pub precision: u8,
    /// Interpret real numeric values as MATLAB datenums and show them as dates.
    pub datetime: bool,
}

impl DisplayOpts {
    pub const DEFAULT_PRECISION: u8 = 6;
    pub const MIN_PRECISION: u8 = 1;
    pub const MAX_PRECISION: u8 = 17;
}

impl Default for DisplayOpts {
    fn default() -> Self {
        DisplayOpts {
            precision: Self::DEFAULT_PRECISION,
            datetime: false,
        }
    }
}

/// Format a floating point value compactly, similar to MATLAB's `format short g`.
pub fn format_f64(v: f64) -> String {
    format_f64_prec(v, DisplayOpts::DEFAULT_PRECISION)
}

/// Format a float with up to `prec` significant digits.
///
/// A non-integer value never renders as a bare integer: if rounding leaves no
/// fractional digits, one is kept (`738000.04` → `738000.0`, not `738000`), so a
/// rounded value is not mistaken for an exact one.
pub fn format_f64_prec(v: f64, prec: u8) -> String {
    let prec = prec.clamp(DisplayOpts::MIN_PRECISION, DisplayOpts::MAX_PRECISION) as i32;
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Inf".into() } else { "-Inf".into() };
    }
    if v == 0.0 {
        return if v.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let a = v.abs();
    // Fixed notation up to 10^max(prec, 6), so that raising the precision shows
    // e.g. datenums (~7.4e5) or prices in full rather than in exponent form.
    let upper = 10f64.powi(prec.max(6));
    if (1e-4..upper).contains(&a) {
        let int_digits = a.log10().floor() as i32 + 1;
        let decimals = (prec - int_digits.max(0)).max(1) as usize;
        let s = format!("{:.*}", decimals, v);
        trim_zeros_keep_one(&s)
    } else {
        let s = format!("{:.*e}", (prec - 1) as usize, v);
        // Rust prints "1.23450e5"; tidy mantissa zeros and exponent sign.
        let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let mant = trim_zeros(mant);
        let exp: i32 = exp.parse().unwrap_or(0);
        format!(
            "{}e{}{:02}",
            mant,
            if exp < 0 { "-" } else { "+" },
            exp.abs()
        )
    }
}

/// Like `trim_zeros`, but keeps at least one digit after the decimal point.
fn trim_zeros_keep_one(s: &str) -> String {
    let t = trim_zeros(s);
    if t.contains('.') || !s.contains('.') {
        t
    } else {
        format!("{}.0", t)
    }
}

/// MATLAB datenum of 1970-01-01.
const DATENUM_UNIX_EPOCH: i64 = 719_529;

/// Render a MATLAB datenum as `YYYY-MM-DD`, `YYYY-MM-DD HH:MM:SS`, or
/// `YYYY-MM-DD HH:MM:SS.fff` (rounded to the millisecond), whichever is the
/// shortest exact rendering. `full` always includes the time and milliseconds.
/// Returns `None` for values outside years 0–9999 or non-finite values.
pub fn format_datenum(v: f64, full: bool) -> Option<String> {
    // datenum(0,1,1) = 1, datenum(10000,1,1) = 3652060.
    if !v.is_finite() || !(1.0..3_652_060.0).contains(&v) {
        return None;
    }
    let mut day = v.floor() as i64;
    let mut ms = ((v - v.floor()) * 86_400_000.0).round() as i64;
    if ms >= 86_400_000 {
        day += 1;
        ms -= 86_400_000;
    }
    let (y, m, d) = civil_from_days(day - DATENUM_UNIX_EPOCH);
    let date = format!("{:04}-{:02}-{:02}", y, m, d);
    if ms == 0 && !full {
        return Some(date);
    }
    let (h, mi, s, frac) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    Some(if frac == 0 && !full {
        format!("{} {:02}:{:02}:{:02}", date, h, mi, s)
    } else {
        format!("{} {:02}:{:02}:{:02}.{:03}", date, h, mi, s, frac)
    })
}

/// Proleptic Gregorian (year, month, day) from days since 1970-01-01.
/// (Howard Hinnant's `civil_from_days`.)
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The value of a real numeric scalar, if it can be shown as a datenum.
fn datenum_value(v: &Scalar) -> Option<f64> {
    match v {
        Scalar::Float(f) => Some(*f),
        Scalar::Int(i) => Some(*i as f64),
        Scalar::Uint(u) => Some(*u as f64),
        _ => None,
    }
}

/// Full-precision rendering of a float (for the detail line).
pub fn format_f64_full(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Inf".into() } else { "-Inf".into() };
    }
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    format!("{}", v)
}

fn trim_zeros(s: &str) -> String {
    if s.contains('.') {
        let t = s.trim_end_matches('0');
        let t = t.trim_end_matches('.');
        t.to_string()
    } else {
        s.to_string()
    }
}

/// Format a scalar for a grid cell.
pub fn format_scalar(v: &Scalar, class: &Class, opts: &DisplayOpts) -> String {
    if opts.datetime {
        if let Some(s) = datenum_value(v).and_then(|f| format_datenum(f, false)) {
            return s;
        }
    }
    // A single carries ~7–9 significant digits; more would only show f32 noise.
    let single_prec = opts.precision.min(9);
    match v {
        Scalar::Float(f) => {
            if *class == Class::Single {
                format_f64_prec((*f as f32) as f64, single_prec)
            } else {
                format_f64_prec(*f, opts.precision)
            }
        }
        Scalar::Int(i) => i.to_string(),
        Scalar::Uint(u) => u.to_string(),
        Scalar::Bool(b) => {
            if *b {
                "1".into()
            } else {
                "0".into()
            }
        }
        Scalar::Complex(re, im) => {
            let p = if *class == Class::Single {
                single_prec
            } else {
                opts.precision
            };
            let sign = if *im < 0.0 || (*im == 0.0 && im.is_sign_negative()) {
                "-"
            } else {
                "+"
            };
            format!(
                "{}{}{}i",
                format_f64_prec(*re, p),
                sign,
                format_f64_prec(im.abs(), p)
            )
        }
        Scalar::Char(c) => char::from_u32(*c).unwrap_or('\u{FFFD}').to_string(),
    }
}

/// Full-precision rendering for the detail line.
pub fn format_scalar_full(v: &Scalar, class: &Class, opts: &DisplayOpts) -> String {
    let raw = match v {
        Scalar::Float(f) => {
            if *class == Class::Single {
                format!("{}", *f as f32)
            } else {
                format_f64_full(*f)
            }
        }
        Scalar::Complex(re, im) => {
            let sign = if *im < 0.0 { "-" } else { "+" };
            format!(
                "{} {} {}i",
                format_f64_full(*re),
                sign,
                format_f64_full(im.abs())
            )
        }
        Scalar::Bool(b) => if *b { "true" } else { "false" }.into(),
        Scalar::Char(c) => format!(
            "{:?} (U+{:04X})",
            char::from_u32(*c).unwrap_or('\u{FFFD}'),
            c
        ),
        Scalar::Int(i) => i.to_string(),
        Scalar::Uint(u) => u.to_string(),
    };
    if opts.datetime {
        if let Some(dt) = datenum_value(v).and_then(|f| format_datenum(f, true)) {
            return format!("{}  (datenum {})", dt, raw);
        }
    }
    raw
}

/// Human-readable byte count.
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} B", n)
    } else if v >= 100.0 {
        format!("{:.0} {}", v, UNITS[i])
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}

/// Render a MATLAB char row for display, replacing control characters.
pub fn sanitize_text(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\t' => ' ',
            '\r' | '\n' => '↵',
            c if c.is_control() => '·',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats() {
        assert_eq!(format_f64(0.0), "0");
        assert_eq!(format_f64(3.0), "3");
        assert_eq!(format_f64(-42.0), "-42");
        assert_eq!(format_f64(std::f64::consts::PI), "3.14159");
        assert_eq!(format_f64(0.5), "0.5");
        assert_eq!(format_f64(1234.5678), "1234.57");
        assert_eq!(format_f64(1e-7), "1e-07");
        assert_eq!(format_f64(1.5e20), "1.5e+20");
        assert_eq!(format_f64(f64::NAN), "NaN");
        assert_eq!(format_f64(f64::NEG_INFINITY), "-Inf");
        assert_eq!(format_f64(1e15), "1e+15");
    }

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn complex() {
        assert_eq!(
            format_scalar(
                &Scalar::Complex(1.0, -2.0),
                &Class::Double,
                &DisplayOpts::default()
            ),
            "1-2i"
        );
        assert_eq!(
            format_scalar(
                &Scalar::Complex(1.0, 2.0),
                &Class::Double,
                &DisplayOpts::default()
            ),
            "1+2i"
        );
    }

    #[test]
    fn precision() {
        assert_eq!(format_f64_prec(std::f64::consts::PI, 3), "3.14");
        assert_eq!(format_f64_prec(std::f64::consts::PI, 12), "3.14159265359");
        assert_eq!(format_f64_prec(1234.5678, 8), "1234.5678");
        assert_eq!(format_f64_prec(1.5e20, 2), "1.5e+20");
        assert_eq!(format_f64_prec(1.23456789e20, 4), "1.235e+20");
        // Higher precision widens the fixed-notation range.
        assert_eq!(format_f64_prec(1234567.891, 10), "1234567.891");
        assert_eq!(format_f64_prec(1234567.891, 6), "1.23457e+06");
        // Non-integers never look like exact integers.
        assert_eq!(format_f64_prec(738000.04, 6), "738000.0");
        assert_eq!(format_f64_prec(738000.5, 6), "738000.5");
        assert_eq!(format_f64_prec(0.99999999, 6), "1.0");
        assert_eq!(format_f64_prec(2.0, 6), "2");
    }

    #[test]
    fn datenums() {
        // datenum(1970,1,1) and datenum(2000,1,1)
        assert_eq!(format_datenum(719529.0, false).unwrap(), "1970-01-01");
        assert_eq!(format_datenum(730486.0, false).unwrap(), "2000-01-01");
        // datenum(2024,2,29,13,45,30)
        let v = 739311.0 + (13.0 * 3600.0 + 45.0 * 60.0 + 30.0) / 86400.0;
        assert_eq!(format_datenum(v, false).unwrap(), "2024-02-29 13:45:30");
        assert_eq!(format_datenum(v, true).unwrap(), "2024-02-29 13:45:30.000");
        let v = 739311.0 + 0.25 + 0.123 / 86400.0;
        assert_eq!(format_datenum(v, false).unwrap(), "2024-02-29 06:00:00.123");
        // Rounding up to midnight rolls the day over.
        assert_eq!(
            format_datenum(739311.0 + 0.9999999999, false).unwrap(),
            "2024-03-01"
        );
        // datenum(0,1,1) = 1
        assert_eq!(format_datenum(1.0, false).unwrap(), "0000-01-01");
        assert!(format_datenum(0.5, false).is_none());
        assert!(format_datenum(f64::NAN, false).is_none());

        let dt = DisplayOpts {
            datetime: true,
            ..Default::default()
        };
        assert_eq!(
            format_scalar(&Scalar::Float(730486.5), &Class::Double, &dt),
            "2000-01-01 12:00:00"
        );
        assert_eq!(
            format_scalar_full(&Scalar::Float(730486.5), &Class::Double, &dt),
            "2000-01-01 12:00:00.000  (datenum 730486.5)"
        );
        // Values outside the datenum range fall back to numbers.
        assert_eq!(
            format_scalar(&Scalar::Float(-3.5), &Class::Double, &dt),
            "-3.5"
        );
        assert_eq!(
            format_scalar(&Scalar::Bool(true), &Class::Logical, &dt),
            "1"
        );
    }
}
