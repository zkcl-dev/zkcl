//! Source-level bounds on the number of values returned on normal completion.

use crate::format::TextEdit;
use crate::navigation::ScopeBinding;
use crate::navigation::index::{closed_list, symbol};
use crate::outline::{Element, elements};
use crate::syntax::{SyntaxKind, SyntaxNode};

use super::super::context::{
    FormContext, LintDocument, LintEmitter, integer_literal_value, is_nil_element, is_nil_syntax,
    is_number_literal_el, is_t_element, unwrap_quote,
};
use super::super::{FixApplicability, LintFix};
use super::{CallSite, ScopeContext, replace_around};

#[derive(Clone, Copy, Debug)]
struct Count {
    min: usize,
    max: Option<usize>,
}

impl Count {
    const UNKNOWN: Self = Self { min: 0, max: None };
    const ONE: Self = Self::fixed(1);

    const fn fixed(values: usize) -> Self {
        Self {
            min: values,
            max: Some(values),
        }
    }

    fn join(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.zip(other.max).map(|(a, b)| a.max(b)),
        }
    }

    fn exact(self) -> Option<usize> {
        self.max.filter(|&max| max == self.min)
    }
}

struct Analysis<'a, 'src> {
    ctx: &'a LintDocument<'src>,
    package: Option<&'a str>,
    lexicals: &'a [ScopeBinding],
    env: &'a FormContext,
    remaining: usize,
}

impl Analysis<'_, '_> {
    fn count(&mut self, el: &Element) -> Count {
        let Some(left) = self.remaining.checked_sub(1) else {
            return Count::UNKNOWN;
        };
        self.remaining = left;
        if let Some(name) = symbol(el) {
            let known_constant = name.prefix == crate::reader_tokens::Prefix::Keyword
                || matches!(
                    self.ctx.workspace.standard_symbol(&name, self.package),
                    Some("NIL" | "T")
                );
            return if !known_constant
                && (self.env.symbol_macros
                    || self.ctx.workspace.is_symbol_macro(&name, self.package))
            {
                Count::UNKNOWN
            } else {
                Count::ONE
            };
        }
        if is_number_literal_el(el)
            || matches!(
                el.kind(),
                SyntaxKind::String
                    | SyntaxKind::RawString
                    | SyntaxKind::Character
                    | SyntaxKind::Vector
            )
        {
            return Count::ONE;
        }
        let Some(node) = el.as_node() else {
            return Count::UNKNOWN;
        };
        if node.kind() == SyntaxKind::ReaderForm {
            return match node.first_token().map(|token| token.kind()) {
                Some(SyntaxKind::Quote | SyntaxKind::FunctionQuote) => Count::ONE,
                _ => Count::UNKNOWN,
            };
        }
        if !closed_list(node) {
            return Count::UNKNOWN;
        }
        let Some(parts) = self.ctx.read_elements(elements(node)) else {
            return Count::UNKNOWN;
        };
        let Some((head, args)) = parts.split_first() else {
            return Count::ONE;
        };
        let Some(name) = symbol(head) else {
            return Count::UNKNOWN;
        };
        let Some(op) = self
            .ctx
            .std_op(&name, self.package, self.lexicals, self.env)
        else {
            return Count::UNKNOWN;
        };
        match op {
            "VALUES" => Count::fixed(args.len()),
            "VALUES-LIST" if args.len() == 1 => self
                .literal_list_size(&args[0])
                .map_or(Count::UNKNOWN, Count::fixed),
            "IF" if (2..=3).contains(&args.len()) => {
                let [test, yes, rest @ ..] = args else {
                    unreachable!()
                };
                let truth = self.truth(test);
                if truth == Some(true) {
                    return self.count(yes);
                }
                let otherwise = rest.first().map_or(Count::ONE, |expr| self.count(expr));
                if truth == Some(false) {
                    otherwise
                } else {
                    otherwise.join(self.count(yes))
                }
            }
            "WHEN" | "UNLESS" if !args.is_empty() => {
                let truth = self.truth(&args[0]).map(|value| value == (op == "WHEN"));
                if truth == Some(false) {
                    return Count::ONE;
                }
                let body = self.last(&args[1..]);
                if truth == Some(true) {
                    body
                } else {
                    body.join(Count::ONE)
                }
            }
            "AND" | "OR" => {
                let Some((last, earlier)) = args.split_last() else {
                    return Count::ONE;
                };
                let continue_on = op == "AND";
                let mut may_short_circuit = false;
                for arg in earlier {
                    match self.truth(arg) {
                        Some(value) if value != continue_on => return Count::ONE,
                        Some(_) => {}
                        None => may_short_circuit = true,
                    }
                }
                let tail = self.count(last);
                if may_short_circuit {
                    tail.join(Count::ONE)
                } else {
                    tail
                }
            }
            "COND" => self.cond_count(args),
            "PROGN" => self.last(args),
            "LOCALLY" => self.body(args),
            "LET" | "LET*" if !args.is_empty() => self.body(&args[1..]),
            "MULTIPLE-VALUE-BIND" if args.len() >= 2 => self.body(&args[2..]),
            "THE" if args.len() == 2 => self.count(&args[1]),
            "UNWIND-PROTECT" | "MULTIPLE-VALUE-PROG1" if !args.is_empty() => self.count(&args[0]),
            _ => builtin_count(op).map_or(Count::UNKNOWN, Count::fixed),
        }
    }

