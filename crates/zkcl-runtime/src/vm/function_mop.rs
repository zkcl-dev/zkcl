//! Storage and VM boundaries for the generic function and method protocols.
use std::cell::RefCell;
use std::ops::Range;

use super::{FunctionName, classes, generic, instances::SlotOp, methods, mop, proper_list};
use crate::compile::{call_signature, generic_required_parameters};
use crate::condition::{Class, Fault};
use crate::heap::{MethodFields, RootFrame, RootSlot, RootSlots, Word};
use crate::package::Package;
use crate::{EvalError, NativeFunction, Root, Runtime};

mod image;

/// Bootstrap the dispatch of the invocation-protocol generic functions on the
/// VM kernel. Other standard GFs can keep using that kernel until an applicable
/// user method on the protocol changes how their methods are computed.
pub(crate) struct InvocationProtocol {
    operations: Vec<ProtocolOperation>,
    extensions: RefCell<ProtocolExtensions>,
    slot_protocols: Vec<SlotProtocol>,
    standard_class: RootSlot,
    standard_effective_slot: RootSlot,
    function: RootSlot,
    computing: RootSlot,
}

struct ProtocolOperation {
    symbol: RootSlot,
    function: RootSlot,
    defaults: Vec<RootSlot>,
    on_method: bool,
}

struct SlotProtocol {
    op: SlotOp,
    function: RootSlot,
    lambda_list: RootSlot,
    combination: RootSlot,
    methods: Vec<RootSlot>,
}

#[derive(Default)]
struct ProtocolExtensions {
    generics: Vec<RootSlot>,
    methods: Vec<RootSlot>,
}

impl InvocationProtocol {
    fn new(rt: &Runtime) -> Result<Self, EvalError> {
        let mut operations = Vec::new();
        for (pkg, name) in [
            (Package::COMMON_LISP, "COMPUTE-APPLICABLE-METHODS"),
            (Package::MOP, "COMPUTE-APPLICABLE-METHODS-USING-CLASSES"),
            (Package::MOP, "COMPUTE-EFFECTIVE-METHOD"),
            (Package::MOP, "GENERIC-FUNCTION-METHOD-COMBINATION"),
            (Package::MOP, "FIND-METHOD-COMBINATION"),
            (Package::COMMON_LISP, "METHOD-QUALIFIERS"),
        ] {
            let sym = rt.intern_package(pkg, name, 0)?.0;
            let gf = rt.heap.symbol_cell(sym, true).expect("invocation protocol");
            let methods = proper_list(rt, rt.heap.generic_info(gf).unwrap().methods, 0)?;
            operations.push(ProtocolOperation {
                symbol: rt.root_slot(sym),
                function: rt.root_slot(gf),
                defaults: methods.into_iter().map(|word| rt.root_slot(word)).collect(),
                on_method: name == "METHOD-QUALIFIERS",
            });
        }
        let f = rt.intern_package(Package::MOP, "%INVOKE-PROTOCOL", 0)?.0;
        let function = rt.root_slot(rt.heap.symbol_cell(f, true).expect("discriminator"));
        let computing = rt.root_slot(
            rt.intern_package(Package::MOP, "*COMPUTING-INVOCATIONS*", 0)?
                .0,
        );
        let mut slot_protocols = Vec::new();
        for op in [
            SlotOp::Value,
            SlotOp::SetValue,
            SlotOp::Boundp,
            SlotOp::Makunbound,
        ] {
            let function = op.binding(rt).1.get(rt).expect("bootstrap slot protocol");
            let info = rt
                .heap
                .generic_info(function)
                .expect("slot generic function");
            slot_protocols.push(SlotProtocol {
                op,
                function: rt.root_slot(function),
                lambda_list: rt.root_slot(info.lambda_list),
                combination: rt.root_slot(info.combination),
                methods: proper_list(rt, info.methods, 0)?
                    .into_iter()
                    .map(|word| rt.root_slot(word))
                    .collect(),
            });
        }
        let standard_class =
            rt.root_slot(classes::builtin_class(rt, "STANDARD-CLASS", 0).map_err(|e| e.host)?);
        let standard_effective_slot = rt.root_slot(
            classes::builtin_class(rt, "STANDARD-EFFECTIVE-SLOT-DEFINITION", 0)
                .map_err(|e| e.host)?,
        );
        Ok(Self {
            operations,
            slot_protocols,
            standard_class,
            standard_effective_slot,
            function,
            computing,
            extensions: Default::default(),
        })
    }

