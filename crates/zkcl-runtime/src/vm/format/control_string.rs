use std::iter::Peekable;
use std::str::CharIndices;

use super::FORMAT_NESTING_LIMIT;

#[derive(Clone, Copy, Debug)]
pub(super) enum PrefixParameter {
    Omitted,
    Integer(i64),
    Character(char),
    NextArgument,
    RemainingArguments,
}

#[derive(Debug)]
pub(super) struct FormatDirective {
    pub(super) operator: char,
    pub(super) prefix_parameters: Vec<PrefixParameter>,
    pub(super) colon: bool,
    pub(super) atsign: bool,
    pub(super) name: Option<String>,
}

pub(super) enum ControlPart<'a> {
    Text(&'a str),
    Directive(FormatDirective),
    Group(DirectiveGroup<'a>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum GroupKind {
    CaseConversion,
    Conditional,
    Iteration,
    Justification,
    LogicalBlock,
}

pub(super) struct DirectiveGroup<'a> {
    pub(super) kind: GroupKind,
    pub(super) opener: FormatDirective,
    pub(super) sections: Vec<DirectiveSection<'a>>,
}

pub(super) struct DirectiveSection<'a> {
    pub(super) parts: Vec<ControlPart<'a>>,
    pub(super) terminator: FormatDirective,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ControlStringError {
    #[error("FORMAT has an unmatched separator or closing directive")]
    UnmatchedTerminator,
    #[error("FORMAT directives nest too deeply")]
    NestingLimit,
    #[error("FORMAT group is not closed")]
    UnclosedGroup,
    #[error("FORMAT group has an unexpected closing directive")]
    MismatchedTerminator,
    #[error("FORMAT character parameter is incomplete")]
    IncompleteCharacterParameter,
    #[error("FORMAT directive is incomplete")]
    IncompleteDirective,
    #[error("duplicate FORMAT modifier")]
    DuplicateModifier,
    #[error("FORMAT ~/ name is not closed")]
    UnclosedFunctionName,
    #[error("invalid character in FORMAT ~/ name")]
    InvalidFunctionNameCharacter,
    #[error("FORMAT ~/ requires a function name")]
    EmptyFunctionName,
}

type ParseResult<T> = Result<T, ControlStringError>;

pub(super) fn parse_control_string(text: &str) -> ParseResult<Vec<ControlPart<'_>>> {
    let mut parser = ControlStringParser {
        text,
        chars: text.char_indices().peekable(),
    };
    let (parts, end) = parser.parse_parts(0)?;
    if end.is_some() {
        return Err(ControlStringError::UnmatchedTerminator);
    }
    Ok(parts)
}

struct ControlStringParser<'a> {
    text: &'a str,
    chars: Peekable<CharIndices<'a>>,
}

impl<'a> ControlStringParser<'a> {
    fn parse_parts(
        &mut self,
        depth: usize,
    ) -> ParseResult<(Vec<ControlPart<'a>>, Option<FormatDirective>)> {
        if depth == FORMAT_NESTING_LIMIT {
            return Err(ControlStringError::NestingLimit);
        }
        let mut parts = Vec::new();
        while let Some((start, _)) = self.chars.peek().copied() {
            if let Some((tilde, _)) = self.chars.by_ref().find(|&(_, c)| c == '~') {
                if start != tilde {
                    parts.push(ControlPart::Text(&self.text[start..tilde]));
                }
            } else {
                parts.push(ControlPart::Text(&self.text[start..]));
                break;
            }
            let directive = self.parse_directive()?;
            let (expected_close, mut kind) = match directive.operator {
                '(' => (')', GroupKind::CaseConversion),
                '[' => (']', GroupKind::Conditional),
                '{' => ('}', GroupKind::Iteration),
                '<' => ('>', GroupKind::Justification),
                ';' | ')' | ']' | '}' | '>' => return Ok((parts, Some(directive))),
                character => {
                    let skip_space = character == '\n' && !directive.colon;
                    parts.push(ControlPart::Directive(directive));
                    if skip_space {
                        while self
                            .chars
                            .next_if(|&(_, c)| matches!(c, ' ' | '\t' | '\r'))
                            .is_some()
                        {}
                    }
                    continue;
                }
            };
            let mut sections = Vec::new();
            loop {
                let (parts, end) = self.parse_parts(depth + 1)?;
                let end = end.ok_or(ControlStringError::UnclosedGroup)?;
                let character = end.operator;
                if character != expected_close
                    && !(character == ';' && matches!(expected_close, ']' | '>'))
                {
                    return Err(ControlStringError::MismatchedTerminator);
                }
                if character == '>' && end.colon {
                    kind = GroupKind::LogicalBlock;
                }
                sections.push(DirectiveSection {
                    parts,
                    terminator: end,
                });
                if character == expected_close {
                    break;
                }
            }
            parts.push(ControlPart::Group(DirectiveGroup {
                kind,
                opener: directive,
                sections,
            }));
        }
        Ok((parts, None))
    }

