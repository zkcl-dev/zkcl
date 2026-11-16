//! Traverse the executable positions of standard Common Lisp operators.

use std::rc::Rc;

use crate::cl_builtins::evaluates_all_arguments;
use crate::navigation::index::{closed_list, possibly_read, symbol};
use crate::outline::{Element, elements};
use crate::reader_tokens::Prefix;
use crate::syntax::{SyntaxKind, SyntaxNode};

use super::super::context::{
    BranchFact, FormContext, LocalMacroTemplate, LoopBinding, is_nil_element, is_nil_syntax,
    repeatable_test,
};
use super::super::macro_template::MacroTemplate;
use super::{
    BodyKind, ScopeContext, WalkContext, body_after_declarations, check_declaim_special_earmuffs,
    check_function_name, clause_fields, correctness, dataflow, grammar, indexed_loop,
    is_lambda_expression, push_definition_block, style_pedantic, suspicious, walk_loop_form,
};
use grammar::LambdaListKind;

pub(super) fn walk<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    list: &SyntaxNode,
    op: &str,
    complete: bool,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    match op {
        "QUOTE" | "IN-PACKAGE" => Some(()),
        "DECLARE" => {
            if complete {
                walk.emit.emit(
                    "correctness/misplaced-declaration",
                    list.text_range(),
                    "`declare` is only valid at the start of a declaration-bearing body",
                    Some(
                        "move the declaration to the start of that body or wrap the forms in `locally`"
                            .into(),
                    ),
                    Vec::new(),
                    None,
                );
            }
            Some(())
        }
        "FUNCTION" => {
            if let Some(target) = args.first() {
                if is_lambda_expression(walk.doc, target) {
                    walk.enqueue(target, &env.nested_expr(false, false));
                } else {
                    check_function_name(walk.doc, target, walk.package, walk.emit);
                }
            }
            Some(())
        }
        "LOAD-TIME-VALUE" => {
            if let Some(value) = args.first() {
                walk.enqueue(value, &FormContext::default());
            }
            Some(())
        }
        "RETURN-FROM" => {
            if let Some(value) = args.get(1) {
                walk.enqueue(value, &env.nested_expr(false, false));
            }
            Some(())
        }
        "DECLAIM" => {
            check_declaim_special_earmuffs(args, walk.package, walk.doc, walk.emit);
            correctness::check_declaration_specs(walk.doc, args, walk.package, walk.emit);
            Some(())
        }
        "DEFPACKAGE" => {
            correctness::check_defpackage_options(walk.doc, args, walk.package, walk.emit);
            style_pedantic::check_defpackage_style(walk.doc, args, walk.package, walk.emit);
            Some(())
        }
        "DEFVAR" | "DEFPARAMETER" | "DEFCONSTANT" => {
            if env.in_top_level {
                style_pedantic::check_top_level_definition_style(
                    walk.doc,
                    op,
                    list,
                    args,
                    walk.package,
                    walk.emit,
                );
            }
            if let Some(init) = args.get(1) {
                walk.enqueue(init, &env.nested_expr(false, false));
            }
            Some(())
        }
        "DEFUN" | "DEFMACRO" | "DEFTYPE" => walk_defun_like(walk, list, op, args, env),
        "DEFINE-COMPILER-MACRO" => walk_compiler_macro(walk, list, args, env),
        "DEFMETHOD" => walk_defmethod(walk, list, args, env),
        "DEFCLASS" | "DEFINE-CONDITION" => {
            if env.in_top_level {
                style_pedantic::check_top_level_definition_style(
                    walk.doc,
                    op,
                    list,
                    args,
                    walk.package,
                    walk.emit,
                );
            }
            if let Some(slots) = args.get(2) {
                correctness::check_duplicate_defclass_slots(
                    walk.doc,
                    walk.package,
                    slots,
                    walk.emit,
                );
                suspicious::check_defclass_suspicious(walk.doc, slots, walk.package, walk.emit);
            }
            super::clos::check(
                walk.doc,
                op,
                args,
                env,
                walk.emit,
                walk.work,
                walk.cancelled,
            )
        }
        "DEFSTRUCT" => {
            if env.in_top_level {
                style_pedantic::check_top_level_definition_style(
                    walk.doc,
                    "DEFSTRUCT",
                    list,
                    args,
                    walk.package,
                    walk.emit,
                );
            }
            correctness::check_duplicate_defstruct_slots(args, walk.emit);
            suspicious::check_defstruct_suspicious(walk.doc, args, walk.package, walk.emit);
            for slot in args.iter().skip(1) {
                if let Some(snode) = slot.as_node().filter(|n| closed_list(n))
                    && let Some(init) = elements(snode).nth(1)
                {
                    walk.enqueue(&init, &env.nested_expr(false, false));
                }
            }
            Some(())
        }
        "DEFGENERIC" => {
            if env.in_top_level {
                style_pedantic::check_top_level_definition_style(
                    walk.doc,
                    "DEFGENERIC",
                    list,
                    args,
                    walk.package,
                    walk.emit,
                );
            }
            if let Some(name) = args.first() {
                check_function_name(walk.doc, name, walk.package, walk.emit);
            }
            if let [name, params, options @ ..] = args {
                style_pedantic::check_lambda_list_style(params, walk.emit);
                super::clos::check_generic(walk, name, params, options, env)?;
            }
            Some(())
        }
        "LAMBDA" => {
            let body_env = env.function_body();
            if let Some(params) = args.first() {
                walk.walk_lambda_initforms(params, LambdaListKind::Ordinary, &body_env)?;
                style_pedantic::check_lambda_list_style(params, walk.emit);
            }
            walk.walk_body(
                args.get(1..).unwrap_or(&[]),
                &body_env,
                BodyKind::Documented,
            )
        }
        "EVAL-WHEN" => {
            correctness::check_eval_when_form(walk.doc, args, walk.package, walk.emit);
            let body = args.get(1..).unwrap_or(&[]);
            for (idx, form) in body.iter().enumerate() {
                let mut child_env = env.clone();
                child_env.entry_test = None;
                if !env.in_top_level {
                    child_env.result_discarded = env.result_discarded || idx + 1 < body.len();
                }
                walk.enqueue(form, &child_env);
            }
            Some(())
        }
        "PROGN" | "LOCALLY" => {
            let declarations = op == "LOCALLY";
            if env.in_top_level {
                let body = if declarations {
                    body_after_declarations(walk.doc, args, walk.package)
                } else {
                    args
                };
                for form in body {
                    walk.enqueue(form, env);
                }
                Some(())
            } else {
                walk.walk_body(
                    args,
                    &env.nested_expr(env.in_boolean_test, env.result_discarded),
                    if declarations {
                        BodyKind::Declared
                    } else {
                        BodyKind::Forms
                    },
                )
            }
        }
        "LET" | "LET*" => walk_let_form(walk, op, args, env),
        "FLET" | "LABELS" | "MACROLET" => walk_local_fn_form(walk, op, args, env),
        "BLOCK" => walk_block_form(walk, args, env),
        "TAGBODY" => walk.walk_tagbody(args, &env.nested_expr(false, true)),
        "LOOP" => walk_loop_form(walk, args, env),
        "DOLIST" | "DOTIMES" | "DO-SYMBOLS" | "DO-EXTERNAL-SYMBOLS" | "DO-ALL-SYMBOLS" => {
            walk_dotimes_like(walk, list, op, args, env)
        }
        "DO" | "DO*" | "PROG" | "PROG*" => walk_do_prog_form(walk, op, args, env),
        "IF" | "WHEN" | "UNLESS" => walk_branch_form(walk, op, args, env),
        "COND" => walk_cond_form(walk, args, env),
        "CASE" | "ECASE" | "CCASE" | "TYPECASE" | "ETYPECASE" | "CTYPECASE" => {
            walk_case_form(walk, op, args, env)
        }
        "AND" | "OR" => {
            for (idx, arg) in args.iter().enumerate() {
                let is_last = idx + 1 == args.len();
                let sub_env = if is_last {
                    env.nested_expr(env.in_boolean_test, env.result_discarded)
                } else {
                    env.nested_expr(
                        op == "AND" || env.in_boolean_test || env.result_discarded,
                        false,
                    )
                };
                walk.enqueue(arg, &sub_env);
            }
            Some(())
        }
        "NOT" => {
            for arg in args {
                walk.enqueue(arg, &env.nested_expr(true, false));
            }
            Some(())
        }
        "UNWIND-PROTECT" => {
            if let Some(protected) = args.first() {
                let mut prot_env = env.nested_expr(env.in_boolean_test, env.result_discarded);
                prot_env.in_open_guard = true;
                walk.enqueue(protected, &prot_env);
            }
            // Cleanup can legally leave an active outer block, tagbody, or catch.
            // Keep lexical checks, without inferring the dynamic extent of the target.
            let cleanup_env = env.nested_expr(false, true);
            for cleanup in args.iter().skip(1) {
                walk.enqueue(cleanup, &cleanup_env);
            }
            Some(())
        }
        "WITH-OPEN-FILE"
        | "WITH-OPEN-STREAM"
        | "WITH-INPUT-FROM-STRING"
        | "WITH-OUTPUT-TO-STRING"
        | "WITH-HASH-TABLE-ITERATOR"
        | "WITH-PACKAGE-ITERATOR"
        | "PRINT-UNREADABLE-OBJECT" => walk_with_form(walk, op, args, env),
        "DESTRUCTURING-BIND" => {
            if let Some(pattern) = args.first() {
                walk.walk_lambda_initforms(pattern, LambdaListKind::Destructuring, env)?;
            }
            if let Some(value) = args.get(1) {
                walk.enqueue(value, &env.nested_expr(false, false));
            }
            walk.walk_body(
                args.get(2..).unwrap_or(&[]),
                &env.nested_expr(env.in_boolean_test, env.result_discarded),
                BodyKind::Declared,
            )
        }
        "SYMBOL-MACROLET" | "WITH-SLOTS" | "WITH-ACCESSORS" => {
            let mut body_env = env.nested_expr(env.in_boolean_test, env.result_discarded);
            body_env.symbol_macros = true;
            let body_start = if op == "SYMBOL-MACROLET" { 1 } else { 2 };
            if body_start == 2
                && let Some(instance) = args.get(1)
            {
                walk.enqueue(instance, &env.nested_expr(false, false));
            }
            walk.walk_body(
                args.get(body_start..).unwrap_or(&[]),
                &body_env,
                BodyKind::Declared,
            )
        }
        "RESTART-CASE" => walk_restart_case(walk, args, env),
        "HANDLER-BIND" | "RESTART-BIND" => walk_condition_bind_form(walk, op, args, env),
        "HANDLER-CASE" => walk_handler_case(walk, args, env),
        "STEP" | "TIME" => {
            if let Some(expression) = args.first() {
                walk.enqueue(
                    expression,
                    &env.nested_expr(env.in_boolean_test, env.result_discarded),
                );
            }
            Some(())
        }
        "MULTIPLE-VALUE-BIND" | "MULTIPLE-VALUE-SETQ" => {
            if let Some(names) = args.first() {
                grammar::multiple_value_names(
                    walk.doc,
                    names,
                    op == "MULTIPLE-VALUE-BIND",
                    walk.package,
                    walk.emit,
                );
            }
            // The first argument is a list of variable names, not a call.
            if let Some(value) = args.get(1) {
                walk.enqueue(value, &env.nested_expr(false, false));
            }
            if op == "MULTIPLE-VALUE-BIND" {
                walk.walk_body(
                    args.get(2..).unwrap_or(&[]),
                    &env.nested_expr(env.in_boolean_test, env.result_discarded),
                    BodyKind::Declared,
                )?;
            }
            Some(())
        }
        "CHECK-TYPE" => {
            // Only the place has evaluated subforms; the type specifier is syntax.
            if let Some(place) = args.first() {
                walk.enqueue_place(place, &env.nested_expr(false, false));
            }
            if let Some(type_spec) = args.get(1) {
                correctness::check_quoted_type_specifier(
                    walk.doc,
                    type_spec,
                    walk.package,
                    walk.emit,
                );
            }
            Some(())
        }
        "PROG1" | "MULTIPLE-VALUE-PROG1" => {
            if let Some(first) = args.first() {
                walk.enqueue(
                    first,
                    &env.nested_expr(env.in_boolean_test, env.result_discarded),
                );
            }
            for rest in args.iter().skip(1) {
                walk.enqueue(rest, &env.nested_expr(false, true));
            }
            Some(())
        }
        "SETF" | "PSETF" | "SETQ" | "PSETQ" => {
            let arg_env = env.nested_expr(false, false);
            for pair in args.as_chunks::<2>().0 {
                if op == "SETF" || op == "PSETF" {
                    correctness::check_setf_literal_place(
                        walk.doc,
                        &pair[0],
                        walk.package,
                        walk.emit,
                    );
                    walk.enqueue_place(&pair[0], &arg_env);
                }
                walk.enqueue(&pair[1], &arg_env);
            }
            Some(())
        }
        "INCF" | "DECF" | "POP" | "REMF" | "PUSH" | "PUSHNEW" | "SHIFTF" | "ROTATEF" => {
            let arg_env = env.nested_expr(false, false);
            for (index, arg) in args.iter().enumerate() {
                let place = match op {
                    "PUSH" | "PUSHNEW" => index == 1,
                    "SHIFTF" => index + 1 < args.len(),
                    "ROTATEF" => true,
                    _ => index == 0,
                };
                if place {
                    walk.enqueue_place(arg, &arg_env);
                } else {
                    walk.enqueue(arg, &arg_env);
                }
            }
            Some(())
        }
        "ASSERT" => {
            if let Some(test) = args.first() {
                walk.enqueue(test, &env.nested_expr(true, false));
            }
            // The second operand is a list of generalized places. The test
            // and the optional error arguments are ordinary expressions.
            if let Some(places) = args
                .get(1)
                .and_then(Element::as_node)
                .filter(|node| closed_list(node))
                && let Some(places) = walk.doc.read_elements(elements(places))
            {
                let arg_env = env.nested_expr(false, false);
                for place in &places {
                    walk.enqueue_place(place, &arg_env);
                }
            }
            for arg in args.iter().skip(2) {
                walk.enqueue(arg, &env.nested_expr(false, false));
            }
            Some(())
        }
        "THE" => {
            if let Some(type_spec) = args.first() {
                correctness::check_quoted_type_specifier(
                    walk.doc,
                    type_spec,
                    walk.package,
                    walk.emit,
                );
            }
            if let Some(form) = args.get(1) {
                // A type assertion can observe the value even when its caller only tests truthiness.
                walk.enqueue(form, &env.nested_expr(false, false));
            }
            Some(())
        }
        _ => {
            if evaluates_all_arguments(op) {
                for arg in args {
                    walk.enqueue(arg, &env.nested_expr(false, false));
                }
            }
            Some(())
        }
    }
}

