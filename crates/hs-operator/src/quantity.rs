//! Kubernetes storage quantities (`1Gi`, `500Mi`, `5G`, `1.5Gi`) read as bytes, so that two
//! sizes can be compared: the operator never asks a claim to shrink ([`larger`]), and the
//! homeserver's bridge client never writes a `Bridge` smaller than the one in the cluster.
//!
//! The grammar is the API server's (`k8s.io/apimachinery/pkg/api/resource`): a decimal number
//! with an optional binary suffix (`Ki` .. `Ei`), decimal suffix (`k`, `M` .. `E`, `m` for
//! thousandths) or exponent (`e3`, `E6`). A fractional byte count is rounded up, as the API
//! server rounds a storage request.

/// The number of bytes `quantity` stands for, or `None` when it is not a quantity (or is
/// negative, or too large for a `u128`).
#[must_use]
pub fn bytes(quantity: &str) -> Option<u128> {
    let q = quantity.trim();
    let split = q
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '+'))
        .unwrap_or(q.len());
    let (number, suffix) = q.split_at(split);
    let number = number.strip_prefix('+').unwrap_or(number);
    if number.is_empty() || number == "." || number.matches('.').count() > 1 {
        return None;
    }
    // (numerator multiplier, denominator) for the suffix.
    let (multiplier, divisor): (u128, u128) = match suffix {
        "" => (1, 1),
        "Ki" => (1 << 10, 1),
        "Mi" => (1 << 20, 1),
        "Gi" => (1 << 30, 1),
        "Ti" => (1 << 40, 1),
        "Pi" => (1 << 50, 1),
        "Ei" => (1 << 60, 1),
        "m" => (1, 1000),
        "k" => (1_000, 1),
        "M" => (1_000_000, 1),
        "G" => (1_000_000_000, 1),
        "T" => (1_000_000_000_000, 1),
        "P" => (1_000_000_000_000_000, 1),
        "E" => (1_000_000_000_000_000_000, 1),
        exp if exp.starts_with(['e', 'E']) => {
            let power: u32 = exp[1..].parse().ok()?;
            (10u128.checked_pow(power)?, 1)
        }
        _ => return None,
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let digits = format!("{whole}{fraction}");
    let mantissa: u128 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    let scale = 10u128.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    let numerator = mantissa.checked_mul(multiplier)?;
    let denominator = scale.checked_mul(divisor)?;
    Some(numerator.div_ceil(denominator))
}

/// Of two quantities, the one standing for more bytes, as written; `a` when they are equal or
/// `b` cannot be read, and `b` when only `a` cannot be read.
#[must_use]
pub fn larger<'a>(a: &'a str, b: &'a str) -> &'a str {
    match (bytes(a), bytes(b)) {
        (Some(x), Some(y)) if y > x => b,
        (None, Some(_)) => b,
        _ => a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_decimal_and_plain_quantities_read_as_bytes() {
        assert_eq!(bytes("1Gi"), Some(1 << 30));
        assert_eq!(bytes("5Gi"), Some(5 << 30));
        assert_eq!(bytes("500Mi"), Some(500 << 20));
        assert_eq!(bytes("1.5Gi"), Some(3 << 29));
        assert_eq!(bytes("2G"), Some(2_000_000_000));
        assert_eq!(bytes("1k"), Some(1000));
        assert_eq!(bytes("1024"), Some(1024));
        assert_eq!(bytes(" 1Ti "), Some(1 << 40));
        assert_eq!(bytes("1e3"), Some(1000));
        assert_eq!(bytes("+3Ki"), Some(3072));
        // A fractional byte rounds up, as the API server rounds a storage request.
        assert_eq!(bytes("1500m"), Some(2));
        assert_eq!(bytes("0.1"), Some(1));
    }

    #[test]
    fn what_is_not_a_quantity_reads_as_none() {
        for bad in [
            "", "Gi", "1GB", "-1Gi", "1..5Gi", "1.2.3", "five", ".", "1 Gi",
        ] {
            assert_eq!(bytes(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn larger_picks_the_bigger_size_as_written() {
        assert_eq!(larger("1Gi", "5Gi"), "5Gi");
        assert_eq!(larger("5Gi", "1Gi"), "5Gi");
        assert_eq!(larger("1024Mi", "1Gi"), "1024Mi", "equal keeps the first");
        assert_eq!(larger("2G", "1Gi"), "2G");
        assert_eq!(larger("junk", "1Gi"), "1Gi");
        assert_eq!(larger("1Gi", "junk"), "1Gi");
    }
}
