//! The fixed, non-evaluating data grammar shared by project metadata files.

use std::ops::Range;

use zkcl_syntax::syntax::{SyntaxKind, SyntaxNode, TextRange, parse_source};

use crate::ProjectMetadataError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Value {
    pub kind: ValueKind,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ValueKind {
    Symbol(String),
    String(String),
    List(Vec<Value>),
}

impl Value {
    pub fn list(&self) -> Result<&[Value], ProjectMetadataError> {
        match &self.kind {
            ValueKind::List(values) => Ok(values),
            _ => Err(self.error("expected a list")),
        }
    }

    pub fn symbol(&self) -> Result<&str, ProjectMetadataError> {
        match &self.kind {
            ValueKind::Symbol(value) => Ok(value),
            _ => Err(self.error("expected a symbol")),
        }
    }

    pub fn string(&self) -> Result<&str, ProjectMetadataError> {
        match &self.kind {
            ValueKind::String(value) => Ok(value),
            _ => Err(self.error("expected a string")),
        }
    }

    pub fn error(&self, message: impl Into<String>) -> ProjectMetadataError {
        ProjectMetadataError::new(message, self.span.clone())
    }
}

pub(crate) fn parse(source: &str) -> Result<Value, ProjectMetadataError> {
    let parsed = parse_source(source);
    if let Some(diagnostic) = parsed.primary_diagnostic() {
        return Err(ProjectMetadataError::new(
            diagnostic.message(),
            byte_range(diagnostic.range()),
        ));
    }
    let mut values = parse_children(&parsed.root())?;
    if values.len() != 1 {
        return Err(ProjectMetadataError::new(
            "expected exactly one top-level declaration",
            values
                .get(1)
                .map_or(0..source.len(), |value| value.span.clone()),
        ));
    }
    Ok(values.remove(0))
}

fn parse_children(node: &SyntaxNode) -> Result<Vec<Value>, ProjectMetadataError> {
    let mut values = Vec::new();
    for element in node.children_with_tokens() {
        let kind = element.kind();
        if kind.is_trivia() || matches!(kind, SyntaxKind::OpenParen | SyntaxKind::CloseParen) {
            continue;
        }
        let span = byte_range(element.text_range());
        let value_kind = match kind {
            SyntaxKind::List => ValueKind::List(parse_children(&element.into_node().unwrap())?),
            SyntaxKind::String => {
                let token = element.into_token().unwrap();
                let text = token.text();
                let mut decoded = String::new();
                let mut characters = text[1..text.len() - 1].chars();
                while let Some(character) = characters.next() {
                    decoded.push(if character == '\\' {
                        characters.next().expect("syntax parser validates escapes")
                    } else {
                        character
                    });
                }
                ValueKind::String(decoded)
            }
            SyntaxKind::Atom => {
                let token = element.into_token().unwrap();
                let text = token.text();
                if !text
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_:./+*<>=!?".contains(&byte))
                {
                    return Err(ProjectMetadataError::new(
                        "escaped or non-data symbols are not allowed",
                        span,
                    ));
                }
                ValueKind::Symbol(text.to_ascii_lowercase())
            }
            _ => {
                return Err(ProjectMetadataError::new(
                    "reader forms and dotted lists are not allowed in metadata",
                    span,
                ));
            }
        };
        values.push(Value {
            kind: value_kind,
            span,
        });
    }
    Ok(values)
}

fn byte_range(range: TextRange) -> Range<usize> {
    usize::from(range.start())..usize::from(range.end())
}

pub(crate) fn quote_string(value: &str) -> String {
    let mut quoted = String::from("\"");
    for character in value.chars() {
        if matches!(character, '\\' | '"') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_and_common_lisp_string_escapes() {
        let value = parse("; comment\n(DATA #|nested #| comment |#|# \"a\\nb\\\"c\")").unwrap();
        let list = value.list().unwrap();
        assert_eq!(list[0].symbol().unwrap(), "data");
        assert_eq!(list[1].string().unwrap(), "anb\"c");
    }

    #[test]
    fn strings_roundtrip() {
        for text in ["", "é\n中文", "\\\"#.(effects)"] {
            assert_eq!(parse(&quote_string(text)).unwrap().string().unwrap(), text);
        }
    }

    #[test]
    fn rejects_reader_effects_and_non_data_forms() {
        for text in [
            "#.(run)",
            "#+feature (data)",
            "#-feature (data)",
            "'(data)",
            "`(data)",
            "#(1)",
            "#S(data)",
            "#x10",
            "#Z(data)",
            "(a . b)",
            "(a |escaped|)",
            "(a b\\c)",
            "(a) (b)",
            "(a",
            "\"unterminated",
        ] {
            assert!(parse(text).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn diagnostics_preserve_utf8_byte_ranges() {
        let source = "(data \"中文\" #.(x))";
        let error = parse(source).unwrap_err();
        assert_eq!(&source[error.span], "#.(x)");
    }
}
