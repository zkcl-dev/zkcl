//! Complexity lint checks: redundant wrappers, boolean bloat, and deep IF chains.

use crate::format::TextEdit;
use crate::navigation::ScopeBinding;
use crate::navigation::index::{closed_list, symbol};
use crate::outline::{Element, elements};
use crate::reader_tokens::Prefix;
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange};

use super::super::context::{
    FormContext, LintDocument, LintEmitter, is_nil_element, is_pure_atom, is_strict_boolean_expr,
    is_string_literal_el, is_t_element, normalized_sexpr,
};
use super::super::{FixApplicability, LintFix, LintTag};

use super::{CallSite, ScopeContext, replace_around};

pub(crate) fn check_call_complexity(
    scope: ScopeContext<'_, '_>,
    call: CallSite<'_>,
    emit: &mut LintEmitter<'_>,
) {
    let ScopeContext {
        doc: ctx,
        package,
        lexicals,
        env,
    } = scope;
    let CallSite {
        list, args, std_op, ..
    } = call;
    let Some(op) = std_op else {
        return;
    };

    match op {
        "IF" => check_if_complexity(ctx, list, args, package, lexicals, env, emit),
        "WHEN" | "UNLESS" => check_collapsible_conditional(scope, op, list, args, emit),
        "NOT" => check_double_negation(scope, list, args, emit),
        "PROGN" => check_redundant_progn(scope, list, args, emit),
        "LET" | "LET*" => check_redundant_let(scope, op, list, args, emit),
        "AND" | "OR" => check_nested_and_or(scope, op, list, args, emit),
        _ => {}
    }
}

fn check_if_complexity(
    ctx: &LintDocument<'_>,
    list: &SyntaxNode,
    args: &[Element],
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    if args.len() < 2 || args.len() > 3 {
        return;
    }
    let test = &args[0];
    let then_el = &args[1];
    let else_el = args.get(2);

    // (if cond t nil) / (if cond nil t) / (if cond t t) / (if cond nil nil)
    if let Some(else_el) = else_el {
        let then_t = is_t_element(then_el, package, ctx.workspace);
        let then_nil = is_nil_element(then_el, package, ctx.workspace);
        let else_t = is_t_element(else_el, package, ctx.workspace);
        let else_nil = is_nil_element(else_el, package, ctx.workspace);

        if then_nil && else_t {
            let replacement = format!("(not {})", ctx.text_of(test.text_range()));
            emit.emit(
                "complexity/if-boolean-branch",
                list.text_range(),
                "`(if cond nil t)` can be simplified to `(not cond)`",
                Some(format!("replace with `{replacement}`")),
                vec![LintTag::Unnecessary],
                ctx.unqualified_standard("not", package, lexicals, env)
                    .then(|| LintFix {
                        title: "Replace with `(not ...)`".into(),
                        applicability: FixApplicability::MachineApplicable,
                        edits: vec![TextEdit {
                            range: list.text_range(),
                            replacement,
                        }],
                    }),
            );
            return;
        }
        if then_t && else_nil {
            let test_src = ctx.text_of(test.text_range());
            let is_exact =
                env.in_boolean_test || is_strict_boolean_expr(ctx, test, package, lexicals, env);
            let applicability = if is_exact {
                FixApplicability::MachineApplicable
            } else {
                FixApplicability::MaybeIncorrect
            };
            let msg = if is_exact {
                "`(if pred t nil)` is redundant when the condition is already boolean"
            } else {
                "`(if expr t nil)` needlessly wraps a condition in `if`; if normalizing truthiness, `(not (null expr))` is clearer"
            };
            emit.emit(
                "complexity/if-boolean-branch",
                list.text_range(),
                msg,
                Some(format!("replace with `{test_src}`")),
                vec![LintTag::Unnecessary],
                Some(LintFix {
                    title: "Simplify redundant `(if ... t nil)`".into(),
                    applicability,
                    edits: vec![TextEdit {
                        range: list.text_range(),
                        replacement: test_src.to_owned(),
                    }],
                }),
            );
            return;
        }
        if (then_t && else_t) || (then_nil && else_nil) {
            emit.emit(
                "complexity/if-boolean-branch",
                list.text_range(),
                "both branches of `if` return the same boolean constant",
                None,
                vec![LintTag::Unnecessary],
                None,
            );
            return;
        }

        // (if (not c) a b) -> (if c b a)
        if let Some(inner) = unwrap_unshadowed_unary_call(ctx, test, "NOT", package, lexicals, env)
            && ctx.unqualified_standard("if", package, lexicals, env)
            && !(emit.is_enabled("complexity/double-negation")
                && negated_operand(ctx, &inner, package, lexicals, env).is_some())
        {
            let replacement = format!(
                "(if {} {} {})",
                ctx.text_of(inner.text_range()),
                ctx.text_of(else_el.text_range()),
                ctx.text_of(then_el.text_range()),
            );
            emit.emit(
                "complexity/if-not-with-else",
                list.text_range(),
                "`(if (not cond) a b)` can flip branches to `(if cond b a)`",
                None,
                Vec::new(),
                Some(LintFix {
                    title: "Remove `not` and swap `if` branches".into(),
                    applicability: FixApplicability::MachineApplicable,
                    edits: vec![TextEdit {
                        range: list.text_range(),
                        replacement,
                    }],
                }),
            );
            return;
        }

        // (if a a b) -> (or a b) when `a` is a pure atom.
        if is_pure_atom(test) && normalized_sexpr(test) == normalized_sexpr(then_el) {
            let replacement = format!(
                "(or {} {})",
                ctx.text_of(test.text_range()),
                ctx.text_of(else_el.text_range()),
            );
            emit.emit(
                "complexity/if-to-and-or",
                list.text_range(),
                "`(if x x fallback)` can be written `(or x fallback)`",
                Some(format!("replace with `{replacement}`")),
                Vec::new(),
                Some(LintFix {
                    title: "Replace with `(or ...)`".into(),
                    applicability: FixApplicability::MaybeIncorrect,
                    edits: vec![TextEdit {
                        range: list.text_range(),
                        replacement,
                    }],
                }),
            );
            return;
        }

        let scope = ScopeContext {
            doc: ctx,
            package,
            lexicals,
            env,
        };
        check_if_chain(scope, list, test, then_el, else_el, emit);
    }
}

