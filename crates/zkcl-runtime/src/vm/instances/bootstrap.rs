//! Install the standard methods used by instance construction, slot access, and class changes.
use std::rc::Rc;
use std::sync::Arc;

use crate::condition::{Class, Fault};
use crate::heap::{RootSlot, Word};
use crate::symbol::Name;
use crate::vm::{FunctionName, Primitive, classes, generic::BuiltinGeneric, methods, streams};
use crate::{EvalError, NativeFunction, Root, Runtime, compile, form_value, read};

use super::{allocate_instance, error};

fn primitive_lambda(runtime: &Runtime, source: &str, names: &[&str]) -> Result<Word, EvalError> {
    let substitutions: Vec<_> = names
        .iter()
        .map(|&name| {
            (
                name,
                Primitive::named(name).expect("registered instance primitive"),
            )
        })
        .collect();
    compile_builtin_lambda(runtime, source, &substitutions)
}

fn native(runtime: &Runtime, callback: Arc<NativeFunction>) -> Word {
    let mut natives = runtime.natives.borrow_mut();
    let index = natives.len();
    natives.push(callback);
    Word::native(index)
}

const WITH_SLOTS_EXPANDER: &str = r#"
(lambda (form environment)
  (declare (ignore environment))
  (let* ((entries (second form))
         (place (third form))
         (body (cdddr form))
         (object (gensym "OBJECT"))
         (bindings
           (mapcar
            (lambda (entry)
              (if (consp entry)
                  (list (first entry)
                        (list 'slot-value object (list 'quote (second entry))))
                (list entry (list 'slot-value object (list 'quote entry)))))
            entries)))
    (list 'let
          (list (list object place))
          (cons 'symbol-macrolet
                (cons bindings (or body (list nil)))))))
"#;

const WITH_ACCESSORS_EXPANDER: &str = r#"
(lambda (form environment)
  (declare (ignore environment))
  (let* ((entries (second form))
         (place (third form))
         (body (cdddr form))
         (object (gensym "OBJECT"))
         (bindings
           (mapcar
            (lambda (entry)
              (list (first entry) (list (second entry) object)))
            entries)))
    (list 'let
          (list (list object place))
          (cons 'symbol-macrolet
                (cons bindings (or body (list nil)))))))
"#;

