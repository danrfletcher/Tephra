//! The `cache_limit` size rule: a pure parser the config loader and any
//! later caller (the `cache set-limit` verb) share.
//!
//! The rule is "a number, an optional single space, a unit":
//!
//! - the number is ASCII digits with an optional fraction
//!   (`10`, `1.5`, `0.25`); there is no sign, no exponent, no thousands
//!   separator and no leading or trailing dot;
//! - between the number and the unit there is at most one space (a
//!   tab, or two spaces, is an error);
//! - the unit is required and is one of `MB`, `GB`, `MiB`, `GiB`, in
//!   exactly that case. `MB` and `GB` are 1000-based
//!   (1 MB = 1,000,000 bytes), `MiB` and `GiB` 1024-based
//!   (1 MiB = 1,048,576 bytes). A bare number is refused rather than
//!   read as bytes, because a limit of `5000` that meant megabytes
//!   would silently evict everything;
//! - no leading or trailing whitespace.
//!
//! Choices the rule leaves open, made here:
//!
//! - **Zero is accepted** (atlas-27 PR-8, G12): `0`, `0 MB` and `0.0 GiB`
//!   are all 0 bytes, and a limit of 0 evicts everything that may be
//!   evicted once it is past its 10-minute window. A bare `0` is the one
//!   number that needs no unit: zero bytes is zero bytes in any of them.
//!   (Earlier the parser refused zero; a limit that `cache set-limit`
//!   accepts and writes to `config.toml` must load again at the next
//!   start, so the file and the verb share this one rule.)
//! - **A fraction of a byte is dropped** (`1.0000005 MB` is
//!   1,000,000 bytes), computed in exact integer arithmetic, never in
//!   floating point. More than 18 decimal places is refused.
//! - **The result must fit in a `u64`**; a larger value is refused.

use std::fmt;

/// The default `cache_limit`: 10 GB, 10,000,000,000 bytes.
pub const DEFAULT_CACHE_LIMIT_BYTES: u64 = 10_000_000_000;

/// The text a default config file carries for [`DEFAULT_CACHE_LIMIT_BYTES`].
pub const DEFAULT_CACHE_LIMIT_TEXT: &str = "10 GB";

/// The most decimal places a size may carry.
const MAX_FRACTION_DIGITS: usize = 18;

/// Why a size string was refused. [`fmt::Display`] says what is wrong
/// and what is accepted, without the name of any key: the caller names
/// the key (`cache_limit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SizeParseError {
    /// The string is empty or only spaces.
    Empty,
    /// There is no number at the start (a sign, a letter, a lone dot).
    NotANumber,
    /// The number has a trailing dot with no digits after it.
    BadFraction,
    /// More than 18 digits after the decimal point.
    TooManyDecimals,
    /// The number runs straight into the end of the string: no unit.
    MissingUnit,
    /// More than one space, or a non-space whitespace character, between
    /// the number and the unit, or whitespace around the whole value.
    BadSpacing,
    /// The unit is not `MB`, `GB`, `MiB` or `GiB`. Carries no text: the
    /// message goes to stderr and the log, and the value is not echoed.
    UnknownUnit,
    /// The value does not fit in a `u64`.
    TooLarge,
}

impl fmt::Display for SizeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const FORM: &str =
            "expected a number and a unit, such as \"10 GB\" (units: MB, GB, MiB, GiB)";
        match self {
            SizeParseError::Empty => write!(f, "is empty; {FORM}"),
            SizeParseError::NotANumber => write!(f, "does not start with a number; {FORM}"),
            SizeParseError::BadFraction => write!(f, "has a decimal point with no digits after it; {FORM}"),
            SizeParseError::TooManyDecimals => {
                write!(f, "has more than {MAX_FRACTION_DIGITS} decimal places")
            }
            SizeParseError::MissingUnit => write!(f, "has no unit; {FORM}"),
            SizeParseError::BadSpacing => write!(
                f,
                "has stray whitespace; use at most one space between the number and the unit and none around the value"
            ),
            SizeParseError::UnknownUnit => write!(f, "has an unknown unit; {FORM}"),
            SizeParseError::TooLarge => {
                write!(f, "is too large; the limit must fit in 64 bits ({} bytes)", u64::MAX)
            }
        }
    }
}

impl std::error::Error for SizeParseError {}

