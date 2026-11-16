use std::ops::Range;
use std::rc::Rc;

use crate::heap::{ListSpine, RootFrame, RootSlot, RootSlots, StreamElement, StreamKind, Word};
use crate::symbol::{Name, Package};
use crate::vm::{self, classes};
use crate::{EvalError, Root, Runtime};

mod expected_type;
use expected_type::ExpectedType;

pub(crate) use zkcl_bytecode::{Class, Test};

const MAX_BUILTIN_SLOTS: usize = 4; // SIMPLE-TYPE-ERROR combines two pairs.

fn direct_slot_names(class: Class) -> &'static [&'static str] {
    match class {
        Class::SimpleCondition => &["FORMAT-CONTROL", "FORMAT-ARGUMENTS"],
        Class::CellError => &["NAME"],
        Class::TypeError => &["DATUM", "EXPECTED-TYPE"],
        Class::UnboundSlot => &["INSTANCE"],
        Class::ArithmeticError => &["OPERATION", "OPERANDS"],
        Class::StreamError => &["STREAM"],
        Class::PackageError => &["PACKAGE"],
        Class::FileError => &["PATHNAME"],
        Class::PrintNotReadable => &["OBJECT"],
        _ => &[],
    }
}

fn inline_slot_names(class: Class) -> ([&'static str; MAX_BUILTIN_SLOTS], usize) {
    fn collect(class: Class, names: &mut [&'static str], count: &mut usize) {
        for &parent in class.direct_superclasses() {
            collect(parent, names, count);
        }
        for &name in direct_slot_names(class) {
            names[*count] = name;
            *count += 1;
        }
    }
    let mut names = [""; MAX_BUILTIN_SLOTS];
    let mut count = 0;
    collect(class, &mut names, &mut count);
    (names, count)
}

fn default_slot_value(name: &str) -> Word {
    if name == "FORMAT-ARGUMENTS" {
        Word::NIL
    } else {
        Word::UNBOUND
    }
}

/// Direct definitions also supply inherited slots for user-defined conditions.
pub(crate) fn builtin_direct_slots(
    runtime: &Runtime,
    class: Class,
    at: usize,
) -> Result<Rc<[classes::DirectSlot]>, Fault> {
    if let Some(slots) = runtime
        .classes
        .borrow()
        .builtin_condition_direct_slots
        .get(&class)
    {
        return Ok(Rc::clone(slots));
    }
    let slots: Rc<[_]> = builtin_slot_declarations(runtime, direct_slot_names(class), at)?.into();
    runtime
        .classes
        .borrow_mut()
        .builtin_condition_direct_slots
        .insert(class, Rc::clone(&slots));
    Ok(slots)
}

/// Reflection, CLOS initialization and slot access must use identical effective
/// metaobjects. The compact native representation uses these slots' indexes.
pub(crate) fn builtin_effective_slots(
    runtime: &Runtime,
    class: Class,
    at: usize,
) -> Result<Rc<[crate::classes::EffectiveSlot]>, Fault> {
    if let Some(slots) = runtime
        .classes
        .borrow()
        .builtin_condition_effective_slots
        .get(&class)
    {
        return Ok(Rc::clone(slots));
    }
    let (names, count) = inline_slot_names(class);
    let slots: Rc<[_]> = builtin_slot_declarations(runtime, &names[..count], at)?
        .into_iter()
        .enumerate()
        .map(|(index, slot)| crate::classes::EffectiveSlot {
            name: slot.name,
            allocation: crate::classes::SlotAllocation::Instance(index),
            initargs: slot.initargs,
            initform: slot.initform,
            initform_source: slot.initform_source,
            metaobject: Default::default(),
            type_constraints: Vec::new(),
            documentation: None,
        })
        .collect();
    runtime
        .classes
        .borrow_mut()
        .builtin_condition_effective_slots
        .insert(class, Rc::clone(&slots));
    Ok(slots)
}

pub(crate) fn allocate_builtin_condition(
    runtime: &Runtime,
    class: Class,
    at: usize,
) -> Result<Word, Fault> {
    let (_, count) = inline_slot_names(class);
    runtime
        .heap
        .condition(class, count, |_| Word::UNBOUND)
        .map_err(|err| EvalError::out_of_memory(at, err).into())
}

pub(crate) fn builtin_prototype(runtime: &Runtime, class: Class, at: usize) -> Result<Word, Fault> {
    let (names, count) = inline_slot_names(class);
    runtime
        .heap
        .condition(class, count, |index| default_slot_value(names[index]))
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn builtin_slot_declarations(
    runtime: &Runtime,
    names: &[&str],
    at: usize,
) -> Result<Vec<classes::DirectSlot>, Fault> {
    names
        .iter()
        .map(|&name| {
            let (name_symbol, _) = runtime.intern_package(Package::COMMON_LISP, name, at)?;
            let name_symbol = Rc::new(runtime.root_slot(name_symbol));
            let initarg = Rc::new(runtime.root_slot(runtime.intern(&Name::keyword(name), at)?));
            let initform = (name == "FORMAT-ARGUMENTS").then(|| {
                Rc::new(runtime.root_slot(
                    vm::Primitive::named("LIST").expect("LIST primitive"),
                ))
            });
            Ok(classes::DirectSlot {
                name: name_symbol,
                initargs: vec![initarg],
                initform,
                initform_source: None,
                readers: None,
                writers: None,
                metaobject: std::cell::OnceCell::new(),
                allocation: classes::SlotKind::Instance,
                type_specifier: None,
                documentation: None,
            })
        })
        .collect()
}

pub(crate) fn named_word(runtime: &Runtime, word: Word, package: Package) -> Option<String> {
    (runtime.heap.symbol_home(word) == Some(package))
        .then(|| runtime.heap.symbol_name(word))
        .flatten()
}

pub(crate) fn instance_class(runtime: &Runtime, word: Word) -> Option<Word> {
    let class = runtime.heap.instance_class(word)?;
    let definition = classes::class_definition(runtime, class)?;
    (definition.kind == classes::RuntimeClassKind::Condition).then_some(class)
}

pub(crate) fn instance_name(runtime: &Runtime, word: Word) -> Option<Word> {
    let class = instance_class(runtime, word)?;
    runtime.heap.class_info(class).map(|info| info.name)
}

pub(crate) fn is_condition(runtime: &Runtime, word: Word) -> bool {
    runtime.heap.condition_class(word).is_some() || instance_class(runtime, word).is_some()
}

pub(crate) fn matches_builtin(runtime: &Runtime, value: Word, expected: Class) -> bool {
    if let Some(actual) = runtime.heap.condition_class(value) {
        return actual.is_a(expected);
    }
    runtime
        .heap
        .instance_class(value)
        .and_then(|class| classes::class_definition(runtime, class))
        .is_some_and(|definition| {
            definition.kind == classes::RuntimeClassKind::Condition
                && definition.precedence.iter().any(|parent| {
                    let info = runtime
                        .heap
                        .class_info(parent.get())
                        .expect("precedence class");
                    classes::builtin_condition(runtime, info.origin_name) == Some(expected)
                })
        })
}

pub(crate) fn make_builtin_condition(
    runtime: &Runtime,
    class: Class,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    if !args.len().is_multiple_of(2) {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "condition initargs must be keyword/value pairs"),
        ));
    }
    let (names, slot_count) = inline_slot_names(class);
    let mut initarg_registers = [None; MAX_BUILTIN_SLOTS];
    let mut allow_other_keys = None;
    let mut unknown_key = None;
    for index in args.step_by(2) {
        let argument = frame.get(index);
        if !runtime.heap.is_symbol(argument) {
            return Err(Fault::plain(
                Class::ProgramError,
                EvalError::new(at, "expected a condition initarg name"),
            ));
        }
        match named_word(runtime, argument, Package::KEYWORD).as_deref() {
            Some("ALLOW-OTHER-KEYS") => {
                allow_other_keys.get_or_insert(frame.get(index + 1) != Word::NIL);
            }
            Some(name)
                if let Some(slot) = names[..slot_count].iter().position(|key| *key == name) =>
            {
                initarg_registers[slot].get_or_insert(index + 1);
            }
            _ => {
                unknown_key.get_or_insert(argument);
            }
        }
    }
    if let Some(key) = unknown_key.filter(|_| allow_other_keys != Some(true)) {
        let name = runtime
            .heap
            .symbol_name(key)
            .expect("validated initarg name");
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, format!("unknown condition initarg {name}")),
        ));
    }
    runtime
        .heap
        .condition(class, slot_count, |slot| {
            initarg_registers[slot]
                .map(|index| frame.get(index))
                .unwrap_or_else(|| default_slot_value(names[slot]))
        })
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

