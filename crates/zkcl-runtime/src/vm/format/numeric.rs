//! FORMAT directives for integers, fixed-point numbers, and scientific notation.
use super::{
    FormatDirective, FormatInterpreter, FormatOutput, PrefixParameterValue, PrefixParameterValues,
    format_error, printer,
};
use crate::{EvalError, condition::Fault, heap::Word};
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{ToPrimitive, Zero};

impl FormatInterpreter<'_> {
    pub(super) fn number(
        &self,
        directive: &FormatDirective,
        mut params: PrefixParameterValues,
        argument: Word,
        output: &mut FormatOutput,
    ) -> Result<(), Fault> {
        let at = self.at;
        let base = match directive.operator {
            'D' => 10,
            'B' => 2,
            'O' => 8,
            'X' => 16,
            'R' => {
                let radix = params.integer(0, at)?;
                let Some(base) = radix else {
                    params.count(1, at)?;
                    return self.english_or_roman(directive, argument, output);
                };
                if !(2..=36).contains(&base) {
                    return Err(format_error(at, "FORMAT ~R radix must be from 2 to 36"));
                }
                params.0.remove(0);
                base as u32
            }
            _ => unreachable!(),
        };
        params.count(4, at)?;
        let width = params.width(0, 0, at)?;
        let padchar = params.character(1, ' ', at)?;
        let integer_digits = if let Some(integer) = argument.as_fixnum() {
            let mut digits = Vec::new();
            let mut number = integer.unsigned_abs();
            loop {
                let digit = (number % base as usize) as u8;
                digits.push(char::from(if digit < 10 {
                    b'0' + digit
                } else {
                    b'A' + digit - 10
                }));
                number /= base as usize;
                if number == 0 {
                    break;
                }
            }
            Some((integer < 0, digits))
        } else {
            self.runtime.heap.integer(argument).map(|integer| {
                let spelling = integer.to_str_radix(base).to_ascii_uppercase();
                let magnitude = spelling.strip_prefix('-');
                let digits = magnitude.unwrap_or(&spelling).chars().rev().collect();
                (magnitude.is_some(), digits)
            })
        };
        let text = if let Some((negative, digits)) = integer_digits {
            let comma = params.character(2, ',', at)?;
            let interval = params.width(3, 3, at)?;
            if directive.colon && interval == 0 {
                return Err(format_error(
                    at,
                    "FORMAT digit group interval must be positive",
                ));
            }
            let mut text = String::new();
            if negative {
                text.push('-');
            } else if directive.atsign {
                text.push('+');
            }
            for (index, digit) in digits.into_iter().enumerate().rev() {
                text.push(digit);
                if directive.colon && index > 0 && index.is_multiple_of(interval) {
                    text.push(comma);
                }
            }
            text
        } else {
            printer::print_text(
                self.runtime,
                printer::PrinterPrimitive::Princ,
                argument,
                Some(base),
                at,
            )?
        };
        output.pad(padchar, width.saturating_sub(text.chars().count()), at)?;
        output.push(&text);
        Ok(())
    }

    /// `~R` without a radix parameter prints an English cardinal, an
    /// English ordinal (with the colon modifier), or a Roman numeral (with
    /// the atsign modifier).  The old-style Roman form is selected by both
    /// modifiers.
    fn english_or_roman(
        &self,
        directive: &FormatDirective,
        argument: Word,
        output: &mut FormatOutput,
    ) -> Result<(), Fault> {
        let at = self.at;
        let integer = self
            .runtime
            .heap
            .integer(argument)
            .and_then(|integer| num_traits::ToPrimitive::to_i64(&integer))
            .ok_or_else(|| {
                Fault::type_error(self.runtime, 
                    argument,
                    "INTEGER",
                    EvalError::new(at, "FORMAT ~R expects an integer"),
                )
            })?;
        let text = if directive.atsign {
            roman_numeral(integer, directive.colon)
                .ok_or_else(|| format_error(at, "FORMAT Roman numeral out of range"))?
        } else if directive.colon {
            english_ordinal(integer)
                .ok_or_else(|| format_error(at, "FORMAT English ordinal out of range"))?
        } else {
            english_cardinal(integer)
                .ok_or_else(|| format_error(at, "FORMAT English cardinal out of range"))?
        };
        output.push(&text);
        Ok(())
    }

    pub(super) fn fixed(
        &self,
        directive: &FormatDirective,
        params: PrefixParameterValues,
        argument: Word,
    ) -> Result<String, Fault> {
        let at = self.at;
        params.count(5, at)?;
        let mincol = params.width(0, 0, at)?;
        let digits = params.integer(1, at)?;
        let scale = params.integer(2, at)?.unwrap_or(0);
        let overflow = params.character_opt(3, at)?;
        let padchar = params.character(4, ' ', at)?;
        let real = self.real_of(argument, at)?;
        let width_supplied = params
            .0
            .first()
            .is_some_and(|value| !matches!(value, PrefixParameterValue::Omitted));
        let (negative, body) = match digits {
            None if !width_supplied => free_fixed(&real, scale),
            None => {
                let digits = (mincol as i64 - 2).max(-1);
                let value = to_ratio(&real) * pow10(scale);
                let negative = value < BigRational::zero();
                (negative, fitted_body(&value, digits, mincol))
            }
            Some(digits) => {
                let digits = usize::try_from(digits.max(0)).unwrap_or(0);
                let value = to_ratio(&real) * pow10(scale);
                let negative = value < BigRational::zero();
                (negative, decimal_body(&value, digits))
            }
        };
        let sign = number_sign(negative, directive.atsign);
        let mut body = body;
        if mincol > 0
            && body.chars().count() + usize::from(sign.is_some()) > mincol
            && body.starts_with("0.")
        {
            body.remove(0);
        }
        Ok(signed_field(sign, &body, mincol, overflow, padchar))
    }

    pub(super) fn exponential(
        &self,
        directive: &FormatDirective,
        params: PrefixParameterValues,
        argument: Word,
    ) -> Result<String, Fault> {
        let at = self.at;
        params.count(7, at)?;
        let mincol = params.width(0, 0, at)?;
        let width_supplied = params
            .0
            .first()
            .is_some_and(|value| !matches!(value, PrefixParameterValue::Omitted));
        let digits = params.integer(1, at)?;
        let exponent_digits = params
            .integer(2, at)?
            .map(|e| usize::try_from(e.max(0)).unwrap_or(0));
        let scale = params.integer(3, at)?.unwrap_or(1);
        if !width_supplied && digits.is_none() && exponent_digits.is_none() && scale == 1 {
            let text = printer::print_text(
                self.runtime,
                printer::PrinterPrimitive::Princ,
                argument,
                None,
                at,
            )?;
            return Ok(exponential_with_sign(&text));
        }
        let overflow = params.character_opt(4, at)?;
        let padchar = params.character(5, ' ', at)?;
        let marker = params.character_opt(6, at)?.unwrap_or('e');
        let real = self.real_of(argument, at)?;
        let value = match &real {
            crate::number::Real::Single(_) | crate::number::Real::Double(_) => {
                crate::vm::numbers::rationalize_real(&real)
            }
            _ => to_ratio(&real),
        };
        let negative = value < BigRational::zero();
        let magnitude = num_traits::Signed::abs(&value);
        let (mut mantissa, exponent) = if magnitude.is_zero() {
            let body = match digits {
                Some(digits) => decimal_body(
                    &BigRational::zero(),
                    usize::try_from(digits.max(0)).unwrap_or(0),
                ),
                None => "0.0".to_string(),
            };
            (body, 0)
        } else {
            let mut exponent = decimal_exponent(&magnitude) - scale + 1;
            let mantissa = &magnitude / pow10(exponent);
            let body = match digits {
                Some(digits) => {
                    let fraction = if scale > 0 {
                        (1 + digits - scale).max(0)
                    } else {
                        digits.max(0)
                    };
                    let significant_digits = scale + fraction;
                    let fraction = usize::try_from(fraction).unwrap_or(0);
                    let mut rounded = rounded_decimal_digits(&mantissa, fraction);
                    if significant_digits > 0 && rounded.len() as i64 > significant_digits {
                        // A carry produces a power of ten. Remove its trailing
                        // zero and increment the exponent to preserve the scale.
                        rounded.pop();
                        exponent += 1;
                    }
                    place_decimal_point(rounded, fraction)
                }
                None => trimmed_digits(&mantissa),
            };
            (body, exponent)
        };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        let exponent_width = exponent_digits.unwrap_or(0);
        let exponent_text = format!(
            "{marker}{exponent_sign}{:0exponent_width$}",
            exponent.unsigned_abs()
        );
        let sign = number_sign(negative, directive.atsign);
        let used =
            mantissa.chars().count() + exponent_text.chars().count() + usize::from(sign.is_some());
        if digits.is_none() && mantissa.ends_with('.') && (!width_supplied || used < mincol) {
            mantissa.push('0');
        }
        mantissa.push_str(&exponent_text);
        Ok(signed_field(sign, &mantissa, mincol, overflow, padchar))
    }

    fn real_of(&self, argument: Word, at: usize) -> Result<crate::number::Real, Fault> {
        match self.runtime.heap.number(argument) {
            Some(crate::number::Number::Real(real)) => Ok(real),
            _ => Err(Fault::type_error(self.runtime, 
                argument,
                "REAL",
                EvalError::new(at, "FORMAT expects a real number"),
            )),
        }
    }
}

