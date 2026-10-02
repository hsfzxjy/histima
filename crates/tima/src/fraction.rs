use std::cmp::Ordering;
use std::error::Error;
use std::fmt;

/// An exact, canonical rational value for outer orchestration.
///
/// The denominator is always positive and the pair is reduced. The initial
/// surface constructor accepts `i64` values, so arithmetic also keeps the
/// reduced denominator within the positive `i64` range. Keeping the field
/// unsigned makes the sign representation unambiguous without exposing an
/// inner-language or ABI type prematurely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fraction {
    numerator: i64,
    denominator: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FractionError {
    NonPositiveDenominator,
    DivisionByZero,
    Overflow,
}

impl Fraction {
    pub fn new(numerator: i64, denominator: i64) -> Result<Self, FractionError> {
        if denominator <= 0 {
            return Err(FractionError::NonPositiveDenominator);
        }
        Self::from_ratio(i128::from(numerator), denominator as u128)
    }

    pub fn numerator(self) -> i64 {
        self.numerator
    }

    pub fn denominator(self) -> u64 {
        self.denominator
    }

    pub fn checked_add(self, other: Self) -> Result<Self, FractionError> {
        let left = i128::from(self.numerator)
            .checked_mul(i128::from(other.denominator))
            .ok_or(FractionError::Overflow)?;
        let right = i128::from(other.numerator)
            .checked_mul(i128::from(self.denominator))
            .ok_or(FractionError::Overflow)?;
        let numerator = left.checked_add(right).ok_or(FractionError::Overflow)?;
        let denominator = u128::from(self.denominator)
            .checked_mul(u128::from(other.denominator))
            .ok_or(FractionError::Overflow)?;
        Self::from_ratio(numerator, denominator)
    }

    pub fn checked_sub(self, other: Self) -> Result<Self, FractionError> {
        let left = i128::from(self.numerator)
            .checked_mul(i128::from(other.denominator))
            .ok_or(FractionError::Overflow)?;
        let right = i128::from(other.numerator)
            .checked_mul(i128::from(self.denominator))
            .ok_or(FractionError::Overflow)?;
        let numerator = left.checked_sub(right).ok_or(FractionError::Overflow)?;
        let denominator = u128::from(self.denominator)
            .checked_mul(u128::from(other.denominator))
            .ok_or(FractionError::Overflow)?;
        Self::from_ratio(numerator, denominator)
    }

    pub fn checked_mul(self, other: Self) -> Result<Self, FractionError> {
        let numerator = i128::from(self.numerator)
            .checked_mul(i128::from(other.numerator))
            .ok_or(FractionError::Overflow)?;
        let denominator = u128::from(self.denominator)
            .checked_mul(u128::from(other.denominator))
            .ok_or(FractionError::Overflow)?;
        Self::from_ratio(numerator, denominator)
    }

    pub fn checked_div(self, other: Self) -> Result<Self, FractionError> {
        if other.numerator == 0 {
            return Err(FractionError::DivisionByZero);
        }
        let mut numerator = i128::from(self.numerator)
            .checked_mul(i128::from(other.denominator))
            .ok_or(FractionError::Overflow)?;
        if other.numerator < 0 {
            numerator = numerator.checked_neg().ok_or(FractionError::Overflow)?;
        }
        let denominator = u128::from(self.denominator)
            .checked_mul(u128::from(other.numerator.unsigned_abs()))
            .ok_or(FractionError::Overflow)?;
        Self::from_ratio(numerator, denominator)
    }

    /// Converts the exact rational to the nearest IEEE-754 `f32`, resolving
    /// halfway cases toward the value whose significand is even.
    pub fn to_f32(self) -> f32 {
        if self.numerator == 0 {
            return 0.0;
        }

        let negative = self.numerator < 0;
        let numerator = u128::from(self.numerator.unsigned_abs());
        let denominator = u128::from(self.denominator);
        let numerator_log = 127 - numerator.leading_zeros() as i32;
        let denominator_log = 127 - denominator.leading_zeros() as i32;
        let mut exponent = numerator_log - denominator_log;
        let at_least_power = if exponent >= 0 {
            numerator >= denominator << exponent
        } else {
            numerator << -exponent >= denominator
        };
        if !at_least_power {
            exponent -= 1;
        }

        let shift = 23 - exponent;
        let (scaled_numerator, scaled_denominator) = if shift >= 0 {
            (numerator << shift, denominator)
        } else {
            (numerator, denominator << -shift)
        };
        let mut significand = scaled_numerator / scaled_denominator;
        let remainder = scaled_numerator % scaled_denominator;
        let twice_remainder = remainder * 2;
        if twice_remainder > scaled_denominator
            || (twice_remainder == scaled_denominator && significand & 1 == 1)
        {
            significand += 1;
        }
        if significand == 1 << 24 {
            significand >>= 1;
            exponent += 1;
        }

        let sign = u32::from(negative) << 31;
        let biased_exponent = u32::try_from(exponent + 127)
            .expect("i64/u64 fractions are always normal finite f32 values");
        let mantissa = u32::try_from(significand - (1 << 23))
            .expect("rounded f32 significand fits its mantissa");
        f32::from_bits(sign | (biased_exponent << 23) | mantissa)
    }