pub(crate) fn from_format_control(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    class: Class,
    at: usize,
) -> Result<Word, Fault> {
    let datum = frame.get(args.start);
    if is_format_control(runtime, datum) {
        let temp = runtime.root_frame(1);
        for index in (args.start + 1..args.end).rev() {
            let pair = runtime
                .heap
                .cons(|| (frame.get(index), temp.get(0)))
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            temp.set(0, pair);
        }
        runtime
            .heap
            .condition(class, 2, |slot| [frame.get(args.start), temp.get(0)][slot])
            .map_err(|error| EvalError::out_of_memory(at, error).into())
    } else {
        Err(Fault::condition_designator(
            runtime,
            datum,
            EvalError::new(
                at,
                "expected a condition, condition type, or format control",
            ),
        ))
    }
}

pub(crate) fn is_format_control(runtime: &Runtime, word: Word) -> bool {
    runtime.heap.is_string(word) || vm::is_function(runtime, word)
}

pub(crate) fn to_eval_error(runtime: &Runtime, condition: Word, at: usize) -> EvalError {
    let (class, rendered, fallback) = if let Some(class) = instance_class(runtime, condition) {
        let name = runtime
            .heap
            .class_info(class)
            .filter(|info| info.name != Word::NIL)
            .and_then(|info| runtime.heap.symbol_name(info.name))
            .unwrap_or_else(|| "CONDITION".into());
        let category = classes::class_definition(runtime, class)
            .and_then(|definition| {
                definition.precedence.iter().find_map(|parent| {
                    let info = runtime.heap.class_info(parent.get())?;
                    classes::builtin_condition(runtime, info.origin_name)
                })
            })
            .unwrap_or(Class::Condition);
        (category, report(runtime, condition, at), Some(name))
    } else {
        let class = runtime
            .heap
            .condition_class(condition)
            .expect("built-in condition");
        let (rendered, fallback) = match inline_simple_format_slots(runtime, condition, class) {
            Some((control, args)) => {
                let fallback = runtime.heap.string_contents(control);
                (simple_report(runtime, control, args, at), fallback)
            }
            None => (Ok(None), None),
        };
        (class, rendered, fallback)
    };
    let message = match rendered {
        // Formatting the condition is best effort. An execution stop still
        // governs the enclosing invocation, including its cleanup policy.
        Err(error) if error.execution_stop().is_some() => return error,
        rendered => rendered.ok().flatten().or(fallback),
    };
    EvalError::new(at, message.unwrap_or_else(|| class.name().into())).with_condition(class)
}

