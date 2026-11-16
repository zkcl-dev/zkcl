//! The types of expressions that can be determined without running Lisp.

use crate::ast::{AstAtom, AstForm, AstList, AstLiteral};
use crate::cl_builtins::evaluates_all_arguments;
use crate::reader_tokens::{NumberLiteralKind, Prefix, number_literal_kind};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange};

const MAX_TYPE_DEPTH: usize = 32;

/// A known argument type that cannot satisfy a standard operator's contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpressionTypeError {
    pub range: TextRange,
    pub message: String,
}

pub(crate) fn infer_expression_type(
    root: &SyntaxNode,
    is_standard_symbol: &impl Fn(&AstAtom, &str) -> bool,
) -> Option<&'static str> {
    let mut forms = root.children_with_tokens().filter_map(AstForm::cast);
    let form = forms.next()?;
    if forms.next().is_some() {
        return None;
    }
    TypeAnalysis { is_standard_symbol }.infer_form(&form, 0)
}

pub(crate) fn expression_type_error(
    root: &SyntaxNode,
    is_standard_symbol: &impl Fn(&AstAtom, &str) -> bool,
) -> Option<ExpressionTypeError> {
    let analysis = TypeAnalysis { is_standard_symbol };
    root.children_with_tokens()
        .filter_map(AstForm::cast)
        .find_map(|form| analysis.check_form(&form, 0))
}

struct TypeAnalysis<'a, F> {
    is_standard_symbol: &'a F,
}

