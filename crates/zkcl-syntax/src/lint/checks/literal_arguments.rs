//! Argument checks that require a literal witness, independent of inferred types.

use crate::navigation::index::{closed_list, symbol};
use crate::outline::{Element, elements};
use crate::reader_tokens::{Prefix, integer_token};
use crate::syntax::SyntaxKind;

use super::super::context::{
    LintDocument, LintEmitter, is_char_literal_el, is_nil_syntax, is_number_literal_el,
    is_string_literal_el, string_literal_value, unwrap_quote,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Literal {
    Nil,
    Symbol,
    Integer,
    Number,
    Character,
    String,
    Vector,
    Cons,
}

fn classify(ctx: &LintDocument<'_>, el: &Element, package: Option<&str>) -> Option<Literal> {
    let quoted = unwrap_quote(el, package, ctx.workspace);
    let value = quoted.as_ref().unwrap_or(el);
    if is_nil_syntax(value, package, ctx.workspace) {
        return Some(Literal::Nil);
    }
    if is_char_literal_el(value) {
        return Some(Literal::Character);
    }
    if is_string_literal_el(value) {
        return Some(Literal::String);
    }
    if is_number_literal_el(value) {
        return Some(
            if value
                .as_token()
                .is_some_and(|token| integer_token(token.text()).is_some())
            {
                Literal::Integer
            } else {
                Literal::Number
            },
        );
    }
    if value.kind() == SyntaxKind::Vector {
        return Some(Literal::Vector);
    }
    if quoted.is_some() && value.kind() == SyntaxKind::List {
        return Some(Literal::Cons);
    }
    if let Some(name) = symbol(value)
        && (quoted.is_some()
            || name.prefix == Prefix::Keyword
            || ctx.workspace.standard_symbol(&name, package) == Some("T"))
    {
        return Some(Literal::Symbol);
    }
    None
}

pub(super) fn check(
    ctx: &LintDocument<'_>,
    op: &str,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    use Literal::{Character, Cons, Integer, Nil, Number, String, Symbol, Vector};
    let (index, type_name, allowed): (usize, &str, &[Literal]) = match op {
        "ELT" | "SUBSEQ" | "LENGTH" | "REVERSE" | "NREVERSE" | "COPY-SEQ" | "SORT"
        | "STABLE-SORT" => (0, "a sequence", &[Nil, Cons, Vector, String]),
        "CHAR" | "SCHAR" => (0, "a string", &[String]),
        "AREF" | "ROW-MAJOR-AREF" | "ARRAY-DIMENSION" | "ARRAY-TOTAL-SIZE" => {
            (0, "an array", &[Vector, String])
        }
        "SVREF" => (0, "a simple vector", &[Vector]),
        "CONSP" | "LISTP" => return,
        "CAR" | "CDR" | "FIRST" | "REST" | "SECOND" | "THIRD" | "FOURTH" | "COPY-LIST" => {
            (0, "a list", &[Nil, Cons])
        }
        "RPLACA" | "RPLACD" => (0, "a cons", &[Cons]),
        "NTH" | "NTHCDR" => (1, "a list", &[Nil, Cons]),
        "SYMBOL-NAME" | "SYMBOL-PACKAGE" | "SYMBOL-VALUE" | "SYMBOL-FUNCTION" | "BOUNDP"
        | "MAKUNBOUND" => (0, "a symbol", &[Nil, Symbol]),
        "CHAR-CODE" | "CHAR-NAME" | "CHAR-UPCASE" | "CHAR-DOWNCASE" => {
            (0, "a character", &[Character])
        }
        "1+" | "1-" | "ZEROP" | "ABS" => (0, "a number", &[Integer, Number]),
        "EVENP" | "ODDP" | "ASH" | "CODE-CHAR" => (0, "an integer", &[Integer]),
        _ => return,
    };
    let Some(target) = args.get(index) else {
        return;
    };
    let literal = classify(ctx, target, package);
    if literal.is_some_and(|value| !allowed.contains(&value)) {
        emit.emit(
            "correctness/literal-argument-type",
            target.text_range(),
            format!(
                "argument {} to `{}` must be {type_name}",
                index + 1,
                op.to_ascii_lowercase()
            ),
            None,
            Vec::new(),
            None,
        );
        return;
    }
    let indices: &[usize] = match op {
        "NTH" | "NTHCDR" => &[0],
        "ELT" | "SVREF" | "CHAR" | "SCHAR" | "ROW-MAJOR-AREF" | "ARRAY-DIMENSION" => &[1],
        "SUBSEQ" => &[1, 2],
        "AREF" => {
            if args.len() != 2 && matches!(literal, Some(String | Vector)) {
                emit.emit(
                    "correctness/literal-array-rank",
                    target.text_range(),
                    format!(
                        "a rank-one array needs one subscript, found {}",
                        args.len() - 1
                    ),
                    None,
                    Vec::new(),
                    None,
                );
                return;
            }
            for subscript in args.iter().skip(1) {
                check_index(ctx, op, subscript, package, false, emit);
            }
            return;
        }
        _ => return,
    };
    for &index in indices {
        if let Some(value) = args.get(index) {
            check_index(ctx, op, value, package, op == "SUBSEQ" && index == 2, emit);
        }
    }
}

fn check_index(
    ctx: &LintDocument<'_>,
    op: &str,
    index: &Element,
    package: Option<&str>,
    nil_allowed: bool,
    emit: &mut LintEmitter<'_>,
) {
    let Some(literal) = classify(ctx, index, package) else {
        return;
    };
    if literal == Literal::Integer {
        let datum = unwrap_quote(index, package, ctx.workspace);
        if datum
            .as_ref()
            .unwrap_or(index)
            .as_token()
            .and_then(|token| integer_token(token.text()))
            .is_some_and(|value| value.starts_with('-'))
        {
            emit.emit(
                "correctness/negative-index",
                index.text_range(),
                format!("`{}` requires a nonnegative index", op.to_ascii_lowercase()),
                None,
                Vec::new(),
                None,
            );
        }
    } else if !(nil_allowed && literal == Literal::Nil) {
        emit.emit(
            "correctness/literal-argument-type",
            index.text_range(),
            format!(
                "`{}` index must be an integer{}",
                op.to_ascii_lowercase(),
                if nil_allowed { " or `nil`" } else { "" }
            ),
            None,
            Vec::new(),
            None,
        );
    }
}

pub(super) fn sequence_len(
    ctx: &LintDocument<'_>,
    el: &Element,
    package: Option<&str>,
) -> Option<usize> {
    let quoted = unwrap_quote(el, package, ctx.workspace);
    let datum = quoted.as_ref().unwrap_or(el);
    if is_nil_syntax(datum, package, ctx.workspace) {
        return Some(0);
    }
    if let Some(text) = string_literal_value(datum) {
        return Some(text.chars().count());
    }
    let node = datum.as_node()?;
    if !(node.kind() == SyntaxKind::Vector || (quoted.is_some() && closed_list(node))) {
        return None;
    }
    // An explicit dimension in #n(…) can repeat the final element.
    if node.kind() == SyntaxKind::Vector
        && let Some(prefix) = node.first_token()
        && let Some(size) = prefix
            .text()
            .strip_prefix('#')
            .and_then(|prefix| prefix.strip_suffix('('))
        && !size.is_empty()
    {
        return size.parse().ok();
    }
    Some(ctx.read_elements(elements(node))?.len())
}