fn check_if_chain(
    scope: ScopeContext<'_, '_>,
    list: &SyntaxNode,
    test: &Element,
    then_form: &Element,
    else_form: &Element,
    emit: &mut LintEmitter<'_>,
) {
    let Some(next) = else_form
        .as_node()
        .filter(|node| node.kind() == SyntaxKind::List)
    else {
        return;
    };
    if !emit.is_enabled("complexity/deep-if-chain")
        || !scope.known_package(list.text_range())
        || inside_else_branch(scope, list)
    {
        return;
    }
    let Some((op, parts)) = scope.standard_call(next) else {
        return;
    };
    let (mut clauses, mut fallback) = match (op.as_str(), parts.as_slice()) {
        ("IF", [second_test, second_then, second_else]) => (
            vec![
                (test.clone(), then_form.clone()),
                (second_test.clone(), second_then.clone()),
            ],
            Some(second_else.clone()),
        ),
        _ => return,
    };
    while let Some((op, args)) = fallback
        .as_ref()
        .and_then(Element::as_node)
        .and_then(|node| scope.standard_call(node))
    {
        if op != "IF" || !(2..=3).contains(&args.len()) {
            break;
        }
        let mut args = args.into_iter();
        clauses.push((args.next().unwrap(), args.next().unwrap()));
        fallback = args.next();
    }
    if clauses.len() < 3 {
        return;
    }
    let fix = if scope.env.in_top_level {
        None
    } else {
        if_chain_fix(scope, list, &clauses, fallback.as_ref())
    };
    emit.emit(
        "complexity/deep-if-chain",
        list.text_range(),
        format!(
            "{} nested `if` tests can be written as one `cond`",
            clauses.len()
        ),
        Some(
            "`cond` keeps the tests in order and returns all the values of the selected branch"
                .into(),
        ),
        Vec::new(),
        fix,
    );
}