    pub(super) fn changed(&self, rt: &Runtime, gf: Word) {
        if !self.operations.iter().any(|op| op.function.get() == gf) {
            return;
        }
        let mut extensions = self.extensions.borrow_mut();
        extensions.generics.clear();
        extensions.methods.clear();
        for op in &self.operations {
            let targets = if op.on_method {
                &mut extensions.methods
            } else {
                &mut extensions.generics
            };
            let baseline = &op.defaults;
            let mut tail = rt.heap.generic_info(op.function.get()).unwrap().methods;
            let mut defaults = 0;
            while let Some((method, next)) = rt.heap.cons_fields(tail) {
                if baseline.iter().any(|b| b.get() == method) {
                    defaults += 1;
                } else if let Some((spec, _)) = rt
                    .heap
                    .cons_fields(rt.heap.method_info(method).unwrap().specializers)
                    && !targets.iter().any(|s| s.get() == spec)
                {
                    targets.push(rt.root_slot(spec));
                }
                tail = next;
            }
            if defaults < baseline.len() {
                targets.push(rt.root_slot(Word::T));
            }
        }
    }

    pub(super) fn has_extensions(&self) -> bool {
        let extensions = self.extensions.borrow();
        !extensions.generics.is_empty() || !extensions.methods.is_empty()
    }

    pub(super) fn standard_slot_storage(
        &self,
        rt: &Runtime,
        op: SlotOp,
        class: Word,
        slot: Word,
    ) -> bool {
        if self.has_extensions() {
            return false;
        }
        let default = self
            .slot_protocols
            .iter()
            .find(|p| p.op == op)
            .expect("storage operation");
        let gf = default.function.get();
        if op.binding(rt).1.get(rt) != Some(gf)
            || rt.heap.generic_function_target(gf) != Some(Word::NIL)
        {
            return false;
        }
        let info = rt.heap.generic_info(gf).expect("slot protocol generic");
        if info.lambda_list != default.lambda_list.get()
            || info.combination != default.combination.get()
        {
            return false;
        }
        // ADD/REMOVE-METHOD can splice or replace an element in place. The
        // list head alone cannot establish that the bootstrap methods remain.
        let mut tail = info.methods;
        for expected in &default.methods {
            let Some((method, rest)) = rt.heap.cons_fields(tail) else {
                return false;
            };
            if method != expected.get() {
                return false;
            }
            tail = rest;
        }
        tail == Word::NIL
            && classes::has_direct_class(rt, class, self.standard_class.get())
            && classes::has_direct_class(rt, slot, self.standard_effective_slot.get())
    }

    fn standard_applicability(&self, rt: &Runtime) -> bool {
        // The first two operations are CAM and CAM-using-classes. A method
        // extension, replaced binding or installed discriminator can make the
        // result depend on arbitrary Lisp state.
        self.extensions.borrow().generics.is_empty()
            && self.operations[..2].iter().all(|op| {
                rt.heap.symbol_cell(op.symbol.get(), true) == Some(op.function.get())
                    && rt.heap.generic_function_target(op.function.get()) == Some(Word::NIL)
            })
    }

    pub(super) fn invocation(
        &self,
        rt: &Runtime,
        gf: Word,
        frame: &RootSlots,
        args: Range<usize>,
        at: usize,
    ) -> Result<Option<(Word, RootFrame)>, Fault> {
        let extensions = self.extensions.borrow();
        if (extensions.generics.is_empty() && extensions.methods.is_empty())
            || self.operations.iter().any(|op| op.function.get() == gf)
        {
            return Ok(None);
        }
        let matches = |target, specs: &[RootSlot]| {
            specs.iter().any(|s| {
                s.get() == Word::T
                    || rt.heap.eql_specializer_object(s.get()) == Some(target)
                    || rt
                        .heap
                        .class_info(s.get())
                        .is_some_and(|class| classes::typep(rt, target, class))
            })
        };
        if !matches(gf, &extensions.generics) {
            if extensions.methods.is_empty() {
                return Ok(None);
            }
            let mut tail = rt.heap.generic_info(gf).expect("generic function").methods;
            loop {
                let Some((method, next)) = rt.heap.cons_fields(tail) else {
                    return Ok(None);
                };
                if matches(method, &extensions.methods) {
                    break;
                }
                tail = next;
            }
        }
        drop(extensions);
        let mut active = rt
            .heap
            .symbol_cell(self.computing.get(), false)
            .unwrap_or(Word::NIL);
        while let Some((current, tail)) = rt.heap.cons_fields(active) {
            if current == gf {
                return Ok(None);
            }
            active = tail;
        }
        let staged = rt.root_frame(2);
        staged.set(0, gf);
        staged.set(
            1,
            methods::list_from_words(rt, args.map(|i| frame.get(i)).collect(), at)?,
        );
        Ok(Some((self.function.get(), staged)))
    }
}