fn walk_defun_like<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    list: &SyntaxNode,
    op: &str,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    if env.in_top_level {
        style_pedantic::check_top_level_definition_style(
            walk.doc,
            op,
            list,
            args,
            walk.package,
            walk.emit,
        );
    }
    let mut body_env = env.function_body();
    body_env.in_defmacro = op == "DEFMACRO";
    if let Some(params) = args.get(1) {
        let kind = if op == "DEFUN" {
            LambdaListKind::Ordinary
        } else {
            LambdaListKind::Macro
        };
        walk.walk_lambda_initforms(params, kind, &body_env)?;
        style_pedantic::check_lambda_list_style(params, walk.emit);
    }
    if op == "DEFMACRO" {
        super::macro_templates::check(walk.doc, args, walk.package, walk.emit, walk.cancelled)?;
    }
    if let Some(name) = args.first() {
        check_function_name(walk.doc, name, walk.package, walk.emit);
        push_definition_block(walk.doc, name, walk.package, &mut body_env);
    }
    walk.walk_body(
        args.get(2..).unwrap_or(&[]),
        &body_env,
        BodyKind::Documented,
    )
}

fn walk_compiler_macro<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    list: &SyntaxNode,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    if env.in_top_level {
        style_pedantic::check_top_level_definition_style(
            walk.doc,
            "DEFINE-COMPILER-MACRO",
            list,
            args,
            walk.package,
            walk.emit,
        );
    }
    let Some(name) = args.first() else {
        return Some(());
    };
    let mut body_env = env.function_body();
    body_env.in_defmacro = false;
    if let Some(params) = args.get(1) {
        walk.walk_lambda_initforms(params, LambdaListKind::Macro, &body_env)?;
    }
    push_definition_block(walk.doc, name, walk.package, &mut body_env);
    walk.walk_body(
        args.get(2..).unwrap_or(&[]),
        &body_env,
        BodyKind::Documented,
    )
}