fn number_sign(negative: bool, force_sign: bool) -> Option<char> {
    if negative {
        Some('-')
    } else {
        force_sign.then_some('+')
    }
}

fn signed_field(
    sign: Option<char>,
    body: &str,
    width: usize,
    overflow: Option<char>,
    pad: char,
) -> String {
    let used = body.chars().count() + usize::from(sign.is_some());
    if let Some(fill) = overflow
        && width > 0
        && used > width
    {
        return std::iter::repeat_n(fill, width).collect();
    }
    let mut text: String = std::iter::repeat_n(pad, width.saturating_sub(used)).collect();
    text.extend(sign);
    text.push_str(body);
    text
}

/// Position of the leading decimal digit of a positive magnitude.
fn decimal_exponent(value: &BigRational) -> i64 {
    let mut exponent = {
        let approximate = value.to_f64().unwrap_or(f64::INFINITY);
        if approximate.is_finite() && approximate > 0.0 {
            approximate.log10().floor() as i64
        } else {
            let digits = value.numer().to_string().len() as i64;
            let divisor = value.denom().to_string().len() as i64;
            digits - divisor
        }
    };
    while value < &pow10(exponent) {
        exponent -= 1;
    }
    while value >= &pow10(exponent + 1) {
        exponent += 1;
    }
    exponent
}