fn problem(at: usize, message: &str) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}

fn configure_generic<'a>(
    rt: &'a Runtime,
    args: &[Root<'a>],
    at: usize,
) -> Result<Vec<Root<'a>>, Fault> {
    let [
        gf,
        name,
        lambda,
        supplied,
        precedence,
        combination,
        spec,
        method_class,
        doc,
        declarations,
    ] = args
    else {
        return Err(problem(at, "invalid generic function initialization"));
    };
    let info = rt
        .heap
        .generic_info(gf.slot.get())
        .ok_or_else(|| problem(at, "expected a generic function"))?;
    if !name.is_nil() {
        FunctionName::parse(rt, name.slot.get(), at)?;
    }
    if !doc.is_nil() && !rt.heap.is_string(doc.slot.get()) {
        return Err(Fault::type_error(rt, 
            doc.slot.get(),
            "STRING",
            EvalError::new(at, "expected generic function documentation"),
        ));
    }
    proper_list(rt, declarations.slot.get(), at)?;
    let method_root = rt.root(classes::builtin_class(rt, "METHOD", at)?);
    if classes::class_subtypep(rt, method_class.slot.get(), method_root.slot.get()) != Some(true) {
        return Err(problem(at, "method class must inherit from METHOD"));
    }
    let decls = rt.root(super::list::copy_tree(rt, declarations.slot.get(), at)?);
    let old_lambda = rt.root(rt.heap.generic_info(gf.slot.get()).unwrap().lambda_list);
    let lambda = if supplied.is_nil() {
        &old_lambda
    } else {
        lambda
    };
    if lambda.slot.get() != Word::UNBOUND {
        generic_required_parameters(rt, lambda, at)?;
        let signature = call_signature(rt, lambda, at)?;
        let prepared_signature =
            generic::prepare_generic_call_signature(rt, &signature, precedence.slot.get(), at)?;
        let info = rt.heap.generic_info(gf.slot.get()).unwrap();
        let current: Vec<_> = proper_list(rt, info.methods, at)?
            .into_iter()
            .map(|m| rt.root(m))
            .collect();
        for m in current {
            let m = rt.heap.method_info(m.slot.get()).unwrap();
            generic::congruent(
                &signature,
                &call_signature(rt, &rt.root(m.lambda_list), at)?,
                at,
            )?;
        }
        let (lambda, precedence) =
            generic::copy_lambda_list_and_precedence(rt, lambda, precedence, at)?;
        rt.heap.set_generic_definition(
            gf.slot.get(),
            lambda.slot.get(),
            prepared_signature(),
            precedence.slot.get(),
            spec.slot.get(),
            doc.slot.get(),
        );
    } else {
        if !precedence.is_nil() {
            return Err(problem(at, "argument precedence requires a lambda list"));
        }
        rt.heap.set_generic_definition(
            gf.slot.get(),
            Word::UNBOUND,
            info.call_signature,
            Word::NIL,
            spec.slot.get(),
            doc.slot.get(),
        );
    }
    rt.heap.set_generic_options(
        gf.slot.get(),
        name.slot.get(),
        method_class.slot.get(),
        combination.slot.get(),
        decls.slot.get(),
    );
    Ok(vec![gf.clone()])
}