pub(crate) fn builtin_slot(
    runtime: &Runtime,
    value: Word,
    expected: Class,
    second_slot: bool,
    at: usize,
) -> Result<Word, Fault> {
    let (names, count) = inline_slot_names(expected);
    let name = names[..count][usize::from(second_slot)];
    let slot_value = match runtime.heap.condition_class(value) {
        Some(class) if class.is_a(expected) => inline_slot(runtime, value, class, name),
        None if matches_builtin(runtime, value, expected) => {
            let slot = runtime
                .find_named_symbol(&Name::common_lisp(name))
                .expect("registered condition slot");
            vm::instances::bound_slot(runtime, value, slot).unwrap_or(Word::UNBOUND)
        }
        _ => {
            return Err(Fault::type_error(
                runtime,
                value,
                expected.name(),
                EvalError::new(at, format!("expected a {}", expected.name())),
            ));
        }
    };
    if slot_value == Word::UNBOUND {
        return Err(EvalError::new(at, "unbound condition slot").into());
    }
    Ok(slot_value)
}

fn inline_slot(runtime: &Runtime, condition: Word, class: Class, name: &str) -> Word {
    let (names, count) = inline_slot_names(class);
    let index = names[..count]
        .iter()
        .position(|&slot| slot == name)
        .expect("slot inherited from the accessor's class");
    runtime
        .heap
        .condition_slot(condition, index)
        .expect("inline condition matches its class layout")
}