pub(in crate::vm) fn initialize_slot_protocol(runtime: &Runtime) -> Result<(), EvalError> {
    for (name, signature, specializers, callback) in [
        (
            "SLOT-MISSING",
            "(class object slot-name operation &optional new-value)",
            &["T"; 4][..],
            runtime.root_slot(native(runtime, Arc::new(default_slot_missing))),
        ),
        (
            "SLOT-UNBOUND",
            "(class instance slot-name)",
            &["T"; 3][..],
            runtime.root_slot(native(runtime, Arc::new(default_slot_unbound))),
        ),
        (
            "ALLOCATE-INSTANCE",
            "(class &rest initargs)",
            &["CLASS"][..],
            runtime.root_slot(native(runtime, Arc::new(default_allocate_instance))),
        ),
        (
            "SHARED-INITIALIZE",
            "(instance slot-names &rest initargs)",
            &["STANDARD-OBJECT", "T"][..],
            runtime.root_slot(primitive_lambda(
                runtime,
                "(lambda (_next _has instance slot-names &rest initargs)
                   (declare (ignore _next _has))
                   (funcall %UPDATE-OBSOLETE-INSTANCE instance)
                   (apply %SHARED-INITIALIZE instance slot-names initargs))",
                &[
                    "%UPDATE-OBSOLETE-INSTANCE",
                    "%SHARED-INITIALIZE",
                ],
            )?),
        ),
        (
            "INITIALIZE-INSTANCE",
            "(instance &rest initargs)",
            &["STANDARD-OBJECT"][..],
            runtime.root_slot(primitive_lambda(
                runtime,
                "(lambda (_next _has instance &rest initargs)
                   (declare (ignore _next _has))
                   (apply #'shared-initialize instance t initargs))",
                &[],
            )?),
        ),
        (
            "REINITIALIZE-INSTANCE",
            "(instance &rest initargs)",
            &["STANDARD-OBJECT"][..],
            runtime.root_slot(primitive_lambda(
                runtime,
                "(lambda (_next _has instance &rest initargs)
                   (declare (ignore _next _has))
                   (apply %VALIDATE-REINITIALIZE-INITARGS instance initargs)
                   (apply #'shared-initialize instance nil initargs)
                   instance)",
                &["%VALIDATE-REINITIALIZE-INITARGS"],
            )?),
        ),
        (
            "PRINT-OBJECT",
            "(object stream)",
            &["T", "T"][..],
            runtime.root_slot(Primitive::named("%DEFAULT-PRINT-OBJECT").expect("default printer method")),
        ),
        (
            "DESCRIBE-OBJECT",
            "(object stream)",
            &["T", "T"][..],
            runtime.root_slot(native(runtime, Arc::new(default_describe_object))),
        ),
        (
            "UPDATE-INSTANCE-FOR-DIFFERENT-CLASS",
            "(previous current &rest initargs)",
            &["STANDARD-OBJECT", "STANDARD-OBJECT"][..],
            runtime.root_slot(primitive_lambda(
                runtime,
                "(lambda (_next _has previous current &rest initargs)
                   (declare (ignore _next _has))
                   (let ((added-slots (funcall %ADDED-SLOT-NAMES previous current)))
                     (apply %VALIDATE-CHANGE-CLASS-INITARGS previous current added-slots initargs)
                     (apply #'shared-initialize current added-slots initargs)))",
                &[
                    "%ADDED-SLOT-NAMES",
                    "%VALIDATE-CHANGE-CLASS-INITARGS",
                ],
            )?),
        ),
        (
            "UPDATE-INSTANCE-FOR-REDEFINED-CLASS",
            "(instance added-slots discarded-slots property-list &rest initargs)",
            &["STANDARD-OBJECT", "T", "T", "T"][..],
            runtime.root_slot(primitive_lambda(
                runtime,
                "(lambda (_next _has instance added-slots discarded-slots property-list &rest initargs)
                   (declare (ignore _next _has))
                   (apply %VALIDATE-REDEFINED-INITARGS instance added-slots discarded-slots property-list initargs)
                   (apply #'shared-initialize instance added-slots initargs))",
                &["%VALIDATE-REDEFINED-INITARGS"],
            )?),
        ),
    ] {
        let generic = BuiltinGeneric::standard(runtime, name, signature)?;
        generic.add(specializers, callback.get())?;
        match name {
            "INITIALIZE-INSTANCE" | "REINITIALIZE-INSTANCE" => { generic.add(&["CONDITION"], callback.get())?; }
            "SHARED-INITIALIZE" => { generic.add(&["CONDITION", "T"], callback.get())?; }
            _ => {}
        }
        if name == "PRINT-OBJECT" {
            runtime.default_print_method.set(callback.get());
            for class in ["STANDARD-OBJECT", "STRUCTURE-OBJECT"] {
                generic.add(&[class, "T"], callback.get())?;
            }
        }
    }

    streams::gray::initialize_generics(runtime)?;
    install_make_instance(runtime)?;
    install_change_class(runtime)?;
    install_make_instances_obsolete(runtime)?;

    // DESCRIBE delegates to the generic function.
    {
        let describe_fn = runtime.root_slot(compile_builtin_lambda(
            runtime,
            "(lambda (object &optional (stream *standard-output*))
               (let ((target (if (null stream)
                                 *standard-output*
                                 (if (eq stream t)
                                     *terminal-io*
                                     stream))))
                 (describe-object object target)
                 (values)))",
            &[],
        )?);
        let sym = runtime.intern_common_lisp("DESCRIBE", 0)?;
        runtime.heap.set_symbol_cell(sym, true, describe_fn.get());
    }

    // WITH-SLOTS and WITH-ACCESSORS expand through SYMBOL-MACROLET.
    for (name, source) in [
        ("WITH-SLOTS", WITH_SLOTS_EXPANDER),
        ("WITH-ACCESSORS", WITH_ACCESSORS_EXPANDER),
    ] {
        let function = runtime.root_slot(compile_builtin_lambda(runtime, source, &[])?);
        let symbol = runtime.intern_common_lisp(name, 0)?;
        runtime.heap.set_macro_expander(symbol, function.get());
    }

    install_class_names(runtime)
}

fn install_make_instance(runtime: &Runtime) -> Result<(), EvalError> {
    let generic = BuiltinGeneric::standard(runtime, "MAKE-INSTANCE", "(class &rest initargs)")?;
    let by_class = runtime.root_slot(primitive_lambda(
        runtime,
        "(lambda (_next _has class &rest initargs)
               (declare (ignore _next _has))
               (let ((all (apply %DEFAULT-INITARGS class initargs)))
                 (let ((instance (apply #'allocate-instance class all)))
                   (apply #'initialize-instance instance all)
                   instance)))",
        &["%DEFAULT-INITARGS"],
    )?);
    let by_symbol = primitive_lambda(
        runtime,
        "(lambda (_next _has class &rest initargs)
               (declare (ignore _next _has))
               (apply #'make-instance (find-class class) initargs))",
        &[],
    )?;
    generic.add(&["CLASS"], by_class.get())?;
    generic.add(&["SYMBOL"], by_symbol)?;
    Ok(())
}

fn install_change_class(runtime: &Runtime) -> Result<(), EvalError> {
    let generic = BuiltinGeneric::standard(
        runtime,
        "CHANGE-CLASS",
        "(instance new-class &rest initargs)",
    )?;
    let by_class = runtime.root_slot(primitive_lambda(
        runtime,
        "(lambda (_next _has instance new-class &rest initargs)
               (declare (ignore _next _has))
               (funcall %UPDATE-OBSOLETE-INSTANCE instance)
               (let ((previous (funcall %CHANGE-CLASS-MIGRATE instance new-class)))
                 (apply #'update-instance-for-different-class previous instance initargs)
                 instance))",
        &["%CHANGE-CLASS-MIGRATE", "%UPDATE-OBSOLETE-INSTANCE"],
    )?);
    let by_symbol = primitive_lambda(
        runtime,
        "(lambda (_next _has instance new-class &rest initargs)
               (declare (ignore _next _has))
               (apply #'change-class instance (find-class new-class) initargs))",
        &[],
    )?;
    generic.add(&["STANDARD-OBJECT", "CLASS"], by_class.get())?;
    generic.add(&["STANDARD-OBJECT", "SYMBOL"], by_symbol)?;
    Ok(())
}

fn install_make_instances_obsolete(runtime: &Runtime) -> Result<(), EvalError> {
    let generic = BuiltinGeneric::standard(runtime, "MAKE-INSTANCES-OBSOLETE", "(class)")?;
    let sym_fn = runtime.root_slot(compile_builtin_lambda(
        runtime,
        "(lambda (_next _has class)
               (declare (ignore _next _has))
               (make-instances-obsolete (find-class class)))",
        &[],
    )?);
    generic.add(
        &["STANDARD-CLASS"],
        native(
            runtime,
            Arc::new(|runtime, args, at| {
                let class = &args[2];
                crate::classes::make_instances_obsolete(runtime, class.slot.get(), at)?;
                Ok(vec![class.clone()])
            }),
        ),
    )?;
    generic.add(&["SYMBOL"], sym_fn.get())?;
    Ok(())
}

fn install_class_names(runtime: &Runtime) -> Result<(), EvalError> {
    let class_name_prim = Primitive::named("CLASS-NAME").expect("registered CLASS-NAME");
    let generic = BuiltinGeneric::standard(runtime, "CLASS-NAME", "(class)")?;
    let method = compile_builtin_lambda(
        runtime,
        "(lambda (_next _has class)
               (declare (ignore _next _has))
               (funcall CLASS-NAME class))",
        &[("CLASS-NAME", class_name_prim)],
    )?;
    generic.add(&["CLASS"], method)?;

    let setter = BuiltinGeneric::setter(runtime, "(setf class-name)", "(new-name class)")?;
    let set_class_name_fn = native(runtime, Arc::new(set_class_name));
    let setter_method = compile_builtin_lambda(
        runtime,
        "(lambda (_next _has new-name class)
               (declare (ignore _next _has))
               (funcall %SET-CLASS-NAME class new-name))",
        &[("%SET-CLASS-NAME", set_class_name_fn)],
    )?;
    setter.add(&["T", "CLASS"], setter_method)?;

    let setf_find_fn = native(runtime, Arc::new(setf_find_class));
    let find_name = form_value::to_value(runtime, &read::read(runtime, "(setf find-class)")?)?;
    let FunctionName::Setf(accessor) =
        FunctionName::parse(runtime, find_name.slot.get(), 0).map_err(|fault| fault.host)?
    else {
        unreachable!("(setf find-class) parses as a SETF function name");
    };
    runtime.heap.set_symbol_setter(accessor, setf_find_fn);
    Ok(())
}

/// Install DOCUMENTATION and (SETF DOCUMENTATION) as generic functions.
///
/// ANSI defines both as standard generic functions so programs can extend
/// them with DEFMETHOD.  The default methods delegate to the built-in query
/// and assignment, preserving documentation for packages, classes, methods,
/// generic functions, variables, structures, types, and SETF names.
pub(crate) fn initialize_documentation_protocol(runtime: &Runtime) -> Result<(), EvalError> {
    let generic = BuiltinGeneric::standard(runtime, "DOCUMENTATION", "(object doc-type)")?;
    let method = compile_builtin_lambda(
        runtime,
        "(lambda (_next _has object doc-type)
           (declare (ignore _next _has))
           (funcall %DOCUMENTATION object doc-type))",
        &[(
            "%DOCUMENTATION",
            Primitive::named("DOCUMENTATION").expect("documentation primitive"),
        )],
    )?;
    generic.add(&["T", "T"], method)?;

    let setter = BuiltinGeneric::setter(
        runtime,
        "(setf documentation)",
        "(new-value object doc-type)",
    )?;
    let writer = runtime.intern_common_lisp("%ZK-SET-DOCUMENTATION", 0)?;
    let setter_method = compile_builtin_lambda(
        runtime,
        "(lambda (_next _has new-value object doc-type)
           (declare (ignore _next _has))
           (funcall %ZK-SET-DOCUMENTATION object doc-type new-value))",
        &[("%ZK-SET-DOCUMENTATION", writer)],
    )?;
    setter.add(&["T", "T", "T"], setter_method)?;

    // The generic SETF function takes the new value first, while the accessor
    // form lists the object and doc type.  Register an expander so (SETF
    // (DOCUMENTATION ...)) reaches the generic function instead of the
    // built-in place, which calls the primitive directly.
    let expander = runtime.root_slot(compile_builtin_lambda(
        runtime,
        DOCUMENTATION_SETF_EXPANDER,
        &[],
    )?);
    let name = runtime.intern_common_lisp("DOCUMENTATION", 0)?;
    runtime.define_setf_expander(name, crate::SetfExpander::Function(expander), None);
    Ok(())
}

/// Install FUNCTION-KEYWORDS as the standard generic function defined by
/// ANSI.  The default method reports the keyword parameters and the
/// &ALLOW-OTHER-KEYS flag of a standard method's lambda list.
pub(crate) fn initialize_function_keywords(runtime: &Runtime) -> Result<(), EvalError> {
    let generic = BuiltinGeneric::standard(runtime, "FUNCTION-KEYWORDS", "(method)")?;
    generic.add(&["METHOD"], native(runtime, Arc::new(function_keywords)))
}

fn function_keywords<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [_, _, method] = args else {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "FUNCTION-KEYWORDS expects a method"),
        ));
    };
    let info = runtime.heap.method_info(method.slot.get()).ok_or_else(|| {
        Fault::type_error(runtime, 
            method.slot.get(),
            "METHOD",
            EvalError::new(at, "expected a method"),
        )
    })?;
    let signature = compile::call_signature(runtime, &runtime.root(info.lambda_list), at)?;
    let mut keys = runtime.nil();
    if let Some(keywords) = signature.keywords {
        for keyword in keywords.into_iter().rev() {
            let word = runtime.intern(&keyword, at)?;
            keys = runtime
                .cons(&runtime.root(word), &keys)
                .map_err(|error| EvalError::out_of_memory(at, error))?;
        }
    }
    let allow_other_keys = if signature.allow_other_keys {
        Word::T
    } else {
        Word::NIL
    };
    Ok(vec![keys, runtime.root(allow_other_keys)])
}