fn walk_defmethod<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    list: &SyntaxNode,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    if env.in_top_level {
        style_pedantic::check_top_level_definition_style(
            walk.doc,
            "DEFMETHOD",
            list,
            args,
            walk.package,
            walk.emit,
        );
    }
    suspicious::check_defmethod_around(walk.doc, list, args, walk.package, walk.emit);
    let Some((name, fields)) = args.split_first() else {
        return Some(());
    };
    check_function_name(walk.doc, name, walk.package, walk.emit);
    super::clos::walk_method(walk, name, fields, list.text_range(), env)
}

fn walk_let_form<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    op: &str,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    let Some(bindings_el) = args.first() else {
        return Some(());
    };
    let body = args.get(1..).unwrap_or(&[]);
    dataflow::check_let_overwritten_binding(
        walk.doc,
        bindings_el,
        body,
        walk.package,
        walk.lexicals,
        env,
        walk.emit,
    );
    let mut body_env = env.nested_expr(env.in_boolean_test, env.result_discarded);
    for spec in grammar::binding_specs(
        walk.doc,
        bindings_el,
        2,
        op == "LET",
        walk.emit,
        walk.cancelled,
    )? {
        let var = &spec[0];
        let init_env = if op == "LET*" {
            body_env.nested_expr(false, false)
        } else {
            env.nested_expr(false, false)
        };
        if let Some(init) = spec.get(1) {
            walk.enqueue(init, &init_env);
        }
        if symbol(var).is_some_and(|name| {
            walk.doc.workspace.standard_symbol(&name, walk.package) == Some("*READ-EVAL*")
        }) {
            body_env.read_eval_disabled = spec.get(1).is_none_or(|init| {
                super::super::context::is_nil_element(init, walk.package, walk.doc.workspace)
            });
        }
    }
    walk.walk_body(body, &body_env, BodyKind::Declared)
}