fn inline_simple_format_slots(
    runtime: &Runtime,
    condition: Word,
    class: Class,
) -> Option<(Word, Word)> {
    class.is_a(Class::SimpleCondition).then(|| {
        (
            inline_slot(runtime, condition, class, "FORMAT-CONTROL"),
            inline_slot(runtime, condition, class, "FORMAT-ARGUMENTS"),
        )
    })
}

fn definition_report(runtime: &Runtime, condition: Word) -> Option<Rc<RootSlot>> {
    let class = instance_class(runtime, condition)?;
    classes::class_definition(runtime, class)?.report.clone()
}

/// Constant reports are safe to use while traversing a composite value
/// without a GC safepoint.
pub(crate) fn constant_report(runtime: &Runtime, condition: Word) -> Option<String> {
    let report = definition_report(runtime, condition)?;
    runtime.heap.string_contents(report.get())
}

pub(crate) fn report(
    runtime: &Runtime,
    condition: Word,
    at: usize,
) -> Result<Option<String>, EvalError> {
    report_to(
        &vm::printer::PrintContext::host(runtime),
        condition,
        None,
        at,
    )
}

pub(crate) fn report_to(
    runtime: &vm::printer::PrintContext<'_>,
    condition: Word,
    destination: Option<&RootSlot>,
    at: usize,
) -> Result<Option<String>, EvalError> {
    if let Some(class) = runtime.heap.condition_class(condition) {
        return match inline_simple_format_slots(runtime, condition, class) {
            Some((control, args)) => simple_report_to(runtime, control, args, destination, at),
            None => Ok(None),
        };
    }
    let (report, condition) = if let Some(report) = runtime.heap.restart_report(condition)
        && report != Word::NIL
    {
        // CERROR keeps its format control and arguments for each report request.
        if let Some((control, arguments)) = runtime.heap.cons_fields(report) {
            return simple_report_to(runtime, control, arguments, destination, at);
        }
        (report, None)
    } else if destination.is_none()
        && let Some(report) = definition_report(runtime, condition)
    {
        (report.get(), Some(runtime.root(condition)))
    } else {
        return inherited_simple_report(runtime, condition, destination, at);
    };
    if let Some(text) = runtime.heap.string_contents(report) {
        return Ok(Some(text));
    }
    let function = runtime.root(report);
    let stream = if let Some(destination) = destination {
        destination.get()
    } else {
        runtime
            .heap
            .stream(
                StreamKind::StringOutput,
                StreamElement::Character,
                0,
                0,
                || Word::NIL,
            )
            .map_err(|error| EvalError::out_of_memory(at, error))?
    };
    let stream = runtime.root(stream);
    let escape = runtime.root_slot(runtime.intern(&Name::common_lisp("*PRINT-ESCAPE*"), at)?);
    let readably = runtime.intern(&Name::common_lisp("*PRINT-READABLY*"), at)?;
    let call = || match &condition {
        Some(condition) => runtime.invoke(&function, &[condition, &stream], at),
        None => runtime.invoke(&function, &[&stream], at),
    };
    if destination.is_some() {
        call().map_err(|fault| fault.host)?;
    } else {
        vm::with_special_bindings(
            runtime,
            [(escape.get(), Word::NIL), (readably, Word::NIL)],
            call,
        )
        .map_err(|fault| fault.host)?;
    }
    if destination.is_some() {
        return Ok(Some(String::new()));
    }
    let getter = runtime
        .root(vm::Primitive::named("GET-OUTPUT-STREAM-STRING").expect("output stream primitive"));
    let result = runtime.call(&getter, &[&stream])?;
    Ok(runtime.string(&result[0]))
}