/// SETF expander for (DOCUMENTATION object doc-type).
///
/// It binds the place arguments once and calls the (SETF DOCUMENTATION)
/// generic function with the new value first, as required by the generic
/// function lambda list.  The accessor form keeps its source argument order.
const DOCUMENTATION_SETF_EXPANDER: &str = r#"
(lambda (form environment)
  (declare (ignore environment))
  (let* ((place form)
         (arguments (rest place))
         (temporaries
           (mapcar (lambda (argument)
                     (declare (ignore argument))
                     (gensym "DOCUMENTATION-ARG"))
                   arguments))
         (store (gensym "DOCUMENTATION-VALUE")))
    (values temporaries
            arguments
            (list store)
            (list 'funcall
                  (list 'function (list 'setf 'documentation))
                  store
                  (first temporaries)
                  (second temporaries))
            (list 'documentation (first temporaries) (second temporaries)))))
"#;

fn set_class_name<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [class, name] = args else {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "%SET-CLASS-NAME expects a class and a name"),
        ));
    };
    let name = classes::set_class_name(runtime, class.slot.get(), name.slot.get(), at)?;
    Ok(vec![runtime.root(name)])
}

fn setf_find_class<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [new_class, name, rest @ ..] = args else {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "(SETF FIND-CLASS) expects a class and a name"),
        ));
    };
    if !runtime.heap.is_symbol(name.slot.get()) {
        return Err(Fault::type_error(runtime, 
            name.slot.get(),
            "SYMBOL",
            EvalError::new(at, "expected a class name symbol"),
        ));
    }
    if let Some(environment) = rest.get(1) {
        crate::macros::validate_environment(runtime, environment.slot.get(), at)?;
    }
    let id = runtime
        .heap
        .symbol_attributes(name.slot.get())
        .expect("validated class name")
        .id;
    let association = if new_class.slot.get() == Word::NIL {
        None
    } else {
        if runtime.heap.class_info(new_class.slot.get()).is_none() {
            return Err(Fault::type_error(runtime, 
                new_class.slot.get(),
                "CLASS",
                EvalError::new(at, "expected a class object or NIL"),
            ));
        }
        Some(runtime.root_slot(new_class.slot.get()))
    };
    crate::classes::check_name_unclaimed(runtime, id, at)?;
    runtime
        .classes
        .borrow_mut()
        .associations
        .insert(id, association);
    Ok(vec![new_class.clone()])
}

