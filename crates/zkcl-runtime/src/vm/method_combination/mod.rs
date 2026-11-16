use std::cell::RefCell;
use std::ops::Range;

use super::{FunctionName, classes, generic, proper_list};
use crate::condition::{Class, Fault};
use crate::heap::{
    CachedStandardMethods, RootFrame, RootSlot, RootSlots, StandardDispatchCacheKey, Word,
};
use crate::read::FormKind;
use crate::symbol::{Name, Package};
use crate::{EvalError, NativeFunction, Root, Runtime, compile, form_value, read};

mod definition;
mod image;
pub(super) use definition::MethodCombinationKind;
use definition::{Definition, define_long_method_combination, define_short_method_combination};

/// Names of the simple built-in method combination types of CLHS 7.6.6.4.
const OPERATORS: [&str; 9] = [
    "+", "AND", "APPEND", "LIST", "MAX", "MIN", "NCONC", "OR", "PROGN",
];

/// Internal COMMON-LISP symbols used by the method combination implementation.
/// They are internal so that user code cannot name them through CL or CL-USER.
const INTERNAL_NAMES: [&str; 21] = [
    "%ZK-CALL-METHOD",
    "%ZK-METHOD-FUNCTION",
    "%ZK-SET-DOCUMENTATION",
    "%ZK-DEFINE-SHORT-METHOD-COMBINATION",
    "%ZK-DEFINE-LONG-METHOD-COMBINATION",
    "%%ZK-EM-ARGUMENTS",
    "%%ZK-GENERIC",
    "%%ZK-METHODS",
    "%%ZK-CHECK",
    "%%ZK-SELF",
    "%%ZK-OPTIONS",
    "%%ZK-GROUP-VALUES",
    "%%ZK-EM-ARGUMENT-VALUES",
    "%%ZK-GENERIC-FUNCTION",
    "%%ZK-NEXT-METHOD",
    "%%ZK-NEXT-METHOD-P",
    "%%ZK-ARGUMENTS",
    "%%ZK-CHANGED",
    "%%ZK-TEMPLATE-BODY",
    "%ZK-SHORT",
    "%ZK-LONG",
];

pub(crate) struct MethodCombinations {
    effective_method_macros: [(Name, Word); 2],
    method_group_trampoline: RootSlot,
    primary_trampolines: [RootSlot; 2],
    pub(super) check_next_arguments: Word,
    combine: RootSlot,
    standard: RootSlot,
    auxiliary: [RootSlot; 3],
    operators: [RootSlot; 9],
    order: [RootSlot; 2],
    definitions: RefCell<Vec<Definition>>,
    /// (lambda (em-arguments generic methods check self) <body>)
    effective_method_template: RootSlot,
    /// (function (lambda (...) ...)) used for (make-method form).
    make_method_template: RootSlot,
}