fn inherited_simple_report(
    runtime: &vm::printer::PrintContext<'_>,
    condition: Word,
    destination: Option<&RootSlot>,
    at: usize,
) -> Result<Option<String>, EvalError> {
    if instance_class(runtime, condition).is_none()
        || !matches_builtin(runtime, condition, Class::SimpleCondition)
    {
        return Ok(None);
    }
    let Ok(control) = builtin_slot(runtime, condition, Class::SimpleCondition, false, at) else {
        return Ok(None);
    };
    let arguments = builtin_slot(runtime, condition, Class::SimpleCondition, true, at)
        .map_err(|fault| fault.host)?;
    simple_report_to(runtime, control, arguments, destination, at)
}

fn simple_report(
    runtime: &Runtime,
    control: Word,
    arguments: Word,
    at: usize,
) -> Result<Option<String>, EvalError> {
    simple_report_to(
        &vm::printer::PrintContext::host(runtime),
        control,
        arguments,
        None,
        at,
    )
}

fn simple_report_to(
    runtime: &vm::printer::PrintContext<'_>,
    control: Word,
    arguments: Word,
    destination: Option<&RootSlot>,
    at: usize,
) -> Result<Option<String>, EvalError> {
    if let Some(text) = runtime.heap.string_contents(control) {
        if !text.contains('~') {
            return Ok(Some(text));
        }
    } else if !vm::is_function(runtime, control) {
        return Ok(None);
    }
    let mut args = vec![
        runtime.root(destination.map_or(Word::NIL, RootSlot::get)),
        runtime.root(control),
    ];
    let mut spine = ListSpine::new(arguments);
    let invalid = || EvalError::new(at, "condition format arguments must be a proper list");
    while let Some((value, _)) = spine.next(&runtime.heap).map_err(|_| invalid())? {
        args.push(runtime.root(value));
    }
    if spine.tail() != Word::NIL {
        return Err(invalid());
    }
    let format = runtime.root(vm::Primitive::named("FORMAT").expect("FORMAT primitive"));
    let args = args.iter().collect::<Vec<_>>();
    let result = runtime
        .invoke(&format, &args, at)
        .map_err(|fault| fault.host)?;
    Ok(if destination.is_some() {
        Some(String::new())
    } else {
        runtime.string(&result[0])
    })
}

/// A native VM failure with a host diagnostic and the data for its Lisp condition.
#[derive(thiserror::Error)]
#[error("{host}")]
pub struct Fault {
    #[source]
    pub(crate) host: EvalError,
    condition_data: ConditionData,
    already_dispatched: bool,
}

impl std::fmt::Debug for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Formatting a Rust diagnostic must not invoke the Lisp printer on rooted objects.
        f.debug_struct("Fault")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

enum ConditionData {
    ClassOnly(Class),
    ExistingCondition(RootSlot),
    SingleSlotCondition(Class, RootSlot),
    UnboundSlot { slot_values: Box<[RootSlot; 2]> },
    Arithmetic(Box<ArithmeticErrorSlots>),
    TypeError(Box<TypeErrorSlots>),
}

struct ArithmeticErrorSlots {
    class: Class,
    operation: RootSlot,
    operands: RootFrame,
}

struct TypeErrorSlots {
    datum: RootSlot,
    expected_type: ExpectedType,
}

impl From<EvalError> for Fault {
    fn from(host: EvalError) -> Self {
        let class = match host.kind() {
            crate::EvalErrorKind::LispCondition(class) => class,
            // A macro expansion error reported through EVAL is a program error,
            // not the simple-error used for ordinary runtime failures.
            crate::EvalErrorKind::MacroExpansion => Class::ProgramError,
            _ => Class::SimpleError,
        };
        Self::plain(class, host)
    }
}

impl Fault {
    /// The shared VM has already searched handlers and unwound this failure.
    /// Native ABI layers may carry it through Rust frames without signaling again.
    pub(crate) fn dispatched(host: EvalError) -> Self {
        let mut fault = Self::from(host);
        fault.already_dispatched = true;
        fault
    }

