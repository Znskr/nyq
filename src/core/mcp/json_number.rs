use super::json_value::Value;
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_lexeme_bytes: usize,
    pub max_exponent_abs: i64,
    pub max_expanded_digits: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_lexeme_bytes: 4096,
            max_exponent_abs: 1_000_000,
            max_expanded_digits: 8192,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidNumber,
    NumberLimitExceeded,
    OutOfMemory,
}

#[derive(Clone, Debug)]
pub struct Number {
    negative: bool,
    digits: Vec<u8>,
    exponent: i64,
}

impl Number {
    pub fn parse(lexeme: &[u8], limits: Limits) -> Result<Number, Error> {
        if lexeme.is_empty() {
            return Err(Error::InvalidNumber);
        }
        if lexeme.len() > limits.max_lexeme_bytes {
            return Err(Error::NumberLimitExceeded);
        }

        let mut index = 0usize;
        let negative = lexeme[index] == b'-';
        if negative {
            index += 1;
            if index == lexeme.len() {
                return Err(Error::InvalidNumber);
            }
        }

        let mut raw_digits: Vec<u8> = Vec::new();
        let mut fraction_digits: usize = 0;
        if lexeme[index] == b'0' {
            raw_digits.push(b'0');
            index += 1;
            if index < lexeme.len() && lexeme[index].is_ascii_digit() {
                return Err(Error::InvalidNumber);
            }
        } else if (b'1'..=b'9').contains(&lexeme[index]) {
            while index < lexeme.len() && lexeme[index].is_ascii_digit() {
                raw_digits.push(lexeme[index]);
                index += 1;
            }
        } else {
            return Err(Error::InvalidNumber);
        }

        if index < lexeme.len() && lexeme[index] == b'.' {
            index += 1;
            let fraction_start = index;
            while index < lexeme.len() && lexeme[index].is_ascii_digit() {
                raw_digits.push(lexeme[index]);
                index += 1;
            }
            if index == fraction_start {
                return Err(Error::InvalidNumber);
            }
            fraction_digits = index - fraction_start;
        }

        let mut explicit_exponent: i64 = 0;
        if index < lexeme.len() && (lexeme[index] == b'e' || lexeme[index] == b'E') {
            index += 1;
            if index == lexeme.len() {
                return Err(Error::InvalidNumber);
            }
            let mut exponent_negative = false;
            if lexeme[index] == b'+' || lexeme[index] == b'-' {
                exponent_negative = lexeme[index] == b'-';
                index += 1;
            }
            let exponent_start = index;
            while index < lexeme.len() && lexeme[index].is_ascii_digit() {
                let digit = (lexeme[index] - b'0') as i64;
                explicit_exponent = explicit_exponent
                    .checked_mul(10)
                    .ok_or(Error::NumberLimitExceeded)?;
                explicit_exponent = explicit_exponent
                    .checked_add(digit)
                    .ok_or(Error::NumberLimitExceeded)?;
                if explicit_exponent > limits.max_exponent_abs {
                    return Err(Error::NumberLimitExceeded);
                }
                index += 1;
            }
            if index == exponent_start {
                return Err(Error::InvalidNumber);
            }
            if exponent_negative {
                explicit_exponent = -explicit_exponent;
            }
        }
        if index != lexeme.len() {
            return Err(Error::InvalidNumber);
        }
        if raw_digits.len() > limits.max_lexeme_bytes {
            return Err(Error::NumberLimitExceeded);
        }

        let mut first_nonzero = 0usize;
        while first_nonzero < raw_digits.len() && raw_digits[first_nonzero] == b'0' {
            first_nonzero += 1;
        }
        if first_nonzero == raw_digits.len() {
            return Ok(Number {
                negative: false,
                digits: b"0".to_vec(),
                exponent: 0,
            });
        }

        let mut significant_end = raw_digits.len();
        while raw_digits[significant_end - 1] == b'0' {
            significant_end -= 1;
        }
        let removed_trailing = raw_digits.len() - significant_end;
        let fractional_exponent = i64::try_from(fraction_digits).map_err(|_| Error::NumberLimitExceeded)?;
        let trailing_exponent = i64::try_from(removed_trailing).map_err(|_| Error::NumberLimitExceeded)?;
        let mut exponent = explicit_exponent
            .checked_sub(fractional_exponent)
            .ok_or(Error::NumberLimitExceeded)?;
        exponent = exponent
            .checked_add(trailing_exponent)
            .ok_or(Error::NumberLimitExceeded)?;
        if exponent > limits.max_exponent_abs || exponent < -limits.max_exponent_abs {
            return Err(Error::NumberLimitExceeded);
        }

        Ok(Number {
            negative,
            digits: raw_digits[first_nonzero..significant_end].to_vec(),
            exponent,
        })
    }

    pub fn is_zero(&self) -> bool {
        self.digits.len() == 1 && self.digits[0] == b'0'
    }