    fn parse_directive(&mut self) -> ParseResult<FormatDirective> {
        let mut prefix_parameters = Vec::new();
        loop {
            let parameter = match self.chars.peek().copied() {
                Some((_, '\'')) => {
                    self.chars.next();
                    Some(PrefixParameter::Character(
                        self.chars
                            .next()
                            .ok_or(ControlStringError::IncompleteCharacterParameter)?
                            .1,
                    ))
                }
                Some((_, 'V' | 'v')) => {
                    self.chars.next();
                    Some(PrefixParameter::NextArgument)
                }
                Some((_, '#')) => {
                    self.chars.next();
                    Some(PrefixParameter::RemainingArguments)
                }
                Some((start, '+' | '-' | '0'..='9')) => {
                    self.chars.next();
                    while self.chars.next_if(|&(_, c)| c.is_ascii_digit()).is_some() {}
                    let end = self
                        .chars
                        .peek()
                        .map_or(self.text.len(), |&(index, _)| index);
                    Some(PrefixParameter::Integer(
                        self.text[start..end].parse().unwrap_or_else(|_| {
                            if self.text[start..].starts_with('-') {
                                i64::MIN
                            } else {
                                i64::MAX
                            }
                        }),
                    ))
                }
                _ => None,
            };
            if self.chars.next_if(|&(_, c)| c == ',').is_some() {
                prefix_parameters.push(parameter.unwrap_or(PrefixParameter::Omitted));
            } else {
                if let Some(parameter) = parameter {
                    prefix_parameters.push(parameter);
                }
                break;
            }
        }
        let mut colon = false;
        let mut atsign = false;
        let character = loop {
            match self
                .chars
                .next()
                .ok_or(ControlStringError::IncompleteDirective)?
                .1
            {
                ':' if !colon => colon = true,
                '@' if !atsign => atsign = true,
                ':' | '@' => return Err(ControlStringError::DuplicateModifier),
                character => break character.to_ascii_uppercase(),
            }
        };
        let mut name = None;
        if character == '/' {
            let mut spelling = String::new();
            loop {
                let (_, character) = self
                    .chars
                    .next()
                    .ok_or(ControlStringError::UnclosedFunctionName)?;
                if character == '/' {
                    break;
                }
                if character == '\n' || character == '~' {
                    return Err(ControlStringError::InvalidFunctionNameCharacter);
                }
                spelling.push(character);
            }
            if spelling.is_empty() {
                return Err(ControlStringError::EmptyFunctionName);
            }
            name = Some(spelling);
        }
        Ok(FormatDirective {
            operator: character,
            prefix_parameters,
            colon,
            atsign,
            name,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::ControlStringError;
    use crate::vm::format::parse_format_control;

    #[test]
    fn format_reports_a_mismatched_terminator_at_the_call_site() {
        let Err(fault) = parse_format_control("~[~)", 23) else {
            panic!("mismatched conditional group was accepted");
        };
        assert_eq!(fault.host.offset(), 23);
        assert_eq!(
            fault.host.message(),
            "FORMAT group has an unexpected closing directive"
        );
        let cause = fault
            .host
            .source()
            .and_then(|error| error.downcast_ref::<ControlStringError>());
        assert!(matches!(
            cause,
            Some(ControlStringError::MismatchedTerminator)
        ));
    }
}