    pub(crate) fn already_dispatched(&self) -> bool {
        self.already_dispatched
    }

    pub(crate) fn arithmetic(
        runtime: &Runtime,
        class: Class,
        operation: Word,
        operands: impl ExactSizeIterator<Item = Word>,
        host: EvalError,
    ) -> Self {
        let operand_roots = runtime.root_frame(operands.len());
        for (index, operand) in operands.enumerate() {
            operand_roots.set(index, operand);
        }
        Self {
            host: host.with_condition(class),
            already_dispatched: false,
            condition_data: ConditionData::Arithmetic(Box::new(ArithmeticErrorSlots {
                class,
                operation: runtime.root_slot(operation),
                operands: operand_roots,
            })),
        }
    }

    pub fn plain(class: Class, host: EvalError) -> Self {
        Self {
            host: host.with_condition(class),
            already_dispatched: false,
            condition_data: ConditionData::ClassOnly(class),
        }
    }

    pub(crate) fn existing_condition(condition: &Root<'_>, host: EvalError) -> Self {
        Self {
            host,
            already_dispatched: false,
            condition_data: ConditionData::ExistingCondition(condition.slot.clone()),
        }
    }

    pub fn eval_error(&self) -> &EvalError {
        &self.host
    }

    pub fn into_eval_error(self) -> EvalError {
        self.host
    }

    pub(crate) fn cell_name(&self) -> Option<(Class, Word)> {
        match &self.condition_data {
            ConditionData::SingleSlotCondition(
                class @ (Class::UnboundVariable | Class::UndefinedFunction),
                name,
            ) => Some((*class, name.get())),
            _ => None,
        }
    }

    pub(crate) fn cell(runtime: &Runtime, class: Class, symbol: Word, host: EvalError) -> Self {
        Self::primary_slot(runtime, class, symbol, host)
    }

    pub(crate) fn unbound_slot(
        runtime: &Runtime,
        name: Word,
        instance: Word,
        host: EvalError,
    ) -> Self {
        Self {
            host: host.with_condition(Class::UnboundSlot),
            already_dispatched: false,
            condition_data: ConditionData::UnboundSlot {
                slot_values: Box::new([runtime.root_slot(name), runtime.root_slot(instance)]),
            },
        }
    }

    pub(crate) fn from_package_error(error: crate::package::PackageError, at: usize) -> Self {
        let package = error.package.word();
        let host = EvalError::from_error(at, error).with_condition(Class::PackageError);
        Self {
            host,
            already_dispatched: false,
            condition_data: ConditionData::SingleSlotCondition(
                Class::PackageError,
                RootSlot::immediate(package),
            ),
        }
    }

    pub(crate) fn package_error(runtime: &Runtime, package: Word, host: EvalError) -> Self {
        Self::primary_slot(runtime, Class::PackageError, package, host)
    }

    pub(crate) fn file_error(runtime: &Runtime, pathname: Word, host: EvalError) -> Self {
        Self::primary_slot(runtime, Class::FileError, pathname, host)
    }

    pub(crate) fn stream_error(
        runtime: &Runtime,
        class: Class,
        stream: Word,
        host: EvalError,
    ) -> Self {
        Self::primary_slot(runtime, class, stream, host)
    }

    pub(crate) fn print_not_readable(runtime: &Runtime, object: Word, host: EvalError) -> Self {
        Self::primary_slot(runtime, Class::PrintNotReadable, object, host)
    }

    fn primary_slot(runtime: &Runtime, class: Class, datum: Word, host: EvalError) -> Self {
        Self {
            host: host.with_condition(class),
            already_dispatched: false,
            condition_data: ConditionData::SingleSlotCondition(class, runtime.root_slot(datum)),
        }
    }

    pub(crate) fn type_error(
        runtime: &Runtime,
        datum: Word,
        expected: &'static str,
        host: EvalError,
    ) -> Self {
        Self::typed(runtime, datum, ExpectedType::Named(expected), host)
    }