    fn truth(&self, el: &Element) -> Option<bool> {
        if is_nil_element(el, self.package, self.ctx.workspace) {
            Some(false)
        } else if is_t_element(el, self.package, self.ctx.workspace) {
            Some(true)
        } else {
            None
        }
    }

    fn last(&mut self, body: &[Element]) -> Count {
        body.last().map_or(Count::ONE, |expr| self.count(expr))
    }

    fn body(&mut self, forms: &[Element]) -> Count {
        self.last(super::body_after_declarations(
            self.ctx,
            forms,
            self.package,
        ))
    }

    fn cond_count(&mut self, clauses: &[Element]) -> Count {
        let mut paths: Option<Count> = None;
        for clause in clauses {
            let Some(node) = clause.as_node().filter(|node| closed_list(node)) else {
                return Count::UNKNOWN;
            };
            let Some(parts) = self.ctx.read_elements(elements(node)) else {
                return Count::UNKNOWN;
            };
            let Some((test, body)) = parts.split_first() else {
                return Count::UNKNOWN;
            };
            let truth = self.truth(test);
            if truth == Some(false) {
                continue;
            }
            let value = self.last(body);
            paths = Some(paths.map_or(value, |previous| previous.join(value)));
            if truth == Some(true) {
                return paths.unwrap_or(Count::ONE);
            }
        }
        paths.map_or(Count::ONE, |previous| previous.join(Count::ONE))
    }

    fn literal_list_size(&self, form: &Element) -> Option<usize> {
        let quoted = unwrap_quote(form, self.package, self.ctx.workspace);
        let literal = quoted.as_ref().unwrap_or(form);
        if is_nil_syntax(literal, self.package, self.ctx.workspace) {
            return Some(0);
        }
        quoted.as_ref()?;
        let node = literal.as_node().filter(|node| closed_list(node))?;
        self.ctx
            .read_elements(elements(node))
            .map(|items| items.len())
    }
}