fn inside_else_branch(scope: ScopeContext<'_, '_>, list: &SyntaxNode) -> bool {
    let Some(parent) = list.parent().filter(|node| node.kind() == SyntaxKind::List) else {
        return false;
    };
    let mut parts = elements(&parent);
    let Some(head) = parts.next().and_then(|el| symbol(&el)) else {
        return false;
    };
    scope
        .doc
        .std_op(&head, scope.package, scope.lexicals, scope.env)
        == Some("IF")
        && parts
            .nth(2)
            .is_some_and(|el| el.text_range() == list.text_range())
}

fn if_chain_fix(
    scope: ScopeContext<'_, '_>,
    list: &SyntaxNode,
    clauses: &[(Element, Element)],
    fallback: Option<&Element>,
) -> Option<LintFix> {
    use std::fmt::Write;

    let cond = scope.standard_spelling("cond")?;
    let indent = scope.continuation(list, cond.len() + 2);
    let text = |el: &Element| scope.doc.text_of(el.text_range());
    let mut replacement = format!("({cond}");
    for (index, (test, body)) in clauses.iter().enumerate() {
        let leading = if index == 0 { " " } else { &indent };
        write!(replacement, "{leading}({} {})", text(test), text(body)).unwrap();
    }
    if let Some(fallback) = fallback
        && !is_nil_element(fallback, scope.package, scope.doc.workspace)
    {
        let t = scope.standard_spelling("t")?;
        write!(replacement, "{indent}({t} {})", text(fallback)).unwrap();
    }
    replacement.push(')');
    Some(LintFix {
        title: "Flatten the `if` chain into `cond`".into(),
        applicability: FixApplicability::MachineApplicable,
        edits: vec![TextEdit {
            range: list.text_range(),
            replacement,
        }],
    })
}

fn check_double_negation(
    scope: ScopeContext<'_, '_>,
    list: &SyntaxNode,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    let [arg] = args else { return };
    let ScopeContext {
        doc: ctx,
        package,
        lexicals,
        env,
    } = scope;
    let Some(target) = negated_operand(ctx, arg, package, lexicals, env) else {
        return;
    };
    if !env.in_boolean_test && !is_strict_boolean_expr(ctx, &target, package, lexicals, env) {
        // The expression can normalize a generalized boolean to T or NIL.
        return;
    }
    emit.emit(
        "complexity/double-negation",
        list.text_range(),
        "redundant double negation",
        Some(format!(
            "simplify directly to `{}`",
            ctx.text_of(target.text_range())
        )),
        vec![LintTag::Unnecessary],
        (!env.in_tagbody
            || target
                .as_node()
                .is_some_and(|node| node.kind() == SyntaxKind::List))
        .then(|| LintFix {
            title: "Remove double negation".into(),
            applicability: FixApplicability::MachineApplicable,
            edits: replace_around(list, target.text_range(), String::new(), String::new()),
        }),
    );
}

fn negated_operand(
    ctx: &LintDocument<'_>,
    form: &Element,
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
) -> Option<Element> {
    unwrap_unshadowed_unary_call(ctx, form, "NOT", package, lexicals, env)
        .or_else(|| unwrap_unshadowed_unary_call(ctx, form, "NULL", package, lexicals, env))
}

fn check_redundant_progn(
    scope: ScopeContext<'_, '_>,
    list: &SyntaxNode,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    let ctx = scope.doc;
    let env = scope.env;
    if args.is_empty() {
        let fix = if env.in_tagbody {
            None
        } else {
            let nil_sym = crate::reader_tokens::SymbolName {
                prefix: Prefix::None,
                name: "NIL".to_owned(),
            };
            let nil_spelling =
                if ctx.workspace.standard_symbol(&nil_sym, scope.package) == Some("NIL") {
                    "nil"
                } else {
                    "common-lisp:nil"
                };
            Some(LintFix {
                title: "Replace empty `(progn)` with `nil`".into(),
                applicability: FixApplicability::MachineApplicable,
                edits: vec![TextEdit {
                    range: list.text_range(),
                    replacement: nil_spelling.into(),
                }],
            })
        };
        emit.emit(
            "complexity/redundant-progn",
            list.text_range(),
            "empty `(progn)` can be replaced with `nil`",
            None,
            vec![LintTag::Unnecessary],
            fix,
        );
        return;
    }
    if args.len() == 1 {
        let inner = ctx.text_of(args[0].text_range()).to_owned();
        let unsafe_tagbody_atom = env.in_tagbody && args[0].as_token().is_some();
        let unsafe_docstring =
            is_string_literal_el(&args[0]) && is_first_form_in_documented_body(scope, list);
        let fix = (!unsafe_tagbody_atom && !unsafe_docstring).then(|| LintFix {
            title: "Unwrap single-form `progn`".into(),
            applicability: FixApplicability::MachineApplicable,
            edits: vec![TextEdit {
                range: list.text_range(),
                replacement: inner,
            }],
        });
        emit.emit(
            "complexity/redundant-progn",
            list.text_range(),
            "single-form `progn` wrapper is unnecessary",
            None,
            vec![LintTag::Unnecessary],
            fix,
        );
        return;
    }
    if !env.in_top_level && is_in_implicit_progn_parent(scope, list) {
        emit.emit(
            "complexity/redundant-progn",
            list.text_range(),
            "redundant `progn` inside an implicit-progn body",
            Some("splice the forms directly into the enclosing body".into()),
            vec![LintTag::Unnecessary],
            None,
        );
    }
}