impl<F: Fn(&AstAtom, &str) -> bool> TypeAnalysis<'_, F> {
    fn standard_operator(&self, list: &AstList) -> Option<String> {
        let (atom, symbol) = list.head_symbol()?;
        (self.is_standard_symbol)(&atom, &symbol.name).then_some(symbol.name)
    }

    fn check_form(&self, form: &AstForm, depth: usize) -> Option<ExpressionTypeError> {
        if depth > MAX_TYPE_DEPTH {
            return None;
        }
        let list = form.as_list().filter(|list| !list.is_dotted())?;
        let operator = self.standard_operator(list)?;
        let args: Vec<_> = list.forms().skip(1).collect();
        self.check_operands(&operator, &args, depth + 1)
            .or_else(|| {
                evaluates_all_arguments(&operator)
                    .then(|| args.iter().find_map(|arg| self.check_form(arg, depth + 1)))
                    .flatten()
            })
    }

    fn check_operands(
        &self,
        operator: &str,
        args: &[AstForm],
        depth: usize,
    ) -> Option<ExpressionTypeError> {
        let first = &args[..args.len().min(1)];
        let (expected, operands, accepts): (&str, &[AstForm], fn(&str) -> bool) = match operator {
            "+" | "-" | "*" | "/" | "1+" | "1-" | "ABS" | "MAX" | "MIN" | "MOD" | "REM"
            | "FLOOR" | "CEILING" | "TRUNCATE" | "ROUND" | "FFLOOR" | "FCEILING" | "FTRUNCATE"
            | "FROUND" | "SQRT" | "EXPT" | "SIN" | "COS" | "TAN" | "ASIN" | "ACOS" | "ATAN"
            | "EXP" | "LOG" | "PHASE" | "REALPART" | "IMAGPART" | "CONJUGATE" | "NUMERATOR"
            | "DENOMINATOR" | "=" | "/=" | "<" | "<=" | ">" | ">=" | "ZEROP" | "PLUSP"
            | "MINUSP" => ("a number", args, |ty| {
                matches!(ty, "integer" | "float" | "rational" | "number")
            }),
            "ASH" | "LOGAND" | "LOGIOR" | "LOGXOR" | "GCD" | "LCM" | "ISQRT" | "EVENP" | "ODDP"
            | "CODE-CHAR" => ("an integer", args, |ty| {
                matches!(ty, "integer" | "rational" | "number")
            }),
            "CAR" | "CDR" | "FIRST" | "REST" | "SECOND" | "THIRD" => ("a list", first, |ty| {
                matches!(ty, "list" | "cons" | "null" | "symbol" | "boolean")
            }),
            "LENGTH" | "SUBSEQ" | "REVERSE" | "NREVERSE" | "ELT" => ("a sequence", first, |ty| {
                matches!(
                    ty,
                    "list" | "cons" | "null" | "symbol" | "boolean" | "vector" | "array" | "string"
                )
            }),
            "CHAR" | "SCHAR" => ("a string", first, |ty| ty == "string"),
            "CHAR-CODE" => ("a character", first, |ty| ty == "character"),
            "AREF" | "SVREF" => ("an array", first, |ty| {
                matches!(ty, "vector" | "array" | "string")
            }),
            "FUNCALL" | "APPLY" => ("a function designator", first, |ty| {
                matches!(ty, "function" | "symbol" | "keyword" | "null" | "boolean")
            }),
            _ => return None,
        };
        operands.iter().find_map(|arg| {
            let found = self.infer_form(arg, depth)?;
            (!accepts(found)).then(|| ExpressionTypeError {
                range: arg.text_range(),
                message: format!("expected {expected}, found {found}"),
            })
        })
    }

    fn infer_form(&self, form: &AstForm, depth: usize) -> Option<&'static str> {
        if depth > MAX_TYPE_DEPTH {
            return None;
        }
        match form {
            AstForm::Atom(atom) => {
                if let Some(ty) = number_type(atom.text()) {
                    return Some(ty);
                }
                let symbol = atom.symbol()?;
                match symbol.prefix {
                    Prefix::Keyword => Some("keyword"),
                    _ if (self.is_standard_symbol)(atom, &symbol.name) => {
                        match symbol.name.as_str() {
                            "NIL" => Some("null"),
                            "T" => Some("boolean"),
                            _ => None,
                        }
                    }
                    _ => None,
                }
            }
            AstForm::Literal(literal) => literal_type(literal),
            AstForm::Vector(vector) if vector.is_closed() => Some("vector"),
            AstForm::ReaderForm(reader) => match reader.prefix_kind()? {
                SyntaxKind::Quote => self.quoted_form_type(&reader.operands().next()?),
                SyntaxKind::FunctionQuote => Some("function"),
                _ => None,
            },
            AstForm::List(list) if list.is_proper_closed() => {
                if list.head().is_none() {
                    return Some("null");
                }
                let operator = self.standard_operator(list)?;
                let args: Vec<_> = list.forms().skip(1).collect();
                self.infer_call(&operator, &args, depth + 1)
            }
            _ => None,
        }
    }

    fn quoted_form_type(&self, form: &AstForm) -> Option<&'static str> {
        match form {
            AstForm::Atom(atom) => {
                if let Some(ty) = number_type(atom.text()) {
                    return Some(ty);
                }
                let symbol = atom.symbol()?;
                Some(
                    if symbol.name == "NIL" && (self.is_standard_symbol)(atom, "NIL") {
                        "null"
                    } else if symbol.prefix == Prefix::Keyword {
                        "keyword"
                    } else {
                        "symbol"
                    },
                )
            }
            AstForm::Literal(literal) => literal_type(literal),
            AstForm::Vector(vector) if vector.is_closed() => Some("vector"),
            AstForm::List(list) if list.is_closed() => Some(if list.head().is_none() {
                "null"
            } else if list.is_dotted() {
                "cons"
            } else {
                "list"
            }),
            _ => None,
        }
    }

    fn infer_call(&self, operator: &str, args: &[AstForm], depth: usize) -> Option<&'static str> {
        let infer = |form: &AstForm| self.infer_form(form, depth);
        let type_of = |index: usize| args.get(index).and_then(infer);
        match operator {
            "QUOTE" => self.quoted_form_type(args.first()?),
            "FUNCTION" | "LAMBDA" => Some("function"),
            "PROGN" | "LOCALLY" => args.last().map_or(Some("null"), infer),
            "PROG1" => type_of(0),
            "PROG2" => type_of(1),
            "LET" | "LET*" => args.get(1..)?.last().map_or(Some("null"), infer),
            "MULTIPLE-VALUE-BIND" | "DESTRUCTURING-BIND" => {
                args.get(2..)?.last().map_or(Some("null"), infer)
            }
            "IF" => {
                let consequent = type_of(1)?;
                let alternative = args.get(2).map_or(Some("null"), infer)?;
                (consequent == alternative).then_some(consequent)
            }
            "AND" if args.is_empty() => Some("boolean"),
            "OR" if args.is_empty() => Some("null"),
            "THE" => self.declared_type(args.first()?),
            "COERCE" => self.declared_type(&self.quoted_operand(args.get(1)?)?),
            "CONCATENATE" => self.declared_type(&self.quoted_operand(args.first()?)?),
            "FORMAT" => {
                let stream = args.first()?.as_atom()?;
                let name = &stream.symbol()?.name;
                if !(self.is_standard_symbol)(stream, name) {
                    return None;
                }
                match name.as_str() {
                    "NIL" => Some("string"),
                    "T" => Some("null"),
                    _ => None,
                }
            }
            "LIST" | "MAKE-LIST" | "MULTIPLE-VALUE-LIST" | "APPEND" | "MAPCAR" | "MAPCAN" => {
                Some("list")
            }
            "CONS" => Some("cons"),
            "VECTOR" => Some("vector"),
            "MAKE-ARRAY" => Some("array"),
            "MAKE-HASH-TABLE" => Some("hash-table"),
            "HASH-TABLE-COUNT" | "LENGTH" | "CHAR-CODE" | "ISQRT" => Some("integer"),
            "CHAR" | "SCHAR" => Some("character"),
            "SUBSEQ" | "REVERSE" | "NREVERSE" => type_of(0),
            "STRING" | "STRING-UPCASE" | "STRING-DOWNCASE" | "STRING-CAPITALIZE"
            | "WRITE-TO-STRING" | "PRIN1-TO-STRING" | "PRINC-TO-STRING" => Some("string"),
            "GENSYM" | "MAKE-SYMBOL" => Some("symbol"),
            "MAKE-PACKAGE" => Some("package"),
            "PATHNAME" | "MERGE-PATHNAMES" | "TRUENAME" => Some("pathname"),
            "NOT" | "NULL" | "ATOM" | "CONSP" | "LISTP" | "NUMBERP" | "INTEGERP" | "STRINGP"
            | "SYMBOLP" | "FUNCTIONP" | "VECTORP" | "ZEROP" | "PLUSP" | "MINUSP" | "EVENP"
            | "ODDP" | "EQ" | "EQL" | "EQUAL" | "EQUALP" | "=" | "/=" | "<" | "<=" | ">" | ">="
            | "TYPEP" => Some("boolean"),
            "+" | "-" | "*" | "1+" | "1-" | "ABS" | "MOD" | "REM" => {
                arithmetic_type(args, infer, "integer")
            }
            "/" => arithmetic_type(args, infer, "rational"),
            "MAX" | "MIN" => arithmetic_type(args, infer, "integer")
                .map(|ty| if ty == "integer" { ty } else { "number" }),
            "ASH" | "LOGAND" | "LOGIOR" | "LOGXOR" | "GCD" | "LCM" => Some("integer"),
            "SQRT" | "EXPT" | "SIN" | "COS" | "TAN" | "ASIN" | "ACOS" | "ATAN" | "EXP" | "LOG" => {
                Some("number")
            }
            "VALUES" if args.len() == 1 => type_of(0),
            _ => None,
        }
    }

    fn quoted_operand(&self, form: &AstForm) -> Option<AstForm> {
        match form {
            AstForm::ReaderForm(reader) if reader.prefix_kind() == Some(SyntaxKind::Quote) => {
                reader.operands().next()
            }
            AstForm::List(list) if self.standard_operator(list).as_deref() == Some("QUOTE") => {
                list.forms().nth(1)
            }
            _ => None,
        }
    }

    fn declared_type(&self, form: &AstForm) -> Option<&'static str> {
        let atom = form.as_atom()?;
        let symbol = atom.symbol()?;
        if !(self.is_standard_symbol)(atom, &symbol.name) {
            return None;
        }
        Some(match symbol.name.as_str() {
            "INTEGER" | "FIXNUM" | "BIGNUM" | "SIGNED-BYTE" | "UNSIGNED-BYTE" => "integer",
            "FLOAT" | "SINGLE-FLOAT" | "DOUBLE-FLOAT" | "SHORT-FLOAT" | "LONG-FLOAT" => "float",
            "NUMBER" | "REAL" => "number",
            "RATIONAL" | "RATIO" => "rational",
            "STRING" | "BASE-STRING" | "SIMPLE-STRING" => "string",
            "CHARACTER" | "BASE-CHAR" | "STANDARD-CHAR" => "character",
            "SYMBOL" => "symbol",
            "CONS" => "cons",
            "NULL" => "null",
            "LIST" => "list",
            "VECTOR" | "SIMPLE-VECTOR" => "vector",
            "ARRAY" => "array",
            "FUNCTION" | "COMPILED-FUNCTION" => "function",
            "PACKAGE" => "package",
            "PATHNAME" => "pathname",
            _ => return None,
        })
    }
}

fn literal_type(literal: &AstLiteral) -> Option<&'static str> {
    match literal.token().kind() {
        SyntaxKind::String | SyntaxKind::RawString => literal.string_value().map(|_| "string"),
        SyntaxKind::Character => literal.character_value().map(|_| "character"),
        _ => None,
    }
}

fn number_type(text: &str) -> Option<&'static str> {
    Some(match number_literal_kind(text)? {
        NumberLiteralKind::Integer => "integer",
        NumberLiteralKind::Ratio => "rational",
        NumberLiteralKind::Float => "float",
    })
}

fn arithmetic_type(
    args: &[AstForm],
    infer: impl Fn(&AstForm) -> Option<&'static str>,
    initial: &'static str,
) -> Option<&'static str> {
    args.iter().try_fold(initial, |result, arg| {
        Some(match (result, infer(arg)?) {
            (_, "number") | ("number", "integer" | "rational" | "float") => "number",
            (_, "float") | ("float", "integer" | "rational") => "float",
            (_, "rational") => "rational",
            (_, "integer") => result,
            _ => return None,
        })
    })
}
