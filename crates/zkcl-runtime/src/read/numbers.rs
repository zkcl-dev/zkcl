use std::num::IntErrorKind;
use std::rc::Rc;

use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{ToPrimitive, Zero};

use crate::form_value::name_of_word;
use crate::heap::Word;
use crate::number::{Number, Real};
use crate::symbol::potential_number;
use crate::{EvalError, Runtime};

use super::{Form, FormKind, Token, features};

pub(super) fn read_base(runtime: &Runtime, token: &Token<'_>) -> Result<u32, EvalError> {
    features::special_value(runtime, "*READ-BASE*", token.at)?
        .as_fixnum()
        .filter(|base| (2..=36).contains(base))
        .map(|base| base as u32)
        .ok_or_else(|| token.error("*READ-BASE* requires an integer radix from 2 to 36"))
}

/// Names matching numeric or reserved token syntax need symbol escapes.
pub(crate) fn numeric_or_reserved(text: &str, base: u32) -> bool {
    potential_number(text, base) || FloatSyntax::parse(text).is_some()
}

pub(super) fn atom(
    runtime: &Runtime,
    token: &Token<'_>,
    read_base: impl FnOnce() -> Result<u32, EvalError>,
) -> Result<Option<FormKind>, EvalError> {
    let text = token.text.as_ref();
    // Escaped constituents and package markers disqualify a numeric token.
    if text.contains(['\\', '|', ':']) {
        return Ok(None);
    }
    let base = read_base()?;
    // Decimal and explicit-radix digits can start numbers. With the
    // exception of sign, dot and caret, any other leading
    // constituent makes this a symbol before its suffix matters.
    if let Some(&first) = text.as_bytes().first()
        && !first.is_ascii_digit()
        && !char::from(first).is_digit(base)
        && !matches!(first, b'+' | b'-' | b'.' | b'^')
    {
        return Ok(None);
    }
    if let Some(number) = rational(runtime, token, text, base)? {
        return Ok(Some(number));
    }
    if let Some(float) = FloatSyntax::parse(text) {
        return float.read(runtime, token).map(Some);
    }
    if potential_number(text, base) && !has_interior_symbol_sign(text, base) {
        return Err(token.error("invalid numeric token"));
    }
    Ok(None)
}

fn has_interior_symbol_sign(text: &str, base: u32) -> bool {
    let bytes = text.as_bytes();
    bytes.iter().enumerate().skip(1).any(|(i, &b)| {
        matches!(b, b'+' | b'-') && {
            let prev = bytes[i - 1];
            let is_exp_marker =
                matches!(prev.to_ascii_lowercase(), b'd' | b'e' | b'f' | b'l' | b's')
                    && !char::from(prev).is_digit(base);
            !is_exp_marker
        }
    })
}

pub(super) fn explicit_radix(runtime: &Runtime, token: &Token<'_>) -> Result<FormKind, EvalError> {
    // The scanner emits the dispatch prefix, parameter, and payload as one atom.
    let body = &token.text[1..];
    let prefix_len = body.bytes().take_while(u8::is_ascii_digit).count();
    let (prefix, body) = body.split_at(prefix_len);
    let (operator, rational_text) = body.split_at(1);
    let base = match operator.as_bytes()[0].to_ascii_uppercase() {
        b'B' if prefix.is_empty() => 2,
        b'O' if prefix.is_empty() => 8,
        b'X' if prefix.is_empty() => 16,
        b'R' => prefix
            .parse::<u32>()
            .ok()
            .filter(|base| (2..=36).contains(base))
            .ok_or_else(|| token.error("#R requires a decimal radix from 2 to 36"))?,
        _ => return Err(token.error("this radix dispatch does not accept a numeric argument")),
    };
    rational(runtime, token, rational_text, base)?
        .ok_or_else(|| token.error("expected a rational number after radix dispatch"))
}

pub(super) fn complex(
    runtime: &Runtime,
    token: &Token<'_>,
    operand: &Form,
) -> Result<FormKind, EvalError> {
    let Some([real, imaginary]) = operand.list() else {
        return Err(EvalError::read(
            operand.at,
            "#C requires a list of two real numbers",
        ));
    };
    let number =
        crate::vm::numbers::complex(real_part(runtime, real)?, real_part(runtime, imaginary)?)
            .map_err(|class| {
                EvalError::read_span(
                    token.at..token.end,
                    format!("cannot construct a complex literal: {}", class.name()),
                )
            })?;
    number_form(runtime, token.at, number)
}

fn real_part(runtime: &Runtime, form: &Form) -> Result<Real, EvalError> {
    match &form.kind {
        FormKind::Integer(value) => return Ok(Real::Integer(BigInt::from(*value))),
        FormKind::Literal(root) => {
            if let Some(Number::Real(real)) = runtime.heap.number(root.get()) {
                return Ok(real);
            }
        }
        _ => {}
    }
    Err(EvalError::read(
        form.at,
        "#C components must be real numbers",
    ))
}

