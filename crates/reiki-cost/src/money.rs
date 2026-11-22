//! Fixed-point price arithmetic. No floating point is used anywhere in this crate.

/// One millionth of the currency's base unit, for example one micro-dollar.
pub const MICROS_PER_UNIT: u128 = 1_000_000;
/// Prices are parsed to nine decimal places, called nanounits here.
pub const NANOS_PER_UNIT: u128 = 1_000_000_000;

/// Half-up division, so a half micro is always charged upward.
pub fn round_div(numerator: u128, divisor: u128) -> u128 {
    (numerator + divisor / 2) / divisor
}

/// Parses a non-negative decimal string into nanounits per million tokens.
///
/// Up to nine fractional digits are exact. Further digits are rounded half-up to
/// nanounits; the error message never echoes the value it rejected.
pub fn price_per_million(text: &str, field: &str) -> Result<u128, String> {
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (text, None),
    };
    if whole.is_empty()
        || whole.len() > 15
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.is_some_and(|fraction| {
            fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(format!("{field} must be a non-negative decimal string"));
    }
    let mut value = whole
        .parse::<u128>()
        .map_err(|_| format!("{field} is out of range"))?
        .checked_mul(NANOS_PER_UNIT)
        .ok_or_else(|| format!("{field} is out of range"))?;
    if let Some(fraction) = fraction {
        let mut scale = NANOS_PER_UNIT / 10;
        for (index, byte) in fraction.bytes().enumerate() {
            let digit = u128::from(byte - b'0');
            if index < 9 {
                value = value
                    .checked_add(digit * scale)
                    .ok_or_else(|| format!("{field} is out of range"))?;
                scale /= 10;
            } else if digit >= 5 {
                // First discarded digit decides the half-up rounding.
                value = value
                    .checked_add(1)
                    .ok_or_else(|| format!("{field} is out of range"))?;
                break;
            }
        }
    }
    Ok(value)
}

/// Cost of `tokens` at `price` nanounits per million, in integer micro-units.
pub fn line_cost_micros(price: u128, tokens: u64) -> u128 {
    round_div(price.saturating_mul(u128::from(tokens)), NANOS_PER_UNIT)
}

/// Renders an integer micro amount as a fixed-point decimal string for display.
pub fn format_micros(micros: u128) -> String {
    let whole = micros / MICROS_PER_UNIT;
    let fraction = micros % MICROS_PER_UNIT;
    format!("{whole}.{fraction:06}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_strings_are_exact_to_nano_units() {
        assert_eq!(price_per_million("2.5", "input").unwrap(), 2_500_000_000);
        assert_eq!(price_per_million("0", "input").unwrap(), 0);
        assert_eq!(price_per_million("0.000000001", "input").unwrap(), 1);
        assert_eq!(
            price_per_million("1.0000000004", "input").unwrap(),
            1_000_000_000
        );
        assert_eq!(
            price_per_million("1.0000000005", "input").unwrap(),
            1_000_000_001
        );
    }

    #[test]
    fn invalid_prices_are_rejected_without_echoing_them() {
        for bad in ["-1", " 1", "1.", ".5", "1e3", "abc", "1.2.3"] {
            let error = price_per_million(bad, "input").unwrap_err();
            assert!(!error.contains(bad), "error echoed the price: {error}");
        }
        assert!(price_per_million("", "input").is_err());
    }

    #[test]
    fn arithmetic_is_integer_and_rounds_half_up() {
        let price = price_per_million("2.5", "input").unwrap();
        assert_eq!(line_cost_micros(price, 1), 3);
        assert_eq!(line_cost_micros(price, 2), 5);
        assert_eq!(line_cost_micros(price, 1_000_000), 2_500_000);
        let tiny = price_per_million("0.000000001", "input").unwrap();
        assert_eq!(line_cost_micros(tiny, 1_000_000_000), 1);
        assert_eq!(line_cost_micros(tiny, 499_999_999), 0);
    }

    #[test]
    fn display_uses_six_fixed_decimals() {
        assert_eq!(format_micros(0), "0.000000");
        assert_eq!(format_micros(3), "0.000003");
        assert_eq!(format_micros(2_500_000), "2.500000");
    }
}