    fn from_ratio(numerator: i128, denominator: u128) -> Result<Self, FractionError> {
        debug_assert_ne!(denominator, 0);
        if numerator == 0 {
            return Ok(Self {
                numerator: 0,
                denominator: 1,
            });
        }
        let divisor = gcd(numerator.unsigned_abs(), denominator);
        let numerator = numerator / i128::try_from(divisor).map_err(|_| FractionError::Overflow)?;
        let denominator = denominator / divisor;
        let numerator = i64::try_from(numerator).map_err(|_| FractionError::Overflow)?;
        if denominator > i64::MAX as u128 {
            return Err(FractionError::Overflow);
        }
        Ok(Self {
            numerator,
            denominator: denominator as u64,
        })
    }
}

impl Ord for Fraction {
    fn cmp(&self, other: &Self) -> Ordering {
        let left = i128::from(self.numerator) * i128::from(other.denominator);
        let right = i128::from(other.numerator) * i128::from(self.denominator);
        left.cmp(&right)
    }
}

impl PartialOrd for Fraction {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Fraction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "fraction({}, {})",
            self.numerator, self.denominator
        )
    }
}

impl fmt::Display for FractionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NonPositiveDenominator => "fraction denominator must be positive",
            Self::DivisionByZero => "fraction division by zero",
            Self::Overflow => "fraction arithmetic overflow",
        })
    }
}

impl Error for FractionError {}

fn gcd(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

#[cfg(test)]
mod tests {
    use super::{Fraction, FractionError};

    #[test]
    fn fractions_are_canonical_and_checked() {
        assert_eq!(Fraction::new(2, 4).unwrap(), Fraction::new(1, 2).unwrap());
        assert_eq!(Fraction::new(0, 99).unwrap().denominator(), 1);
        assert_eq!(
            Fraction::new(1, 0),
            Err(FractionError::NonPositiveDenominator)
        );
        assert_eq!(
            Fraction::new(1, -2),
            Err(FractionError::NonPositiveDenominator)
        );
    }

    #[test]
    fn arithmetic_reduces_exact_results() {
        let half = Fraction::new(1, 2).unwrap();
        let third = Fraction::new(1, 3).unwrap();
        assert_eq!(
            half.checked_add(third).unwrap(),
            Fraction::new(5, 6).unwrap()
        );
        assert_eq!(
            half.checked_sub(third).unwrap(),
            Fraction::new(1, 6).unwrap()
        );
        assert_eq!(
            half.checked_mul(third).unwrap(),
            Fraction::new(1, 6).unwrap()
        );
        assert_eq!(
            half.checked_div(third).unwrap(),
            Fraction::new(3, 2).unwrap()
        );
        assert_eq!(
            half.checked_div(Fraction::new(0, 1).unwrap()),
            Err(FractionError::DivisionByZero)
        );
        assert_eq!(
            Fraction::new(i64::MAX, 1)
                .unwrap()
                .checked_add(Fraction::new(1, 1).unwrap()),
            Err(FractionError::Overflow)
        );
        assert_eq!(
            Fraction::new(1, i64::MAX)
                .unwrap()
                .checked_mul(Fraction::new(1, 2).unwrap()),
            Err(FractionError::Overflow)
        );
    }

    #[test]
    fn f32_conversion_rounds_halfway_to_even() {
        assert_eq!(Fraction::new(1, 3).unwrap().to_f32(), 1.0f32 / 3.0);
        assert_eq!(
            Fraction::new(16_777_217, 16_777_216)
                .unwrap()
                .to_f32()
                .to_bits(),
            1.0f32.to_bits()
        );
        assert_eq!(
            Fraction::new(16_777_219, 16_777_216)
                .unwrap()
                .to_f32()
                .to_bits(),
            1.000_000_2f32.to_bits()
        );
        assert_eq!(Fraction::new(-1, 2).unwrap().to_f32(), -0.5);
    }
}