fn is_first_form_in_documented_body(scope: ScopeContext<'_, '_>, list: &SyntaxNode) -> bool {
    let Some(parent) = list.parent() else {
        return false;
    };
    if parent.kind() != SyntaxKind::List || !closed_list(&parent) {
        return false;
    }
    let Some(head) = elements(&parent).next().and_then(|e| symbol(&e)) else {
        return false;
    };
    let Some(std) = scope
        .doc
        .std_op(&head, scope.package, scope.lexicals, scope.env)
    else {
        return false;
    };
    if !matches!(
        std,
        "DEFUN" | "DEFMACRO" | "DEFMETHOD" | "LAMBDA" | "FLET" | "LABELS" | "MACROLET"
    ) {
        return false;
    }
    let body_start = match std {
        "LAMBDA" => 2,
        "DEFUN" | "DEFMACRO" => 3,
        "DEFMETHOD" => {
            let args: Vec<_> = elements(&parent).skip(1).collect();
            crate::outline::defmethod_lambda_list_index(&args).map_or(usize::MAX, |i| i + 2)
        }
        _ => return false,
    };
    let parent_elems: Vec<_> = elements(&parent).collect();
    parent_elems.len() > body_start + 1
        && parent_elems
            .get(body_start)
            .is_some_and(|first| first.text_range() == list.text_range())
}

fn is_in_implicit_progn_parent(scope: ScopeContext<'_, '_>, list: &SyntaxNode) -> bool {
    let Some(parent) = list.parent() else {
        return false;
    };
    if parent.kind() != SyntaxKind::List || !closed_list(&parent) {
        return false;
    }
    let Some(head) = elements(&parent).next().and_then(|e| symbol(&e)) else {
        // Parent might be a COND / CASE / TYPECASE / HANDLER-CASE clause list.
        if let Some(grandparent) = parent.parent()
            && grandparent.kind() == SyntaxKind::List
            && closed_list(&grandparent)
            && let Some(ghead) = elements(&grandparent).next().and_then(|e| symbol(&e))
            && let Some(gstd) = scope
                .doc
                .std_op(&ghead, scope.package, scope.lexicals, scope.env)
        {
            let clause_body_start = match gstd {
                "COND" => 1,
                "CASE" | "ECASE" | "CCASE" | "TYPECASE" | "ETYPECASE" | "CTYPECASE"
                    if elements(&grandparent)
                        .nth(1)
                        .is_some_and(|key| key.text_range() != parent.text_range()) =>
                {
                    1
                }
                "HANDLER-CASE"
                    if elements(&grandparent)
                        .nth(1)
                        .is_some_and(|expr| expr.text_range() != parent.text_range()) =>
                {
                    2
                }
                _ => return false,
            };
            return elements(&parent)
                .nth(clause_body_start)
                .is_some_and(|b| b.text_range().start() <= list.text_range().start());
        }
        return false;
    };
    let Some(std) = scope
        .doc
        .std_op(&head, scope.package, scope.lexicals, scope.env)
    else {
        return false;
    };
    let body_idx = match std {
        "PROGN" | "LOCALLY" | "WITH-STANDARD-IO-SYNTAX" => 1,
        "WHEN"
        | "UNLESS"
        | "LET"
        | "LET*"
        | "FLET"
        | "LABELS"
        | "MACROLET"
        | "SYMBOL-MACROLET"
        | "LAMBDA"
        | "BLOCK"
        | "UNWIND-PROTECT"
        | "WITH-OPEN-FILE"
        | "WITH-OPEN-STREAM"
        | "WITH-INPUT-FROM-STRING"
        | "WITH-OUTPUT-TO-STRING" => 2,
        "DEFUN"
        | "DEFMACRO"
        | "DEFINE-COMPILER-MACRO"
        | "MULTIPLE-VALUE-BIND"
        | "DESTRUCTURING-BIND"
        | "PROGV"
        | "WITH-SLOTS"
        | "WITH-ACCESSORS" => 3,
        "DEFMETHOD" => {
            let args: Vec<_> = elements(&parent).skip(1).collect();
            let Some(lambda_idx) = crate::outline::defmethod_lambda_list_index(&args) else {
                return false;
            };
            lambda_idx + 2
        }
        _ => return false,
    };
    elements(&parent)
        .nth(body_idx)
        .is_some_and(|b| b.text_range().start() <= list.text_range().start())
}