pub(in crate::vm) fn compile_builtin_lambda(
    runtime: &Runtime,
    source: &str,
    substitutions: &[(&str, Word)],
) -> Result<Word, EvalError> {
    let substitutions: Vec<_> = substitutions
        .iter()
        .map(|&(key, word)| (key, Rc::new(runtime.root_slot(word))))
        .collect();
    let mut form = read::read(runtime, source)?;
    if !substitutions.is_empty() {
        substitute_form(&mut form, &substitutions);
    }
    let read::FormKind::List(items, None) = &form.kind else {
        unreachable!("builtin lambda form must be a proper list");
    };
    let (code, _) = compile::compile_lambda(runtime, &items[1], &items[2..], 0)?;
    code.instantiate(runtime)
}

fn substitute_form(form: &mut read::Form, substitutions: &[(&str, Rc<RootSlot>)]) {
    match &mut form.kind {
        read::FormKind::Name(name) => {
            if let Some((_, value)) = substitutions
                .iter()
                .find(|&&(key, _)| key == name.spelling())
            {
                form.kind = read::FormKind::Literal(Rc::clone(value));
            }
        }
        read::FormKind::List(items, tail) => {
            for item in items {
                substitute_form(item, substitutions);
            }
            if let Some(tail) = tail {
                substitute_form(tail, substitutions);
            }
        }
        _ => {}
    }
}

