//! Writing JavaScript source text.
//!
//! Generated modules are read by people as well as executed, so the rules for
//! laying that text out live here instead of inside the emitters: two-space
//! indentation, one object field per line, and object keys written without
//! quotes whenever the field name is a plain identifier.

use serde_json::Value;

use crate::emit::EmitError;

/// The largest integer a JavaScript number holds exactly.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// The longest one-line object that still reads as a single value.
const INLINE_LIMIT: usize = 64;
const LINE_WIDTH: usize = 100;

/// One level of indentation.
///
/// Two spaces keep a step function that nests a dispatch loop, a switch and a
/// case body readable on a narrow screen.
pub(super) fn indent_str(level: usize) -> String {
    "  ".repeat(level)
}

/// Write a value as one JavaScript literal on a single line.
///
/// Used where the data reads as a value rather than as structure: literals,
/// scalar arrays, and anything the caller already keeps on one line.
pub(super) fn json(value: &Value) -> Result<String, EmitError> {
    reject_inexact_integers(value)?;
    let text = value.to_string();
    if !text.contains(['\u{2028}', '\u{2029}']) {
        return Ok(text);
    }
    let mut output = String::with_capacity(text.len());
    write_json_text(&mut output, &text);
    Ok(output)
}

/// Keep JSON text usable by parsers that treat these Unicode separators as
/// line terminators inside strings.
fn write_json_text(out: &mut String, text: &str) {
    let mut start = 0;
    for (index, separator) in text.match_indices(['\u{2028}', '\u{2029}']) {
        out.push_str(&text[start..index]);
        out.push_str(if separator == "\u{2028}" {
            "\\u2028"
        } else {
            "\\u2029"
        });
        start = index + separator.len();
    }
    out.push_str(&text[start..]);
}

fn write_quoted(out: &mut String, text: &str) -> Result<(), EmitError> {
    write_json_text(out, &serde_json::to_string(text)?);
    Ok(())
}

/// Write a value as JavaScript source, indented `level` levels deep.
///
/// Objects expand one field per line because generated data is read field by
/// field. A small record of plain fields stays on one line, where it reads as a
/// value, and so do arrays of scalars.
pub(super) fn write_value(out: &mut String, value: &Value, level: usize) -> Result<(), EmitError> {
    if let Value::Object(fields) = value {
        // A record of plain fields reads as one value; anything nested or long
        // takes the expanded form below.
        if let Some(inline) = inline_object(fields)? {
            out.push_str(&inline);
            return Ok(());
        }
    }
    match value {
        Value::Object(fields) if !fields.is_empty() => {
            out.push_str("{\n");
            for (index, (field, field_value)) in fields.iter().enumerate() {
                out.push_str(&indent_str(level + 1));
                write_key(out, field)?;
                out.push_str(": ");
                write_value(out, field_value, level + 1)?;
                if index + 1 < fields.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&indent_str(level));
            out.push('}');
            Ok(())
        }
        Value::Array(items) if !items.is_empty() => {
            if items.iter().all(|item| !is_nested(item)) {
                let items = items.iter().map(json).collect::<Result<Vec<_>, _>>()?;
                if items.iter().all(|item| item.len() <= INLINE_LIMIT) {
                    write_scalar_array(out, &items, level);
                    return Ok(());
                }
            }
            out.push_str("[\n");
            for item in items {
                out.push_str(&indent_str(level + 1));
                write_value(out, item, level + 1)?;
                out.push_str(",\n");
            }
            out.push_str(&indent_str(level));
            out.push(']');
            Ok(())
        }
        Value::String(text) => write_string(out, text, level),
        _ => {
            out.push_str(&json(value)?);
            Ok(())
        }
    }
}

fn write_scalar_array(out: &mut String, items: &[String], level: usize) {
    let inline_width = items.iter().map(String::len).sum::<usize>() + 2 * items.len();
    if inline_width <= INLINE_LIMIT {
        out.push('[');
        out.push_str(&items.join(", "));
        out.push(']');
        return;
    }
    out.push('[');
    let indent = indent_str(level + 1);
    let mut width = LINE_WIDTH;
    for item in items {
        if width + item.len() + 2 > LINE_WIDTH {
            out.push('\n');
            out.push_str(&indent);
            width = indent.len();
        } else {
            out.push(' ');
            width += 1;
        }
        out.push_str(item);
        out.push(',');
        width += item.len() + 1;
    }
    out.push('\n');
    out.push_str(&indent_str(level));
    out.push(']');
}

