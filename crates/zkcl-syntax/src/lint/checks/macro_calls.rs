//! Project source macro operands back into the shared lint traversal.

use std::rc::Rc;

use crate::navigation::{Namespace, ScopeBinding};
use crate::outline::Element;
use crate::reader_tokens::SymbolName;
use crate::syntax::TextRange;

use super::super::LintRelated;
use super::super::context::{ControlTarget, FormContext, LintEmitter, MacroOrigin};
use super::super::macro_template::{OperandRole, TemplateBody};
use super::{BodyKind, WalkContext, body_after_declarations};

pub(super) fn walk<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    env: &FormContext,
    head: &SymbolName,
    call: TextRange,
    local: Option<&ScopeBinding>,
    args: &[Element],
) -> Option<()> {
    let (template, document, definition_package) = if let Some(local) = local {
        let local_macro = env.local_macros.as_deref().and_then(|macros| {
            macros
                .iter()
                .find(|entry| Some(entry.definition) == local.def_range)
        });
        let Some(local_macro) = local_macro else {
            return Some(());
        };
        (local_macro.template.as_ref(), None, walk.package)
    } else {
        if env.is_fn_shadowed(head) {
            return Some(());
        }
        let Some((document, declaration, template)) = walk.doc.workspace.lookup_macro_template(
            head,
            walk.package,
            walk.doc.source,
            call.start(),
        ) else {
            return Some(());
        };
        if !template.supports_definition(
            walk.doc.workspace,
            declaration.name.package.as_deref(),
            |_| false,
        ) {
            return Some(());
        }
        let mut definition_package = declaration.name.package.as_deref();
        if walk.package.is_none()
            && definition_package.is_some_and(|name| {
                walk.doc.workspace.canonical_global_package(name) == Some("COMMON-LISP-USER")
            })
        {
            definition_package = None;
        }
        (template, document, definition_package)
    };
    // Literal operators keep their identity from the definition, and the
    // code inserted into the expansion uses the caller's lexical environment.
    if !template.supports_expansion(|name| {
        env.local_fns.iter().any(|candidate| candidate == name)
            || walk.lexicals.iter().any(|binding| {
                binding.namespace == Namespace::Function
                    && binding.spelling.name == name
                    && walk
                        .doc
                        .workspace
                        .standard_symbol(&binding.spelling, binding.package.as_deref())
                        .is_some()
            })
    }) {
        return Some(());
    }
    let Some(operands) = template.operands(walk.doc.source, walk.doc.workspace, args) else {
        return Some(());
    };
    // Reuse only operands where the original lexical environment survives.
    // All other projected forms still get local expression checks.
    let mut transparent: Vec<_> = operands
        .iter()
        .filter(|operand| {
            !operand.context.binds_values
                && !operand.context.nil_block
                && operand.blocks.is_empty()
                && !operand.context.deferred
                && operand.context.usage != OperandRole::Place
                && matches!(
                    operand.body,
                    None | Some(TemplateBody::Forms | TemplateBody::Arguments)
                )
        })
        .flat_map(|operand| operand.forms.iter().cloned())
        .collect();
    if !transparent.is_empty() {
        transparent.sort_unstable_by_key(|form| form.text_range().start());
        walk.doc
            .macro_operands
            .borrow_mut()
            .insert(call, transparent);
    }
    for operand in operands {
        if walk.is_cancelled() {
            return None;
        }
        if operand.forms.is_empty() {
            continue;
        }
        let (boolean, discarded) = match operand.context.usage {
            OperandRole::Inherited => (env.in_boolean_test, env.result_discarded),
            OperandRole::Test => (true, false),
            OperandRole::Discarded => (false, true),
            OperandRole::Value | OperandRole::Place => (false, false),
        };
        let mut child = if operand.context.deferred {
            env.function_body()
        } else {
            env.nested_expr(boolean, discarded)
        };
        child.in_boolean_test = boolean;
        child.result_discarded = discarded;
        child.in_top_level = env.in_top_level && operand.context.top_level;
        child.in_loop |= operand.context.in_loop;
        child.in_open_guard |= operand.context.open_guard;
        // Generated lexical bindings are outside the source navigation scope.
        // Facts that depend on resolving a bare variable stay unknown.
        child.symbol_macros |= operand.context.binds_values;
        if operand.context.nil_block {
            child.push_nil_block();
        }
        for (name, from_template) in operand.blocks.into_iter().rev() {
            let package = if from_template {
                definition_package
            } else {
                walk.package
            };
            if let Some(target) = walk.doc.control_name(name, package) {
                if matches!(&target, ControlTarget::Symbol(name, Some(package)) if name == "NIL" && package == "COMMON-LISP")
                {
                    child.push_nil_block();
                } else {
                    child.push_block(target);
                }
            }
        }
        let origin = Rc::new(MacroOrigin {
            location: LintRelated {
                document: document.map(str::to_owned),
                range: operand.origin,
                message: "the macro template inserts this operand here".into(),
            },
            enclosing: env.macro_origin.clone(),
        });
        child.macro_origin = Some(origin.clone());
        let first_diagnostic = walk.emit.diagnostics.len();
        match operand.body {
            Some(TemplateBody::Tagbody) => walk.walk_tagbody(&operand.forms, &child)?,
            Some(
                body @ (TemplateBody::Forms | TemplateBody::Declared | TemplateBody::Documented),
            ) => {
                let kind = match body {
                    TemplateBody::Forms => BodyKind::Forms,
                    TemplateBody::Declared => BodyKind::Declared,
                    TemplateBody::Documented => BodyKind::Documented,
                    TemplateBody::Arguments | TemplateBody::Tagbody => unreachable!(),
                };
                if child.in_top_level {
                    let forms = if kind == BodyKind::Declared {
                        body_after_declarations(walk.doc, &operand.forms, walk.package)
                    } else {
                        &operand.forms
                    };
                    for form in forms {
                        walk.enqueue(form, &child);
                    }
                } else {
                    walk.walk_body(&operand.forms, &child, kind)?;
                }
            }
            None | Some(TemplateBody::Arguments) => {
                for form in &operand.forms {
                    if operand.context.usage == OperandRole::Place {
                        walk.enqueue_place(form, &child);
                    } else {
                        walk.enqueue(form, &child);
                    }
                }
            }
        }
        add_origin(walk.emit, first_diagnostic, &origin);
    }
    Some(())
}

pub(super) fn add_origin(emit: &mut LintEmitter<'_>, first: usize, origin: &MacroOrigin) {
    for diagnostic in &mut emit.diagnostics[first..] {
        let mut next = Some(origin);
        for _ in 0..4 {
            let Some(origin) = next else { break };
            if !diagnostic.related.contains(&origin.location) {
                diagnostic.related.push(origin.location.clone());
            }
            next = origin.enclosing.as_deref();
        }
    }
}