fn default_allocate_instance<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let class = &args[2].slot;
    let name = if classes::allocates_class_metaobjects(runtime, class.get()) {
        let key = runtime.intern(&Name::keyword("NAME"), at)?;
        args[3..]
            .as_chunks::<2>()
            .0
            .iter()
            .find(|[name, _]| name.slot.get() == key)
            .map_or(Word::NIL, |[_, value]| value.slot.get())
    } else {
        Word::NIL
    };
    if name != Word::NIL
        && let Some((symbol, _)) = runtime
            .packages
            .borrow()
            .present(crate::package::Package::MOP, "*PROVISIONAL-CLASS*")
        && let Some((meta, reserved)) = runtime
            .heap
            .symbol_cell(symbol, false)
            .and_then(|value| runtime.heap.cons_fields(value))
        && meta == class.get()
        && runtime
            .heap
            .class_info(reserved)
            .is_some_and(|info| info.origin_name == name)
    {
        runtime.heap.set_symbol_cell(symbol, false, Word::NIL);
        return Ok(vec![runtime.root(reserved)]);
    }
    let instance = allocate_instance(runtime, class, name, at)?;
    Ok(vec![runtime.root(instance)])
}

fn default_describe_object<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let object = &args[2];
    let stream = args[3].slot.get();
    let obj_repr = runtime.repr(object).map_err(Fault::from)?;
    let class = classes::class_of(runtime, object.slot.get(), at)?;
    let class_repr = runtime.repr(&runtime.root(class)).map_err(Fault::from)?;
    let mut text = format!("{obj_repr} is an instance of {class_repr}.\n");
    if let Some(layout) = runtime.heap.instance_layout_of(object.slot.get())
        && let Some(entries) = runtime.heap.instance_layout_entries(layout)
    {
        for (index, (slot_name, cell)) in entries.into_iter().enumerate() {
            let slot_name = runtime.root(slot_name);
            let slot_text = runtime.repr(&slot_name).map_err(Fault::from)?;
            let val = match cell {
                Some(c) => runtime.heap.read_cell(c),
                None => runtime.heap.instance_slot(object.slot.get(), index),
            }
            .unwrap_or(Word::UNBOUND);
            if val == Word::UNBOUND {
                text.push_str(&format!("  {slot_text}: #<unbound>\n"));
            } else {
                let val_text = runtime.repr(&runtime.root(val)).map_err(Fault::from)?;
                text.push_str(&format!("  {slot_text}: {val_text}\n"));
            }
        }
    }
    streams::write_text(runtime, stream, &text, at)?;
    Ok(vec![runtime.nil()])
}