// Only special operators and ordinary functions are used during compilation.
// The trampoline keeps method calls and the NO-* hooks on the same VM stack.
// Its last parameter selects the primary chain: NIL runs the standard
// call-next-method chain, otherwise the argument combines the primary methods.
const STANDARD_COMBINATION: &str = r#"
  (lambda (generic methods around before primary after arguments check-arguments operator combine)
    (labels ((call-chain (chain current core)
               (let* ((entry (car chain))
                      (more (cdr chain)))
                 (apply (cdr entry)
                        (lambda (&rest changed)
                          (let ((actual (if changed
                                            (progn
                                              (funcall check-arguments generic methods changed)
                                              changed)
                                            current)))
                            (if more
                                (call-chain more actual core)
                                (if core
                                    (funcall core actual)
                                    (apply #'no-next-method generic (car entry) actual)))))
                        (if more (zk-mop::%next-methods more core) (if core (zk-mop::%next-methods nil core) nil))
                        current)))
             (call-auxiliary (entries current)
               (tagbody
                 next
                   (if entries
                       (progn
                         (apply (cdr (car entries)) nil nil current)
                         (setq entries (cdr entries))
                         (go next)))))
             (call-primary (current)
               (call-auxiliary before current)
               (multiple-value-prog1
                   (if combine
                       (funcall combine operator primary current)
                       (call-chain primary current nil))
                 (call-auxiliary after current))))
      (if around
          (call-chain around arguments #'call-primary)
          (if primary
              (call-primary arguments)
              (apply #'no-applicable-method generic arguments)))))
  "#;

// Primary-only standard combinations can forward fixed arity arguments
// directly. Each next function captures its own original arguments, including
// when a method retains the function after the generic call returns.
const PRIMARY_CHAIN: &str = r#"
  (lambda (generic methods primary check-arguments @args)
    (labels ((walk (entries remaining @args)
               (let ((entry (car entries)) (more (cdr entries))
                     (tail (cdr remaining)))
                 (funcall (cdr entry)
                          (lambda (&rest changed)
                            (if changed
                                (progn
                                  (funcall check-arguments generic methods changed)
                                  (if more
                                      (apply #'walk more tail changed)
                                      (apply #'no-next-method generic (car entry) changed)))
                                (if more
                                    (walk more tail @args)
                                    (no-next-method generic (car entry) @args))))
                          (if tail (copy-list tail) nil)
                          @args))))
      (walk primary methods @args)))
  "#;

// CLHS 7.6.6.4 defines the effective method as (operator <M1> ... <Mk>).
// Function operands are evaluated before the call. AND, OR and PROGN
// preserve every value of the last operand when it is reached.
const OPERATOR_COMBINATION: &str = r#"
  (lambda (operator primary arguments)
    (labels ((call-primary (entry)
               (apply (cdr entry) nil nil arguments))
             (collect (entries)
               (let ((values nil))
                 (tagbody
                   next
                     (if entries
                         (progn
                           (setq values (cons (call-primary (car entries)) values))
                           (setq entries (cdr entries))
                           (go next))))
                 (nreverse values)))
             (sequence (entries)
               (block result
                 (tagbody
                   next
                     (let ((entry (car entries))
                           (more (cdr entries)))
                       (if (null more)
                           (return-from result (call-primary entry)))
                       (let ((value (call-primary entry)))
                         (if (or (and (eq operator 'and) (null value))
                                 (and (eq operator 'or) value))
                             (return-from result value)))
                       (setq entries more)
                       (go next))))))
      (cond ((or (eq operator 'and) (eq operator 'or) (eq operator 'progn))
             (sequence primary))
            ((eq operator 'list) (collect primary))
            (t (apply operator (collect primary))))))
  "#;

// CALL-METHOD is implemented by a Lisp trampoline that follows the same
// method calling convention as the standard method combination: a method is
// applied to a next-method function, a next-method-p flag, and the arguments.
const CALL_METHOD_LAMBDA: &str = r#"
  (lambda (entry next-entries em-arguments generic methods check &rest extra)
    (labels ((invoke (entry next-entries current)
               (let ((function (if (functionp entry)
                                   entry
                                   (funcall #'cl::%zk-method-function entry))))
                 (apply function
                        (lambda (&rest changed)
                          (let ((actual (if changed
                                            (progn
                                              (funcall check generic methods changed)
                                              changed)
                                            current)))
                            (if next-entries
                                (invoke (car next-entries) (cdr next-entries) actual)
                                (apply #'no-next-method
                                       generic
                                       (if (functionp entry) nil entry)
                                       actual))))
                        (mapcar #'zk-mop::%next-object next-entries)
                        current))))
      (if extra
          (apply (zk-mop::method-function entry) em-arguments (mapcar #'zk-mop::%next-object next-entries) extra)
          (invoke entry next-entries em-arguments))))
  "#;

// (make-method form) is expanded to a function using the method calling
// convention. The body form is substituted for the placeholder. Give any
// :ARGUMENTS references a fresh cache for this call's (possibly changed) argv.
const MAKE_METHOD_TEMPLATE: &str = r#"
  (function
    (lambda (cl::%%zk-next-method cl::%%zk-next-method-p &rest cl::%%zk-arguments)
      (declare (ignore cl::%%zk-next-method-p))
      (flet ((call-next-method (&rest cl::%%zk-changed)
               (if cl::%%zk-next-method
                   (if cl::%%zk-changed
                       (apply cl::%%zk-next-method cl::%%zk-changed)
                       (funcall cl::%%zk-next-method))
                   (error "No next method.")))
             (next-method-p () (and cl::%%zk-next-method t)))
        (let ((cl::%%zk-em-arguments cl::%%zk-arguments)
              (cl::%%zk-em-argument-values nil))
          (declare (ignorable cl::%%zk-em-argument-values))
          cl::%%zk-template-body))))
  "#;

// The effective method is compiled as a function of the arguments it
// received, plus the state needed by CALL-METHOD. The extra SELF parameter
// keeps the compiled function reachable from its own argument frame.
const EFFECTIVE_METHOD_TEMPLATE: &str = r#"
  (lambda (cl::%%zk-em-arguments cl::%%zk-generic cl::%%zk-methods
           cl::%%zk-check cl::%%zk-self)
    cl::%%zk-template-body)
  "#;

// A Lisp macro expander. It receives the whole DEFINE-METHOD-COMBINATION form
// and the environment. The result evaluates the method group :order forms in
// the lexical environment of the definition and compiles the body into a
// closure that computes the effective method form for each set of methods.
const DEFINE_METHOD_COMBINATION: &str =
    include_str!("../../../lisp/runtime/clos/define-method-combination.lisp");

pub(crate) fn initialize(runtime: &Runtime) -> Result<MethodCombinations, EvalError> {
    // Compile directly while the initial reader and package state is active.
    let method_group_trampoline = compile_source_lambda(runtime, STANDARD_COMBINATION)?;
    let primary_trampolines = [
        compile_source_lambda(runtime, &PRIMARY_CHAIN.replace("@args", "a"))?,
        compile_source_lambda(runtime, &PRIMARY_CHAIN.replace("@args", "a b"))?,
    ];
    let combine = compile_source_lambda(runtime, OPERATOR_COMBINATION)?;
    install_internal_symbols(runtime)?;

    let first = runtime.natives.borrow().len();
    for (name, signature, specializers, callback) in [
        (
            "NO-APPLICABLE-METHOD",
            "(generic-function &rest arguments)",
            &["T"][..],
            Word::native(first + 1),
        ),
        (
            "NO-NEXT-METHOD",
            "(generic-function method &rest arguments)",
            &["STANDARD-GENERIC-FUNCTION", "STANDARD-METHOD"][..],
            Word::native(first + 2),
        ),
    ] {
        generic::BuiltinGeneric::standard(runtime, name, signature)?.add(specializers, callback)?;
    }
    let callbacks: [std::sync::Arc<NativeFunction>; 9] = [
        std::sync::Arc::new(check_next_arguments),
        std::sync::Arc::new(default_no_applicable_method),
        std::sync::Arc::new(default_no_next_method),
        std::sync::Arc::new(define_short_method_combination),
        std::sync::Arc::new(define_long_method_combination),
        std::sync::Arc::new(set_documentation),
        std::sync::Arc::new(method_function),
        std::sync::Arc::new(|rt, args, at| effective_macro(rt, args, at, expand_call_method)),
        std::sync::Arc::new(|rt, args, at| effective_macro(rt, args, at, expand_make_method)),
    ];
    runtime.natives.borrow_mut().extend(callbacks);
    super::instances::initialize_slot_protocol(runtime)?;

    // CALL-METHOD's trampoline.
    let call_method = compile_source_lambda(runtime, CALL_METHOD_LAMBDA)?;
    set_function(runtime, "%ZK-CALL-METHOD", call_method.get())?;
    set_function(runtime, "%ZK-METHOD-FUNCTION", Word::native(first + 6))?;
    set_function(runtime, "%ZK-SET-DOCUMENTATION", Word::native(first + 5))?;
    set_function(
        runtime,
        "%ZK-DEFINE-SHORT-METHOD-COMBINATION",
        Word::native(first + 3),
    )?;
    set_function(
        runtime,
        "%ZK-DEFINE-LONG-METHOD-COMBINATION",
        Word::native(first + 4),
    )?;

    let effective_method_template =
        runtime.root_slot(read_template(runtime, EFFECTIVE_METHOD_TEMPLATE)?);
    let make_method_template = runtime.root_slot(read_template(runtime, MAKE_METHOD_TEMPLATE)?);

    // DEFINE-METHOD-COMBINATION is a macro whose expander is ordinary Lisp.
    {
        let expander = compile_source_lambda(runtime, DEFINE_METHOD_COMBINATION)?;
        let symbol = runtime.intern_common_lisp("DEFINE-METHOD-COMBINATION", 0)?;
        runtime.heap.set_macro_expander(symbol, expander.get());
    }

    // (setf (documentation object kind) new) writes documentation for generic
    // functions, methods and method combination names.
    {
        let documentation = runtime.intern_common_lisp("DOCUMENTATION", 0)?;
        let writer = runtime.intern_common_lisp("%ZK-SET-DOCUMENTATION", 0)?;
        runtime.define_setf_expander(
            documentation,
            crate::SetfExpander::Short(runtime.root_slot(writer)),
            None,
        );
    }

    Ok(MethodCombinations {
        effective_method_macros: [
            (Name::common_lisp("CALL-METHOD"), Word::native(first + 7)),
            (Name::common_lisp("MAKE-METHOD"), Word::native(first + 8)),
        ],
        method_group_trampoline,
        primary_trampolines,
        check_next_arguments: Word::native(first),
        combine,
        standard: runtime.root_slot(runtime.intern_common_lisp("STANDARD", 0)?),
        auxiliary: symbols(
            runtime,
            [
                Name::keyword("AROUND"),
                Name::keyword("BEFORE"),
                Name::keyword("AFTER"),
            ],
        )?,
        operators: symbols(runtime, OPERATORS.map(Name::common_lisp))?,
        order: symbols(
            runtime,
            [
                Name::keyword("MOST-SPECIFIC-FIRST"),
                Name::keyword("MOST-SPECIFIC-LAST"),
            ],
        )?,
        definitions: RefCell::default(),
        effective_method_template,
        make_method_template,
    })
}

fn install_internal_symbols(runtime: &Runtime) -> Result<(), EvalError> {
    for name in INTERNAL_NAMES {
        runtime.intern_package(Package::COMMON_LISP, name, 0)?;
    }
    Ok(())
}

fn set_function(runtime: &Runtime, spelling: &str, function: Word) -> Result<(), EvalError> {
    let symbol = runtime.intern_common_lisp(spelling, 0)?;
    runtime.heap.set_symbol_cell(symbol, true, function);
    Ok(())
}

fn compile_source_lambda(runtime: &Runtime, source: &str) -> Result<RootSlot, EvalError> {
    super::instances::compile_builtin_lambda(runtime, source, &[])
        .map(|word| runtime.root_slot(word))
}

fn read_template(runtime: &Runtime, source: &str) -> Result<Word, EvalError> {
    let form = read::read(runtime, source)?;
    let value = form_value::to_value(runtime, &form)?;
    Ok(value.slot.get())
}

fn symbols<const N: usize>(
    runtime: &Runtime,
    names: [Name; N],
) -> Result<[RootSlot; N], EvalError> {
    let mut slots = Vec::with_capacity(N);
    for name in names {
        slots.push(runtime.root_slot(runtime.intern(&name, 0)?));
    }
    let Ok(slots) = <[RootSlot; N]>::try_from(slots) else {
        unreachable!("the method combination symbol table is fixed");
    };
    Ok(slots)
}

pub(super) fn prepare_generic_call(
    runtime: &Runtime,
    generic: Word,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<(Word, RootFrame), Fault> {
    let combination = runtime
        .method_combinations
        .get()
        .expect("initialized runtime");
    let info = runtime
        .heap
        .generic_info(generic)
        .expect("generic function");
    let cache_epoch = (generic::fixed_dispatch_arity(info.call_signature, args.len())
        && runtime.heap.cons_fields(info.combination)
            == Some((combination.standard.get(), Word::NIL)))
    .then(|| runtime.heap.gc_epoch());
    let cached = cache_epoch.and_then(|_| {
        runtime
            .heap
            .cached_standard_dispatch(generic, info.lambda_list, |i| {
                classes::dispatch_class_identity(runtime, frame.get(args.start + i))
            })
    });
    if let Some(cached_methods) = cached
        && cached_methods.direct_primary != Word::NIL
    {
        let call = runtime.root_frame(2 + args.len());
        for (index, argument) in args.enumerate() {
            call.set(2 + index, frame.get(argument));
        }
        return Ok((cached_methods.direct_primary, call));
    }
    let cached = cached.map(|cached_methods| cached_methods.lists);
    if let Some([methods, Word::NIL, Word::NIL, primary, Word::NIL]) = cached {
        let call = runtime.root_frame(4 + args.len());
        call.set(0, generic);
        call.set(1, methods);
        call.set(2, primary);
        call.set(3, combination.check_next_arguments);
        for (index, argument) in args.clone().enumerate() {
            call.set(4 + index, frame.get(argument));
        }
        return Ok((combination.primary_trampolines[args.len() - 1].get(), call));
    }
    if let Some(lists) = cached {
        let result = runtime.root_frame(10);
        result.set(0, generic);
        for (slot, list) in (1..).zip(lists) {
            result.set(slot, list);
        }
        build_arguments_list(runtime, &result, 6, frame, args, at)?;
        result.set(7, combination.check_next_arguments);
        return Ok((combination.method_group_trampoline.get(), result));
    }
    let generic = runtime.root_slot(generic);
    let methods = generic::applicable_methods(runtime, generic.get(), frame, args.clone(), at)?;
    prepare_call_with_applicable_methods(runtime, &generic, methods, frame, args, cache_epoch, at)
}

pub(super) fn prepare_call_with_applicable_methods(
    runtime: &Runtime,
    generic: &RootSlot,
    methods: Vec<Word>,
    frame: &RootSlots,
    args: Range<usize>,
    cache_epoch: Option<usize>,
    at: usize,
) -> Result<(Word, RootFrame), Fault> {
    let combination = runtime
        .method_combinations
        .get()
        .expect("initialized runtime");
    let methods: Vec<_> = methods
        .into_iter()
        .map(|word| runtime.root_slot(word))
        .collect();
    if !methods.is_empty() {
        generic::validate_arguments(runtime, generic.get(), &methods, frame, args.clone(), at)?;
    }
    let info = runtime
        .heap
        .generic_info(generic.get())
        .expect("generic function");
    let kind = combination.parse(runtime, info.combination, at)?;
    // With no applicable methods the standard trampoline reports
    // NO-APPLICABLE-METHOD, whatever the method combination type is.
    let kind = if methods.is_empty() {
        MethodCombinationKind::Standard
    } else {
        kind
    };
    let call = ApplicableCall {
        generic,
        methods: &methods,
        arguments: frame.slice(args.clone()),
    };
    let (operator, most_specific_last) = match kind {
        MethodCombinationKind::Short {
            name,
            operator,
            identity,
            most_specific_last,
        } => {
            return prepare_short_combination_call(
                runtime,
                combination,
                call,
                name,
                operator,
                identity,
                most_specific_last,
                at,
            );
        }
        MethodCombinationKind::Long {
            bind_options,
            options,
            ..
        } => {
            return prepare_long_combination_call(
                runtime,
                combination,
                call,
                bind_options,
                options,
                at,
            );
        }
        MethodCombinationKind::Standard => (None, false),
        MethodCombinationKind::SimpleBuiltIn {
            operator,
            most_specific_last,
        } => (Some(operator), most_specific_last),
    };
    let mut groups: [Vec<usize>; 4] = Default::default();
    // Group order matches the trampoline parameters: around, before,
    // primary, after. Only the after group runs least-specific-first.
    for (index, method) in methods.iter().enumerate() {
        let info = runtime
            .heap
            .method_info(method.get())
            .expect("applicable method");
        let group = match operator {
            None => standard_group(runtime, combination, info.qualifiers, at)?,
            Some(operator) => {
                simple_builtin_group(runtime, combination, operator.get(), info.qualifiers, at)?
            }
        };
        groups[group].push(index);
    }
    if operator.is_none() {
        groups[3].reverse();
    } else if most_specific_last {
        groups[2].reverse();
    }
    if groups[2].is_empty() && !methods.is_empty() {
        return Err(method_error(at, "no applicable primary method"));
    }

    let result = runtime.root_frame(10);
    result.set(0, generic.get());
    build_methods_list(runtime, &result, 1, &methods, at)?;
    for (slot, group) in (2..).zip(groups) {
        for index in group.into_iter().rev() {
            let entry = runtime.root_slot(
                runtime
                    .heap
                    .cons(|| {
                        let method = methods[index].get();
                        let info = runtime.heap.method_info(method).expect("applicable method");
                        (method, info.function)
                    })
                    .map_err(|error| EvalError::out_of_memory(at, error))?,
            );
            push(runtime, &result, slot, || entry.get(), at)?;
        }
    }
    if let Some(gc_epoch) = cache_epoch.filter(|&epoch| epoch == runtime.heap.gc_epoch())
        && !methods.is_empty()
        && let Some(argument) = generic::single_dispatch_argument(runtime, info.methods)
        && let Some(class_identity) =
            classes::dispatch_class_identity(runtime, frame.get(args.start + argument))
    {
        let key = StandardDispatchCacheKey {
            argument,
            class_identity,
            lambda_list: info.lambda_list,
            gc_epoch,
        };
        // The trampoline reads these private lists. An escaped next-method
        // closure can retain them independently of later cache invalidation.
        let lists = std::array::from_fn(|index| result.get(index + 1));
        let direct_primary = match lists {
            [_, Word::NIL, Word::NIL, primary, Word::NIL] => {
                let (entry, _) = runtime.heap.cons_fields(primary).expect("primary method");
                let (_, function) = runtime.heap.cons_fields(entry).expect("method entry");
                independent_primary(runtime, function).unwrap_or(Word::NIL)
            }
            _ => Word::NIL,
        };
        runtime.heap.cache_standard_dispatch(
            result.get(0),
            key,
            CachedStandardMethods {
                lists,
                direct_primary,
            },
        );
    }
    build_arguments_list(runtime, &result, 6, frame, args, at)?;
    result.set(7, combination.check_next_arguments);
    if let Some(operator) = operator {
        result.set(8, operator.get());
        result.set(9, combination.combine.get());
    }
    Ok((combination.method_group_trampoline.get(), result))
}

// The first two required parameters of the internal method convention carry
// CALL-NEXT-METHOD and NEXT-METHOD-P. If the compiled body never reads them,
// pass NIL in those slots and skip building the continuation, even when other
// primary methods are applicable. Keep the actual closure and its captures.
fn independent_primary(runtime: &Runtime, function: Word) -> Option<Word> {
    let code = runtime
        .heap
        .function_info(runtime.heap.function_code(function)?)?;
    let code = runtime.heap.code_store.get(code.code_id);
    (code.arity.required >= 2
        && !reads_parameters(&code.canonical, &code.handlers, &code.restarts, 0..2))
    .then_some(function)
}

fn reads_parameters(
    instructions: &[zkcl_bytecode::Instruction],
    handlers: &[zkcl_bytecode::HandlerTable],
    restarts: &[zkcl_bytecode::RestartTable],
    parameters: Range<usize>,
) -> bool {
    // Unwind side tables and the saved argument vector can be read
    // independently of the instruction's ordinary register operands.
    !handlers.is_empty()
        || !restarts.is_empty()
        || instructions.iter().any(|inst| {
            match inst {
                zkcl_bytecode::Instruction::OptionalArgument(index, _)
                    if parameters.contains(index) =>
                {
                    return true;
                }
                zkcl_bytecode::Instruction::RestArguments { start, .. }
                | zkcl_bytecode::Instruction::CheckKeywords { start, .. }
                    if *start < parameters.end =>
                {
                    return true;
                }
                _ => {}
            }
            let reads = inst.register_operands().reads;
            parameters.clone().any(|reg| reads.contains(reg))
        })
}

struct ApplicableCall<'a> {
    generic: &'a RootSlot,
    methods: &'a [RootSlot],
    arguments: &'a RootSlots,
}

/// Arguments of EFFECTIVE_METHOD_TEMPLATE. The same frame roots them during
/// computation of the effective form, its compilation, and the eventual call.
struct EffectiveMethodCall(RootFrame);

impl EffectiveMethodCall {
    fn from_applicable(
        runtime: &Runtime,
        combinations: &MethodCombinations,
        call: &ApplicableCall<'_>,
        at: usize,
    ) -> Result<Self, Fault> {
        let frame = runtime.root_frame(5);
        frame.set(1, call.generic.get());
        frame.set(3, combinations.check_next_arguments);
        build_methods_list(runtime, &frame, 2, call.methods, at)?;
        build_arguments_list(
            runtime,
            &frame,
            0,
            call.arguments,
            0..call.arguments.len(),
            at,
        )?;
        Ok(Self(frame))
    }

    fn compile(
        self,
        runtime: &Runtime,
        combinations: &MethodCombinations,
        form: &Root<'_>,
        at: usize,
    ) -> Result<(Word, RootFrame), Fault> {
        let (function, _) = compile_effective_method(runtime, combinations, form, at)?;
        self.0.set(4, function.slot.get());
        Ok((function.slot.get(), self.0))
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal short method-combination helper passes call metadata directly"
)]
fn prepare_short_combination_call(
    runtime: &Runtime,
    combinations: &MethodCombinations,
    call: ApplicableCall<'_>,
    name: RootSlot,
    operator: RootSlot,
    identity: bool,
    most_specific_last: bool,
    at: usize,
) -> Result<(Word, RootFrame), Fault> {
    let mut around: Vec<usize> = Vec::new();
    let mut primary: Vec<usize> = Vec::new();
    for (index, method) in call.methods.iter().enumerate() {
        let info = runtime
            .heap
            .method_info(method.get())
            .expect("applicable method");
        let Some(single) = single_qualifier(runtime, info.qualifiers) else {
            return Err(method_error(
                at,
                "the method does not have exactly one qualifier",
            ));
        };
        if single == combinations.auxiliary[0].get() {
            around.push(index);
        } else if single == name.get() {
            primary.push(index);
        } else {
            return Err(method_error(
                at,
                "the method has an unsupported qualifier for this method combination",
            ));
        }
    }
    if primary.is_empty() {
        return Err(method_error(at, "no applicable primary method"));
    }
    if most_specific_last {
        primary.reverse();
    }

    let invocation = EffectiveMethodCall::from_applicable(runtime, combinations, &call, at)?;
    let mut calls = Vec::with_capacity(primary.len());
    for &index in &primary {
        calls.push(call_method_form(runtime, &call.methods[index], at)?);
    }
    let primary_form = if calls.len() == 1 && identity {
        calls.pop().expect("one primary method")
    } else {
        let operator = runtime.root(operator.get());
        call_form(runtime, &operator, &calls, at)?
    };
    let effective_method_form = if around.is_empty() {
        primary_form
    } else {
        let mut next = Vec::with_capacity(around.len());
        for &index in &around[1..] {
            next.push(runtime.root(call.methods[index].get()));
        }
        next.push(make_method_form(runtime, &primary_form, at)?);
        let next = generic::list(runtime, &next, at)?;
        let first = runtime.root(call.methods[around[0]].get());
        let head = cl_symbol(runtime, "CALL-METHOD", at)?;
        generic::list(runtime, &[head, first, next], at)?
    };
    invocation.compile(runtime, combinations, &effective_method_form, at)
}

fn prepare_long_combination_call(
    runtime: &Runtime,
    combinations: &MethodCombinations,
    call: ApplicableCall<'_>,
    bind_options: RootSlot,
    options: RootSlot,
    at: usize,
) -> Result<(Word, RootFrame), Fault> {
    let bind_options = runtime.root(bind_options.get());
    let options = runtime.root(options.get());
    let bound_definition = runtime
        .call(&bind_options, &[&options])?
        .into_iter()
        .next()
        .unwrap_or_else(|| runtime.nil());
    let Some((group_specifiers, definition_tail)) =
        runtime.heap.cons_fields(bound_definition.slot.get())
    else {
        return Err(method_error(
            at,
            "invalid long method combination definition",
        ));
    };
    let Some((compute_form, Word::NIL)) = runtime.heap.cons_fields(definition_tail) else {
        return Err(method_error(
            at,
            "invalid long method combination definition",
        ));
    };
    let group_specifiers = runtime.root_slot(group_specifiers);
    let compute_form = runtime.root_slot(compute_form);

    let groups = group_methods(
        runtime,
        combinations,
        group_specifiers.get(),
        call.methods,
        at,
    )?;
    let invocation = EffectiveMethodCall::from_applicable(runtime, combinations, &call, at)?;
    let method_groups = groups_to_list(runtime, &groups, call.methods, at)?;
    let compute_form = runtime.root(compute_form.get());
    let generic = runtime.root(call.generic.get());
    let values = runtime.call(&compute_form, &[&method_groups, &generic])?;
    let effective_method_form = values.into_iter().next().unwrap_or_else(|| runtime.nil());
    invocation.compile(runtime, combinations, &effective_method_form, at)
}

struct MethodGroup {
    patterns: RootSlot,
    most_specific_last: bool,
    required: bool,
    members: Vec<usize>,
}

fn group_methods(
    runtime: &Runtime,
    combinations: &MethodCombinations,
    descriptors: Word,
    methods: &[RootSlot],
    at: usize,
) -> Result<Vec<MethodGroup>, Fault> {
    let mut specs = Vec::new();
    for descriptor in proper_list(runtime, descriptors, at)? {
        // The option parser returns (name patterns order required). Only its prefix is used.
        let field = |list| {
            runtime
                .heap
                .cons_fields(list)
                .ok_or_else(|| method_error(at, "invalid method group specifier"))
        };
        let (_, rest) = field(descriptor)?;
        let (patterns, rest) = field(rest)?;
        let (order, rest) = field(rest)?;
        let (required, _) = field(rest)?;
        specs.push(MethodGroup {
            patterns: runtime.root_slot(patterns),
            most_specific_last: combinations.most_specific_last(
                order,
                at,
                "invalid method group order",
            )?,
            required: required != Word::NIL,
            members: Vec::new(),
        });
    }
    // A qualifier predicate may call Lisp. Snapshot every method's qualifiers first.
    let qualifiers: Vec<RootSlot> = methods
        .iter()
        .map(|method| {
            runtime.root_slot(
                runtime
                    .heap
                    .method_info(method.get())
                    .expect("applicable method")
                    .qualifiers,
            )
        })
        .collect();
    'methods: for (index, qualifiers) in qualifiers.iter().enumerate() {
        for spec in &mut specs {
            if group_matches(runtime, &spec.patterns, qualifiers, at)? {
                spec.members.push(index);
                continue 'methods;
            }
        }
        return Err(method_error(
            at,
            "an applicable method does not belong to any method group",
        ));
    }
    for spec in &mut specs {
        if spec.required && spec.members.is_empty() {
            return Err(method_error(
                at,
                "a required method group has no applicable methods",
            ));
        }
        if spec.most_specific_last {
            spec.members.reverse();
        }
    }

    Ok(specs)
}

fn groups_to_list<'rt>(
    runtime: &'rt Runtime,
    groups: &[MethodGroup],
    methods: &[RootSlot],
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let frame = runtime.root_frame(groups.len() + 1);
    let tail = groups.len();
    for (slot, group) in groups.iter().enumerate() {
        for &index in group.members.iter().rev() {
            push(runtime, &frame, slot, || methods[index].get(), at)?;
        }
    }
    for slot in (0..tail).rev() {
        push(runtime, &frame, tail, || frame.get(slot), at)?;
    }
    Ok(runtime.root(frame.get(tail)))
}

fn group_matches(
    runtime: &Runtime,
    patterns: &RootSlot,
    qualifiers: &RootSlot,
    at: usize,
) -> Result<bool, Fault> {
    let mut cursor = runtime.root(patterns.get());
    loop {
        let Some((pattern, rest)) = runtime.cons_fields(&cursor) else {
            break;
        };
        let word = pattern.slot.get();
        if word == Word::NIL {
            if qualifiers.get() == Word::NIL {
                return Ok(true);
            }
        } else if runtime.heap.is_symbol(word) {
            if is_star(runtime, word, at)? {
                return Ok(true);
            }
            let qualifiers = runtime.root(qualifiers.get());
            let results = runtime.call(&pattern, &[&qualifiers])?;
            if results.first().is_some_and(|value| !value.is_nil()) {
                return Ok(true);
            }
        } else if runtime.heap.cons_fields(word).is_some() {
            let qualifiers = runtime.root(qualifiers.get());
            if qualifier_pattern_matches(runtime, &pattern, &qualifiers, at)? {
                return Ok(true);
            }
        } else {
            return Err(method_error(at, "invalid qualifier pattern"));
        }
        cursor = rest;
    }
    Ok(false)
}

fn qualifier_pattern_matches(
    runtime: &Runtime,
    pattern: &Root<'_>,
    qualifiers: &Root<'_>,
    at: usize,
) -> Result<bool, Fault> {
    let mut pattern = pattern.clone();
    let mut qualifiers = qualifiers.clone();
    loop {
        let Some((element, rest)) = runtime.cons_fields(&pattern) else {
            return Ok(false);
        };
        let Some((qualifier, remaining)) = runtime.cons_fields(&qualifiers) else {
            return Ok(false);
        };
        if !is_star(runtime, element.slot.get(), at)?
            && !super::equality::equal(&runtime.heap, element.slot.get(), qualifier.slot.get())
        {
            return Ok(false);
        }
        let rest_word = rest.slot.get();
        let remaining_word = remaining.slot.get();
        if rest_word == Word::NIL {
            return Ok(remaining_word == Word::NIL);
        }
        if runtime.heap.cons_fields(rest_word).is_none() {
            return is_star(runtime, rest_word, at);
        }
        if remaining_word == Word::NIL {
            return Ok(false);
        }
        pattern = rest;
        qualifiers = remaining;
    }
}

fn is_star(runtime: &Runtime, word: Word, at: usize) -> Result<bool, Fault> {
    let star = runtime.intern_common_lisp("*", at)?;
    Ok(word == star)
}

fn single_qualifier(runtime: &Runtime, qualifiers: Word) -> Option<Word> {
    match runtime.heap.cons_fields(qualifiers) {
        Some((name, Word::NIL)) => Some(name),
        _ => None,
    }
}

fn standard_group(
    runtime: &Runtime,
    combination: &MethodCombinations,
    qualifiers: Word,
    at: usize,
) -> Result<usize, Fault> {
    let auxiliary = &combination.auxiliary;
    let group = match single_qualifier(runtime, qualifiers) {
        None if qualifiers == Word::NIL => 2,
        Some(name) if name == auxiliary[0].get() => 0,
        Some(name) if name == auxiliary[1].get() => 1,
        Some(name) if name == auxiliary[2].get() => 3,
        _ => {
            return Err(method_error(
                at,
                "invalid qualifier for standard method combination",
            ));
        }
    };
    Ok(group)
}

fn simple_builtin_group(
    runtime: &Runtime,
    combination: &MethodCombinations,
    operator: Word,
    qualifiers: Word,
    at: usize,
) -> Result<usize, Fault> {
    match single_qualifier(runtime, qualifiers) {
        Some(name) if name == combination.auxiliary[0].get() => Ok(0),
        Some(name) if name == operator => Ok(2),
        _ => Err(method_error(
            at,
            "invalid qualifier for simple method combination",
        )),
    }
}

fn push(
    runtime: &Runtime,
    frame: &RootSlots,
    slot: usize,
    value: impl FnOnce() -> Word,
    at: usize,
) -> Result<(), EvalError> {
    let list = runtime
        .heap
        .cons(|| (value(), frame.get(slot)))
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    frame.set(slot, list);
    Ok(())
}

fn build_methods_list(
    runtime: &Runtime,
    frame: &RootFrame,
    slot: usize,
    methods: &[RootSlot],
    at: usize,
) -> Result<(), Fault> {
    frame.set(slot, Word::NIL);
    for method in methods.iter().rev() {
        push(runtime, frame, slot, || method.get(), at)?;
    }
    Ok(())
}

fn build_arguments_list(
    runtime: &Runtime,
    frame: &RootFrame,
    slot: usize,
    arguments: &RootSlots,
    range: Range<usize>,
    at: usize,
) -> Result<(), Fault> {
    frame.set(slot, Word::NIL);
    for index in range.rev() {
        push(runtime, frame, slot, || arguments.get(index), at)?;
    }
    Ok(())
}

fn cl_symbol<'rt>(runtime: &'rt Runtime, spelling: &str, at: usize) -> Result<Root<'rt>, Fault> {
    let word = runtime
        .intern_common_lisp(spelling, at)
        .map_err(Fault::from)?;
    Ok(runtime.root(word))
}

fn call_form<'rt>(
    runtime: &'rt Runtime,
    head: &Root<'rt>,
    arguments: &[Root<'rt>],
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let mut items = Vec::with_capacity(arguments.len() + 1);
    items.push(head.clone());
    items.extend(arguments.iter().cloned());
    generic::list(runtime, &items, at)
}

fn call_method_form<'rt>(
    runtime: &'rt Runtime,
    method: &RootSlot,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let head = cl_symbol(runtime, "CALL-METHOD", at)?;
    let method = runtime.root(method.get());
    generic::list(runtime, &[head, method], at)
}

fn make_method_form<'rt>(
    runtime: &'rt Runtime,
    form: &Root<'rt>,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let head = cl_symbol(runtime, "MAKE-METHOD", at)?;
    let form = form.clone();
    generic::list(runtime, &[head, form], at)
}

// CALL-METHOD and MAKE-METHOD are lexical macros in the effective method.
// Let the compiler expand them where they occur, including in other macros' output.
fn effective_macro<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
    expand: fn(&'rt Runtime, &MethodCombinations, &Root<'rt>, usize) -> Result<Root<'rt>, Fault>,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [whole, _environment] = args else {
        return Err(method_error(
            at,
            "expected a macro form and its environment",
        ));
    };
    let (_, operands) = runtime
        .cons_fields(whole)
        .ok_or_else(|| method_error(at, "expected an effective method macro form"))?;
    let combinations = runtime
        .method_combinations
        .get()
        .expect("initialized method combinations");
    Ok(vec![expand(runtime, combinations, &operands, at)?])
}

fn expand_call_method<'rt>(
    runtime: &'rt Runtime,
    combinations: &MethodCombinations,
    arguments: &Root<'rt>,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let arguments = proper_list(runtime, arguments.slot.get(), at)?;
    let arguments: Vec<Root<'_>> = arguments
        .into_iter()
        .map(|word| runtime.root(word))
        .collect();
    let make_method = cl_symbol(runtime, "MAKE-METHOD", at)?;
    let (method, next, extra) = match arguments.as_slice() {
        [method] => (method.clone(), runtime.nil(), &[][..]),
        [method, next, extra @ ..] => (method.clone(), next.clone(), extra),
        _ => return Err(method_error(at, "CALL-METHOD expects a method")),
    };
    let method = match runtime.cons_fields(&method) {
        Some((head, tail)) if head.same_identity(&make_method) => {
            expand_make_method(runtime, combinations, &tail, at)?
        }
        _ => method,
    };
    let next = if next.is_nil() {
        runtime.nil()
    } else {
        let entries = proper_list(runtime, next.slot.get(), at)
            .map_err(|_| method_error(at, "CALL-METHOD next methods must be a proper list"))?;
        let entries: Vec<Root<'_>> = entries.into_iter().map(|word| runtime.root(word)).collect();
        let mut values = Vec::with_capacity(entries.len());
        for entry in entries {
            match runtime.cons_fields(&entry) {
                Some((head, tail)) if head.same_identity(&make_method) => {
                    values.push(expand_make_method(runtime, combinations, &tail, at)?);
                }
                Some(_) => {
                    return Err(method_error(at, "invalid CALL-METHOD next method"));
                }
                None => values.push(entry),
            }
        }
        let head = cl_symbol(runtime, "LIST", at)?;
        call_form(runtime, &head, &values, at)?
    };
    let head = cl_symbol(runtime, "%ZK-CALL-METHOD", at)?;
    let em = cl_symbol(runtime, "%%ZK-EM-ARGUMENTS", at)?;
    let generic = cl_symbol(runtime, "%%ZK-GENERIC", at)?;
    let methods = cl_symbol(runtime, "%%ZK-METHODS", at)?;
    let check = cl_symbol(runtime, "%%ZK-CHECK", at)?;
    let mut operands = vec![head, method, next, em, generic, methods, check];
    operands.extend(extra.iter().cloned());
    generic::list(runtime, &operands, at)
}

fn expand_make_method<'rt>(
    runtime: &'rt Runtime,
    combinations: &MethodCombinations,
    arguments: &Root<'rt>,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let arguments = proper_list(runtime, arguments.slot.get(), at)?;
    let [body] = arguments.as_slice() else {
        return Err(method_error(at, "MAKE-METHOD expects one form"));
    };
    let body = runtime.root(*body);
    let template = runtime.root(combinations.make_method_template.get());
    let placeholder = runtime.root(
        runtime
            .intern_common_lisp("%%ZK-TEMPLATE-BODY", at)
            .map_err(Fault::from)?,
    );
    substitute(runtime, &template, &placeholder, &body, at)
}

fn substitute<'rt>(
    runtime: &'rt Runtime,
    tree: &Root<'rt>,
    placeholder: &Root<'rt>,
    value: &Root<'rt>,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    if tree.same_identity(placeholder) {
        return Ok(value.clone());
    }
    let Some((head, tail)) = runtime.cons_fields(tree) else {
        return Ok(tree.clone());
    };
    let head = substitute(runtime, &head, placeholder, value, at)?;
    let tail = substitute(runtime, &tail, placeholder, value, at)?;
    runtime
        .cons(&head, &tail)
        .map_err(|error| Fault::from(EvalError::out_of_memory(at, error)))
}

pub(super) fn compile_effective_method<'rt>(
    runtime: &'rt Runtime,
    combinations: &MethodCombinations,
    form: &Root<'rt>,
    at: usize,
) -> Result<(Root<'rt>, bool), Fault> {
    let template = runtime.root(combinations.effective_method_template.get());
    let placeholder = runtime.root(
        runtime
            .intern_common_lisp("%%ZK-TEMPLATE-BODY", at)
            .map_err(Fault::from)?,
    );
    let lambda = substitute(runtime, &template, &placeholder, form, at)?;
    let lambda = form_value::from_value(runtime, &lambda, at).map_err(Fault::from)?;
    let FormKind::List(items, None) = &lambda.kind else {
        return Err(method_error(at, "invalid effective method form"));
    };
    let (code, _) = compile::compile_lambda_with(
        runtime,
        &items[1],
        &items[2..],
        at,
        None,
        &combinations.effective_method_macros,
    )
    .map_err(Fault::from)?;
    // Register 2 is the original ordered set passed to a CALL-METHOD
    // continuation. Inspect the compiled body so lexical macroexpansion and
    // captures in nested functions are accounted for. Side-table users take
    // the conservative path.
    let uses_methods = reads_parameters(&code.instructions, &code.handlers, &code.restarts, 2..3);
    let function = code.instantiate(runtime).map_err(Fault::from)?;
    Ok((runtime.root(function), uses_methods))
}

fn method_function<'rt>(
    runtime: &'rt Runtime,
    arguments: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [method] = arguments else {
        return Err(method_error(at, "%ZK-METHOD-FUNCTION expects a method"));
    };
    let info = runtime.heap.method_info(method.slot.get()).ok_or_else(|| {
        Fault::type_error(runtime, 
            method.slot.get(),
            "METHOD",
            EvalError::new(at, "expected a method"),
        )
    })?;
    Ok(vec![runtime.root(info.function)])
}

fn set_documentation<'rt>(
    runtime: &'rt Runtime,
    arguments: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [object, kind, new_doc] = arguments else {
        return Err(method_error(at, "invalid documentation assignment"));
    };
    store_documentation(
        runtime,
        object.slot.get(),
        kind.slot.get(),
        new_doc.slot.get(),
        at,
    )?;
    Ok(vec![new_doc.clone()])
}

pub(super) fn store_documentation(
    runtime: &Runtime,
    object: Word,
    kind: Word,
    doc: Word,
    at: usize,
) -> Result<(), Fault> {
    if doc != Word::NIL && !runtime.heap.is_string(doc) {
        return Err(Fault::type_error(runtime, 
            doc,
            "STRING",
            EvalError::new(at, "expected a documentation string or NIL"),
        ));
    }
    if kind == Word::T
        && let Some(package) = crate::package::Package::from_word(object)
    {
        runtime
            .packages
            .borrow_mut()
            .set_documentation(package, doc)
            .map_err(|error| Fault::package_error(runtime, object, EvalError::new(at, error.to_string())))?;
    } else if kind == Word::T || runtime.find_common_lisp("FUNCTION") == Some(kind) {
        let target = if super::is_function(runtime, object) || kind == Word::T {
            Some(object)
        } else {
            FunctionName::parse(runtime, object, at)?.get(runtime)
        };
        if let Some(target) = target {
            if runtime.heap.generic_info(target).is_some() {
                runtime.heap.set_generic_documentation(target, doc);
            } else if runtime.heap.method_info(target).is_some() {
                runtime.heap.set_method_documentation(target, doc);
            } else {
                runtime.heap.set_function_documentation(target, doc);
            }
        }
    } else if runtime.find_common_lisp("VARIABLE") == Some(kind) {
        if !runtime.heap.is_symbol(object) {
            return Err(Fault::type_error(runtime, 
                object,
                "SYMBOL",
                EvalError::new(at, "expected a variable name"),
            ));
        }
        runtime.heap.set_variable_documentation(object, doc);
    } else if runtime.find_named_symbol(&Name::keyword("METHOD-COMBINATION")) == Some(kind) {
        runtime
            .method_combinations
            .get()
            .expect("initialized runtime")
            .set_definition_documentation(runtime, object, doc);
    } else if runtime.find_common_lisp("SETF") == Some(kind) {
        runtime.set_setf_documentation(object, doc);
    }
    Ok(())
}

fn check_next_arguments<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let changed = proper_list(runtime, args[2].slot.get(), at)?;
    let count = changed.len();
    let frame = runtime.root_frame(count);
    for (index, value) in changed.into_iter().enumerate() {
        frame.set(index, value);
    }
    let applicable =
        generic::applicable_methods(runtime, args[0].slot.get(), &frame, 0..count, at)?;
    if applicable != proper_list(runtime, args[1].slot.get(), at)? {
        return Err(method_error(
            at,
            "CALL-NEXT-METHOD arguments changed the ordered set of applicable methods",
        ));
    }
    let applicable: Vec<_> = applicable
        .into_iter()
        .map(|word| runtime.root_slot(word))
        .collect();
    generic::validate_arguments(
        runtime,
        args[0].slot.get(),
        &applicable,
        &frame,
        0..count,
        at,
    )?;
    Ok(Vec::new())
}

fn default_no_applicable_method<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    Err(Fault::type_error(
        runtime,
        args[2].slot.get(),
        "NIL",
        EvalError::new(at, "no applicable method"),
    ))
}

fn default_no_next_method<'rt>(
    _runtime: &'rt Runtime,
    _args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    Err(method_error(at, "no next method"))
}

fn method_error(at: usize, message: &'static str) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}
