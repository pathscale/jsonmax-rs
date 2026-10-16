//! Strict validation may skip the binary64 conversion of a float only when the
//! numeric policy provably cannot reject it. Compare every outcome with the
//! lexer plus a full `parse_f64`, around the overflow and underflow edges.

use jmax_abi::number::NumericPolicy;
use jmax_syntax::{Limits, Scratch, validate_borrowed};

fn reference(text: &str, policy: NumericPolicy) -> Option<(jmax_syntax::ErrorCode, u32)> {
    let token = match jmax_number::lex::lex_number(text.as_bytes(), 0, &policy) {
        Ok(token) => token,
        Err(error) => return Some((error.code, error.byte_offset)),
    };
    if token.kind() == jmax_abi::span::NumberLexKind::FloatDecimal {
        if let Err(error) = jmax_number::float::parse_f64(&token, &policy) {
            return Some((error.code, error.byte_offset));
        }
    }
    None
}

#[test]
fn shortcut_matches_full_conversion() {
    let mantissas = [
        "1", "9", "17976931348623157", "17976931348623159", "2", "247", "25", "4", "49", "5",
        "123456789", "0.5", "0.0001", "0.00000000000000000001", "9.99", "1.7976931348623157",
        "1.8", "2.4703282292062327", "2.4703282292062328", "4.9406564584124654",
    ];
    let mut policies = vec![NumericPolicy::strict_default()];
    let mut loose = NumericPolicy::strict_default();
    loose.reject_negative_zero = false;
    policies.push(loose);
    let mut loose = NumericPolicy::strict_default();
    loose.reject_non_finite_float = false;
    policies.push(loose);
    let mut checked = 0;
    for policy in policies {
        for mantissa in mantissas {
            for exponent in -345..=330 {
                for sign in ["", "-"] {
                    for form in [format!("{sign}{mantissa}e{exponent}"), format!("{sign}{mantissa}E+{exponent}")] {
                        if form.contains("E+-") {
                            continue;
                        }
                        let actual = validate_borrowed(
                            form.as_bytes(),
                            Limits::strict_default(),
                            Some(policy),
                            &mut Scratch::default(),
                        )
                        .err()
                        .map(|error| (error.code, error.byte_offset));
                        assert_eq!(actual, reference(&form, policy), "{form} {policy:?}");
                        checked += 1;
                    }
                }
            }
        }
    }
    assert!(checked > 10_000);
}