fn builtin_count(op: &str) -> Option<usize> {
    Some(match op {
        "QUOTE"
        | "FUNCTION"
        | "LAMBDA"
        | "PROG1"
        | "PROG2"
        | "SETQ"
        | "PSETQ"
        | "MULTIPLE-VALUE-SETQ"
        | "MULTIPLE-VALUE-LIST"
        | "NTH-VALUE"
        | "FORMAT"
        | "+"
        | "-"
        | "*"
        | "/"
        | "1+"
        | "1-"
        | "ABS"
        | "MIN"
        | "MAX"
        | "MOD"
        | "REM"
        | "NOT"
        | "NULL"
        | "EQ"
        | "EQL"
        | "EQUAL"
        | "EQUALP"
        | "="
        | "/="
        | "<"
        | "<="
        | ">"
        | ">="
        | "ZEROP"
        | "PLUSP"
        | "MINUSP"
        | "EVENP"
        | "ODDP"
        | "CONSP"
        | "ATOM"
        | "LISTP"
        | "NUMBERP"
        | "INTEGERP"
        | "STRINGP"
        | "SYMBOLP"
        | "CHARACTERP"
        | "VECTORP"
        | "ARRAYP"
        | "TYPEP"
        | "CAR"
        | "CDR"
        | "CAAR"
        | "CADR"
        | "CDAR"
        | "CDDR"
        | "FIRST"
        | "SECOND"
        | "THIRD"
        | "FOURTH"
        | "REST"
        | "CONS"
        | "LIST"
        | "LIST*"
        | "VECTOR"
        | "MAKE-LIST"
        | "MAKE-ARRAY"
        | "MAKE-STRING"
        | "MAKE-HASH-TABLE"
        | "LENGTH"
        | "LIST-LENGTH"
        | "AREF"
        | "SVREF"
        | "CHAR"
        | "SCHAR"
        | "ELT"
        | "NTH"
        | "SUBSEQ"
        | "APPEND"
        | "REVERSE"
        | "NREVERSE"
        | "COPY-LIST"
        | "COPY-SEQ"
        | "CONCATENATE"
        | "STRING-UPCASE"
        | "STRING-DOWNCASE"
        | "STRING-TRIM"
        | "PRINC-TO-STRING"
        | "PRIN1-TO-STRING"
        | "WRITE-TO-STRING"
        | "SYMBOL-NAME"
        | "GENSYM" => 1,
        "GETHASH" | "FLOOR" | "CEILING" | "TRUNCATE" | "ROUND" | "FFLOOR" | "FCEILING"
        | "FTRUNCATE" | "FROUND" | "INTERN" | "FIND-SYMBOL" | "READ-LINE" | "PARSE-INTEGER"
        | "READ-FROM-STRING" | "SUBTYPEP" | "MACROEXPAND" | "MACROEXPAND-1" => 2,
        "DECODE-FLOAT" | "INTEGER-DECODE-FLOAT" | "GET-PROPERTIES" => 3,
        "DECODE-UNIVERSAL-TIME" | "GET-DECODED-TIME" => 9,
        _ => return None,
    })
}

pub(super) fn returns_one(
    ctx: &LintDocument<'_>,
    form: &Element,
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
) -> bool {
    Analysis {
        ctx,
        package,
        lexicals,
        env,
        remaining: 96,
    }
    .count(form)
    .exact()
        == Some(1)
}

const MIN_MULTIPLE_VALUES_LIMIT: i64 = 20;

enum ListConsumer {
    AllValues,
    FixedIndex(u8),
    WrittenIndex { portable: bool },
}