fn check_redundant_let(
    scope: ScopeContext<'_, '_>,
    op: &str,
    list: &SyntaxNode,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    let ScopeContext {
        doc: ctx,
        package,
        lexicals,
        env,
    } = scope;
    let Some(bindings_el) = args.first() else {
        return;
    };
    let body = args.get(1..).unwrap_or(&[]);
    let has_declare = body.iter().any(|b| {
        b.as_node().is_some_and(|n| {
            closed_list(n)
                && elements(n)
                    .next()
                    .and_then(|e| symbol(&e))
                    .is_some_and(|s| ctx.workspace.standard_symbol(&s, package) == Some("DECLARE"))
        })
    });

    if body.is_empty() {
        emit.emit(
            "suspicious/empty-body",
            list.text_range(),
            format!(
                "`{}` has an empty body and always returns `nil`",
                op.to_ascii_lowercase()
            ),
            None,
            Vec::new(),
            None,
        );
        return;
    }

    let binding_items: Vec<Element> =
        if super::super::context::is_nil_syntax(bindings_el, package, ctx.workspace) {
            Vec::new()
        } else if let Some(bnode) = bindings_el.as_node().filter(|n| closed_list(n)) {
            let Some(items) = ctx.read_elements(elements(bnode)) else {
                return;
            };
            items
        } else {
            return;
        };

    if binding_items.is_empty() && !has_declare {
        let unsafe_single = body.len() == 1
            && ((env.in_tagbody && body[0].as_token().is_some())
                || (is_string_literal_el(&body[0])
                    && is_first_form_in_documented_body(scope, list)));
        let replacement = if body.len() == 1 && !unsafe_single {
            ctx.text_of(body[0].text_range()).to_owned()
        } else {
            let joined = body
                .iter()
                .map(|b| ctx.text_of(b.text_range()))
                .collect::<Vec<_>>()
                .join(" ");
            format!("(progn {joined})")
        };
        emit.emit(
            "complexity/redundant-let",
            list.text_range(),
            format!(
                "empty `{}` binding list is unnecessary",
                op.to_ascii_lowercase()
            ),
            None,
            vec![LintTag::Unnecessary],
            Some(LintFix {
                title: format!("Remove empty `{}` wrapper", op.to_ascii_lowercase()),
                applicability: if env.in_top_level {
                    FixApplicability::MaybeIncorrect
                } else {
                    FixApplicability::MachineApplicable
                },
                edits: vec![TextEdit {
                    range: list.text_range(),
                    replacement,
                }],
            }),
        );
        return;
    }

    // Immediate return of single binding: `(let ((x expr)) x)` -> `expr`
    if binding_items.len() == 1
        && body.len() == 1
        && !has_declare
        && let Some((var_el, Some(init_el))) = parse_binding_pair(&binding_items[0])
        && let Some(var_sym) = symbol(&var_el)
        && var_sym.prefix != Prefix::Uninterned
        && let Some(ret_sym) = symbol(&body[0])
        && var_sym.name == ret_sym.name
        && var_sym.prefix == ret_sym.prefix
    {
        let init = ctx.text_of(init_el.text_range());
        let fix = if !scope.known_package(list.text_range()) {
            None
        } else if !env.in_top_level
            && !(env.in_tagbody && init_el.as_token().is_some())
            && super::values::returns_one(ctx, &init_el, package, lexicals, env)
        {
            Some(init.to_owned())
        } else {
            // The binding consumes one primary value even when the initform
            // returns zero or several values. A VALUES argument does the same.
            scope
                .standard_spelling("values")
                .map(|values| format!("({values} {init})"))
        }
        .map(|replacement| LintFix {
            title: "Inline immediately returned binding".into(),
            applicability: FixApplicability::MachineApplicable,
            edits: vec![TextEdit {
                range: list.text_range(),
                replacement,
            }],
        });
        emit.emit(
            "complexity/redundant-let",
            list.text_range(),
            format!(
                "`{}` binds `{}` only to return it immediately",
                op.to_ascii_lowercase(),
                var_sym.name.to_ascii_lowercase()
            ),
            Some("return the initializer's primary value directly".into()),
            vec![LintTag::Unnecessary],
            fix,
        );
        return;
    }

    // Single-binding LET* -> LET (or independent LET* bindings -> LET)
    if op == "LET*" && !binding_items.is_empty() {
        let mut head_tok = elements(list).next();
        if let Some(head_el) = head_tok.take() {
            if binding_items.len() == 1 {
                let fix = scope.standard_spelling("let").map(|let_kw| LintFix {
                    title: "Replace `let*` with `let`".into(),
                    applicability: FixApplicability::MachineApplicable,
                    edits: vec![TextEdit {
                        range: head_el.text_range(),
                        replacement: let_kw.into_owned(),
                    }],
                });
                emit.emit(
                    "complexity/redundant-let",
                    head_el.text_range(),
                    "`let*` with a single binding can be written as `let`",
                    None,
                    vec![LintTag::Unnecessary],
                    fix,
                );
            } else if let_star_bindings_are_independent(&binding_items, body, ctx, package) {
                let fix = scope.standard_spelling("let").map(|let_kw| LintFix {
                    title: "Replace independent `let*` with `let`".into(),
                    applicability: FixApplicability::MaybeIncorrect,
                    edits: vec![TextEdit {
                        range: head_el.text_range(),
                        replacement: let_kw.into_owned(),
                    }],
                });
                emit.emit(
                    "complexity/redundant-let",
                    head_el.text_range(),
                    "`let*` bindings do not depend on each other; prefer `let`",
                    None,
                    vec![LintTag::Unnecessary],
                    fix,
                );
            }
        }
    }

    // Two-step initialization: `(let ((x nil)) (setq x expr) ...)`
    if !has_declare
        && binding_items.len() == 1
        && body.len() >= 2
        && let Some((var_el, init_opt)) = parse_binding_pair(&binding_items[0])
        && let Some(var_sym) = symbol(&var_el)
        && var_sym.prefix != Prefix::Uninterned
        && !(var_sym.name.starts_with('*') && var_sym.name.ends_with('*'))
        && !ctx.workspace.is_proclaimed_special(&var_sym, package)
        && init_opt.is_none_or(|init| is_nil_element(&init, package, ctx.workspace))
        && let Some(first_body) = body[0].as_node().filter(|n| closed_list(n))
        && let mut bparts = elements(first_body)
        && let Some(bhead) = bparts.next().and_then(|e| symbol(&e))
        && matches!(
            ctx.workspace.standard_symbol(&bhead, package),
            Some("SETQ" | "SETF")
        )
        && let Some(target_el) = bparts.next()
        && let Some(target_sym) = symbol(&target_el)
        && target_sym.name == var_sym.name
        && target_sym.prefix == var_sym.prefix
        && let Some(rhs_el) = bparts.next()
        && bparts.next().is_none()
        && !expr_mentions_symbol(&rhs_el, &var_sym)
    {
        emit.emit(
            "complexity/redundant-let",
            first_body.text_range(),
            format!(
                "`{}` is initialized to `nil` and immediately assigned in the first body form",
                var_sym.name.to_ascii_lowercase()
            ),
            Some("initialize the binding directly inside `let`".into()),
            Vec::new(),
            None,
        );
    }
}