fn walk_local_fn_form<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    op: &str,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    let Some(defs_el) = args.first() else {
        return Some(());
    };
    let mut body_env = env.nested_expr(env.in_boolean_test, env.result_discarded);
    if let Some(dnode) = defs_el.as_node().filter(|n| closed_list(n)) {
        let definitions: Vec<_> = elements(dnode)
            .filter_map(|def| possibly_read(def, walk.doc.source.known_features()))
            .filter_map(|(def, known)| {
                def.into_node()
                    .filter(closed_list)
                    .map(|node| (node, known))
            })
            .collect();
        let names = || {
            definitions.iter().filter_map(|(node, _)| {
                elements(node)
                    .next()
                    .and_then(|name| symbol(&name))
                    .map(|name| name.name)
            })
        };
        if op == "LABELS" {
            Rc::make_mut(&mut body_env.local_fns).extend(names());
            body_env.fn_scope_id = walk.next_fn_scope_id();
        }
        for (def_node, known) in &definitions {
            if !known {
                continue;
            }
            let mut dparts = elements(def_node);
            let fn_name = dparts.next();
            let mut fn_env = if op == "LABELS" {
                body_env.function_body()
            } else {
                env.function_body()
            };
            fn_env.in_defmacro = op == "MACROLET";
            if let Some(params) = dparts.next() {
                let kind = if op == "MACROLET" {
                    LambdaListKind::Macro
                } else {
                    LambdaListKind::Ordinary
                };
                walk.walk_lambda_initforms(&params, kind, &fn_env)?;
                style_pedantic::check_lambda_list_style(&params, walk.emit);
                if op == "MACROLET" {
                    let macro_args: Vec<_> = elements(def_node).collect();
                    super::macro_templates::check(
                        walk.doc,
                        &macro_args,
                        walk.package,
                        walk.emit,
                        walk.cancelled,
                    )?;
                    if let Some(name) = &fn_name
                        && let Some(template) = MacroTemplate::infer(macro_args.into_iter().skip(1))
                        && template.supports_definition(walk.doc.workspace, walk.package, |name| {
                            env.is_fn_shadowed(name)
                                || crate::navigation::queries::find_local_fn(
                                    walk.lexicals,
                                    name,
                                    walk.package,
                                    walk.doc.workspace,
                                )
                                .is_some()
                        })
                    {
                        let templates = body_env.local_macros.get_or_insert_with(Rc::default);
                        Rc::make_mut(templates).push(LocalMacroTemplate {
                            definition: name.text_range(),
                            template: Rc::new(template),
                        });
                    }
                }
            }
            if let Some(name) = &fn_name {
                check_function_name(walk.doc, name, walk.package, walk.emit);
                push_definition_block(walk.doc, name, walk.package, &mut fn_env);
            }
            let fn_body: Vec<_> = dparts.collect();
            walk.walk_body(&fn_body, &fn_env, BodyKind::Documented)?;
        }
        if op != "LABELS" {
            Rc::make_mut(&mut body_env.local_fns).extend(names());
            body_env.fn_scope_id = walk.next_fn_scope_id();
        }
    }
    walk.walk_body(args.get(1..).unwrap_or(&[]), &body_env, BodyKind::Declared)
}