pub(super) fn check_list_consumer(
    scope: ScopeContext<'_, '_>,
    call: CallSite<'_>,
    operator: &str,
    emit: &mut LintEmitter<'_>,
) {
    let (list, consumer) = match (operator, call.args) {
        ("VALUES-LIST", [list]) => (list, ListConsumer::AllValues),
        ("NTH", [index, list]) => match integer_literal_value(index) {
            Some(value) if value < 0 => return,
            index => (
                list,
                ListConsumer::WrittenIndex {
                    portable: index.is_some_and(|value| value < MIN_MULTIPLE_VALUES_LIMIT),
                },
            ),
        },
        (_, [list]) => {
            let index = match operator {
                "CAR" | "FIRST" => 0,
                "CADR" | "SECOND" => 1,
                "CADDR" | "THIRD" => 2,
                "CADDDR" | "FOURTH" => 3,
                "FIFTH" => 4,
                "SIXTH" => 5,
                "SEVENTH" => 6,
                "EIGHTH" => 7,
                "NINTH" => 8,
                "TENTH" => 9,
                _ => return,
            };
            (list, ListConsumer::FixedIndex(index))
        }
        _ => return,
    };
    let Some(node) = list.as_node() else {
        return;
    };
    if !emit.is_enabled("perf/materialized-multiple-values")
        || !scope.known_package(call.list.text_range())
    {
        return;
    }
    let Some((constructor, args)) = scope.standard_call(node) else {
        return;
    };
    let [producer] = args.as_slice() else {
        return;
    };
    if constructor != "MULTIPLE-VALUE-LIST" {
        return;
    }

    let (message, help) = match consumer {
        ListConsumer::AllValues => (
            "multiple values are put into a list and immediately returned as values again",
            "return the values directly without the intermediate list",
        ),
        ListConsumer::WrittenIndex { portable: false } => (
            "a list of all returned values is built to read just one value",
            "use `nth-value` if the index is below `multiple-values-limit`",
        ),
        _ => (
            "a list of all returned values is built to read just one value",
            "use `nth-value` to select that value directly",
        ),
    };
    emit.emit(
        "perf/materialized-multiple-values",
        list.text_range(),
        message,
        Some(help.into()),
        Vec::new(),
        values_consumer_fix(scope, call, &consumer, node, producer),
    );
}

fn values_consumer_fix(
    scope: ScopeContext<'_, '_>,
    call: CallSite<'_>,
    consumer: &ListConsumer,
    list: &SyntaxNode,
    producer: &Element,
) -> Option<LintFix> {
    let value_range = producer.text_range();
    let around = |outer, prefix, suffix| replace_around(outer, value_range, prefix, suffix);
    let (title, edits) = match consumer {
        // Exposing a producer directly at file top level can change when it
        // is evaluated, for example if it contains an EVAL-WHEN.
        ListConsumer::AllValues if scope.env.in_top_level => return None,
        ListConsumer::AllValues => {
            if let Some(node) = producer.as_node()
                && let Some(head) = elements(node).next().and_then(|element| symbol(&element))
                && scope.doc.workspace.standard_symbol(&head, scope.package) == Some("DECLARE")
            {
                return None;
            }
            let wrapper = if matches!(producer.kind(), SyntaxKind::String | SyntaxKind::RawString) {
                // A bare string could be read as a docstring in a body.
                Some(scope.standard_spelling("values")?)
            } else if scope.env.result_discarded && producer.kind() == SyntaxKind::Atom {
                // A bare symbol or integer can be a tag rather than an expression.
                Some(scope.standard_spelling("multiple-value-prog1")?)
            } else {
                None
            };
            let (prefix, suffix) = wrapper.map_or_else(
                || (String::new(), String::new()),
                |operator| (format!("({operator} "), ")".into()),
            );
            (
                "Return the values without an intermediate list",
                around(call.list, prefix, suffix),
            )
        }
        ListConsumer::FixedIndex(index) => {
            let operator = scope.standard_spelling("nth-value")?;
            (
                "Select the returned value directly with `nth-value`",
                around(call.list, format!("({operator} {index} "), ")".into()),
            )
        }
        ListConsumer::WrittenIndex { .. } => {
            let operator = scope.standard_spelling("nth-value")?;
            let head = elements(call.list).next()?;
            let mut edits = around(list, String::new(), String::new());
            // The original index and producer keep their order and source spelling.
            edits.push(TextEdit {
                range: head.text_range(),
                replacement: operator.into_owned(),
            });
            ("Select the returned value directly with `nth-value`", edits)
        }
    };
    Some(LintFix {
        title: title.into(),
        applicability: if matches!(consumer, ListConsumer::WrittenIndex { portable: false }) {
            FixApplicability::MaybeIncorrect
        } else {
            FixApplicability::MachineApplicable
        },
        edits,
    })
}