pub(crate) fn initialize_load_forms(runtime: &Runtime) -> Result<(), EvalError> {
    let generic =
        BuiltinGeneric::standard(runtime, "MAKE-LOAD-FORM", "(object &optional environment)")?;
    let missing = runtime.root(compile_builtin_lambda(
        runtime,
        "(lambda (_next _has object &optional environment)
           (declare (ignore _next _has environment))
           (error \"No MAKE-LOAD-FORM method for instances of ~S.\" (class-name (class-of object))))",
        &[],
    )?);
    for class in ["STANDARD-OBJECT", "STRUCTURE-OBJECT", "CONDITION"] {
        generic.add(&[class], missing.slot.get())?;
    }
    let by_class = compile_builtin_lambda(
        runtime,
        "(lambda (_next _has class &optional environment)
           (declare (ignore _next _has))
           (let ((name (class-name class)))
             (unless (eq class (find-class name nil environment))
               (error \"Class ~S has no proper name in the compilation environment.\" name))
             (list 'find-class (list 'quote name))))",
        &[],
    )?;
    generic.add(&["CLASS"], by_class)?;

    let function = native(
        runtime,
        Arc::new(|runtime, args, at| {
            let [object] = args else {
                return Err(error(at, "%STRUCTURE-SLOT-NAMES expects one structure"));
            };
            let (name, _) = runtime
                .heap
                .structure_layout(object.slot.get())
                .ok_or_else(|| {
                    Fault::type_error(runtime, 
                        object.slot.get(),
                        "STRUCTURE-OBJECT",
                        EvalError::new(at, "expected a structure"),
                    )
                })?;
            let definition = runtime
                .structure_definition(name)
                .expect("linked structure definition");
            let names = definition
                .slots
                .iter()
                .map(|slot| slot.name.get())
                .collect();
            Ok(vec![
                runtime.root(methods::list_from_words(runtime, names, at)?),
            ])
        }),
    );
    let name = runtime.root(runtime.intern_common_lisp("MAKE-LOAD-FORM-SAVING-SLOTS", 0)?);
    let saving_slots = compile_builtin_lambda(
        runtime,
        include_str!("../../../lisp/runtime/clos/make-load-form-saving-slots.lisp"),
        &[("%STRUCTURE-SLOT-NAMES", function)],
    )?;
    runtime
        .heap
        .set_symbol_cell(name.slot.get(), true, saving_slots);
    Ok(())
}

fn default_slot_missing<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let name = runtime.repr(&args[4]).map_err(Fault::from)?;
    Err(error(at, format!("no slot named {name}")))
}

fn default_slot_unbound<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let name = runtime.repr(&args[4]).map_err(Fault::from)?;
    Err(Fault::unbound_slot(runtime, 
        args[4].slot.get(),
        args[3].slot.get(),
        EvalError::new(at, format!("slot {name} is unbound")),
    ))
}