fn walk_block_form<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    let mut body_env = env.nested_expr(env.in_boolean_test, env.result_discarded);
    if let Some(name) = args.first() {
        if is_nil_syntax(name, walk.package, walk.doc.workspace) {
            body_env.push_nil_block();
        } else if let Some(target) = walk.doc.control_block(name, walk.package) {
            body_env.push_block(target);
        } else if name.kind() != SyntaxKind::ReaderForm && symbol(name).is_none() {
            grammar::report(
                walk.emit,
                "correctness/invalid-control-target",
                name,
                "a block name must be a symbol",
            );
        }
    }
    walk.walk_body(args.get(1..).unwrap_or(&[]), &body_env, BodyKind::Forms)
}

fn walk_dotimes_like<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    list: &SyntaxNode,
    op: &str,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    let mut body_env = env.nested_expr(false, true);
    body_env.push_nil_block();
    let mut empty_body = false;
    if let Some(spec) = args.first()
        && let Some(parts) = grammar::proper_list(
            walk.doc,
            spec,
            walk.package,
            "correctness/invalid-binding-spec",
            "expected an iteration binding list",
            walk.emit,
        )
    {
        let bounds = match op {
            "DOLIST" | "DOTIMES" => 2..=3,
            "DO-ALL-SYMBOLS" => 1..=2,
            _ => 1..=3,
        };
        if !bounds.contains(&parts.len()) {
            grammar::report(
                walk.emit,
                "correctness/invalid-binding-spec",
                spec,
                "wrong number of fields in this iteration binding",
            );
        }
        empty_body = matches!(op, "DOLIST" | "DOTIMES") && parts.len() == 2 && args.len() == 1;
        if op != "DO-ALL-SYMBOLS"
            && let Some(bound) = parts.get(1)
        {
            let mut bound_env = env.nested_expr(false, false);
            bound_env.push_nil_block();
            walk.enqueue(bound, &bound_env);
        }
        if let Some(result) = parts.get(if op == "DO-ALL-SYMBOLS" { 1 } else { 2 }) {
            walk.enqueue(result, &body_env.nested_expr(false, env.result_discarded));
        }
        if let Some(variable) = parts.first()
            && let Some(name) = grammar::variable_name(walk.doc, walk.package, variable, walk.emit)
        {
            let indexed_list = if op == "DOTIMES"
                && bounds.contains(&parts.len())
                && walk.emit.is_enabled("perf/indexed-list-loop")
            {
                let scope = ScopeContext {
                    doc: walk.doc,
                    package: walk.package,
                    lexicals: walk.lexicals,
                    env,
                };
                indexed_loop::describe(scope, list, &parts[1..], &args[1..], &name).map(Rc::new)
            } else {
                None
            };
            Rc::make_mut(&mut body_env.loop_bindings).push(LoopBinding {
                name,
                definition: variable.text_range(),
                indexed_list,
            });
        }
    }
    body_env.in_loop = true;
    if empty_body {
        walk.emit.emit(
            "suspicious/empty-body",
            list.text_range(),
            format!(
                "`{}` has an empty body and does no useful iteration",
                op.to_ascii_lowercase()
            ),
            Some("add body forms or provide a result expression".into()),
            Vec::new(),
            None,
        );
    }
    let body = body_after_declarations(walk.doc, args.get(1..).unwrap_or(&[]), walk.package);
    walk.walk_tagbody(body, &body_env)
}