fn parse_binding_pair(spec: &Element) -> Option<(Element, Option<Element>)> {
    if let Some(node) = spec.as_node().filter(|n| closed_list(n)) {
        let mut parts = elements(node);
        let var = parts.next()?;
        let init = parts.next();
        if parts.next().is_some() {
            return None;
        }
        Some((var, init))
    } else if spec.as_token().is_some() {
        Some((spec.clone(), None))
    } else {
        None
    }
}

fn let_star_bindings_are_independent(
    items: &[Element],
    body: &[Element],
    ctx: &LintDocument<'_>,
    package: Option<&str>,
) -> bool {
    // `(declare (special ...))` changes semantics between `let` and `let*` when
    // earlier bindings are observed by later initializers.
    if body_declares_special(body, ctx, package) {
        return false;
    }
    let mut prior_names: Vec<String> = Vec::new();
    for item in items {
        let Some((var_el, init_opt)) = parse_binding_pair(item) else {
            return false;
        };
        if let Some(init) = init_opt
            && expr_mentions_any_symbol(&init, &prior_names)
        {
            return false;
        }
        if let Some(sym) = symbol(&var_el) {
            if sym.prefix == Prefix::None && prior_names.contains(&sym.name) {
                // `let*` allows duplicate binding names, whereas `let` rejects them.
                return false;
            }
            if sym.name.starts_with('*') && sym.name.ends_with('*') {
                // Dynamic bindings might be observed by initializers.
                return false;
            }
            if ctx.workspace.is_proclaimed_special(&sym, package) {
                return false;
            }
            prior_names.push(sym.name);
        }
    }
    true
}