    pub fn is_negative(&self) -> bool {
        self.negative
    }

    pub fn is_integer(&self) -> bool {
        self.is_zero() || self.exponent >= 0
    }

    pub fn order(&self, right: &Number) -> Ordering {
        if self.is_zero() && right.is_zero() {
            return Ordering::Equal;
        }
        if self.is_zero() {
            return if right.negative {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        if right.is_zero() {
            return if self.negative {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        if self.negative != right.negative {
            return if self.negative {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let magnitude = magnitude_order(self, right);
        if self.negative {
            match magnitude {
                Ordering::Less => Ordering::Greater,
                Ordering::Equal => Ordering::Equal,
                Ordering::Greater => Ordering::Less,
            }
        } else {
            magnitude
        }
    }

    pub fn eql(&self, right: &Number) -> bool {
        self.order(right) == Ordering::Equal
    }

    pub fn to_non_negative_usize(&self) -> Option<usize> {
        if self.negative || !self.is_integer() {
            return None;
        }
        let mut result: usize = 0;
        for &digit in &self.digits {
            result = result.checked_mul(10)?;
            result = result.checked_add((digit - b'0') as usize)?;
        }
        let zero_count = usize::try_from(self.exponent).ok()?;
        for _ in 0..zero_count {
            result = result.checked_mul(10)?;
        }
        Some(result)
    }

    pub fn is_multiple_of(
        &self,
        divisor: &Number,
        limits: Limits,
    ) -> Result<bool, Error> {
        if divisor.is_zero() {
            return Err(Error::InvalidNumber);
        }
        if self.is_zero() {
            return Ok(true);
        }
        let delta = self
            .exponent
            .checked_sub(divisor.exponent)
            .ok_or(Error::NumberLimitExceeded)?;
        let numerator_zeros = if delta > 0 {
            usize::try_from(delta).map_err(|_| Error::NumberLimitExceeded)?
        } else {
            0
        };
        let denominator_zeros = if delta < 0 {
            usize::try_from(delta.unsigned_abs()).map_err(|_| Error::NumberLimitExceeded)?
        } else {
            0
        };
        let numerator_len = self
            .digits
            .len()
            .checked_add(numerator_zeros)
            .ok_or(Error::NumberLimitExceeded)?;
        let denominator_len = divisor
            .digits
            .len()
            .checked_add(denominator_zeros)
            .ok_or(Error::NumberLimitExceeded)?;
        if numerator_len > limits.max_expanded_digits
            || denominator_len > limits.max_expanded_digits
        {
            return Err(Error::NumberLimitExceeded);
        }

        let numerator_text = expanded_digits(&self.digits, numerator_zeros);
        let denominator_text = expanded_digits(&divisor.digits, denominator_zeros);
        Ok(decimal_remainder_is_zero(&numerator_text, &denominator_text))
    }
}

pub fn from_value(value: &Value, limits: Limits) -> Result<Option<Number>, Error> {
    match value {
        Value::NumberString(lexeme) => Number::parse(lexeme.as_bytes(), limits).map(Some),
        Value::Integer(integer) => {
            let lexeme = integer.to_string();
            Number::parse(lexeme.as_bytes(), limits).map(Some)
        }
        // A float has already lost its source lexeme and may have rounded before
        // reaching the validator. Callers that parse JSON must retain numbers as
        // number_string; integer remains exact for programmatic values.
        Value::Float(_) => Err(Error::InvalidNumber),
        _ => Ok(None),
    }
}

fn magnitude_order(left: &Number, right: &Number) -> Ordering {
    let left_places = (left.digits.len() as i64) + left.exponent;
    let right_places = (right.digits.len() as i64) + right.exponent;
    if left_places < right_places {
        return Ordering::Less;
    }
    if left_places > right_places {
        return Ordering::Greater;
    }
    let max_digits = left.digits.len().max(right.digits.len());
    for index in 0..max_digits {
        let left_digit = if index < left.digits.len() {
            left.digits[index]
        } else {
            b'0'
        };
        let right_digit = if index < right.digits.len() {
            right.digits[index]
        } else {
            b'0'
        };
        if left_digit < right_digit {
            return Ordering::Less;
        }
        if left_digit > right_digit {
            return Ordering::Greater;
        }
    }
    Ordering::Equal
}

fn expanded_digits(digits: &[u8], zero_count: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(digits.len() + zero_count);
    output.extend_from_slice(digits);
    output.resize(digits.len() + zero_count, b'0');
    output
}

fn trim_leading_zeros(digits: &mut Vec<u8>) {
    let mut start = 0;
    while start + 1 < digits.len() && digits[start] == b'0' {
        start += 1;
    }
    if start > 0 {
        digits.drain(0..start);
    }
    if digits.is_empty() {
        digits.push(b'0');
    }
}

fn decimal_cmp(left: &[u8], right: &[u8]) -> Ordering {
    if left.len() != right.len() {
        return left.len().cmp(&right.len());
    }
    left.cmp(right)
}

fn decimal_sub(left: &[u8], right: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(left.len());
    let mut borrow = 0i32;
    let mut i = left.len();
    let mut j = right.len();
    while i > 0 {
        i -= 1;
        let mut digit = (left[i] - b'0') as i32 - borrow;
        if j > 0 {
            j -= 1;
            digit -= (right[j] - b'0') as i32;
        }
        if digit < 0 {
            digit += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        result.push(b'0' + digit as u8);
    }
    result.reverse();
    trim_leading_zeros(&mut result);
    result
}

fn decimal_mul_small(digits: &[u8], multiplier: u8) -> Vec<u8> {
    if multiplier == 0 {
        return vec![b'0'];
    }
    let mut result = Vec::with_capacity(digits.len() + 1);
    let mut carry = 0u32;
    let multiplier = multiplier as u32;
    for i in (0..digits.len()).rev() {
        let product = (digits[i] - b'0') as u32 * multiplier + carry;
        result.push(b'0' + (product % 10) as u8);
        carry = product / 10;
    }
    while carry > 0 {
        result.push(b'0' + (carry % 10) as u8);
        carry /= 10;
    }
    result.reverse();
    result
}

fn decimal_remainder_is_zero(numerator: &[u8], denominator: &[u8]) -> bool {
    let mut remainder: Vec<u8> = Vec::new();
    for &digit in numerator {
        remainder.push(digit);
        trim_leading_zeros(&mut remainder);
        let mut quotient_digit = 9u8;
        while quotient_digit > 0 {
            let product = decimal_mul_small(denominator, quotient_digit);
            if decimal_cmp(&remainder, &product) != Ordering::Less {
                remainder = decimal_sub(&remainder, &product);
                break;
            }
            quotient_digit -= 1;
        }
        if remainder.is_empty() {
            remainder.push(b'0');
        }
    }
    trim_leading_zeros(&mut remainder);
    remainder.len() == 1 && remainder[0] == b'0'
}

#[cfg(test)]
mod tests {
    use super::{from_value, Number};
    use super::Limits;
    use std::cmp::Ordering;

    fn expect_equivalent(left: &str, right: &str) {
        let left_number = Number::parse(left.as_bytes(), Limits::default()).unwrap();
        let right_number = Number::parse(right.as_bytes(), Limits::default()).unwrap();
        assert!(left_number.eql(&right_number));
    }

    #[test]
    fn exact_json_numbers_normalize_signed_zero_exponents_and_high_precision() {
        expect_equivalent("0", "-0.0e+999");
        expect_equivalent("1", "1.000e0");
        expect_equivalent("9007199254740993", "9.007199254740993e15");
        expect_equivalent(
            "0.12345678901234567890123456789",
            "12345678901234567890123456789e-29",
        );

        let lower = Number::parse(b"9007199254740992", Limits::default()).unwrap();
        let upper = Number::parse(b"9007199254740993", Limits::default()).unwrap();
        assert_eq!(Ordering::Less, lower.order(&upper));
    }

    #[test]
    fn exact_json_numbers_detect_integers_and_multiple_of_without_floating_point() {
        let integer = Number::parse(b"1e3", Limits::default()).unwrap();
        let fraction = Number::parse(b"1e-3", Limits::default()).unwrap();
        assert!(integer.is_integer());
        assert!(!fraction.is_integer());

        let value = Number::parse(b"0.300000000000000000000000000000", Limits::default()).unwrap();
        let divisor = Number::parse(b"0.1", Limits::default()).unwrap();
        assert!(value.is_multiple_of(&divisor, Limits::default()).unwrap());
        let not_multiple =
            Number::parse(b"0.300000000000000000000000000001", Limits::default()).unwrap();
        assert!(!not_multiple
            .is_multiple_of(&divisor, Limits::default())
            .unwrap());
    }

    #[test]
    fn exact_json_numbers_order_signed_magnitudes_around_zero() {
        let zero = Number::parse(b"0", Limits::default()).unwrap();
        let positive = Number::parse(b"0.0000000000000000000000000001", Limits::default()).unwrap();
        let negative = Number::parse(b"-0.0000000000000000000000000001", Limits::default()).unwrap();

        assert_eq!(Ordering::Less, zero.order(&positive));
        assert_eq!(Ordering::Greater, zero.order(&negative));
        assert_eq!(Ordering::Greater, positive.order(&zero));
        assert_eq!(Ordering::Less, negative.order(&zero));
    }

    #[test]
    fn from_value_rejects_rounded_programmatic_floats() {
        let value = super::Value::Float(9_007_199_254_740_992.0);
        assert!(matches!(
            from_value(&value, Limits::default()),
            Err(super::Error::InvalidNumber)
        ));
    }
}