    pub(crate) fn type_error_specifier(
        runtime: &Runtime,
        datum: Word,
        expected: Word,
        host: EvalError,
    ) -> Self {
        Self::typed(
            runtime,
            datum,
            ExpectedType::Specifier(runtime.root_slot(expected)),
            host,
        )
    }

    pub(crate) fn format_control(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(
            runtime,
            datum,
            ExpectedType::SymbolList(&["OR", "STRING", "FUNCTION"]),
            host,
        )
    }

    pub(crate) fn function_designator(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(
            runtime,
            datum,
            ExpectedType::SymbolList(&["OR", "FUNCTION", "SYMBOL"]),
            host,
        )
    }

    pub(crate) fn function_name(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(runtime, datum, ExpectedType::FunctionName, host)
    }

    pub(crate) fn compile_definition(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(runtime, datum, ExpectedType::CompileDefinition, host)
    }

    pub(crate) fn string_designator(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(
            runtime,
            datum,
            ExpectedType::SymbolList(&["OR", "STRING", "SYMBOL", "CHARACTER"]),
            host,
        )
    }

    pub(crate) fn character_designator(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(runtime, datum, ExpectedType::CharacterDesignator, host)
    }

    pub(crate) fn character_code(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::integer_range(
            runtime,
            datum,
            0,
            (crate::character::CODE_LIMIT - 1) as isize,
            host,
        )
    }

    pub(crate) fn package_designator(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(
            runtime,
            datum,
            ExpectedType::SymbolList(&["OR", "PACKAGE", "STRING", "SYMBOL", "CHARACTER"]),
            host,
        )
    }

    pub(crate) fn pathname_designator(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(
            runtime,
            datum,
            ExpectedType::SymbolList(&["OR", "PATHNAME", "STRING"]),
            host,
        )
    }

    pub(crate) fn optional_condition(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(
            runtime,
            datum,
            ExpectedType::SymbolList(&["OR", "NULL", "CONDITION"]),
            host,
        )
    }

    pub(crate) fn restart_designator(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(runtime, datum, ExpectedType::RestartDesignator, host)
    }

    pub(crate) fn condition_designator(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(
            runtime,
            datum,
            ExpectedType::SymbolList(&["OR", "CONDITION", "SYMBOL", "STRING", "FUNCTION"]),
            host,
        )
    }

    pub(crate) fn non_negative_integer(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(runtime, datum, ExpectedType::IntegerRange(0, None), host)
    }

    pub(crate) fn integer_range(
        runtime: &Runtime,
        datum: Word,
        minimum: isize,
        maximum: isize,
        host: EvalError,
    ) -> Self {
        Self::typed(
            runtime,
            datum,
            ExpectedType::IntegerRange(minimum, Some(maximum)),
            host,
        )
    }

    pub(crate) fn positive_integer(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(runtime, datum, ExpectedType::IntegerRange(1, None), host)
    }

    pub(crate) fn vector_satisfying(
        runtime: &Runtime,
        datum: Word,
        predicate: &'static str,
        host: EvalError,
    ) -> Self {
        Self::typed(
            runtime,
            datum,
            ExpectedType::VectorSatisfying(predicate),
            host,
        )
    }

    pub(crate) fn gensym_argument(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(
            runtime,
            datum,
            ExpectedType::NonNegativeIntegerOr("STRING"),
            host,
        )
    }

    pub(crate) fn print_limit(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(
            runtime,
            datum,
            ExpectedType::NonNegativeIntegerOr("NULL"),
            host,
        )
    }

    pub(crate) fn random_state_designator(
        runtime: &Runtime,
        datum: Word,
        host: EvalError,
    ) -> Self {
        Self::typed(
            runtime,
            datum,
            ExpectedType::SymbolList(&["OR", "RANDOM-STATE", "BOOLEAN"]),
            host,
        )
    }

    pub(crate) fn random_limit(runtime: &Runtime, datum: Word, host: EvalError) -> Self {
        Self::typed(runtime, datum, ExpectedType::RandomLimit, host)
    }

    fn typed(runtime: &Runtime, datum: Word, expected: ExpectedType, host: EvalError) -> Self {
        Self {
            host: host.with_condition(Class::TypeError),
            already_dispatched: false,
            condition_data: ConditionData::TypeError(Box::new(TypeErrorSlots {
                datum: runtime.root_slot(datum),
                expected_type: expected,
            })),
        }
    }