fn configure_method<'a>(
    rt: &'a Runtime,
    args: &[Root<'a>],
    at: usize,
) -> Result<Vec<Root<'a>>, Fault> {
    let [m, quals, lambda, specs, public, entry, doc, slot, inline] = args else {
        return Err(problem(at, "invalid method initialization"));
    };
    let old = rt
        .heap
        .method_info(m.slot.get())
        .ok_or_else(|| problem(at, "expected method"))?;
    if old.function != Word::UNBOUND {
        return Err(problem(at, "method is already initialized"));
    }
    if !super::is_function(rt, public.slot.get()) || !super::is_function(rt, entry.slot.get()) {
        return Err(problem(at, "expected a method function"));
    }
    if !doc.is_nil() && !rt.heap.is_string(doc.slot.get()) {
        return Err(problem(at, "expected method documentation"));
    }
    let qualifiers = proper_list(rt, quals.slot.get(), at)?;
    if qualifiers
        .iter()
        .any(|&q| q == Word::NIL || rt.heap.cons_fields(q).is_some())
    {
        return Err(problem(at, "expected non-list method qualifiers"));
    }
    let signature = call_signature(rt, lambda, at)?;
    let specializers = proper_list(rt, specs.slot.get(), at)?;
    if signature.required.len() != specializers.len()
        || specializers.iter().any(|&s| !rt.heap.is_specializer(s))
    {
        return Err(problem(
            at,
            "expected one specializer metaobject per required parameter",
        ));
    }
    if !slot.is_nil()
        && !rt
            .heap
            .slot_definition_info(slot.slot.get())
            .is_some_and(|s| s.direct)
    {
        return Err(problem(
            at,
            "accessor method requires a direct slot definition",
        ));
    }
    let qualifiers = rt.root(super::list::copy_tree(rt, quals.slot.get(), at)?);
    let lambda = rt.root(super::list::copy_tree(rt, lambda.slot.get(), at)?);
    let specials = rt.root(methods::list_from_words(
        rt,
        proper_list(rt, specs.slot.get(), at)?,
        at,
    )?);
    rt.heap.initialize_method(
        m.slot.get(),
        MethodFields {
            qualifiers: qualifiers.slot.get(),
            specializers: specials.slot.get(),
            lambda_list: lambda.slot.get(),
            function: entry.slot.get(),
            documentation: doc.slot.get(),
            owner: Word::NIL,
            accessor_slot: slot.slot.get(),
            inline_definition: inline.slot.get(),
        },
        public.slot.get(),
    );
    Ok(vec![m.clone()])
}

fn compile_effective_method<'a>(
    runtime: &'a Runtime,
    args: &[Root<'a>],
    at: usize,
) -> Result<Vec<Root<'a>>, Fault> {
    let [form] = args else {
        return Err(problem(at, "expected an effective method form"));
    };
    let combinations = runtime
        .method_combinations
        .get()
        .expect("method combinations initialized");
    let (function, uses_methods) =
        super::method_combination::compile_effective_method(runtime, combinations, form, at)?;
    Ok(vec![
        function,
        runtime.root(if uses_methods { Word::T } else { Word::NIL }),
    ])
}

fn standard_effective_methods<'rt>(
    rt: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [generic, arguments, uses_methods] = args else {
        return Err(problem(
            at,
            "expected an effective function and its arguments",
        ));
    };
    let info = rt
        .heap
        .generic_info(generic.slot.get())
        .ok_or_else(|| problem(at, "expected a generic function"))?;
    if !rt
        .invocation_protocol
        .get()
        .is_some_and(|p| p.standard_applicability(rt))
    {
        return Ok(vec![rt.nil(), rt.nil()]);
    }
    let words = proper_list(rt, arguments.slot.get(), at)?;
    let methods = if uses_methods.is_nil() {
        generic::validate_arity(info.call_signature, words.len(), at)?;
        Word::NIL
    } else if let Some(cached_methods) =
            rt.heap
                .cached_standard_dispatch(generic.slot.get(), words.len(), |i| {
                    classes::dispatch_class_identity(rt, words[i])
                })
    {
        super::list::copy_list(rt, cached_methods.lists[0], at)?
    } else {
        let frame = rt.root_frame(words.len());
        for (i, word) in words.into_iter().enumerate() {
            frame.set(i, word);
        }
        let methods =
            generic::applicable_methods(rt, generic.slot.get(), &frame, 0..frame.len(), at)?;
        methods::list_from_words(rt, methods, at)?
    };
    Ok(vec![rt.root(methods), rt.root(Word::T)])
}

fn call_values<'a>(
    rt: &'a Runtime,
    function: Word,
    frame: &RootFrame,
    at: usize,
) -> Result<Vec<Root<'a>>, Fault> {
    let function = rt.root(function);
    let arguments =
        methods::list_from_words(rt, (0..frame.len()).map(|i| frame.get(i)).collect(), at)?;
    Ok(vec![function, rt.root(arguments)])
}

fn generic_call_values<'a>(
    rt: &'a Runtime,
    args: &[Root<'a>],
    at: usize,
) -> Result<Vec<Root<'a>>, Fault> {
    let [generic, arguments, rest @ ..] = args else {
        return Err(problem(at, "invalid dispatch invocation"));
    };
    if rt.heap.generic_info(generic.slot.get()).is_none() {
        return Err(problem(at, "expected a generic function"));
    }
    let arguments = proper_list(rt, arguments.slot.get(), at)?;
    let argument_frame = rt.root_frame(arguments.len());
    for (index, argument) in arguments.into_iter().enumerate() {
        argument_frame.set(index, argument);
    }
    let argument_range = 0..argument_frame.len();
    let (function, call_frame) = match rest {
        [] => super::method_combination::prepare_generic_call(
            rt,
            generic.slot.get(),
            &argument_frame,
            argument_range,
            at,
        )?,
        [applicable_methods] => {
            let methods = proper_list(rt, applicable_methods.slot.get(), at)?;
            if methods
                .iter()
                .any(|&method| rt.heap.method_info(method).is_none())
            {
                return Err(problem(at, "expected method metaobjects"));
            }
            super::method_combination::prepare_call_with_applicable_methods(
                rt,
                &generic.slot,
                methods,
                &argument_frame,
                argument_range,
                None,
                at,
            )?
        }
        _ => return Err(problem(at, "invalid dispatch invocation")),
    };
    call_values(rt, function, &call_frame, at)
}