/// Parses `text` by the rule in the module docs into a byte count.
///
/// Pure: no I/O, no environment. Exact: `1.5 GiB` is
/// 1,610,612,736 and `0.1 MB` is 100,000, with no rounding error.
pub fn parse_cache_limit(text: &str) -> Result<u64, SizeParseError> {
    if text.trim().is_empty() {
        return Err(SizeParseError::Empty);
    }
    if text.trim() != text {
        return Err(SizeParseError::BadSpacing);
    }

    let int_len = text.bytes().take_while(u8::is_ascii_digit).count();
    if int_len == 0 {
        return Err(SizeParseError::NotANumber);
    }
    let (int_part, mut rest) = text.split_at(int_len);

    let mut frac_part = "";
    if let Some(after_dot) = rest.strip_prefix('.') {
        let frac_len = after_dot.bytes().take_while(u8::is_ascii_digit).count();
        if frac_len == 0 {
            return Err(SizeParseError::BadFraction);
        }
        frac_part = &after_dot[..frac_len];
        rest = &after_dot[frac_len..];
    }

    // A bare zero (`0`, `0.0`) is 0 bytes in every unit, so it needs none.
    if rest.is_empty()
        && int_part.bytes().all(|b| b == b'0')
        && frac_part.bytes().all(|b| b == b'0')
    {
        return Ok(0);
    }

    let unit_text = match rest.strip_prefix(' ') {
        Some(after_space) => after_space,
        None => rest,
    };
    if unit_text.is_empty() {
        return Err(SizeParseError::MissingUnit);
    }
    if unit_text.starts_with(char::is_whitespace) {
        return Err(SizeParseError::BadSpacing);
    }
    let factor: u128 = match unit_text {
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        _ => return Err(SizeParseError::UnknownUnit),
    };

    // Trailing zeros of the fraction carry no value, so `1.50` and
    // `1.5` are alike and 18 places means 18 significant ones.
    let frac_part = frac_part.trim_end_matches('0');
    if frac_part.len() > MAX_FRACTION_DIGITS {
        return Err(SizeParseError::TooManyDecimals);
    }

    let whole: u128 = int_part.parse().map_err(|_| SizeParseError::TooLarge)?;
    let mut bytes = whole.checked_mul(factor).ok_or(SizeParseError::TooLarge)?;
    if !frac_part.is_empty() {
        // frac_part / 10^len, times factor: at most 10^18 * 2^30 fits in
        // a u128 with room to spare.
        let numerator: u128 = frac_part.parse().expect("at most 18 ASCII digits");
        let denominator = 10u128.pow(frac_part.len() as u32);
        bytes = bytes
            .checked_add(numerator * factor / denominator)
            .ok_or(SizeParseError::TooLarge)?;
    }

    u64::try_from(bytes).map_err(|_| SizeParseError::TooLarge)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_text_parses_to_the_default_bytes() {
        assert_eq!(
            parse_cache_limit(DEFAULT_CACHE_LIMIT_TEXT),
            Ok(DEFAULT_CACHE_LIMIT_BYTES)
        );
        assert_eq!(DEFAULT_CACHE_LIMIT_BYTES, 10_000_000_000);
    }

    #[test]
    fn valid_forms_decimals_and_the_two_bases() {
        let cases: &[(&str, u64)] = &[
            ("1 MB", 1_000_000),
            ("1MB", 1_000_000),
            ("1 GB", 1_000_000_000),
            ("1GB", 1_000_000_000),
            ("1 MiB", 1_048_576),
            ("1MiB", 1_048_576),
            ("1 GiB", 1_073_741_824),
            ("1GiB", 1_073_741_824),
            ("10 GB", 10_000_000_000),
            ("512 MB", 512_000_000),
            ("1.5 GB", 1_500_000_000),
            ("1.5GiB", 1_610_612_736),
            ("0.5 MiB", 524_288),
            ("0.1 MB", 100_000),
            ("2.50 GB", 2_500_000_000),
            ("007 MB", 7_000_000),
            ("1.0000005 MB", 1_000_000),
            ("0.000001 GB", 1000),
            ("18446744073 GB", 18_446_744_073_000_000_000),
            ("1 MB", 1_000_000),
        ];
        for (text, want) in cases {
            assert_eq!(parse_cache_limit(text), Ok(*want), "{text:?}");
        }
    }

    /// atlas-27 PR-8 (G12): zero is a limit, in any spelling, and a bare
    /// `0` needs no unit. It is also what a tiny fraction rounds down to.
    #[test]
    fn zero_is_accepted_with_or_without_a_unit() {
        for text in [
            "0",
            "0.0",
            "000",
            "0 GB",
            "0MB",
            "0.0 MiB",
            "0 GiB",
            "0.000000000000000001 GB",
        ] {
            assert_eq!(parse_cache_limit(text), Ok(0), "{text:?}");
        }
    }

    #[test]
    fn the_largest_value_that_fits_is_accepted_and_one_more_byte_is_not() {
        // 2^34 GiB is 2^64 bytes: one over.
        assert_eq!(
            parse_cache_limit("17179869183 GiB"),
            Ok(17_179_869_183 * (1 << 30))
        );
        assert_eq!(
            parse_cache_limit("17179869184 GiB"),
            Err(SizeParseError::TooLarge)
        );
        assert_eq!(
            parse_cache_limit("18446744073709 MB"),
            Ok(18_446_744_073_709_000_000)
        );
        assert_eq!(
            parse_cache_limit("18446744073710 MB"),
            Err(SizeParseError::TooLarge)
        );
    }

    #[test]
    fn bad_values_are_refused_with_the_reason() {
        let cases: &[(&str, SizeParseError)] = &[
            // empty
            ("", SizeParseError::Empty),
            ("   ", SizeParseError::Empty),
            // a bare number has no unit
            ("5000", SizeParseError::MissingUnit),
            ("5000 ", SizeParseError::BadSpacing),
            ("1.5", SizeParseError::MissingUnit),
            // negative and signed
            ("-1 GB", SizeParseError::NotANumber),
            ("-0 GB", SizeParseError::NotANumber),
            ("+1 GB", SizeParseError::NotANumber),
            // a bare number that is not zero still has no unit
            ("0.5", SizeParseError::MissingUnit),
            ("00.1", SizeParseError::MissingUnit),
            ("0 ", SizeParseError::BadSpacing),
            ("-0", SizeParseError::NotANumber),
            ("0.", SizeParseError::BadFraction),
            // unknown or wrong-case units
            ("5 KB", SizeParseError::UnknownUnit),
            ("5 TB", SizeParseError::UnknownUnit),
            ("5 B", SizeParseError::UnknownUnit),
            ("5 gb", SizeParseError::UnknownUnit),
            ("5 Gib", SizeParseError::UnknownUnit),
            ("5 mb", SizeParseError::UnknownUnit),
            ("5 GBB", SizeParseError::UnknownUnit),
            ("5 G", SizeParseError::UnknownUnit),
            ("5 GB extra", SizeParseError::UnknownUnit),
            // spacing
            ("5  GB", SizeParseError::BadSpacing),
            ("5\tGB", SizeParseError::BadSpacing),
            (" 5 GB", SizeParseError::BadSpacing),
            ("5 GB ", SizeParseError::BadSpacing),
            // shape of the number
            (".5 GB", SizeParseError::NotANumber),
            ("5. GB", SizeParseError::BadFraction),
            ("5.GB", SizeParseError::BadFraction),
            ("1e3 MB", SizeParseError::UnknownUnit),
            ("1,000 MB", SizeParseError::UnknownUnit),
            ("1_000 MB", SizeParseError::UnknownUnit),
            ("GB", SizeParseError::NotANumber),
            ("ten GB", SizeParseError::NotANumber),
            ("1.2.3 GB", SizeParseError::UnknownUnit),
            ("0x10 GB", SizeParseError::UnknownUnit),
            // too many decimal places
            ("1.1234567890123456789 MB", SizeParseError::TooManyDecimals),
            // does not fit in a u64
            ("18446744073709551616 MiB", SizeParseError::TooLarge),
            (
                "99999999999999999999999999999999999999999 GB",
                SizeParseError::TooLarge,
            ),
            (
                "340282366920938463463374607431768211456 MB",
                SizeParseError::TooLarge,
            ),
            ("18446744073710 MB", SizeParseError::TooLarge),
            ("1000000000000 GiB", SizeParseError::TooLarge),
        ];
        for (text, want) in cases {
            assert_eq!(parse_cache_limit(text).as_ref(), Err(want), "{text:?}");
        }
    }

    #[test]
    fn a_non_ascii_digit_or_space_is_not_one() {
        // Arabic-Indic digit, no-break space, full-width digit.
        for text in ["٣ GB", "5\u{a0}GB", "５ GB"] {
            assert!(parse_cache_limit(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn eighteen_decimal_places_are_exact() {
        assert_eq!(parse_cache_limit("1.000000000000000001 MB"), Ok(1_000_000));
        assert_eq!(
            parse_cache_limit("1.500000000000000000000 MB"),
            Ok(1_500_000)
        );
    }

    #[test]
    fn every_error_says_what_to_write_without_naming_a_key() {
        for text in [
            "",
            "5",
            "5 KB",
            "-1 GB",
            "5  GB",
            "99999999999999999999999 GB",
        ] {
            let msg = parse_cache_limit(text).unwrap_err().to_string();
            assert!(!msg.contains("cache_limit"), "{msg}");
            assert!(!msg.is_empty());
        }
        assert!(parse_cache_limit("5 KB")
            .unwrap_err()
            .to_string()
            .contains("GiB"));
    }
}
