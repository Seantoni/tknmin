//! Monetary values as integer minor units.
//!
//! Binary floating point is never used for money anywhere in this application.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    #[error("currency must be a 3-letter ISO 4217 code, got {0:?}")]
    InvalidCurrency(String),
    #[error("minor unit exponent {0} is out of the supported range 0..=6")]
    InvalidExponent(u8),
    #[error("cannot combine {left} with {right}")]
    MismatchedCurrency { left: String, right: String },
    #[error("monetary amount overflowed")]
    Overflow,
}

/// An exact monetary amount.
///
/// `amount_minor` is expressed in units of `10^-minor_unit_exponent` of the
/// currency, so `USD 1.25` is `amount_minor: 125, minor_unit_exponent: 2`.
/// Token pricing is often quoted per million tokens, so exponents beyond the
/// currency's usual two digits are permitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Money {
    pub amount_minor: i64,
    /// Uppercase ISO 4217 alphabetic code.
    pub currency: String,
    pub minor_unit_exponent: u8,
}

impl Money {
    pub const MAX_EXPONENT: u8 = 6;

    pub fn new(amount_minor: i64, currency: &str, minor_unit_exponent: u8) -> Result<Self, MoneyError> {
        let currency = normalize_currency(currency)?;
        if minor_unit_exponent > Self::MAX_EXPONENT {
            return Err(MoneyError::InvalidExponent(minor_unit_exponent));
        }
        Ok(Self { amount_minor, currency, minor_unit_exponent })
    }

    /// Re-express the amount at a finer scale so two values can be combined.
    pub fn rescaled(&self, exponent: u8) -> Result<Self, MoneyError> {
        if exponent > Self::MAX_EXPONENT {
            return Err(MoneyError::InvalidExponent(exponent));
        }
        if exponent < self.minor_unit_exponent {
            // Downscaling would discard precision, so it is not offered.
            return Err(MoneyError::InvalidExponent(exponent));
        }
        let steps = u32::from(exponent - self.minor_unit_exponent);
        let factor = 10i64.checked_pow(steps).ok_or(MoneyError::Overflow)?;
        let amount_minor = self.amount_minor.checked_mul(factor).ok_or(MoneyError::Overflow)?;
        Ok(Self { amount_minor, currency: self.currency.clone(), minor_unit_exponent: exponent })
    }

    /// Add two amounts of the same currency, aligning their scales first.
    pub fn checked_add(&self, other: &Money) -> Result<Money, MoneyError> {
        if self.currency != other.currency {
            return Err(MoneyError::MismatchedCurrency {
                left: self.currency.clone(),
                right: other.currency.clone(),
            });
        }
        let exponent = self.minor_unit_exponent.max(other.minor_unit_exponent);
        let left = self.rescaled(exponent)?;
        let right = other.rescaled(exponent)?;
        let amount_minor = left.amount_minor.checked_add(right.amount_minor).ok_or(MoneyError::Overflow)?;
        Ok(Money { amount_minor, currency: left.currency, minor_unit_exponent: exponent })
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let exponent = usize::from(self.minor_unit_exponent);
        if exponent == 0 {
            return write!(f, "{} {}", self.amount_minor, self.currency);
        }
        let divisor = 10i64.pow(self.minor_unit_exponent as u32);
        let sign = if self.amount_minor < 0 { "-" } else { "" };
        let magnitude = self.amount_minor.unsigned_abs();
        let whole = magnitude / divisor as u64;
        let fraction = magnitude % divisor as u64;
        write!(f, "{sign}{whole}.{fraction:0>exponent$} {}", self.currency)
    }
}

fn normalize_currency(currency: &str) -> Result<String, MoneyError> {
    let trimmed = currency.trim();
    let is_iso_shape = trimmed.len() == 3 && trimmed.chars().all(|c| c.is_ascii_alphabetic());
    if !is_iso_shape {
        return Err(MoneyError::InvalidCurrency(currency.to_string()));
    }
    Ok(trimmed.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uppercases_and_validates_currency() {
        assert_eq!(Money::new(125, "usd", 2).unwrap().currency, "USD");
        assert!(matches!(Money::new(1, "US", 2), Err(MoneyError::InvalidCurrency(_))));
        assert!(matches!(Money::new(1, "US1", 2), Err(MoneyError::InvalidCurrency(_))));
    }

    #[test]
    fn adds_across_different_scales() {
        let cents = Money::new(125, "USD", 2).unwrap();
        let micros = Money::new(1_500, "USD", 6).unwrap();
        let sum = cents.checked_add(&micros).unwrap();
        assert_eq!(sum, Money::new(1_251_500, "USD", 6).unwrap());
    }

    #[test]
    fn refuses_to_mix_currencies() {
        let usd = Money::new(1, "USD", 2).unwrap();
        let eur = Money::new(1, "EUR", 2).unwrap();
        assert!(matches!(usd.checked_add(&eur), Err(MoneyError::MismatchedCurrency { .. })));
    }

    #[test]
    fn formats_with_its_own_scale() {
        assert_eq!(Money::new(125, "USD", 2).unwrap().to_string(), "1.25 USD");
        assert_eq!(Money::new(-5, "USD", 2).unwrap().to_string(), "-0.05 USD");
        assert_eq!(Money::new(7, "JPY", 0).unwrap().to_string(), "7 JPY");
    }
}