type Callback = for<'rt> fn(&'rt Runtime, &[Root<'rt>], usize) -> Result<Vec<Root<'rt>>, Fault>;

pub(super) fn initialize(rt: &Runtime) -> Result<(), EvalError> {
    for name in [
        "ENSURE-GENERIC-FUNCTION-USING-CLASS",
        "COMPUTE-DISCRIMINATING-FUNCTION",
        "COMPUTE-EFFECTIVE-METHOD",
        "FIND-METHOD-COMBINATION",
        "METHOD-COMBINATION-NAME",
        "METHOD-COMBINATION-ARGLIST",
        "GENERIC-FUNCTION-METHOD-COMBINATION",
        "METHOD-FUNCTION",
        "MAKE-METHOD-LAMBDA",
        "READER-METHOD-CLASS",
        "WRITER-METHOD-CLASS",
        "EXTRACT-LAMBDA-LIST",
        "EXTRACT-SPECIALIZER-NAMES",
        "ADD-DIRECT-METHOD",
        "REMOVE-DIRECT-METHOD",
        "SPECIALIZER-DIRECT-METHODS",
        "EQL-SPECIALIZER-OBJECT",
        "SPECIALIZER-DIRECT-GENERIC-FUNCTIONS",
        "ADD-DEPENDENT",
        "REMOVE-DEPENDENT",
        "MAP-DEPENDENTS",
        "UPDATE-DEPENDENT",
        "ADD-DIRECT-SUBCLASS",
        "REMOVE-DIRECT-SUBCLASS",
        "CLASSP",
    ] {
        mop::export(rt, name)?;
    }
    let hooks: &[(&str, Callback)] = &[
        ("EXTRACT-LAMBDA-LIST", |rt, args, at| {
            let [input] = args else {
                return Err(problem(at, "EXTRACT-LAMBDA-LIST expects one argument"));
            };
            Ok(vec![crate::macros::extract_method_parameters(
                rt, input, false, at,
            )?])
        }),
        ("EXTRACT-SPECIALIZER-NAMES", |rt, args, at| {
            let [input] = args else {
                return Err(problem(
                    at,
                    "EXTRACT-SPECIALIZER-NAMES expects one argument",
                ));
            };
            Ok(vec![crate::macros::extract_method_parameters(
                rt, input, true, at,
            )?])
        }),
        ("%EQL-SPECIALIZER-OBJECT", |rt, args, at| {
            let [specializer] = args else {
                return Err(problem(at, "expected one EQL specializer"));
            };
            if let Some(value) = rt.heap.eql_specializer_object(specializer.slot.get()) {
                return Ok(vec![rt.root(value), rt.root(Word::T)]);
            }
            Ok(vec![rt.nil(), rt.nil()])
        }),
        ("%INITIALIZE-EQL-SPECIALIZER", |rt, args, at| {
            let [s, object] = args else {
                return Err(problem(at, "expected a specializer and its value"));
            };
            if !rt
                .heap
                .initialize_eql_specializer(s.slot.get(), object.slot.get())
            {
                return Err(problem(at, "invalid or initialized EQL specializer"));
            }
            Ok(vec![s.clone()])
        }),
        ("%REMEMBER-METHOD-FUNCTION", |rt, args, at| {
            let [method, function] = args else {
                return Err(problem(at, "expected method and function"));
            };
            if !super::is_function(rt, function.slot.get()) {
                return Err(problem(at, "expected method function"));
            }
            Ok(vec![
                rt.root(
                    rt.heap
                        .remember_method_function(method.slot.get(), function.slot.get())
                        .ok_or_else(|| problem(at, "expected a method"))?,
                ),
            ])
        }),
        ("%REMEMBER-COMBINATION", |rt, args, at| {
            let [gf, combination] = args else {
                return Err(problem(
                    at,
                    "expected generic function and method combination",
                ));
            };
            Ok(vec![
                rt.root(
                    rt.heap
                        .remember_combination(gf.slot.get(), combination.slot.get())
                        .ok_or_else(|| problem(at, "expected generic function"))?,
                ),
            ])
        }),
        ("%CHECK-ENVIRONMENT", |rt, args, at| {
            let [env] = args else {
                return Err(problem(at, "expected environment"));
            };
            crate::macros::validate_environment(rt, env.slot.get(), at)?;
            Ok(vec![env.clone()])
        }),
        ("%SUBCLASS-LINK", |rt, args, at| {
            let [parent, child, add] = args else {
                return Err(problem(at, "expected direct subclass relationship"));
            };
            crate::classes::edit_subclass(
                rt,
                parent.slot.get(),
                child.slot.get(),
                !add.is_nil(),
                at,
            )?;
            Ok(vec![child.clone()])
        }),
        ("%INLINE-DEFINITION", |rt, args, at| {
            let [gf, create] = args else {
                return Err(problem(at, "expected a generic function and creation flag"));
            };
            let token = if create.is_nil() {
                rt.heap
                    .generic_inline_definition(gf.slot.get(), None)
                    .ok_or_else(|| problem(at, "expected a generic function"))?
            } else {
                generic::inline_definition(rt, gf, at)?
            };
            Ok(vec![rt.root(token)])
        }),
        ("%RETIRE-INLINE-DEFINITION", |rt, args, at| {
            let [gf, token] = args else {
                return Err(problem(
                    at,
                    "expected a generic function and definition token",
                ));
            };
            if rt.heap.generic_inline_definition(gf.slot.get(), None) == Some(token.slot.get()) {
                rt.heap
                    .generic_inline_definition(gf.slot.get(), Some(Word::NIL));
            }
            Ok(vec![gf.clone()])
        }),
        ("%CONFIGURE-GENERIC", configure_generic),
        ("%CONFIGURE-METHOD", configure_method),
        ("%PREPARE-GENERIC-CALL", generic_call_values),
        ("%COMPILE-EFFECTIVE", compile_effective_method),
        ("%STANDARD-EFFECTIVE-METHODS", standard_effective_methods),
        ("%GF-PARAMETERS", |rt, args, at| {
            let [gf] = args else {
                return Err(problem(at, "expected a generic function"));
            };
            let info = rt
                .heap
                .generic_info(gf.slot.get())
                .ok_or_else(|| problem(at, "expected a generic function"))?;
            Ok(vec![
                rt.root(Word::fixnum(info.call_signature.required as isize).unwrap()),
                rt.root(if info.lambda_list == Word::UNBOUND {
                    Word::NIL
                } else {
                    Word::T
                }),
                rt.root(info.combination),
                rt.root(Word::fixnum(info.call_signature.positional as isize).unwrap()),
            ])
        }),
        ("%CHECK-DISPATCH", |rt, args, at| {
            let [gf, argv, rest @ ..] = args else {
                return Err(problem(at, "expected generic function arguments"));
            };
            let params = rt
                .heap
                .generic_call_signature(gf.slot.get())
                .ok_or_else(|| problem(at, "expected generic function"))?;
            let words = proper_list(rt, argv.slot.get(), at)?;
            generic::validate_arity(params, words.len(), at)?;
            if let [methods] = rest {
                let frame = rt.root_frame(words.len());
                for (i, w) in words.into_iter().enumerate() {
                    frame.set(i, w);
                }
                let methods = proper_list(rt, methods.slot.get(), at)?
                    .into_iter()
                    .map(|word| rt.root_slot(word))
                    .collect::<Vec<_>>();
                if methods
                    .iter()
                    .any(|m| rt.heap.method_info(m.get()).is_none())
                {
                    return Err(problem(at, "expected an applicable method"));
                }
                generic::validate_arguments(
                    rt,
                    gf.slot.get(),
                    &methods,
                    &frame,
                    0..frame.len(),
                    at,
                )?;
            }
            Ok(vec![
                rt.root(Word::fixnum(params.required as isize).unwrap()),
            ])
        }),
        ("%GF-COMBINATION", |rt, args, at| {
            let [gf] = args else {
                return Err(problem(at, "expected a generic function"));
            };
            Ok(vec![
                rt.root(
                    rt.heap
                        .generic_options(gf.slot.get())
                        .ok_or_else(|| problem(at, "expected generic function"))?[1],
                ),
            ])
        }),
        ("%CHECK-COMBINATION", |rt, args, at| {
            let [spec] = args else {
                return Err(problem(at, "expected a combination specifier"));
            };
            let combinations = rt
                .method_combinations
                .get()
                .expect("initialized method combinations");
            let kind = combinations.parse(rt, spec.slot.get(), at)?;
            if let super::method_combination::MethodCombinationKind::Long {
                argument_lambda_list,
                ..
            } = kind
            {
                Ok(vec![
                    spec.clone(),
                    rt.root(argument_lambda_list.get()),
                    rt.root(Word::T),
                ])
            } else {
                Ok(vec![spec.clone()])
            }
        }),
        ("%METHOD-ENTRIES", |rt, args, at| {
            let [method] = args else {
                return Err(problem(at, "expected a method"));
            };
            let m = rt
                .heap
                .method_info(method.slot.get())
                .ok_or_else(|| problem(at, "expected method"))?;
            if m.function == Word::UNBOUND {
                return Err(problem(at, "method function is uninitialized"));
            }
            Ok(vec![
                rt.root(rt.heap.portable_method_function(method.slot.get()).unwrap()),
                rt.root(m.function),
                rt.root(m.inline_definition),
            ])
        }),
        ("%IMPLICIT-LAMBDA", |rt, args, at| {
            let [lambda] = args else {
                return Err(problem(at, "expected a method lambda list"));
            };
            Ok(vec![generic::implicit_lambda_list(
                rt,
                &call_signature(rt, lambda, at)?,
                at,
            )?])
        }),
        ("%RESOLVE-SPECIALIZERS", |rt, args, at| {
            let [specs] = args else {
                return Err(problem(at, "expected specializers"));
            };
            let specs = generic::resolve_specializers(rt, specs.slot.get(), at)?;
            Ok(vec![rt.root(methods::list_from_words(
                rt,
                specs.iter().map(|s| s.slot.get()).collect(),
                at,
            )?)])
        }),
        ("%FUNCTION-TARGET", |rt, args, at| {
            let [name] = args else {
                return Err(problem(at, "expected a function name"));
            };
            let f = FunctionName::parse(rt, name.slot.get(), at)?;
            if let Some(existing) = f.get(rt) {
                if rt.heap.generic_info(existing).is_none() {
                    return Err(problem(at, "name denotes an ordinary function"));
                }
                return Ok(vec![rt.root(existing)]);
            }
            f.check_definable(rt, at)?;
            if rt.heap.macro_expander(name.slot.get()).is_some() {
                return Err(problem(
                    at,
                    "cannot replace a macro with a generic function",
                ));
            }
            Ok(vec![rt.nil()])
        }),
        ("%INSTALL-GENERIC", |rt, args, at| {
            let [name, gf] = args else {
                return Err(problem(at, "expected function name and generic function"));
            };
            if rt.heap.generic_info(gf.slot.get()).is_none() {
                return Err(problem(at, "expected a generic function"));
            }
            let n = FunctionName::parse(rt, name.slot.get(), at)?;
            if n.get(rt) != Some(gf.slot.get()) {
                n.install(rt, gf.slot.get(), at)?;
            }
            Ok(vec![gf.clone()])
        }),
        ("%ENSURE-FUNCTION-STORAGE", |rt, args, at| {
            let [o] = args else {
                return Err(problem(at, "expected callable metaobject"));
            };
            if rt.heap.instance_class(o.slot.get()).is_none() {
                if rt.heap.generic_info(o.slot.get()).is_none()
                    && rt.heap.method_info(o.slot.get()).is_none()
                {
                    return Err(problem(at, "expected callable metaobject"));
                }
                let class = rt.root(classes::class_of(rt, o.slot.get(), at)?);
                let def = classes::class_definition(rt, class.slot.get())
                    .ok_or_else(|| problem(at, "metaobject has no layout"))?;
                let payload = rt
                    .heap
                    .instance(&class.slot, &def.instance_layout)
                    .map_err(|e| EvalError::out_of_memory(at, e))?;
                rt.heap.set_additional_meta_instance(o.slot.get(), payload);
            }
            Ok(vec![o.clone()])
        }),
        ("%REMEMBER-METHOD", |rt, args, at| {
            let [class, m] = args else {
                return Err(problem(at, "expected accessor owner and method"));
            };
            crate::classes::remember_accessor(rt, class.slot.get(), m.slot.get())
                .ok_or_else(|| problem(at, "invalid accessor method"))?;
            Ok(vec![m.clone()])
        }),
        ("%RELATIONS", |rt, args, at| {
            let [o, dependent] = args else {
                return Err(problem(at, "expected a metaobject relation"));
            };
            let list = rt
                .heap
                .mop_relations(o.slot.get(), !dependent.is_nil())
                .ok_or_else(|| problem(at, "wrong metaobject for relation"))?;
            Ok(vec![rt.root(methods::list_from_words(
                rt,
                proper_list(rt, list, at)?,
                at,
            )?)])
        }),
        ("%CHANGE-RELATION", |rt, args, at| {
            let [o, member, dependent, add] = args else {
                return Err(problem(at, "expected a metaobject relation update"));
            };
            methods::change_relation(
                rt,
                o.slot.get(),
                member.slot.get(),
                !dependent.is_nil(),
                !add.is_nil(),
                at,
            )?;
            Ok(vec![o.clone()])
        }),
        ("%CHECK-METHOD", |rt, args, at| {
            let [gf, m] = args else {
                return Err(problem(at, "expected generic function and method"));
            };
            let known = methods::check_method(rt, gf.slot.get(), m.slot.get(), at)?;
            Ok(vec![rt.root(if known { Word::T } else { Word::NIL })])
        }),
        ("%ATTACH-METHOD", |rt, args, at| {
            let [gf, m] = args else {
                return Err(problem(at, "expected generic function and method"));
            };
            methods::attach(
                rt,
                gf.slot.get(),
                m.slot.get(),
                methods::MethodUpdate::Protocol,
                at,
            )?;
            Ok(vec![gf.clone()])
        }),
        ("%DETACH-METHOD", |rt, args, at| {
            let [gf, m] = args else {
                return Err(problem(at, "expected generic function and method"));
            };
            methods::detach(
                rt,
                gf.slot.get(),
                m.slot.get(),
                methods::MethodUpdate::Protocol,
                at,
            )?;
            Ok(vec![gf.clone()])
        }),
        ("%DISCARD-METHOD", |rt, args, at| {
            let [gf, m] = args else {
                return Err(problem(at, "expected generic function and method"));
            };
            // On a nonlocal exit the method may have been partially unlinked
            // or reattached by a nested operation.
            if rt
                .heap
                .method_info(m.slot.get())
                .ok_or_else(|| problem(at, "expected method"))?
                .owner
                == Word::NIL
            {
                methods::update_specializer_direct_methods(rt, &m.slot, false, at)?;
            } else {
                methods::remove(rt, gf.slot.get(), m.slot.get(), at)?;
            }
            Ok(vec![gf.clone()])
        }),
        ("%INSTALL-DISCRIMINATOR", |rt, args, at| {
            let [gf, f] = args else {
                return Err(problem(at, "expected generic function and discriminator"));
            };
            if rt.heap.generic_info(gf.slot.get()).is_none()
                || !f.is_nil() && !super::is_function(rt, f.slot.get())
            {
                return Err(problem(at, "invalid discriminator"));
            }
            rt.heap
                .set_generic_function_target(gf.slot.get(), f.slot.get());
            Ok(vec![gf.clone()])
        }),
        ("%SYNTHETIC-METHOD", |rt, args, at| {
            let [public, fast] = args else {
                return Err(problem(at, "expected method entries"));
            };
            if !super::is_function(rt, public.slot.get())
                || !super::is_function(rt, fast.slot.get())
            {
                return Err(problem(at, "expected method entries"));
            }
            let m = rt.root(
                rt.heap
                    .method(MethodFields::uninitialized)
                    .map_err(|e| EvalError::out_of_memory(at, e))?,
            );
            let mut info = MethodFields::uninitialized();
            info.function = fast.slot.get();
            rt.heap
                .initialize_method(m.slot.get(), info, public.slot.get());
            Ok(vec![m])
        }),
    ];
    for &(name, callback) in hooks {
        let symbol = rt.intern_package(Package::MOP, name, 0)?.0;
        let mut natives = rt.natives.borrow_mut();
        let fnword = Word::native(natives.len());
        natives.push(std::sync::Arc::new(callback) as std::sync::Arc<NativeFunction>);
        rt.heap.set_symbol_cell(symbol, true, fnword);
    }
    rt.load_builtin_source(include_str!("../../lisp/runtime/clos/mop-functions.lisp"))?;
    let ensure = rt.intern_package(Package::MOP, "%ENSURE-GENERIC", 0)?.0;
    let function = rt
        .heap
        .symbol_cell(ensure, true)
        .expect("MOP generic definition function");
    let name = rt.intern_common_lisp("ENSURE-GENERIC-FUNCTION", 0)?;
    rt.heap.set_symbol_cell(name, true, function);
    rt.invocation_protocol
        .set(InvocationProtocol::new(rt)?)
        .ok()
        .expect("invocation protocol initialized once");
    Ok(())
}
