use std::sync::LazyLock;

use crate::unicode::{Property, simple_fold};
use crate::{Assertion, InputDomain, Rule, Set};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} at regex byte {offset}")]
pub struct RegexError {
    pub offset: usize,
    pub message: String,
}

pub fn parse_regex(domain: InputDomain, pattern: &str) -> Result<Rule, RegexError> {
    let mut parser = Parser {
        domain,
        pattern,
        offset: 0,
    };
    let rule = parser.choice(0, false)?;
    if parser.peek().is_some() {
        return Err(parser.error("unmatched closing parenthesis"));
    }
    Ok(rule)
}

struct Parser<'a> {
    domain: InputDomain,
    pattern: &'a str,
    offset: usize,
}

enum Atom {
    Unit(u32),
    Class(Set),
    Rule(Rule),
}

impl Atom {
    fn class(self) -> Option<Set> {
        match self {
            Self::Unit(unit) => Some(Set::range(unit, unit)),
            Self::Class(set) => Some(set),
            Self::Rule(_) => None,
        }
    }
}

impl Parser<'_> {
    fn error(&self, message: impl Into<String>) -> RegexError {
        RegexError {
            offset: self.offset,
            message: message.into(),
        }
    }

    fn rest(&self) -> &str {
        &self.pattern[self.offset..]
    }
    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn pop(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.offset += ch.len_utf8();
        Some(ch)
    }

    fn take(&mut self, prefix: &str) -> bool {
        if self.rest().starts_with(prefix) {
            self.offset += prefix.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, prefix: &str) -> Result<(), RegexError> {
        self.take(prefix)
            .then_some(())
            .ok_or_else(|| self.error(format!("expected {prefix:?}")))
    }

    fn choice(&mut self, depth: usize, folded: bool) -> Result<Rule, RegexError> {
        if depth > 128 {
            return Err(self.error("regex nesting exceeds 128 groups"));
        }
        let mut parts = vec![self.sequence(depth, folded)?];
        while self.take("|") {
            parts.push(self.sequence(depth, folded)?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Rule::Choice(parts)
        })
    }

    fn sequence(&mut self, depth: usize, folded: bool) -> Result<Rule, RegexError> {
        let mut parts = Vec::new();
        while self.peek().is_some_and(|next| !matches!(next, '|' | ')')) {
            let atom = self.atom(depth, folded)?;
            let quantifier = match self.peek() {
                Some('?') => {
                    self.pop();
                    Some((0, Some(1)))
                }
                Some('*') => {
                    self.pop();
                    Some((0, None))
                }
                Some('+') => {
                    self.pop();
                    Some((1, None))
                }
                Some('{') => {
                    self.pop();
                    let min = self.decimal()?;
                    let max = if self.take(",") {
                        if self.peek() == Some('}') {
                            None
                        } else {
                            Some(self.decimal()?)
                        }
                    } else {
                        Some(min)
                    };
                    self.expect("}")?;
                    if max.is_some_and(|max| max < min) {
                        return Err(self.error("repetition upper bound is below its lower bound"));
                    }
                    Some((min, max))
                }
                _ => None,
            };
            if let Some((min, max)) = quantifier {
                let greedy = !self.take("?");
                parts.push(atom.repeat(min, max, greedy));
            } else if let Rule::Literal(units) = atom {
                if let Some(Rule::Literal(previous)) = parts.last_mut() {
                    previous.extend(units);
                } else {
                    parts.push(Rule::Literal(units));
                }
            } else {
                parts.push(atom);
            }
        }
        Ok(match parts.len() {
            0 => Rule::Literal(Vec::new()),
            1 => parts.pop().unwrap(),
            _ => Rule::Sequence(parts),
        })
    }

    fn decimal(&mut self) -> Result<usize, RegexError> {
        let start = self.offset;
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            self.pop();
        }
        self.pattern[start..self.offset]
            .parse()
            .map_err(|_| self.error("expected a representable decimal repetition count"))
    }

    fn number(&mut self, base: u32, digits: Option<usize>) -> Result<u32, RegexError> {
        let start = self.offset;
        while digits.is_none_or(|digits| self.offset - start < digits)
            && self.peek().is_some_and(|ch| ch.is_digit(base))
        {
            self.pop();
        }
        if digits.is_some_and(|digits| self.offset - start != digits) {
            return Err(self.error("incomplete character escape"));
        }
        u32::from_str_radix(&self.pattern[start..self.offset], base)
            .map_err(|_| self.error("invalid code point escape"))
    }

    fn atom(&mut self, depth: usize, folded: bool) -> Result<Rule, RegexError> {
        let offset = self.offset;
        let ch = self.pop().expect("a regex atom is available");
        let atom = match ch {
            '(' => {
                let folded = if self.take("?i:") {
                    true
                } else if self.take("?:") {
                    folded
                } else if self.peek() == Some('?') {
                    return Err(self.error("unsupported regex group"));
                } else {
                    folded
                };
                let body = self.choice(depth + 1, folded)?;
                self.expect(")")?;
                return Ok(body);
            }
            '[' => return self.character_class(folded).map(Rule::Set),
            '\\' => self.escape(false)?,
            '^' => Atom::Rule(Rule::Assert(Assertion::Start)),
            '$' => Atom::Rule(Rule::Assert(Assertion::End)),
            '.' => Atom::Class(Set::range(0, self.domain.limit())),
            '*' | '+' | '?' | '{' | '}' | ']' => {
                return Err(RegexError {
                    offset,
                    message: format!("unexpected regex metacharacter {ch:?}"),
                });
            }
            _ => Atom::Unit(self.literal(ch)?),
        };
        Ok(match atom {
            Atom::Unit(unit) if folded => Rule::Set(fold(Set::range(unit, unit), self.domain)),
            Atom::Unit(unit) => Rule::Literal(vec![unit]),
            Atom::Class(set) => Rule::Set(set),
            Atom::Rule(rule) => rule,
        })
    }

    fn literal(&self, character: char) -> Result<u32, RegexError> {
        if self.domain == InputDomain::Bytes && !character.is_ascii() {
            Err(self.error("non-ASCII byte literals must use \\xHH"))
        } else {
            Ok(character as u32)
        }
    }

    fn escape(&mut self, in_class: bool) -> Result<Atom, RegexError> {
        let offset = self.offset.saturating_sub(1);
        let ch = self
            .pop()
            .ok_or_else(|| self.error("incomplete regex escape"))?;
        let unit = match ch {
            'n' => 10,
            'r' => 13,
            't' => 9,
            'f' => 12,
            'v' => 11,
            'x' if self.domain == InputDomain::Bytes => self.number(16, Some(2))?,
            'u' if self.domain == InputDomain::Text => {
                self.expect("{")?;
                let unit = self.number(16, None)?;
                self.expect("}")?;
                if char::from_u32(unit).is_none() {
                    return Err(self.error("escape is not a Unicode scalar"));
                }
                unit
            }
            'b' if !in_class => return Ok(Atom::Rule(Rule::Assert(Assertion::Word))),
            'B' if !in_class => return Ok(Atom::Rule(Rule::Assert(Assertion::NotWord))),
            'X' if !in_class && self.domain == InputDomain::Text => {
                return Ok(Atom::Rule(Rule::Grapheme));
            }
            'p' | 'P' if self.domain == InputDomain::Text => {
                self.expect("{")?;
                let start = self.offset;
                while self.peek().is_some_and(|ch| ch != '}') {
                    self.pop();
                }
                let value = &self.pattern[start..self.offset];
                self.expect("}")?;
                let (name, value) = value
                    .split_once('=')
                    .map_or((value, None), |(name, value)| (name, Some(value)));
                let property = Property::parse(name, value).map_err(|error| RegexError {
                    offset,
                    message: error.to_string(),
                })?;
                let set = Set::property(property);
                return Ok(Atom::Class(if ch == 'P' {
                    set.complement(self.domain)
                } else {
                    set
                }));
            }
            'w' | 'W' | 'd' | 'D' | 's' | 'S' => {
                let name = match ch.to_ascii_lowercase() {
                    'w' => "word",
                    'd' => "digit",
                    _ => "space",
                };
                let set = ascii_class(name).expect("built-in ASCII class");
                return Ok(Atom::Class(if ch.is_ascii_uppercase() {
                    set.complement(self.domain)
                } else {
                    set
                }));
            }
            '\\' | '^' | '$' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '-'
            | '|' => ch as u32,
            _ => {
                return Err(RegexError {
                    offset,
                    message: format!("unsupported regex escape \\{ch}"),
                });
            }
        };
        Ok(Atom::Unit(unit))
    }

    fn class_atom(&mut self) -> Result<Atom, RegexError> {
        if self.take("[:") {
            let complement = self.take("^");
            let len = self
                .rest()
                .find(":]")
                .ok_or_else(|| self.error("unterminated POSIX class"))?;
            let name = &self.rest()[..len];
            let set = ascii_class(name)
                .ok_or_else(|| self.error(format!("unknown POSIX class {name:?}")))?;
            self.offset += len + 2;
            return Ok(Atom::Class(if complement {
                set.complement(self.domain)
            } else {
                set
            }));
        }
        match self.pop() {
            Some('\\') => self.escape(true),
            Some(']' | '-') | None => Err(self.error("expected character class member")),
            Some(value) => Ok(Atom::Unit(self.literal(value)?)),
        }
    }

    fn character_class(&mut self, folded: bool) -> Result<Set, RegexError> {
        let invert = self.take("^");
        let mut literals = Vec::new();
        let mut classes = Vec::new();
        while self.peek() != Some(']') {
            let atom = self.class_atom()?;
            if self.take("-") {
                let Atom::Unit(low) = atom else {
                    return Err(self.error("range starts with a character class"));
                };
                let Atom::Unit(high) = self.class_atom()? else {
                    return Err(self.error("range ends with a character class"));
                };
                if low > high {
                    return Err(self.error("descending character range"));
                }
                literals.push((low, high));
            } else if let Atom::Unit(unit) = atom {
                literals.push((unit, unit));
            } else if let Some(class) = atom.class() {
                classes.extend(class.0);
            }
        }
        self.expect("]")?;
        if literals.is_empty() && classes.is_empty() {
            return Err(self.error("empty character class"));
        }
        if folded {
            classes.extend(fold(Set::from_ranges(literals), self.domain).0);
        } else {
            classes.extend(literals);
        }
        let set = Set::from_ranges(classes);
        Ok(if invert {
            set.complement(self.domain)
        } else {
            set
        })
    }
}