fn walk_do_prog_form<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    op: &str,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    let is_do = matches!(op, "DO" | "DO*");
    let mut body_env = env.nested_expr(false, true);
    body_env.push_nil_block();
    if is_do {
        body_env.in_loop = true;
    }
    if let Some(bindings) = args.first() {
        let unique = matches!(op, "DO" | "PROG");
        for spec in grammar::binding_specs(
            walk.doc,
            bindings,
            if is_do { 3 } else { 2 },
            unique,
            walk.emit,
            walk.cancelled,
        )? {
            if let Some(init) = spec.get(1) {
                walk.enqueue(init, &body_env.nested_expr(false, false));
            }
            if let Some(step) = spec.get(2) {
                walk.enqueue(step, &body_env.nested_expr(false, false));
            }
        }
    }
    let body_start = if is_do { 2 } else { 1 };
    if is_do
        && let Some(end_test) = args
            .get(1)
            .and_then(|e| e.as_node())
            .filter(|n| closed_list(n))
    {
        let mut eparts = elements(end_test);
        if let Some(test) = eparts.next() {
            walk.enqueue(&test, &body_env.nested_expr(true, false));
        }
        let res: Vec<_> = eparts.collect();
        walk.walk_body(
            &res,
            &body_env.nested_expr(false, env.result_discarded),
            BodyKind::Declared,
        )?;
    }
    let body = args.get(body_start..).unwrap_or(&[]);
    let body = body_after_declarations(walk.doc, body, walk.package);
    walk.walk_tagbody(body, &body_env)
}

fn normalized_branch_test(
    walk: &WalkContext<'_, '_, '_, impl Fn() -> bool>,
    test: &Element,
    env: &FormContext,
) -> Option<(String, bool)> {
    let mut current = test.clone();
    let mut negated = false;
    while let Some(node) = current.as_node()
        && closed_list(node)
    {
        let mut elems = elements(node);
        let Some(head_sym) = elems.next().as_ref().and_then(symbol) else {
            break;
        };
        let Some(arg) = elems.next() else {
            break;
        };
        if elems.next().is_some()
            || !matches!(
                walk.doc.std_op(&head_sym, walk.package, walk.lexicals, env),
                Some("NOT" | "NULL")
            )
        {
            break;
        }
        negated = !negated;
        current = arg;
    }
    repeatable_test(walk.doc, &current, walk.package, walk.lexicals, env).map(|key| (key, negated))
}