/// Shortest representation of a magnitude in the range [1, 10).
fn trimmed_digits(mantissa: &BigRational) -> String {
    let digits = shortest_digits(mantissa.to_f64().unwrap_or(0.0));
    if digits.is_empty() || digits == "0" {
        return "0.0".into();
    }
    let mut text = String::new();
    text.push(digits.as_bytes()[0] as char);
    text.push('.');
    if digits.len() > 1 {
        text.push_str(&digits[1..]);
    }
    text
}

/// Render a real in ordinary free fixed-format notation.
fn free_fixed(real: &crate::number::Real, scale: i64) -> (bool, String) {
    use crate::number::Real;
    use num_traits::Signed;
    let negative = match real {
        Real::Integer(value) => value.is_negative(),
        Real::Ratio(value) => value.is_negative(),
        Real::Single(value) => value.is_sign_negative(),
        Real::Double(value) => value.is_sign_negative(),
    };
    let exponent = i32::try_from(scale).unwrap_or(0);
    let mut body = match real {
        Real::Single(value) => format!("{}", (value * 10f32.powi(exponent)).abs()),
        Real::Double(value) => format!("{}", (value * 10f64.powi(exponent)).abs()),
        Real::Integer(value) => {
            let scaled = BigRational::from_integer(value.clone()) * pow10(scale);
            scaled.to_f64().unwrap_or(f64::INFINITY).abs().to_string()
        }
        Real::Ratio(value) => {
            let scaled = value * pow10(scale);
            if scaled.is_integer() {
                format!("{}.0", scaled.to_integer().abs())
            } else {
                format!("{}", (to_f64(&Real::Ratio(scaled))).abs())
            }
        }
    };
    if !body.contains('.') {
        body.push_str(".0");
    }
    (negative, body)
}