fn ascii_class(name: &str) -> Option<Set> {
    let ranges = match name {
        "alnum" => &[(48, 57), (65, 90), (97, 122)][..],
        "alpha" => &[(65, 90), (97, 122)],
        "ascii" => &[(0, 127)],
        "blank" => &[(9, 9), (32, 32)],
        "cntrl" => &[(0, 31), (127, 127)],
        "digit" => &[(48, 57)],
        "graph" => &[(33, 126)],
        "lower" => &[(97, 122)],
        "print" => &[(32, 126)],
        "punct" => &[(33, 47), (58, 64), (91, 96), (123, 126)],
        "space" => &[(9, 13), (32, 32)],
        "upper" => &[(65, 90)],
        "word" => &[(48, 57), (65, 90), (95, 95), (97, 122)],
        "xdigit" => &[(48, 57), (65, 70), (97, 102)],
        _ => return None,
    };
    Some(Set(ranges.to_vec()))
}

fn fold(set: Set, domain: InputDomain) -> Set {
    static SIMPLE_CASE_FOLDS: LazyLock<Vec<(u32, u32)>> = LazyLock::new(|| {
        (0..=0x10ffff)
            .filter_map(char::from_u32)
            .filter_map(|character| {
                let folded = simple_fold(character);
                (folded != character).then_some((character as u32, folded as u32))
            })
            .collect()
    });
    if set.0.is_empty() {
        return set;
    }
    if domain == InputDomain::Bytes {
        let opposite = (b'A'..=b'Z').filter_map(|upper| {
            let upper = u32::from(upper);
            if set.contains(upper) {
                Some((upper + 32, upper + 32))
            } else if set.contains(upper + 32) {
                Some((upper, upper))
            } else {
                None
            }
        });
        return Set::from_ranges(set.0.iter().copied().chain(opposite));
    }
    let folded = Set::from_ranges(
        set.0.iter().copied().chain(
            SIMPLE_CASE_FOLDS
                .iter()
                .filter(|(unit, _)| set.contains(*unit))
                .map(|(_, canonical)| (*canonical, *canonical)),
        ),
    );
    Set::from_ranges(
        folded.0.iter().copied().chain(
            SIMPLE_CASE_FOLDS
                .iter()
                .filter(|(_, canonical)| folded.contains(*canonical))
                .map(|(unit, _)| (*unit, *unit)),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::parse_regex;
    use crate::{InputDomain, MatchMode, Program};

    #[test]
    fn unicode_case_insensitive_matching_accepts_all_sigma_forms() {
        let rule = parse_regex(InputDomain::Text, "(?i:Σ)").unwrap();
        let program = Program::compile(InputDomain::Text, MatchMode::Full, &[(rule, 0)]).unwrap();
        for sigma in ['Σ', 'σ', 'ς'] {
            assert!(program.run(&[u32::from(sigma)]).is_some(), "{sigma}");
        }
        assert!(program.run(&[u32::from('s')]).is_none());
    }

    #[test]
    fn simple_fold_equivalence_classes_and_properties_respect_domain_rules() {
        for code in 0..=0x10ffffu32 {
            let Some(ch) = char::from_u32(code) else {
                continue;
            };
            let folded = crate::unicode::simple_fold(ch);
            assert_eq!(
                crate::unicode::simple_fold(folded),
                folded,
                "simple_fold not idempotent for U+{code:04X}"
            );
        }
        // Kelvin sign K / k / U+212A
        let kelvin = Program::compile(
            InputDomain::Text,
            MatchMode::Full,
            &[(parse_regex(InputDomain::Text, "(?i:k)").unwrap(), 0)],
        )
        .unwrap();
        for ch in ['k', 'K', '\u{212a}'] {
            assert!(kelvin.run(&[u32::from(ch)]).is_some(), "{ch}");
        }
        // Named properties and POSIX classes are not folded by (?i:...)
        let lu = Program::compile(
            InputDomain::Text,
            MatchMode::Full,
            &[(parse_regex(InputDomain::Text, r"(?i:\p{Lu})").unwrap(), 0)],
        )
        .unwrap();
        assert!(lu.run(&[u32::from('A')]).is_some());
        assert!(lu.run(&[u32::from('a')]).is_none());
        let posix_lower = Program::compile(
            InputDomain::Text,
            MatchMode::Full,
            &[(
                parse_regex(InputDomain::Text, "(?i:[[:lower:]])").unwrap(),
                0,
            )],
        )
        .unwrap();
        assert!(posix_lower.run(&[u32::from('a')]).is_some());
        assert!(posix_lower.run(&[u32::from('A')]).is_none());
        // Negated class in (?i:...) folds literals before complementing
        let neg = Program::compile(
            InputDomain::Text,
            MatchMode::Full,
            &[(parse_regex(InputDomain::Text, "(?i:[^a])").unwrap(), 0)],
        )
        .unwrap();
        assert!(neg.run(&[u32::from('a')]).is_none());
        assert!(neg.run(&[u32::from('A')]).is_none());
        assert!(neg.run(&[u32::from('b')]).is_some());
        // Byte domain (?i:...) folds ASCII only
        let byte_fold = Program::compile(
            InputDomain::Bytes,
            MatchMode::Full,
            &[(parse_regex(InputDomain::Bytes, r"(?i:\x41)").unwrap(), 0)],
        )
        .unwrap();
        assert!(byte_fold.run(&[b'A' as u32]).is_some());
        assert!(byte_fold.run(&[b'a' as u32]).is_some());
        assert!(byte_fold.run(&[b'b' as u32]).is_none());
    }

    #[test]
    fn invalid_regex_patterns_report_accurate_errors() {
        assert!(parse_regex(InputDomain::Text, "").is_ok());
        for (domain, pattern, substr) in [
            (InputDomain::Text, "[]", "empty character class"),
            (InputDomain::Text, "[^]", "empty character class"),
            (InputDomain::Text, "[z-a]", "descending character range"),
            (InputDomain::Text, "[a-]", "character class member"),
            (InputDomain::Text, "[-a]", "character class member"),
            (InputDomain::Text, "[a", "character class member"),
            (InputDomain::Text, "[[:bogus:]]", "unknown POSIX class"),
            (InputDomain::Text, r"\u{d800}", "Unicode scalar"),
            (InputDomain::Text, r"\u{}", "invalid code point"),
            (InputDomain::Text, r"\x41", "unsupported regex escape"),
            (InputDomain::Bytes, r"\u{41}", "unsupported regex escape"),
            (InputDomain::Bytes, r"\X", "unsupported regex escape"),
            (InputDomain::Bytes, r"\p{ASCII}", "unsupported regex escape"),
            (InputDomain::Bytes, "é", "non-ASCII"),
            (InputDomain::Text, "a{5,2}", "below its lower bound"),
            (InputDomain::Text, "a**", "unexpected regex metacharacter"),
            (InputDomain::Text, "(?=a)", "unsupported regex group"),
            (InputDomain::Text, "(a", "expected \")\""),
            (InputDomain::Text, "a)", "unmatched closing parenthesis"),
        ] {
            let err = parse_regex(domain, pattern).unwrap_err();
            assert!(
                err.message.contains(substr),
                "{pattern:?} => {err:?} (expected {substr:?})"
            );
        }
    }

    #[test]
    fn syntax_error_reports_the_utf8_byte_offset() {
        let error = parse_regex(InputDomain::Text, "雪]").unwrap_err();
        assert_eq!(error.offset, 3);
        assert_eq!(
            error.to_string(),
            "unexpected regex metacharacter ']' at regex byte 3"
        );
    }
}