fn body_declares_special(body: &[Element], ctx: &LintDocument<'_>, package: Option<&str>) -> bool {
    for el in body {
        let Some(node) = el.as_node().filter(|n| closed_list(n)) else {
            if el
                .as_token()
                .is_some_and(|t| matches!(t.kind(), SyntaxKind::String | SyntaxKind::RawString))
            {
                continue;
            }
            break;
        };
        let mut parts = elements(node);
        let Some(head) = parts.next().and_then(|e| symbol(&e)) else {
            break;
        };
        if ctx.workspace.standard_symbol(&head, package) != Some("DECLARE") {
            break;
        }
        for spec in parts {
            if let Some(snode) = spec.as_node().filter(|n| closed_list(n))
                && let Some(shead) = elements(snode).next().and_then(|e| symbol(&e))
                && ctx.workspace.standard_symbol(&shead, package) == Some("SPECIAL")
            {
                return true;
            }
        }
    }
    false
}

fn expr_mentions_symbol(el: &Element, target: &crate::reader_tokens::SymbolName) -> bool {
    if let Some(sym) = symbol(el) {
        return sym.prefix == target.prefix && sym.name == target.name;
    }
    if let Some(node) = el.as_node() {
        return elements(node).any(|child| expr_mentions_symbol(&child, target));
    }
    false
}

fn expr_mentions_any_symbol(el: &Element, names: &[String]) -> bool {
    if let Some(sym) = symbol(el) {
        return matches!(sym.prefix, crate::reader_tokens::Prefix::None)
            && names.iter().any(|n| n == &sym.name);
    }
    if let Some(node) = el.as_node() {
        return elements(node).any(|child| expr_mentions_any_symbol(&child, names));
    }
    false
}

