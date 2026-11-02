//! Numeric conversions that saturate instead of wrapping. `as` silently
//! wraps (70 000 rows as `u16` is 4 464) or turns a negative into a huge
//! count; these clamp to the target's range, so a value too large for the
//! screen just fills it.

/// A count or length as a terminal size, at most `u16::MAX`
pub fn to_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// A count or length as `u32`, at most `u32::MAX`
pub fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A 64-bit count as `usize`, at most `usize::MAX`
pub fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// A signed value as unsigned: negatives become 0
pub fn non_negative(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// A float as a whole non-negative number, toward zero: NaN and negatives
/// give 0, values past the range `u64::MAX`
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn float_to_u64(x: f64) -> u64 {
    // Float-to-int `as` saturates and maps NaN to 0 (Rust 1.45+); the
    // explicit check documents the negative case
    if x.is_nan() || x <= 0.0 { 0 } else { x as u64 }
}

/// A float as a terminal size, toward zero, within `0..=u16::MAX`
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn float_to_u16(x: f64) -> u16 {
    // Saturating `as`, see float_to_u64
    if x.is_nan() || x <= 0.0 { 0 } else { x as u16 }
}

/// A Kubernetes quantity such as `838259748Ki` or `1G` in bytes; `None`
/// for a suffix it doesn't know (milli, exa, exponents)
///
/// ```
/// use talos9s_core::convert::parse_quantity;
///
/// assert_eq!(parse_quantity("838259748Ki"), Some(838_259_748 * 1024));
/// assert_eq!(parse_quantity("1.5Gi"), Some(1_610_612_736));
/// assert_eq!(parse_quantity("2G"), Some(2_000_000_000));
/// assert_eq!(parse_quantity("512"), Some(512));
/// assert_eq!(parse_quantity("12Xi"), None);
/// assert_eq!(parse_quantity(""), None);
/// ```
pub fn parse_quantity(q: &str) -> Option<u64> {
    let q = q.trim();
    let split = q
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(q.len());
    let (number, suffix) = q.split_at(split);
    let multiplier: u64 = match suffix {
        "" => 1,
        "Ki" => 1 << 10,
        "Mi" => 1 << 20,
        "Gi" => 1 << 30,
        "Ti" => 1 << 40,
        "Pi" => 1 << 50,
        "k" => 1_000,
        "M" => 1_000_000,
        "G" => 1_000_000_000,
        "T" => 1_000_000_000_000,
        "P" => 1_000_000_000_000_000,
        _ => return None,
    };
    let value: f64 = number.parse().ok()?;
    Some(float_to_u64(value * multiplier as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_saturate_instead_of_wrapping() {
        assert_eq!(to_u16(70_000), u16::MAX);
        assert_eq!(to_u16(42), 42);
        assert_eq!(to_u32(7), 7);
        assert_eq!(to_usize(9), 9);
        assert_eq!(non_negative(-5), 0);
        assert_eq!(non_negative(5), 5);
        assert_eq!(float_to_u64(-1.5), 0);
        assert_eq!(float_to_u64(f64::NAN), 0);
        assert_eq!(float_to_u64(2.9), 2);
        assert_eq!(float_to_u64(1e30), u64::MAX);
        assert_eq!(float_to_u16(1e9), u16::MAX);
        assert_eq!(float_to_u16(12.7), 12);
    }
}