fn walk_branch_form<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    op: &str,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    let Some(test) = args.first() else {
        return Some(());
    };
    walk.enqueue(test, &env.nested_expr(true, false));
    let fact = if walk
        .emit
        .is_enabled("suspicious/redundant-enclosing-condition")
    {
        normalized_branch_test(walk, test, env)
    } else {
        None
    };
    let branch = |when_truth: bool| {
        let mut branch_env = env.nested_expr(env.in_boolean_test, env.result_discarded);
        branch_env.entry_test = fact.as_ref().map(|(test_key, negated)| {
            Rc::new(BranchFact {
                test: test_key.clone(),
                range: test.text_range(),
                truth: if *negated { !when_truth } else { when_truth },
            })
        });
        branch_env
    };
    if op == "IF" {
        if let Some(then_el) = args.get(1) {
            walk.enqueue(then_el, &branch(true));
        }
        if let Some(else_el) = args.get(2) {
            walk.enqueue(else_el, &branch(false));
        }
        Some(())
    } else {
        walk.walk_body(&args[1..], &branch(op == "WHEN"), BodyKind::Forms)
    }
}

fn walk_cond_form<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    for clause in args {
        let Some(parts) = clause_fields(walk.doc, clause, walk.package, 1, walk.emit) else {
            continue;
        };
        let test_only = parts.len() > 1 || env.in_boolean_test || env.result_discarded;
        walk.enqueue(&parts[0], &env.nested_expr(test_only, false));
        walk.walk_body(
            &parts[1..],
            &env.nested_expr(env.in_boolean_test, env.result_discarded),
            BodyKind::Forms,
        )?;
    }
    Some(())
}

fn walk_case_form<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    op: &str,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    if matches!(op, "CASE" | "ECASE" | "CCASE") {
        correctness::check_case_form(walk.doc, op, args, walk.package, walk.emit);
    }
    if let Some(keyform) = args.first() {
        let key_env = env.nested_expr(false, false);
        if matches!(op, "CCASE" | "CTYPECASE") {
            walk.enqueue_place(keyform, &key_env);
        } else {
            walk.enqueue(keyform, &key_env);
        }
    }
    let is_typecase = matches!(op, "TYPECASE" | "ETYPECASE" | "CTYPECASE");
    for clause in args.iter().skip(1) {
        let Some(parts) = clause_fields(walk.doc, clause, walk.package, 1, walk.emit) else {
            continue;
        };
        if is_typecase {
            correctness::check_quoted_type_specifier(walk.doc, &parts[0], walk.package, walk.emit);
        }
        walk.walk_body(
            &parts[1..],
            &env.nested_expr(env.in_boolean_test, env.result_discarded),
            BodyKind::Forms,
        )?;
    }
    Some(())
}

fn walk_with_form<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    op: &str,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    let mut body_env = env.nested_expr(env.in_boolean_test, env.result_discarded);
    if let Some(spec) = args.first()
        && let Some(parts) = grammar::proper_list(
            walk.doc,
            spec,
            walk.package,
            "correctness/invalid-binding-spec",
            "expected an option or binding list",
            walk.emit,
        )
    {
        match op {
            "WITH-HASH-TABLE-ITERATOR" | "WITH-PACKAGE-ITERATOR" => {
                if let Some(name) = parts.first().and_then(symbol) {
                    Rc::make_mut(&mut body_env.local_fns).push(name.name);
                }
            }
            "PRINT-UNREADABLE-OBJECT" => {}
            _ => {
                if let Some(var) = parts.first() {
                    grammar::variable_name(walk.doc, walk.package, var, walk.emit);
                }
            }
        }
        let mut init_env = env.nested_expr(false, false);
        // WITH-OPEN-STREAM takes ownership of its stream argument.
        init_env.in_open_guard |= op == "WITH-OPEN-STREAM";
        if op == "WITH-INPUT-FROM-STRING" {
            if let [_, input, options @ ..] = parts.as_slice() {
                walk.enqueue(input, &init_env);
                for [key, operand] in options.as_chunks::<2>().0 {
                    if symbol(key)
                        .is_some_and(|name| name.prefix == Prefix::Keyword && name.name == "INDEX")
                    {
                        walk.enqueue_place(operand, &init_env);
                    } else {
                        walk.enqueue(operand, &init_env);
                    }
                }
            }
        } else {
            for arg in parts
                .iter()
                .skip(usize::from(op != "PRINT-UNREADABLE-OBJECT"))
            {
                walk.enqueue(arg, &init_env);
            }
        }
        if op == "WITH-OUTPUT-TO-STRING"
            && parts
                .get(1)
                .is_none_or(|form| is_nil_element(form, walk.package, walk.doc.workspace))
        {
            body_env.result_discarded = true;
            body_env.in_boolean_test = false;
        }
    }
    walk.walk_body(
        args.get(1..).unwrap_or(&[]),
        &body_env,
        if op == "PRINT-UNREADABLE-OBJECT" {
            BodyKind::Forms
        } else {
            BodyKind::Declared
        },
    )
}