fn rational(
    runtime: &Runtime,
    token: &Token<'_>,
    text: &str,
    base: u32,
) -> Result<Option<FormKind>, EvalError> {
    let (integer, integer_base) = text.strip_suffix('.').map_or((text, base), |n| (n, 10));
    let large_integer = match isize::from_str_radix(integer, integer_base) {
        Ok(value) if Word::fixnum(value).is_some() => {
            return Ok(Some(FormKind::Integer(value)));
        }
        Ok(value) => Some(BigInt::from(value)),
        // Overflow may occur before the end of the token, and BigInt accepts underscores.
        Err(error)
            if matches!(
                error.kind(),
                IntErrorKind::PosOverflow | IntErrorKind::NegOverflow
            ) && digits(integer.trim_prefix(['+', '-']), integer_base) =>
        {
            Some(
                BigInt::parse_bytes(integer.as_bytes(), integer_base)
                    .expect("a checked integer token"),
            )
        }
        Err(_) => None,
    };
    if let Some(value) = large_integer {
        return number_form(runtime, token.at, Number::Real(Real::Integer(value))).map(Some);
    }
    if let Some((numerator, denominator)) = text.split_once('/')
        && digits(numerator.trim_prefix(['+', '-']), base)
        && digits(denominator, base)
    {
        let numerator =
            BigInt::parse_bytes(numerator.as_bytes(), base).expect("a checked numerator");
        let denominator =
            BigInt::parse_bytes(denominator.as_bytes(), base).expect("a checked denominator");
        if denominator.is_zero() {
            return Err(token.error("a ratio denominator cannot be zero"));
        }
        let ratio = BigRational::new(numerator, denominator);
        let value = if ratio.is_integer() {
            Real::Integer(ratio.into_raw().0)
        } else {
            Real::Ratio(ratio)
        };
        return number_form(runtime, token.at, Number::Real(value)).map(Some);
    }
    Ok(None)
}

fn digits(text: &str, base: u32) -> bool {
    !text.is_empty() && text.bytes().all(|digit| char::from(digit).is_digit(base))
}

fn number_form(runtime: &Runtime, at: usize, value: Number) -> Result<FormKind, EvalError> {
    if let Number::Real(Real::Integer(integer)) = &value
        && let Some(small) = integer.to_isize()
        && Word::fixnum(small).is_some()
    {
        return Ok(FormKind::Integer(small));
    }
    let word = runtime
        .heap
        .allocate_number(&value)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(FormKind::Literal(Rc::new(runtime.root_slot(word))))
}

struct FloatSyntax<'a> {
    mantissa: &'a str,
    exponent: Option<(u8, &'a str)>,
}

impl<'a> FloatSyntax<'a> {
    fn parse(text: &'a str) -> Option<Self> {
        let unsigned = text.trim_prefix(['+', '-']);
        let whole = unsigned.bytes().take_while(u8::is_ascii_digit).count();
        let rest = &unsigned[whole..];
        let (fraction, rest) = if let Some(fractional) = rest.strip_prefix('.') {
            let fraction = fractional.bytes().take_while(u8::is_ascii_digit).count();
            (fraction, &fractional[fraction..])
        } else {
            (0, rest)
        };
        if whole + fraction == 0 {
            return None;
        }
        let exponent = if rest.is_empty() {
            if fraction == 0 {
                return None;
            }
            None
        } else {
            let (&marker, _) = rest.as_bytes().split_first()?;
            let marker = marker.to_ascii_uppercase();
            if !matches!(marker, b'E' | b'S' | b'F' | b'D' | b'L') {
                return None;
            }
            let exponent = &rest[1..];
            if !digits(exponent.trim_prefix(['+', '-']), 10) {
                return None;
            }
            Some((marker, exponent))
        };
        Some(Self {
            mantissa: &text[..text.len() - rest.len()],
            exponent,
        })
    }

    fn read(self, runtime: &Runtime, token: &Token<'_>) -> Result<FormKind, EvalError> {
        let is_double = match self.exponent {
            Some((b'S' | b'F', _)) => false,
            Some((b'D' | b'L', _)) => true,
            _ => {
                let symbol =
                    features::special_value(runtime, "*READ-DEFAULT-FLOAT-FORMAT*", token.at)?;
                let name = name_of_word(runtime, symbol);
                match name.as_ref().map(|name| name.operator()) {
                    Some("SHORT-FLOAT" | "SINGLE-FLOAT") => false,
                    Some("DOUBLE-FLOAT" | "LONG-FLOAT") => true,
                    _ => {
                        return Err(
                            token.error("*READ-DEFAULT-FLOAT-FORMAT* requires a float type")
                        );
                    }
                }
            }
        };
        let normalized;
        let decimal = if let Some((_, exponent)) = self.exponent {
            normalized = format!("{}e{exponent}", self.mantissa);
            &normalized
        } else {
            self.mantissa
        };
        let overflow = || token.error("floating-point literal is outside its format's range");
        let real = if is_double {
            Real::Double(
                decimal
                    .parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite())
                    .ok_or_else(overflow)?,
            )
        } else {
            Real::Single(
                decimal
                    .parse::<f32>()
                    .ok()
                    .filter(|value| value.is_finite())
                    .ok_or_else(overflow)?,
            )
        };
        number_form(runtime, token.at, Number::Real(real))
    }
}