pub(super) fn check(
    scope: super::ScopeContext<'_, '_>,
    list: &SyntaxNode,
    op: &str,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    let super::ScopeContext {
        doc: ctx,
        package,
        lexicals,
        env,
    } = scope;
    let mut facts = Analysis {
        ctx,
        package,
        lexicals,
        env,
        remaining: 96,
    };
    match (op, args) {
        ("MULTIPLE-VALUE-BIND" | "MULTIPLE-VALUE-SETQ", [variables, value, ..]) => {
            let Some(node) = variables.as_node().filter(|node| closed_list(node)) else {
                return;
            };
            let Some(names) = ctx.read_elements(elements(node)) else {
                return;
            };
            if names.is_empty() {
                return;
            }
            let count = facts.count(value);
            let Some(max) = count.max.filter(|&max| max < names.len()) else {
                return;
            };
            let first_missing = &names[max];
            if names[max..]
                .iter()
                .all(|name| symbol(name).is_some_and(|symbol| symbol.name.starts_with('_')))
            {
                return;
            }
            let single = op == "MULTIPLE-VALUE-BIND" && count.exact() == Some(1);
            let (rule, range) = if single {
                ("suspicious/single-value-in-mvb", variables.text_range())
            } else {
                (
                    "suspicious/missing-multiple-values",
                    first_missing.text_range(),
                )
            };
            if let Some(diag) = emit.diagnostic(
                rule,
                range,
                format!(
                    "`{}` receives at most {max} value{} for {} variables",
                    op.to_ascii_lowercase(),
                    if max == 1 { "" } else { "s" },
                    names.len()
                ),
                Some("excess variables receive `nil`".into()),
                Vec::new(),
                None,
            ) {
                diag.note(
                    value.text_range(),
                    "this expression determines the maximum number of values",
                );
            }
        }
        ("NTH-VALUE", [index, producer]) => {
            let Some(index_number) = integer_literal_value(index) else {
                return;
            };
            if index_number < 0 {
                super::grammar::report(emit, "correctness/negative-index", index, "`nth-value` requires a nonnegative index");
            } else if let Some(max) = facts.count(producer).max.filter(|&max| index_number as u64 >= max as u64)
                && let Some(diag) = emit.diagnostic("suspicious/nth-value-out-of-range", index.text_range(),
                    format!("value {index_number} is always `nil` because the expression returns at most {max} values"),
                    Some("`nth-value` indices start at zero".into()), Vec::new(), None)
            {
                diag.note(producer.text_range(), "these are the values selected by `nth-value`");
            }
        }
        ("PROG1", [first, ..]) => {
            let count = facts.count(first);
            // A variable number of values may intentionally be reduced to
            // its primary value. Require a provable change on every return.
            let message = if count.min >= 2 {
                "`prog1` keeps only the primary value of this expression"
            } else if count.max == Some(0) {
                "`prog1` returns `nil` where this expression produces zero values"
            } else {
                return;
            };
            if let Some(diag) = emit.diagnostic(
                "suspicious/prog1-drops-values",
                list.text_range(),
                message,
                Some("use `multiple-value-prog1` to preserve the returned values".into()),
                Vec::new(),
                None,
            ) {
                diag.note(
                    first.text_range(),
                    "the values from this expression change at the `prog1` boundary",
                );
            }
        }
        ("MULTIPLE-VALUE-CALL", [target, producers @ ..]) => {
            let Some(actual) = producers.iter().try_fold(0usize, |total, form| {
                total.checked_add(facts.count(form).exact()?)
            }) else {
                return;
            };
            super::call_arguments::check_indirect_target(
                scope,
                target,
                actual,
                true,
                None,
                list.text_range(),
                emit,
            );
        }
        _ => {}
    }
}