fn walk_restart_case<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    let result_env = env.nested_expr(env.in_boolean_test, env.result_discarded);
    if let Some(restartable) = args.first() {
        walk.enqueue(restartable, &result_env);
    }
    let known_features = walk.doc.source.known_features();
    for clause in args.iter().skip(1) {
        let Some(node) = clause.as_node().filter(|node| closed_list(node)) else {
            continue;
        };
        // A reader conditional can remove a name, lambda list, or option.
        let Some(parts) = elements(node)
            .filter_map(|part| possibly_read(part, known_features))
            .map(|(part, known)| known.then_some(part))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let [_, lambda_list, rest @ ..] = parts.as_slice() else {
            continue;
        };
        walk.walk_lambda_initforms(lambda_list, LambdaListKind::Ordinary, env)?;
        let mut body = rest;
        while let [option, value, tail @ ..] = body
            && crate::navigation::scope::restart_case_option(option)
        {
            if symbol(value).is_some() {
                correctness::check_function_of_macro(
                    walk.doc,
                    value,
                    walk.package,
                    walk.lexicals,
                    env,
                    walk.emit,
                );
            } else {
                walk.enqueue(value, &env.nested_expr(false, false));
            }
            body = tail;
        }
        walk.walk_body(body, &result_env, BodyKind::Declared)?;
    }
    Some(())
}

fn walk_condition_bind_form<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    op: &str,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    if let Some(specs) = args.first()
        && let Some(clauses) = grammar::proper_list(
            walk.doc,
            specs,
            walk.package,
            "correctness/malformed-clause",
            "expected a list of handler or restart bindings",
            walk.emit,
        )
    {
        let init_env = env.nested_expr(false, false);
        for clause in clauses {
            let Some(parts) = clause_fields(walk.doc, &clause, walk.package, 2, walk.emit) else {
                continue;
            };
            if op == "HANDLER-BIND" && parts.len() != 2 {
                grammar::report(
                    walk.emit,
                    "correctness/malformed-clause",
                    &clause,
                    "a handler binding takes a type and a handler expression",
                );
            }
            for expression in &parts[1..] {
                walk.enqueue(expression, &init_env);
            }
        }
    }
    walk.walk_body(
        args.get(1..).unwrap_or(&[]),
        &env.nested_expr(env.in_boolean_test, env.result_discarded),
        BodyKind::Forms,
    )
}

fn walk_handler_case<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    if let Some(expr) = args.first() {
        walk.enqueue(
            expr,
            &env.nested_expr(env.in_boolean_test, env.result_discarded),
        );
    }
    for clause in args.iter().skip(1) {
        let Some(parts) = clause_fields(walk.doc, clause, walk.package, 2, walk.emit) else {
            continue;
        };
        let no_error = symbol(&parts[0])
            .is_some_and(|name| name.prefix == Prefix::Keyword && name.name == "NO-ERROR");
        if no_error {
            walk.walk_lambda_initforms(&parts[1], LambdaListKind::Ordinary, env)?;
        } else {
            correctness::check_quoted_type_specifier(walk.doc, &parts[0], walk.package, walk.emit);
            if let Some(names) = grammar::proper_list(
                walk.doc,
                &parts[1],
                walk.package,
                "correctness/malformed-clause",
                "a handler requires `()` or one condition variable",
                walk.emit,
            ) {
                if names.len() > 1 {
                    grammar::report(
                        walk.emit,
                        "correctness/malformed-clause",
                        &parts[1],
                        "a handler accepts at most one condition variable",
                    );
                }
                if let Some(name) = names.first() {
                    grammar::variable_name(walk.doc, walk.package, name, walk.emit);
                }
            }
        }
        walk.walk_body(
            &parts[2..],
            &env.nested_expr(env.in_boolean_test, env.result_discarded),
            BodyKind::Declared,
        )?;
    }
    Some(())
}