    pub(crate) fn make_condition(&self, runtime: &Runtime) -> Option<Word> {
        match &self.condition_data {
            ConditionData::ExistingCondition(condition) => Some(condition.get()),
            ConditionData::Arithmetic(slots) => {
                let mut tail = runtime.root_slot(Word::NIL);
                for index in (0..slots.operands.len()).rev() {
                    let value = runtime
                        .heap
                        .cons(|| (slots.operands.get(index), tail.get()))
                        .ok()?;
                    tail = runtime.root_slot(value);
                }
                runtime
                    .heap
                    .condition(slots.class, 2, |slot| {
                        [slots.operation.get(), tail.get()][slot]
                    })
                    .ok()
            }
            ConditionData::ClassOnly(Class::SimpleError) => {
                let text = runtime.root_slot(runtime.heap.string(self.host.message()).ok()?);
                runtime
                    .heap
                    .condition(Class::SimpleError, 2, |slot| [text.get(), Word::NIL][slot])
                    .ok()
            }
            ConditionData::ClassOnly(class) => {
                let (names, count) = inline_slot_names(*class);
                runtime
                    .heap
                    .condition(*class, count, |slot| default_slot_value(names[slot]))
                    .ok()
            }
            ConditionData::SingleSlotCondition(class, datum) => {
                runtime.heap.condition(*class, 1, |_| datum.get()).ok()
            }
            ConditionData::UnboundSlot { slot_values } => runtime
                .heap
                .condition(Class::UnboundSlot, 2, |index| slot_values[index].get())
                .ok(),
            ConditionData::TypeError(slots) => {
                let expected_type = slots
                    .expected_type
                    .to_type_specifier(runtime, self.host.offset())?;
                runtime
                    .heap
                    .condition(Class::TypeError, 2, |slot| {
                        [slots.datum.get(), expected_type.get()][slot]
                    })
                    .ok()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::Heap;

    #[test]
    fn extra_inline_condition_slots_survive_relocation() {
        let mut config = Heap::builder();
        config.always_defrag = true;
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        let condition = runtime
            .eval(
                "(make-condition 'simple-type-error
                   :datum (list 5) :expected-type (list 'integer 1 4))",
            )
            .unwrap();
        let condition = &condition[0];
        let before = condition.slot.get();
        runtime.collect();
        assert_ne!(condition.slot.get(), before);

        let inspect = runtime
            .eval("(lambda (c) (values (type-error-datum c) (type-error-expected-type c)))")
            .unwrap();
        let values = runtime.call(&inspect[0], &[condition]).unwrap();
        assert_eq!(runtime.repr(&values[0]).unwrap(), "(5)");
        assert_eq!(runtime.repr(&values[1]).unwrap(), "(INTEGER 1 4)");
    }

    #[test]
    fn condition_reports_leave_formatter_arguments_rooted() {
        let mut config = Heap::builder();
        config.always_defrag = true;
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        runtime
            .register_function("collect-during-condition-report", |runtime, _| {
                runtime.collect();
                Ok(Vec::new())
            })
            .unwrap();
        let values = runtime
            .eval(
                r#"(let ((condition
                          (make-condition 'simple-type-error
                            :format-control (lambda (stream value)
                                              (collect-during-condition-report)
                                              (format stream "bad ~D" value))
                            :format-arguments '(5))))
                      (values (format nil "~A ~S" condition (list 8 9))
                              (format nil "~{~A ~S~}" (list condition (list 8 9)))
                              (with-output-to-string (stream)
                                (assert (eq condition
                                            (write condition :stream stream :escape nil))))))"#,
            )
            .unwrap();
        assert_eq!(runtime.string(&values[0]).as_deref(), Some("bad 5 (8 9)"));
        assert_eq!(runtime.string(&values[1]).as_deref(), Some("bad 5 (8 9)"));
        assert_eq!(runtime.string(&values[2]).as_deref(), Some("bad 5"));
    }
}