fn write_string(out: &mut String, text: &str, level: usize) -> Result<(), EmitError> {
    // UTF-8 length is a lower bound on the quoted width. Skip serializing
    // potentially large strings twice when they already need to wrap.
    let available = LINE_WIDTH.saturating_sub(level * 2);
    if text.len() + 2 <= available {
        let start = out.len();
        write_quoted(out, text)?;
        if out.len() - start <= available {
            return Ok(());
        }
        out.truncate(start);
    }
    // Split the raw text at character boundaries, then quote each piece. This
    // keeps Lisp source snapshots legible and preserves every original byte.
    let indent = indent_str(level + 1);
    let limit = LINE_WIDTH.saturating_sub(indent.len() + 5).max(24);
    out.push_str("(\n");
    let mut write_chunk = |start: usize, end: usize| -> Result<(), EmitError> {
        out.push_str(&indent);
        write_quoted(out, &text[start..end])?;
        if end < text.len() {
            out.push_str(" +");
        }
        out.push('\n');
        Ok(())
    };
    let mut start = 0;
    let mut width = 0;
    for (at, character) in text.char_indices() {
        let length = match character {
            '"' | '\\' | '\n' | '\r' | '\t' => 2,
            '\u{0}'..='\u{1f}' | '\u{2028}' | '\u{2029}' => 6,
            other => other.len_utf8(),
        };
        if width + length > limit {
            write_chunk(start, at)?;
            start = at;
            width = 0;
        }
        width += length;
        if character == '\n' {
            let end = at + 1;
            write_chunk(start, end)?;
            start = end;
            width = 0;
        }
    }
    if start < text.len() || text.is_empty() {
        write_chunk(start, text.len())?;
    }
    out.push_str(&indent_str(level));
    out.push(')');
    Ok(())
}

/// Whether a value is a structure that earns its own lines.
fn is_nested(value: &Value) -> bool {
    match value {
        Value::Object(fields) => !fields.is_empty(),
        Value::Array(items) => items.iter().any(is_nested),
        _ => false,
    }
}

/// The one-line form of a record whose fields are all plain values, when it
/// stays short enough to read as a value. A field that is itself a structure
/// always takes the expanded form.
fn inline_object(fields: &serde_json::Map<String, Value>) -> Result<Option<String>, EmitError> {
    if fields.is_empty() || fields.values().any(is_nested) {
        return Ok(None);
    }
    let mut inline = String::from("{ ");
    for (index, (field, value)) in fields.iter().enumerate() {
        if index > 0 {
            inline.push_str(", ");
        }
        write_key(&mut inline, field)?;
        inline.push_str(": ");
        inline.push_str(&json(value)?);
    }
    inline.push_str(" }");
    Ok((inline.len() <= INLINE_LIMIT).then_some(inline))
}

/// Write an object key, unquoted when the field name is a plain identifier.
fn write_key(out: &mut String, field: &str) -> Result<(), EmitError> {
    if is_identifier(field) && !is_reserved_word(field) {
        out.push_str(field);
        return Ok(());
    }
    write_quoted(out, field)
}

fn is_identifier(text: &str) -> bool {
    let mut bytes = text.bytes();
    match bytes.next() {
        Some(first) => {
            (first.is_ascii_alphabetic() || first == b'_' || first == b'$')
                && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$')
        }
        None => false,
    }
}

/// Quote reserved identifiers in generated records as a layout convention.
fn is_reserved_word(text: &str) -> bool {
    matches!(
        text,
        "await"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "implements"
            | "import"
            | "in"
            | "instanceof"
            | "interface"
            | "let"
            | "new"
            | "null"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "static"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
    )
}

/// Reject numbers that JavaScript would silently round.
fn reject_inexact_integers(value: &Value) -> Result<(), EmitError> {
    match value {
        Value::Number(number)
            if number.as_u64().is_some_and(|n| n > MAX_SAFE_INTEGER)
                || number
                    .as_i64()
                    .is_some_and(|n| n < -(MAX_SAFE_INTEGER as i64)) =>
        {
            Err(EmitError::IntegerRange {
                value: number.to_string(),
            })
        }
        Value::Array(items) => items.iter().try_for_each(reject_inexact_integers),
        Value::Object(items) => items.values().try_for_each(reject_inexact_integers),
        _ => Ok(()),
    }
}

/// Check an index emitted directly as a JavaScript number.
pub(super) fn check_exact_unsigned(value: u64) -> Result<(), EmitError> {
    if value > MAX_SAFE_INTEGER {
        return Err(EmitError::IntegerRange {
            value: value.to_string(),
        });
    }
    Ok(())
}