/// Render the body of ~F when W is supplied and D is omitted: as many
/// fractional digits as fit the requested width, with trailing zero digits
/// suppressed except for a single zero when the fraction rounds to zero.
fn fitted_body(value: &BigRational, digits: i64, width: usize) -> String {
    use num_traits::Signed;
    let magnitude = value.abs();
    let scaled = if digits >= 0 {
        magnitude * pow10(digits)
    } else {
        magnitude / pow10(-digits)
    };
    let rounded = round_half_even(&scaled);
    let mut body = if digits <= 0 {
        format!("{rounded}.0")
    } else {
        let digits = digits as usize;
        let mut ds = rounded.to_string();
        if ds.len() <= digits {
            ds = format!("{}{}", "0".repeat(digits + 1 - ds.len()), ds);
        }
        let split = ds.len() - digits;
        let integer = &ds[..split];
        let mut fraction = ds[split..].to_string();
        while fraction.len() > 1 && fraction.ends_with('0') {
            fraction.pop();
        }
        format!("{integer}.{fraction}")
    };
    if body.starts_with("0.") && width as i64 == digits + 1 {
        body.remove(0);
    }
    body
}

/// Turn PRIN1 exponential output into ~E output by forcing an exponent sign.
fn exponential_with_sign(text: &str) -> String {
    let marker = text.char_indices().find(|&(_, character)| {
        matches!(
            character,
            'e' | 'E' | 'd' | 'D' | 'f' | 'F' | 's' | 'S' | 'l' | 'L'
        )
    });
    let Some((index, character)) = marker else {
        return text.to_string();
    };
    let (mantissa, rest) = text.split_at(index);
    let exponent = &rest[character.len_utf8()..];
    if exponent.starts_with(['+', '-']) {
        text.to_string()
    } else {
        format!("{mantissa}{character}+{exponent}")
    }
}

/// Exact rational value of a real number.
fn to_ratio(real: &crate::number::Real) -> num_rational::BigRational {
    use crate::number::Real;
    use num_rational::BigRational;
    match real {
        Real::Integer(value) => BigRational::from_integer(value.clone()),
        Real::Ratio(value) => value.clone(),
        Real::Single(value) => {
            BigRational::from_float(f64::from(*value)).expect("finite single float")
        }
        Real::Double(value) => BigRational::from_float(*value).expect("finite double float"),
    }
}

fn to_f64(real: &crate::number::Real) -> f64 {
    use crate::number::Real;
    use num_traits::ToPrimitive;
    match real {
        Real::Integer(value) => value.to_f64().unwrap_or(f64::INFINITY),
        Real::Ratio(value) => value.to_f64().unwrap_or(f64::INFINITY),
        Real::Single(value) => f64::from(*value),
        Real::Double(value) => *value,
    }
}

fn pow10(exponent: i64) -> num_rational::BigRational {
    use num_bigint::BigInt;
    use num_rational::BigRational;
    let magnitude = u32::try_from(exponent.unsigned_abs()).unwrap_or(u32::MAX);
    let power = BigInt::from(10u8).pow(magnitude);
    if exponent < 0 {
        BigRational::new(BigInt::from(1u8), power)
    } else {
        BigRational::from_integer(power)
    }
}

/// Round to the nearest integer, breaking ties towards even.
fn round_half_even(value: &num_rational::BigRational) -> num_bigint::BigInt {
    use num_integer::Integer;
    use num_traits::{One, Signed, Zero};
    let numerator = value.numer();
    let denominator = value.denom();
    let negative = value.is_negative();
    let abs = Signed::abs(numerator);
    let quotient = &abs / denominator;
    let remainder = &abs % denominator;
    let doubled = &remainder * BigInt::from(2u8);
    let rounded = match doubled.cmp(denominator) {
        std::cmp::Ordering::Less => quotient,
        std::cmp::Ordering::Greater => quotient + BigInt::one(),
        std::cmp::Ordering::Equal => match quotient.is_even() {
            true => quotient,
            false => quotient + BigInt::one(),
        },
    };
    if negative && !rounded.is_zero() {
        -rounded
    } else {
        rounded
    }
}