fn check_nested_and_or(
    scope: ScopeContext<'_, '_>,
    op: &str,
    list: &SyntaxNode,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    let ScopeContext {
        doc: ctx,
        package,
        lexicals,
        env,
    } = scope;
    if args.len() == 1 {
        let inner = ctx.text_of(args[0].text_range()).to_owned();
        emit.emit(
            "complexity/nested-and-or",
            list.text_range(),
            format!(
                "single-argument `({})` is redundant",
                op.to_ascii_lowercase()
            ),
            None,
            vec![LintTag::Unnecessary],
            Some(LintFix {
                title: format!("Unwrap single-argument `{}`", op.to_ascii_lowercase()),
                applicability: FixApplicability::MachineApplicable,
                edits: vec![TextEdit {
                    range: list.text_range(),
                    replacement: inner,
                }],
            }),
        );
        return;
    }

    for arg in args {
        if let Some(inner) = arg
            .as_node()
            .filter(|n| n.kind() == SyntaxKind::List && closed_list(n))
            && let Some(inner_head) = elements(inner).next().and_then(|e| symbol(&e))
            && ctx.std_op(&inner_head, package, lexicals, env) == Some(op)
            // An empty inner form can change the returned value count.
            && elements(inner).nth(1).is_some()
        {
            emit.emit(
                "complexity/nested-and-or",
                inner.text_range(),
                format!(
                    "nested `{}` inside `{}` can be flattened",
                    op.to_ascii_lowercase(),
                    op.to_ascii_lowercase()
                ),
                None,
                vec![LintTag::Unnecessary],
                None,
            );
        }
    }
}

/// Like the shape-only helper, for a rewrite that relies on the resolved operator.
pub(super) fn unwrap_unshadowed_unary_call(
    ctx: &LintDocument<'_>,
    el: &Element,
    operator: &str,
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
) -> Option<Element> {
    let node = el.as_node().filter(|node| closed_list(node))?;
    let mut parts =
        elements(node).filter_map(|part| super::possibly_read(part, ctx.source.known_features()));
    let (head, true) = parts.next()? else {
        return None;
    };
    let head = symbol(&head)?;
    if ctx.std_op(&head, package, lexicals, env) != Some(operator) {
        return None;
    }
    let (arg, true) = parts.next()? else {
        return None;
    };
    parts.next().is_none().then_some(arg)
}

fn check_collapsible_conditional(
    scope: ScopeContext<'_, '_>,
    op: &str,
    list: &SyntaxNode,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    if !emit.is_enabled("complexity/collapsible-conditional") || args.len() != 2 {
        return;
    }
    let ctx = scope.doc;
    let Some(inner_node) = args[1].as_node().filter(|n| closed_list(n)) else {
        return;
    };
    let Some((_, inner_elems, inner_std)) =
        ctx.resolve_head(inner_node, scope.package, scope.lexicals, scope.env)
    else {
        return;
    };
    if inner_std.as_deref() != Some(op) || inner_elems.len() < 2 {
        return;
    }
    let combiner = if op == "WHEN" { "and" } else { "or" };
    let has_outer_comments = ctx
        .text_of(TextRange::new(
            args[0].text_range().end(),
            inner_node.text_range().start(),
        ))
        .contains(';')
        || ctx
            .text_of(TextRange::new(
                inner_node.text_range().end(),
                list.text_range().end(),
            ))
            .contains(';');
    let fix = if scope.known_package(list.text_range()) && !has_outer_comments {
        scope.standard_spelling(combiner).map(|comb_kw| {
            let outer_test = ctx.text_of(args[0].text_range());
            let inner_test = ctx.text_of(inner_elems[0].text_range());
            let op_text = ctx.text_of(elements(list).next().expect("head").text_range());
            let body_slice = if inner_elems.len() >= 2 {
                format!(
                    " {}",
                    ctx.text_of(TextRange::new(
                        inner_elems[1].text_range().start(),
                        inner_elems.last().expect("non-empty").text_range().end(),
                    ))
                )
            } else {
                String::new()
            };
            LintFix {
                title: format!(
                    "Collapse nested `{}` into `({} ({} ...) ...)`",
                    op.to_ascii_lowercase(),
                    op.to_ascii_lowercase(),
                    combiner
                ),
                applicability: FixApplicability::MachineApplicable,
                edits: vec![TextEdit {
                    range: list.text_range(),
                    replacement: format!(
                        "({op_text} ({comb_kw} {outer_test} {inner_test}){body_slice})"
                    ),
                }],
            }
        })
    } else {
        None
    };
    emit.emit(
        "complexity/collapsible-conditional",
        list.text_range(),
        format!(
            "nested `{}` blocks can be combined with `{combiner}`",
            op.to_ascii_lowercase()
        ),
        Some(format!(
            "combine into `({} ({combiner} ...) ...)`",
            op.to_ascii_lowercase()
        )),
        vec![LintTag::Unnecessary],
        fix,
    );
}