fn decimal_body(value: &num_rational::BigRational, digits: usize) -> String {
    place_decimal_point(rounded_decimal_digits(value, digits), digits)
}

fn rounded_decimal_digits(value: &BigRational, fraction: usize) -> String {
    use num_traits::Signed;
    let scaled = (value * pow10(fraction as i64)).abs();
    round_half_even(&scaled).to_string()
}

fn place_decimal_point(mut digits: String, fraction: usize) -> String {
    if digits.len() <= fraction {
        digits = format!("{}{}", "0".repeat(fraction + 1 - digits.len()), digits);
    }
    digits.insert(digits.len() - fraction, '.');
    digits
}

/// The significant digits of the shortest scientific representation.
fn shortest_digits(value: f64) -> String {
    let text = format!("{:e}", value.abs());
    let (mantissa, _) = text.split_once('e').expect("scientific notation");
    let digits: String = mantissa.chars().filter(|&c| c != '.').collect();
    digits.trim_end_matches('0').to_owned()
}

const ENGLISH_UNITS: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];

const ENGLISH_TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

/// English cardinal names for the magnitudes ANSI examples require.
fn english_cardinal(value: i64) -> Option<String> {
    if value < 0 {
        let magnitude = value.checked_neg()?;
        return english_cardinal(magnitude).map(|name| format!("negative {name}"));
    }
    if value > 999 {
        return None;
    }
    let value = value as usize;
    let mut words = Vec::new();
    if value >= 100 {
        words.push(ENGLISH_UNITS[value / 100].to_string());
        words.push("hundred".to_string());
        let rest = value % 100;
        if rest != 0 {
            words.push(english_below_hundred(rest));
        }
    } else {
        words.push(english_below_hundred(value));
    }
    Some(words.join(" "))
}

fn english_below_hundred(value: usize) -> String {
    if value < 20 {
        return ENGLISH_UNITS[value].to_string();
    }
    let tens = value / 10;
    let units = value % 10;
    if units == 0 {
        ENGLISH_TENS[tens].to_string()
    } else {
        format!("{}-{}", ENGLISH_TENS[tens], ENGLISH_UNITS[units])
    }
}

fn english_ordinal(value: i64) -> Option<String> {
    let cardinal = english_cardinal(value)?;
    let split = cardinal
        .char_indices()
        .rev()
        .find(|&(_, character)| character == '-' || character == ' ')
        .map(|(index, _)| index);
    let (prefix, last) = match split {
        Some(index) => (&cardinal[..=index], &cardinal[index + 1..]),
        None => ("", cardinal.as_str()),
    };
    let ordinal = match last {
        "one" => "first",
        "two" => "second",
        "three" => "third",
        "five" => "fifth",
        "eight" => "eighth",
        "nine" => "ninth",
        "twelve" => "twelfth",
        _ if last.ends_with('y') => {
            return Some(format!("{prefix}{}ieth", &last[..last.len() - 1]));
        }
        _ => return Some(format!("{prefix}{last}th")),
    };
    Some(format!("{prefix}{ordinal}"))
}

fn roman_numeral(value: i64, old_style: bool) -> Option<String> {
    if value <= 0 || value > 4999 {
        return None;
    }
    if old_style {
        let mut value = value as usize;
        let mut text = String::new();
        for (amount, symbol) in [
            (1000, 'M'),
            (500, 'D'),
            (100, 'C'),
            (50, 'L'),
            (10, 'X'),
            (5, 'V'),
            (1, 'I'),
        ] {
            while value >= amount {
                value -= amount;
                text.push(symbol);
            }
        }
        return Some(text);
    }
    let mut value = value as usize;
    let mut text = String::new();
    for (amount, symbols) in [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ] {
        while value >= amount {
            value -= amount;
            text.push_str(symbols);
        }
    }
    Some(text)
}
