use std::sync::{Arc, LazyLock};

use rustc_hash::FxHashMap;

use crate::condition::{Class, Fault};
use crate::heap::{ListSpine, Word};
use crate::symbol::{Name, Package};
use crate::{EvalError, EvalErrorKind, NativeFunction, Root, Runtime};

#[path = "assertion.rs"]
mod assertion;
#[path = "case_forms.rs"]
mod case_forms;
#[path = "defclass.rs"]
mod defclass;
#[path = "define_condition.rs"]
mod define_condition;
#[path = "defpackage.rs"]
mod defpackage;
#[path = "defstruct.rs"]
mod defstruct;
#[path = "generic.rs"]
mod generic;
pub(crate) use generic::extract_method_parameters;
#[path = "places.rs"]
pub(super) mod places;
#[path = "streams.rs"]
mod streams;

type Expansion<'rt> = Result<Root<'rt>, EvalError>;
type Expand = for<'rt> fn(&'rt Runtime, &[Root<'rt>]) -> Expansion<'rt>;
type ExpandWithEnvironment = for<'rt> fn(&'rt Runtime, &[Root<'rt>], &Root<'rt>) -> Expansion<'rt>;

const BUILTINS: &[(&str, Expand)] = &[
    ("IN-PACKAGE", expand_in_package),
    ("AND", expand_and),
    ("OR", expand_or),
    ("COND", expand_cond),
    ("CASE", case_forms::case),
    ("ECASE", case_forms::ecase),
    ("TYPECASE", case_forms::typecase),
    ("ETYPECASE", case_forms::etypecase),
    ("WHEN", expand_when),
    ("UNLESS", expand_unless),
    ("DOTIMES", iteration::dotimes),
    ("DOLIST", iteration::dolist),
    ("DO-SYMBOLS", iteration::do_symbols),
    ("DO-EXTERNAL-SYMBOLS", iteration::do_external_symbols),
    ("DO-ALL-SYMBOLS", iteration::do_all_symbols),
    ("WITH-PACKAGE-ITERATOR", iteration::with_package_iterator),
    (
        "WITH-HASH-TABLE-ITERATOR",
        iteration::with_hash_table_iterator,
    ),
    ("DO", iteration::do_parallel),
    ("DO*", iteration::do_sequential),
    ("LOOP", loop_forms::expand),
    ("PROG", expand_prog),
    ("PROG*", expand_prog_star),
    ("PROG1", expand_prog1),
    ("PROG2", expand_prog2),
    ("ASSERT", assertion::expand),
    ("CHECK-TYPE", expand_check_type),
    ("TIME", expand_time),
    ("WITH-SIMPLE-RESTART", expand_with_simple_restart),
    ("MULTIPLE-VALUE-LIST", expand_multiple_value_list),
    ("MULTIPLE-VALUE-SETQ", places::expand_multiple_value_setq),
    ("NTH-VALUE", expand_nth_value),
    ("DEFPACKAGE", defpackage::expand),
    ("DEFCLASS", expand_defclass),
    ("DEFINE-CONDITION", define_condition::expand),
    ("DEFSTRUCT", defstruct::expand),
    ("DEFGENERIC", generic::defgeneric),
    ("DEFMETHOD", generic::defmethod),
    ("DEFINE-SYMBOL-MACRO", expand_define_symbol_macro),
    ("DEFTYPE", expand_deftype),
    ("DEFSETF", places::expand_defsetf),
    ("DEFINE-SETF-EXPANDER", places::expand_define_setf_expander),
    ("DEFINE-MODIFY-MACRO", places::expand_define_modify_macro),
    ("WITH-COMPILATION-UNIT", expand_with_compilation_unit),
    ("WITH-STANDARD-IO-SYNTAX", streams::with_standard_io_syntax),
    ("WITH-INPUT-FROM-STRING", streams::with_input_from_string),
    ("WITH-OUTPUT-TO-STRING", streams::with_output_to_string),
    ("WITH-OPEN-STREAM", streams::with_open_stream),
    ("WITH-OPEN-FILE", streams::with_open_file),
    ("PPRINT-LOGICAL-BLOCK", pprint_logical_block),
    ("PRINT-UNREADABLE-OBJECT", print_unreadable_object),
    // Standard macros the compiler lowers directly. Their expander functions
    // exist for MACRO-FUNCTION and MACROEXPAND; the compiler keeps the original
    // form so that compile-time effects stay at their definition site.
    ("RETURN", expand_return),
    ("LAMBDA", expand_lambda),
    ("IGNORE-ERRORS", expand_ignore_errors),
    ("MULTIPLE-VALUE-BIND", expand_multiple_value_bind),
    ("FORMATTER", expand_formatter),
    ("STEP", expand_step),
    ("DECLAIM", expand_declaim),
    ("DEFCONSTANT", expand_defconstant),
    ("DEFMACRO", expand_defmacro),
    ("DEFINE-COMPILER-MACRO", expand_define_compiler_macro),
    ("DEFPARAMETER", expand_defparameter),
    ("DEFUN", expand_defun),
    ("DEFVAR", expand_defvar),
    ("DESTRUCTURING-BIND", expand_destructuring_bind),
    ("HANDLER-BIND", expand_handler_bind),
    ("HANDLER-CASE", expand_handler_case),
    ("RESTART-BIND", expand_restart_bind),
    ("RESTART-CASE", expand_restart_case),
    ("WITH-CONDITION-RESTARTS", expand_with_condition_restarts),
];

#[path = "iteration.rs"]
mod iteration;
#[path = "loop_forms.rs"]
mod loop_forms;

const ENV_BUILTINS: &[(&str, ExpandWithEnvironment)] = &[
    ("CCASE", case_forms::ccase),
    ("CTYPECASE", case_forms::ctypecase),
    ("SETF", places::expand_setf),
    ("PSETF", places::expand_psetf),
    ("PSETQ", places::expand_psetq),
    ("INCF", places::expand_incf),
    ("DECF", places::expand_decf),
    ("PUSH", places::expand_push),
    ("PUSHNEW", places::expand_pushnew),
    ("REMF", places::expand_remf),
    ("POP", places::expand_pop),
    ("SHIFTF", places::expand_shiftf),
    ("ROTATEF", places::expand_rotatef),
];

const MACRO_COUNT: usize = BUILTINS.len() + ENV_BUILTINS.len();

// Installer IDs used across compiled-file serialization; keep all IDs in one table.
const SWITCH_PACKAGE: u32 = 0;
pub(super) const SET_PACKAGE_DOCUMENTATION: u32 = 1;
const DEFINE_SYMBOL_MACRO: u32 = 2;
const REGISTER_STRUCTURE: u32 = 3;
const MAKE_STRUCTURE: u32 = 4;
const STRUCTURE_PREDICATE: u32 = 5;
const COPY_STRUCTURE: u32 = 6;
const READ_STRUCTURE_SLOT: u32 = 7;
pub(super) const WRITE_STRUCTURE_SLOT: u32 = 8;
const DEFINE_GENERIC: u32 = 9;
const DEFINE_METHOD: u32 = 10;
pub(super) const REGISTER_CLASS: u32 = 11;
pub(crate) const INITIALIZE_SHARED_SLOT: u32 = 13;
const DEFINE_TYPE: u32 = 14;
pub(super) const DEFINE_SETF_EXPANDER: u32 = 15;
pub(super) const DEFINE_SHORT_DEFSETF: u32 = 16;
pub(super) const DEFINE_LONG_DEFSETF: u32 = 17;
pub(super) const PACKAGE_ITERATOR_ENTRIES: u32 = 18;
pub(super) const HASH_TABLE_STEP: u32 = 19;
pub(super) const SET_MACRO_FUNCTION: u32 = 20;
pub(crate) const DEFINE_CONSTANT: u32 = 21;
pub(super) const DEFINE_PACKAGE: u32 = 22;
const PRINT_DEPTH: u32 = 24;
pub(crate) const RESIDUAL_MACRO_ERROR: u32 = 25;
pub(crate) const INSTALL_TRANSFORM: u32 = 26;
pub(crate) const INSTALL_VOP: u32 = 27;
type Installer = for<'rt> fn(&'rt Runtime, &[Root<'rt>], usize) -> Result<Vec<Root<'rt>>, Fault>;

macro_rules! define_installers {
    ($(($id:expr, $name:expr, $handler:expr $(,)?)),* $(,)?) => {
        const INSTALLERS: &[(u32, Installer)] = &[
            $(($id, $handler)),*
        ];

        pub(crate) const BUILTIN_INSTALLER_DESCRIPTORS: &[(u32, &str)] = &[
            $(($id, $name)),*
        ];
    };
}

define_installers! {
    (SWITCH_PACKAGE, "SWITCH-PACKAGE", switch_package),
    (
        SET_PACKAGE_DOCUMENTATION,
        "SET-PACKAGE-DOCUMENTATION",
        defpackage::set_documentation,
    ),
    (DEFINE_SYMBOL_MACRO, "DEFINE-SYMBOL-MACRO", define_symbol_macro),
    (REGISTER_STRUCTURE, "REGISTER-STRUCTURE", crate::vm::structures::register),
    (MAKE_STRUCTURE, "MAKE-STRUCTURE", crate::vm::structures::make),
    (
        STRUCTURE_PREDICATE,
        "STRUCTURE-PREDICATE",
        crate::vm::structures::predicate,
    ),
    (COPY_STRUCTURE, "COPY-STRUCTURE", crate::vm::structures::copy),
    (
        READ_STRUCTURE_SLOT,
        "READ-STRUCTURE-SLOT",
        crate::vm::structures::read_slot,
    ),
    (
        WRITE_STRUCTURE_SLOT,
        "WRITE-STRUCTURE-SLOT",
        crate::vm::structures::write_slot,
    ),
    (DEFINE_GENERIC, "DEFINE-GENERIC", crate::vm::generic::define_generic),
    (DEFINE_METHOD, "DEFINE-METHOD", crate::vm::generic::define_method),
    (
        REGISTER_CLASS,
        "REGISTER-CLASS",
        crate::vm::class_definition::register_declaration,
    ),
    (
        INITIALIZE_SHARED_SLOT,
        "INITIALIZE-SHARED-SLOT",
        defclass::initialize_shared_slot,
    ),
    (DEFINE_TYPE, "DEFINE-TYPE", define_type),
    (
        DEFINE_SETF_EXPANDER,
        "DEFINE-SETF-EXPANDER",
        places::install_setf_expander,
    ),
    (
        DEFINE_SHORT_DEFSETF,
        "DEFINE-SHORT-DEFSETF",
        places::install_short_defsetf,
    ),
    (
        DEFINE_LONG_DEFSETF,
        "DEFINE-LONG-DEFSETF",
        places::install_long_defsetf,
    ),
    (
        PACKAGE_ITERATOR_ENTRIES,
        "PACKAGE-ITERATOR-ENTRIES",
        crate::vm::packages::iterator_entries,
    ),
    (
        HASH_TABLE_STEP,
        "HASH-TABLE-STEP",
        crate::vm::hash_tables::iterator_step,
    ),
    (SET_MACRO_FUNCTION, "SET-MACRO-FUNCTION", set_macro_function),
    (
        DEFINE_CONSTANT,
        "DEFINE-CONSTANT",
        crate::compile::constants::define_constant,
    ),
    (DEFINE_PACKAGE, "DEFINE-PACKAGE", defpackage::define_package),
    (PRINT_DEPTH, "PRINT-DEPTH", crate::vm::structures::print_depth),
    (
        RESIDUAL_MACRO_ERROR,
        "RESIDUAL-MACRO-ERROR",
        crate::compile::raise_residual_macro_error,
    ),
    (
        INSTALL_TRANSFORM,
        "INSTALL-TRANSFORM",
        crate::compile::vop::install_transform,
    ),
    (INSTALL_VOP, "INSTALL-VOP", crate::compile::vop::install_vop),
}

const _: () = {
    let mut i = 0;
    while i < INSTALLERS.len() {
        let mut j = i + 1;
        while j < INSTALLERS.len() {
            assert!(INSTALLERS[i].0 != INSTALLERS[j].0, "duplicate installer ID");
            j += 1;
        }
        i += 1;
    }
};

pub(crate) fn compiled_native_id(word: Word) -> Option<u32> {
    INSTALLERS
        .get(word.native_index()?.checked_sub(MACRO_COUNT)?)
        .map(|&(id, _)| id)
}

pub(crate) fn compiled_native(id: u32) -> Option<Word> {
    let index = INSTALLERS.iter().position(|&(key, _)| key == id)?;
    Some(Word::native(MACRO_COUNT + index))
}

pub(super) fn expander(spelling: &str) -> Option<Word> {
    if spelling.is_empty() {
        return None;
    }
    static NATIVE_INDICES: LazyLock<FxHashMap<&'static str, usize>> = LazyLock::new(|| {
        let mut indices = FxHashMap::default();
        for (index, name) in BUILTINS
            .iter()
            .map(|&(name, _)| name)
            .chain(ENV_BUILTINS.iter().map(|&(name, _)| name))
            .enumerate()
        {
            indices.insert(name, index);
        }
        indices
    });
    NATIVE_INDICES.get(spelling).copied().map(Word::native)
}

/// Reserve the leading native function indices for the standard expanders.
pub(crate) fn builtin_natives() -> Vec<Arc<NativeFunction>> {
    let mut natives: Vec<_> = BUILTINS
        .iter()
        .map(|&(name, expand)| native_expander(name, move |rt, forms, _| expand(rt, forms)))
        .chain(
            ENV_BUILTINS
                .iter()
                .map(|&(name, expand)| native_expander(name, expand)),
        )
        .collect();
    natives.extend(
        INSTALLERS
            .iter()
            .map(|&(_, install)| Arc::new(install) as Arc<NativeFunction>),
    );
    natives
}

fn native_expander(
    name: &'static str,
    expand: impl for<'rt> Fn(&'rt Runtime, &[Root<'rt>], &Root<'rt>) -> Expansion<'rt>
        + Send
        + Sync
        + 'static,
) -> Arc<NativeFunction> {
    Arc::new(move |runtime, args, at| {
        let invoke = || {
            let [form, environment] = args else {
                return Err(invalid(
                    name,
                    "the expander expects the whole form and environment",
                ));
            };
            let Some((_, operands)) = runtime.cons_fields(form) else {
                return Err(invalid(name, "expected a macro call"));
            };
            let operands = proper_list(runtime, operands, name)?;
            Ok(vec![expand(runtime, &operands, environment)?])
        };
        invoke().map_err(|error| expansion_fault(error, at))
    })
}

fn expansion_fault(error: EvalError, at: usize) -> Fault {
    match error.kind() {
        EvalErrorKind::MacroExpansion => {
            Fault::plain(Class::ProgramError, error.during_macro_expansion(at))
        }
        _ => error.into(),
    }
}

fn invalid(name: &str, message: &str) -> EvalError {
    EvalError::macro_expansion(0, format!("{name} {message}"))
}

fn proper_list<'rt>(
    runtime: &'rt Runtime,
    list: Root<'rt>,
    name: &str,
) -> Result<Vec<Root<'rt>>, EvalError> {
    let mut spine = ListSpine::new(list.slot.get());
    let mut elements = Vec::new();
    while let Some((car, _)) = spine
        .next(&runtime.heap)
        .map_err(|_| invalid(name, "expects a proper list"))?
    {
        elements.push(runtime.root(car));
    }
    if spine.tail() != Word::NIL {
        return Err(invalid(name, "expects a proper list"));
    }
    Ok(elements)
}

fn symbol<'rt>(runtime: &'rt Runtime, name: &str) -> Expansion<'rt> {
    runtime
        .intern_common_lisp(name, 0)
        .map(|word| runtime.root(word))
}

fn call<'rt, 'arg>(
    runtime: &'rt Runtime,
    operator: &str,
    args: impl DoubleEndedIterator<Item = &'arg Root<'rt>>,
) -> Expansion<'rt>
where
    'rt: 'arg,
{
    let operator = symbol(runtime, operator)?;
    call_with_operator(runtime, &operator, args)
}

fn call_with_operator<'rt, 'arg>(
    runtime: &'rt Runtime,
    operator: &Root<'rt>,
    args: impl DoubleEndedIterator<Item = &'arg Root<'rt>>,
) -> Expansion<'rt>
where
    'rt: 'arg,
{
    let operands = list(runtime, args)?;
    runtime
        .cons(operator, &operands)
        .map_err(|error| EvalError::out_of_memory(0, error))
}

fn list<'rt, 'arg>(
    runtime: &'rt Runtime,
    items: impl DoubleEndedIterator<Item = &'arg Root<'rt>>,
) -> Expansion<'rt>
where
    'rt: 'arg,
{
    let allocation_error = |source| EvalError::out_of_memory(0, source);
    let mut result = runtime.nil();
    for item in items.rev() {
        result = runtime.cons(item, &result).map_err(allocation_error)?;
    }
    Ok(result)
}

pub(super) fn quote<'rt>(runtime: &'rt Runtime, value: &Root<'rt>) -> Expansion<'rt> {
    call(runtime, "QUOTE", std::iter::once(value))
}

pub(super) fn binding<'rt>(
    runtime: &'rt Runtime,
    variable: &Root<'rt>,
    value: &Root<'rt>,
) -> Expansion<'rt> {
    list(runtime, [variable, value].into_iter())
}

pub(super) fn let_star<'rt>(
    runtime: &'rt Runtime,
    bindings: &[Root<'rt>],
    body: Root<'rt>,
) -> Expansion<'rt> {
    if bindings.is_empty() {
        return Ok(body);
    }
    let bindings = list(runtime, bindings.iter())?;
    call(runtime, "LET*", [&bindings, &body].into_iter())
}

pub(super) fn progn<'rt>(runtime: &'rt Runtime, forms: Vec<Root<'rt>>) -> Expansion<'rt> {
    match forms.len() {
        0 => Ok(runtime.nil()),
        1 => Ok(forms.into_iter().next().expect("one form")),
        _ => call(runtime, "PROGN", forms.iter()),
    }
}

pub(super) fn keyword_name_opt(runtime: &Runtime, value: &Root<'_>) -> Option<String> {
    (runtime.heap.symbol_home(value.slot.get()) == Some(crate::package::Package::KEYWORD))
        .then(|| runtime.symbol_name(value))
        .flatten()
}

fn define_at_toplevel<'rt>(runtime: &'rt Runtime, body: Root<'rt>) -> Expansion<'rt> {
    let situations = ["COMPILE-TOPLEVEL", "LOAD-TOPLEVEL", "EXECUTE"]
        .map(|name| {
            runtime
                .intern(&Name::keyword(name), 0)
                .map(|word| runtime.root(word))
        })
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    let situations = list(runtime, situations.iter())?;
    call(runtime, "EVAL-WHEN", [&situations, &body].into_iter())
}

fn expand_in_package<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let [designator] = forms else {
        return Err(invalid("IN-PACKAGE", "expects one package name"));
    };
    let word = designator.slot.get();
    if !runtime.heap.is_symbol(word)
        && !runtime.heap.is_string(word)
        && word.as_character().is_none()
    {
        return Err(invalid("IN-PACKAGE", "expects a literal string designator"));
    }
    let argument = quote(runtime, designator)?;
    let body = native_call(runtime, SWITCH_PACKAGE, [&argument])?;
    define_at_toplevel(runtime, body)
}

fn switch_package<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [designator] = args else {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "expected one package name"),
        ));
    };
    let word = designator.slot.get();
    let name = runtime.string_designator_text(word).ok_or_else(|| {
        Fault::package_designator(runtime, word, EvalError::new(at, "expected a package name"))
    })?;
    let package = runtime.find_package_named(&name).ok_or_else(|| {
        Fault::package_error(runtime, 
            word,
            EvalError::new(at, format!("no package named {name:?}")),
        )
    })?;
    let result = package.word();
    runtime
        .heap
        .set_symbol_cell(runtime.package_variable(), false, result);
    Ok(vec![runtime.root(result)])
}

/// DEFTYPE binds an expander that turns a type specifier's arguments into
/// another type specifier, and returns the type name.
fn expand_deftype<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let [name, parameters, body @ ..] = forms else {
        return Err(invalid(
            "DEFTYPE",
            "expects a type name, lambda list, and body",
        ));
    };
    if !runtime.heap.is_symbol(name.slot.get()) {
        return Err(invalid("DEFTYPE", "expects a symbol as the type name"));
    }
    crate::compile::declarations::reject_declaration(runtime, name.slot.get(), "DEFTYPE", 0)?;
    let quoted = call(runtime, "QUOTE", [name].into_iter())?;
    let parameters = deftype_lambda_list(runtime, parameters)?;
    let body = deftype_expander_body(runtime, name, body)?;
    let expander = call(
        runtime,
        "LAMBDA",
        std::iter::once(&parameters).chain(body.iter()),
    )?;
    let definition = native_call(runtime, DEFINE_TYPE, [&quoted, &expander])?;
    let body = call(runtime, "PROGN", [&definition, &quoted].into_iter())?;
    define_at_toplevel(runtime, body)
}

#[derive(Clone, Copy, PartialEq)]
enum LambdaSection {
    Positional,
    Optional,
    Key,
}

/// A deftype lambda list is a macro lambda list whose unsupplied optional and
/// keyword parameters default to the symbol * rather than NIL, so an omitted
/// argument leaves its position unspecified instead of collapsing it to NIL
/// (CLHS 3.4.8).  A parameter that supplies its own default keeps it.
fn deftype_lambda_list<'rt>(runtime: &'rt Runtime, parameters: &Root<'rt>) -> Expansion<'rt> {
    let items = proper_list(runtime, parameters.clone(), "DEFTYPE")?;
    // The default is the symbol * itself, so the default form quotes it.
    let star_symbol = symbol(runtime, "*")?;
    let star = quote(runtime, &star_symbol)?;
    let mut section = LambdaSection::Positional;
    let mut rewritten = Vec::with_capacity(items.len());
    for item in items {
        let keyword = runtime
            .heap
            .is_symbol(item.slot.get())
            .then(|| runtime.heap.symbol_name(item.slot.get()))
            .flatten();
        if let Some(keyword) = keyword.as_deref()
            && keyword.starts_with('&')
        {
            section = match keyword {
                "&OPTIONAL" => LambdaSection::Optional,
                "&KEY" => LambdaSection::Key,
                _ => LambdaSection::Positional,
            };
            rewritten.push(item);
            continue;
        }
        if section == LambdaSection::Positional {
            rewritten.push(item);
        } else {
            rewritten.push(deftype_parameter_default(runtime, &item, &star)?);
        }
    }
    list(runtime, rewritten.iter())
}

/// Gives one optional or keyword parameter the * default it lacks.
fn deftype_parameter_default<'rt>(
    runtime: &'rt Runtime,
    parameter: &Root<'rt>,
    star: &Root<'rt>,
) -> Expansion<'rt> {
    if runtime.heap.is_symbol(parameter.slot.get()) {
        return list(runtime, [parameter, star].into_iter());
    }
    let items = proper_list(runtime, parameter.clone(), "DEFTYPE")?;
    if items.len() != 1 {
        return Ok(parameter.clone());
    }
    // A single binding element, either (variable) or ((keyword variable)),
    // takes the default after it.
    list(runtime, items.iter().chain(std::iter::once(star)))
}

/// The expander body: an optional documentation string and the declarations
/// first, then the forms enclosed in a block named after the type.  The body
/// of a DEFTYPE form is implicitly enclosed in that block, and declarations
/// are only valid at the start of a lambda body.
fn deftype_expander_body<'rt>(
    runtime: &'rt Runtime,
    name: &Root<'rt>,
    body: &[Root<'rt>],
) -> Result<Vec<Root<'rt>>, EvalError> {
    let mut index = 0;
    let mut prefix = Vec::with_capacity(body.len() + 1);
    if let Some(first) = body.first()
        && runtime.heap.is_string(first.slot.get())
    {
        prefix.push(first.clone());
        index = 1;
    }
    while let Some(declaration) = body.get(index) {
        let is_declaration = runtime
            .heap
            .cons_fields(declaration.slot.get())
            .and_then(|(head, _)| runtime.heap.symbol_name(head))
            .is_some_and(|head| head == "DECLARE");
        if !is_declaration {
            break;
        }
        prefix.push(declaration.clone());
        index += 1;
    }
    let block = call(
        runtime,
        "BLOCK",
        std::iter::once(name).chain(body[index..].iter()),
    )?;
    prefix.push(block);
    Ok(prefix)
}

fn define_type<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let invalid = |message| Fault::plain(Class::ProgramError, EvalError::new(at, message));
    let [name, function] = args else {
        return Err(invalid("expected a type name and its expander"));
    };
    let word = name.slot.get();
    if runtime
        .heap
        .symbol_attributes(word)
        .is_none_or(|data| data.constant || data.canonical)
    {
        return Err(invalid("expected a non-standard type name"));
    }
    if !crate::vm::is_function(runtime, function.slot.get()) {
        return Err(invalid("expected a function as the type expander"));
    }
    runtime.define_type_expander(word, function.slot.get());
    Ok(vec![name.clone()])
}

fn expand_define_symbol_macro<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let [name, expansion] = forms else {
        return Err(invalid(
            "DEFINE-SYMBOL-MACRO",
            "expects a symbol and expansion",
        ));
    };
    if !runtime.heap.is_symbol(name.slot.get()) {
        return Err(invalid("DEFINE-SYMBOL-MACRO", "expects a symbol name"));
    }
    let name = quote(runtime, name)?;
    let expansion = quote(runtime, expansion)?;
    let body = native_call(runtime, DEFINE_SYMBOL_MACRO, [&name, &expansion])?;
    define_at_toplevel(runtime, body)
}

fn define_symbol_macro<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let invalid = |message| Fault::plain(Class::ProgramError, EvalError::new(at, message));
    let [name, expansion] = args else {
        return Err(invalid("expected a symbol name and its expansion"));
    };
    let word = name.slot.get();
    if runtime
        .heap
        .symbol_attributes(word)
        .is_none_or(|data| data.constant || data.canonical)
    {
        return Err(invalid("expected a non-standard variable symbol"));
    }
    if runtime.heap.is_special(word) {
        return Err(invalid("a special variable cannot be a symbol macro"));
    }
    let expander = super::symbol_expander(runtime, expansion, at)?;
    runtime
        .heap
        .set_symbol_macro_expander(name.slot.get(), expander.slot.get());
    Ok(vec![name.clone()])
}

fn expand_and<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    if let [form] = forms {
        // Keep the operand in an ordinary form context even at top level.
        return call(runtime, "IF", [&runtime.root(Word::T), form].into_iter());
    }
    and_branch(runtime, forms)
}

fn and_branch<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    match forms {
        [] => Ok(runtime.root(Word::T)),
        [form] => Ok(form.clone()),
        _ => {
            // The left half is tested for its primary value. Balancing keeps
            // a flat sequence from exhausting the expansion nesting limit.
            let (tests, tail) = forms.split_at(forms.len() / 2);
            let test = and_branch(runtime, tests)?;
            let tail = and_branch(runtime, tail)?;
            call(runtime, "IF", [&test, &tail].into_iter())
        }
    }
}

fn fresh<'rt>(runtime: &'rt Runtime, name: &str) -> Expansion<'rt> {
    let (_, word) = runtime.fresh_symbol(name, 0)?;
    Ok(runtime.root(word))
}

fn expand_check_type<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let [place, type_spec, rest @ ..] = forms else {
        return Err(invalid("CHECK-TYPE", "requires a place and a type"));
    };
    if rest.len() > 1 {
        return Err(invalid(
            "CHECK-TYPE",
            "accepts one optional type description",
        ));
    }
    let observed = fresh(runtime, "CHECK-TYPE DATUM")?;
    let quoted = quote(runtime, type_spec)?;
    let check = call(runtime, "TYPEP", [&observed, &quoted].into_iter())?;
    let type_error = quote(runtime, &symbol(runtime, "SIMPLE-TYPE-ERROR")?)?;
    let datum = keyword_symbol(runtime, "DATUM")?;
    let expected = keyword_symbol(runtime, "EXPECTED-TYPE")?;
    let control = keyword_symbol(runtime, "FORMAT-CONTROL")?;
    let arguments = keyword_symbol(runtime, "FORMAT-ARGUMENTS")?;
    let text = runtime.root(cl_string(
        runtime,
        if rest.is_empty() {
            "~S has value ~S, which is not of type ~S."
        } else {
            "~S has value ~S, which is not ~A."
        },
    ));
    let place_name = quote(runtime, place)?;
    let report_args = call(
        runtime,
        "LIST",
        [&place_name, &observed, rest.first().unwrap_or(&quoted)].into_iter(),
    )?;
    let error = call(
        runtime,
        "ERROR",
        [
            &type_error,
            &datum,
            &observed,
            &expected,
            &quoted,
            &control,
            &text,
            &arguments,
            &report_args,
        ]
        .into_iter(),
    )?;
    let value = fresh(runtime, "CHECK-TYPE VALUE")?;
    let parameters = list(runtime, [&value].into_iter())?;
    let setter = call(runtime, "SETF", [place, &value].into_iter())?;
    let store = symbol(runtime, "STORE-VALUE")?;
    let report = keyword_symbol(runtime, "REPORT")?;
    let message = runtime.root(cl_string(runtime, "Supply a new value and check its type."));
    let interactive = keyword_symbol(runtime, "INTERACTIVE")?;
    let prompt = case_forms::prompt_for_value(runtime)?;
    let clause = list(
        runtime,
        [
            &store,
            &parameters,
            &report,
            &message,
            &interactive,
            &prompt,
            &setter,
        ]
        .into_iter(),
    )?;
    let restart = call(runtime, "RESTART-CASE", [&error, &clause].into_iter())?;
    let end = list(runtime, [&check, &runtime.nil()].into_iter())?;
    let read = list(runtime, [&observed, place, place].into_iter())?;
    let bindings = list(runtime, [&read].into_iter())?;
    call(runtime, "DO", [&bindings, &end, &restart].into_iter())
}

fn expand_time<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let [form] = forms else {
        return Err(invalid("TIME", "expects one form"));
    };
    let real_start = fresh(runtime, "TIME REAL START")?;
    let run_start = fresh(runtime, "TIME RUN START")?;
    let units = symbol(runtime, "INTERNAL-TIME-UNITS-PER-SECOND")?;
    let trace = symbol(runtime, "*TRACE-OUTPUT*")?;
    let elapsed_seconds = |clock, start| {
        let now = call(runtime, clock, std::iter::empty())?;
        let elapsed = call(runtime, "-", [&now, start].into_iter())?;
        call(runtime, "/", [&elapsed, &units].into_iter())
    };
    let real_seconds = elapsed_seconds("GET-INTERNAL-REAL-TIME", &real_start)?;
    let run_seconds = elapsed_seconds("GET-INTERNAL-RUN-TIME", &run_start)?;
    let control = runtime.root(cl_string(
        runtime,
        "~&Elapsed time: ~,3F seconds~%Run time: ~,3F seconds~%",
    ));
    let report = call(
        runtime,
        "FORMAT",
        [&trace, &control, &real_seconds, &run_seconds].into_iter(),
    )?;
    let values = call(runtime, "MULTIPLE-VALUE-PROG1", [form, &report].into_iter())?;
    let bindings = [
        binding(
            runtime,
            &real_start,
            &call(runtime, "GET-INTERNAL-REAL-TIME", std::iter::empty())?,
        )?,
        binding(
            runtime,
            &run_start,
            &call(runtime, "GET-INTERNAL-RUN-TIME", std::iter::empty())?,
        )?,
    ];
    let_star(runtime, &bindings, values)
}

fn expand_with_simple_restart<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let (spec, body) = forms
        .split_first()
        .ok_or_else(|| invalid("WITH-SIMPLE-RESTART", "requires a restart spec"))?;
    let spec = proper_list(runtime, spec.clone(), "WITH-SIMPLE-RESTART")?;
    let [name, control, arguments @ ..] = spec.as_slice() else {
        return Err(invalid(
            "WITH-SIMPLE-RESTART",
            "requires a restart name and a format control",
        ));
    };
    if !runtime.heap.is_symbol(name.slot.get()) {
        return Err(invalid("WITH-SIMPLE-RESTART", "expected a restart name"));
    }
    let name = quote(runtime, name)?;
    let body = call(runtime, "PROGN", body.iter())?;
    let thunk = call(runtime, "LAMBDA", [&runtime.nil(), &body].into_iter())?;
    let thunk = call(runtime, "FUNCTION", [&thunk].into_iter())?;
    let stream = fresh(runtime, "REPORT STREAM")?;
    let report_args = list(runtime, [&stream].into_iter())?;
    let format = call(
        runtime,
        "FORMAT",
        [&stream, control].into_iter().chain(arguments),
    )?;
    let reporter = call(runtime, "LAMBDA", [&report_args, &format].into_iter())?;
    let reporter = call(runtime, "FUNCTION", [&reporter].into_iter())?;
    let operator = runtime.root(
        crate::vm::Primitive::named("%CALL-WITH-SIMPLE-RESTART").expect("simple restart primitive"),
    );
    call(
        runtime,
        "FUNCALL",
        [&operator, &name, &reporter, &thunk].into_iter(),
    )
}

fn keyword_symbol<'rt>(runtime: &'rt Runtime, name: &str) -> Expansion<'rt> {
    runtime
        .intern(&Name::keyword(name), 0)
        .map(|word| runtime.root(word))
}

fn expand_or<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let Some((last, earlier)) = forms.split_last() else {
        return Ok(runtime.nil());
    };
    if earlier.is_empty() {
        return call(runtime, "IF", [&runtime.root(Word::T), last].into_iter());
    }
    // A flat block keeps the expansion depth independent of the operand count.
    let exit = fresh(runtime, "OR EXIT")?;
    let mut body = Vec::with_capacity(forms.len());
    for form in earlier {
        body.push(return_primary_if_true(runtime, &exit, form)?);
    }
    call(
        runtime,
        "BLOCK",
        std::iter::once(&exit)
            .chain(&body)
            .chain(std::iter::once(last)),
    )
}

fn expand_cond<'rt>(runtime: &'rt Runtime, clauses: &[Root<'rt>]) -> Expansion<'rt> {
    if clauses.is_empty() {
        return Ok(runtime.nil());
    }
    let exit = fresh(runtime, "COND EXIT")?;
    let mut branches = Vec::with_capacity(clauses.len());
    for clause in clauses {
        let clause = proper_list(runtime, clause.clone(), "COND clause")?;
        let Some((test, body)) = clause.split_first() else {
            return Err(invalid("COND clause", "requires a test form"));
        };
        let branch = if body.is_empty() {
            return_primary_if_true(runtime, &exit, test)?
        } else {
            let body = call(runtime, "PROGN", body.iter())?;
            let result = call(runtime, "RETURN-FROM", [&exit, &body].into_iter())?;
            call(runtime, "IF", [test, &result].into_iter())?
        };
        branches.push(branch);
    }
    // Each selected clause transfers its values. Falling through yields one NIL.
    call(runtime, "BLOCK", std::iter::once(&exit).chain(&branches))
}

fn return_primary_if_true<'rt>(
    runtime: &'rt Runtime,
    exit: &Root<'rt>,
    form: &Root<'rt>,
) -> Expansion<'rt> {
    let primary = fresh(runtime, "PRIMARY")?;
    let binding = list(runtime, [&primary, form].into_iter())?;
    let bindings = list(runtime, std::iter::once(&binding))?;
    let result = call(runtime, "RETURN-FROM", [exit, &primary].into_iter())?;
    let test = call(runtime, "IF", [&primary, &result].into_iter())?;
    call(runtime, "LET", [&bindings, &test].into_iter())
}

fn expand_when<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    expand_conditional(runtime, forms, true)
}

fn expand_unless<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    expand_conditional(runtime, forms, false)
}

fn expand_conditional<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    when_true: bool,
) -> Expansion<'rt> {
    let Some((test, body)) = forms.split_first() else {
        return Err(invalid(
            if when_true { "WHEN" } else { "UNLESS" },
            "requires a test form",
        ));
    };
    let body = call(runtime, "PROGN", body.iter())?;
    let (yes, no) = if when_true {
        (body, runtime.nil())
    } else {
        (runtime.nil(), body)
    };
    call(runtime, "IF", [test, &yes, &no].into_iter())
}

fn expand_prog<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    expand_prog_bindings(runtime, forms, "PROG", "LET")
}

fn expand_prog_star<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    expand_prog_bindings(runtime, forms, "PROG*", "LET*")
}

fn expand_prog_bindings<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    name: &str,
    binding_form: &str,
) -> Expansion<'rt> {
    let Some((bindings, body)) = forms.split_first() else {
        return Err(invalid(name, "requires a binding list"));
    };
    let declare = symbol(runtime, "DECLARE")?;
    let declaration_count = body
        .iter()
        .take_while(|form| {
            runtime
                .cons_fields(form)
                .is_some_and(|(head, _)| head.same_identity(&declare))
        })
        .count();
    let (declarations, statements) = body.split_at(declaration_count);
    let statements = call(runtime, "TAGBODY", statements.iter())?;
    let binding = call(
        runtime,
        binding_form,
        std::iter::once(bindings)
            .chain(declarations)
            .chain(std::iter::once(&statements)),
    )?;
    call(runtime, "BLOCK", [&runtime.nil(), &binding].into_iter())
}

fn expand_prog1<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    if forms.is_empty() {
        return Err(invalid("PROG1", "requires at least one form"));
    }
    let sequence = call(runtime, "MULTIPLE-VALUE-PROG1", forms.iter())?;
    call(runtime, "VALUES", [&sequence].into_iter())
}

fn expand_prog2<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let [first, _, ..] = forms else {
        return Err(invalid("PROG2", "requires at least two forms"));
    };
    let second = expand_prog1(runtime, &forms[1..])?;
    // Use an ordinary form context for the first operand and the retained value.
    call(runtime, "LET", [&runtime.nil(), first, &second].into_iter())
}

fn expand_multiple_value_list<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let [form] = forms else {
        return Err(invalid("MULTIPLE-VALUE-LIST", "expects one form"));
    };
    let function = call(runtime, "FUNCTION", [&symbol(runtime, "LIST")?].into_iter())?;
    call(
        runtime,
        "MULTIPLE-VALUE-CALL",
        [&function, form].into_iter(),
    )
}

fn expand_nth_value<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let [index, form] = forms else {
        return Err(invalid(
            "NTH-VALUE",
            "expects an index and a value-producing form",
        ));
    };
    let values = expand_multiple_value_list(runtime, std::slice::from_ref(form))?;
    call(runtime, "NTH", [index, &values].into_iter())
}

fn expand_with_compilation_unit<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let Some((options, body)) = forms.split_first() else {
        return Err(invalid(
            "WITH-COMPILATION-UNIT",
            "expects an options list and body",
        ));
    };
    let options = proper_list(runtime, options.clone(), "WITH-COMPILATION-UNIT")?;
    let (pairs, remainder) = options.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(invalid(
            "WITH-COMPILATION-UNIT",
            "expects keyword/value option pairs",
        ));
    }
    let mut override_form = None;
    for [key, value] in pairs {
        if keyword_name_opt(runtime, key).as_deref() == Some("OVERRIDE") && override_form.is_none()
        {
            override_form = Some(value.clone());
        } else {
            return Err(invalid(
                "WITH-COMPILATION-UNIT",
                "only supports a single :OVERRIDE option",
            ));
        }
    }
    let locally = call(runtime, "LOCALLY", body.iter())?;
    if let Some(override_expr) = override_form {
        let ignored = fresh(runtime, "OVERRIDE")?;
        let pair = binding(runtime, &ignored, &override_expr)?;
        let bindings = list(runtime, std::iter::once(&pair))?;
        let declare_ignore = call(
            runtime,
            "DECLARE",
            [&call(runtime, "IGNORE", std::iter::once(&ignored))?].into_iter(),
        )?;
        call(
            runtime,
            "LET",
            [&bindings, &declare_ignore, &locally].into_iter(),
        )
    } else {
        Ok(locally)
    }
}

pub(super) fn native_call<'rt, 'arg, I>(
    runtime: &'rt Runtime,
    id: u32,
    operands: I,
) -> Expansion<'rt>
where
    'rt: 'arg,
    I: IntoIterator<Item = &'arg Root<'rt>>,
    I::IntoIter: DoubleEndedIterator,
{
    let function = runtime.root(compiled_native(id).expect("registered native function"));
    let tail = call_with_operator(runtime, &function, operands.into_iter())?;
    runtime
        .cons(&symbol(runtime, "FUNCALL")?, &tail)
        .map_err(|error| EvalError::out_of_memory(0, error))
}

// ---------------------------------------------------------------------------
// Pretty-printer macros

fn printer_primitive<'rt>(runtime: &'rt Runtime, name: &str) -> Root<'rt> {
    let function = crate::vm::Primitive::named(name).expect("registered printer primitive");
    runtime.root(function)
}

pub(super) fn pprint_logical_block<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
) -> Expansion<'rt> {
    let [spec, body @ ..] = forms else {
        return Err(invalid(
            "PPRINT-LOGICAL-BLOCK",
            "expects a stream, object, and body",
        ));
    };
    let items = proper_list(runtime, spec.clone(), "PPRINT-LOGICAL-BLOCK")?;
    let [stream_symbol, object, options @ ..] = items.as_slice() else {
        return Err(invalid(
            "PPRINT-LOGICAL-BLOCK",
            "expects a stream designator and an object",
        ));
    };
    let mut prefix = None;
    let mut per_line = None;
    let mut suffix = None;
    for pair in options.chunks(2) {
        let key = keyword_name_opt(runtime, &pair[0])
            .ok_or_else(|| invalid("PPRINT-LOGICAL-BLOCK", "expects keyword options"))?;
        let Some(value) = pair.get(1) else {
            return Err(invalid(
                "PPRINT-LOGICAL-BLOCK",
                "has an odd keyword argument list",
            ));
        };
        match key.as_str() {
            "PREFIX" => prefix = Some(value),
            "PER-LINE-PREFIX" => per_line = Some(value),
            "SUFFIX" => suffix = Some(value),
            _ => return Err(invalid("PPRINT-LOGICAL-BLOCK", "has an unknown keyword")),
        }
    }
    if prefix.is_some() && per_line.is_some() {
        return Err(invalid(
            "PPRINT-LOGICAL-BLOCK",
            "cannot take both :PREFIX and :PER-LINE-PREFIX",
        ));
    }
    let Some(stream_name) = runtime
        .symbol_name(stream_symbol)
        .filter(|_| runtime.heap.is_symbol(stream_symbol.slot.get()))
    else {
        return Err(invalid(
            "PPRINT-LOGICAL-BLOCK",
            "the stream designator must be a symbol",
        ));
    };
    let common_lisp = runtime.heap.symbol_home(stream_symbol.slot.get())
        == Some(crate::package::Package::COMMON_LISP);
    let stream_variable = match stream_name.as_str() {
        "NIL" if common_lisp => symbol(runtime, "*STANDARD-OUTPUT*")?,
        "T" if common_lisp => symbol(runtime, "*TERMINAL-IO*")?,
        _ => stream_symbol.clone(),
    };
    let start = printer_primitive(runtime, "%PPRINT-BLOCK-START");
    let end = printer_primitive(runtime, "%PPRINT-BLOCK-END");
    let check = printer_primitive(runtime, "%PPRINT-LENGTH-CHECK");

    let target = fresh(runtime, "TARGET")?;
    let object_var = fresh(runtime, "OBJECT")?;
    let prefix_var = fresh(runtime, "PREFIX")?;
    let per_line_var = fresh(runtime, "PER-LINE-PREFIX")?;
    let suffix_var = fresh(runtime, "SUFFIX")?;
    let state = fresh(runtime, "STATE")?;
    let list_var = fresh(runtime, "LIST")?;
    let buffer = fresh(runtime, "BUFFER")?;
    let counter = fresh(runtime, "COUNTER")?;
    let block = fresh(runtime, "BLOCK")?;
    let depth = runtime.root(
        crate::vm::format::pprint::logical_block_depth_symbol(runtime)
            .expect("initialized printer depth"),
    );
    let nil = runtime.nil();
    let zero = runtime.fixnum(0).expect("zero is a fixnum");
    let supplied = |present| runtime.root(if present { Word::T } else { Word::NIL });

    let apply_check = call(
        runtime,
        "FUNCALL",
        [&check, &list_var, &buffer, &counter].into_iter(),
    )?;
    let return_nil = call(runtime, "RETURN-FROM", [&block, &nil].into_iter())?;
    let pop = if object.slot.get() == Word::NIL {
        nil.clone()
    } else {
        call(runtime, "POP", std::iter::once(&list_var))?
    };
    let pop_form = call(runtime, "IF", [&apply_check, &pop, &return_nil].into_iter())?;
    let exhausted = call(runtime, "NULL", std::iter::once(&list_var))?;
    let exit_form = call(runtime, "WHEN", [&exhausted, &return_nil].into_iter())?;
    let pop_name = symbol(runtime, "PPRINT-POP")?;
    let pop_quoted = quote(runtime, &pop_form)?;
    let pop_definition = list(runtime, [&pop_name, &nil, &pop_quoted].into_iter())?;
    let exit_name = symbol(runtime, "PPRINT-EXIT-IF-LIST-EXHAUSTED")?;
    let exit_quoted = quote(runtime, &exit_form)?;
    let exit_definition = list(runtime, [&exit_name, &nil, &exit_quoted].into_iter())?;
    let definitions = list(runtime, [&pop_definition, &exit_definition].into_iter())?;

    let finish = call(
        runtime,
        "FUNCALL",
        [&end, &state, &suffix_var, &supplied(suffix.is_some())].into_iter(),
    )?;
    let protected = call(
        runtime,
        "UNWIND-PROTECT",
        [&progn(runtime, body.to_vec())?, &finish].into_iter(),
    )?;
    let body = call(runtime, "MACROLET", [&definitions, &protected].into_iter())?;
    let block_form = call(runtime, "BLOCK", [&block, &body].into_iter())?;
    let stream_binding = binding(runtime, &stream_variable, &buffer)?;
    let stream_bound = list(runtime, std::iter::once(&stream_binding))?;
    let inner = call(runtime, "LET", [&stream_bound, &block_form].into_iter())?;
    let state_buffer = call(runtime, "CAR", std::iter::once(&state))?;
    let length_counter = call(runtime, "CONS", [&zero, &nil].into_iter())?;
    let block_bindings = [
        binding(runtime, &list_var, &object_var)?,
        binding(runtime, &buffer, &state_buffer)?,
        binding(runtime, &counter, &length_counter)?,
    ];
    let block_bindings = list(runtime, block_bindings.iter())?;
    let inner = call(runtime, "LET", [&block_bindings, &inner].into_iter())?;
    let next_depth = call(runtime, "1+", std::iter::once(&depth))?;
    let depth_binding = binding(runtime, &depth, &next_depth)?;
    let depth_bindings = list(runtime, std::iter::once(&depth_binding))?;
    let inner = call(runtime, "LET", [&depth_bindings, &inner].into_iter())?;
    let when_state = call(runtime, "WHEN", [&state, &inner].into_iter())?;

    let start_form = call(
        runtime,
        "FUNCALL",
        [
            &start,
            &target,
            &object_var,
            &prefix_var,
            &per_line_var,
            &supplied(prefix.is_some()),
            &supplied(per_line.is_some()),
        ]
        .into_iter(),
    )?;
    let suffix = suffix
        .cloned()
        .unwrap_or_else(|| runtime.root(cl_string(runtime, "")));
    let init_bindings = [
        binding(runtime, &target, &stream_variable)?,
        binding(runtime, &object_var, object)?,
        binding(runtime, &prefix_var, prefix.unwrap_or(&nil))?,
        binding(runtime, &per_line_var, per_line.unwrap_or(&nil))?,
        binding(runtime, &suffix_var, &suffix)?,
        binding(runtime, &state, &start_form)?,
    ];
    let_star(runtime, &init_bindings, when_state)
}

const CORE_FORMS_REGISTRY: &str = "%ZK-CORE-FORMS%";
const CORE_OPERATORS: &[&str] = &[
    "DECLAIM",
    "DEFCONSTANT",
    "DEFMACRO",
    "DEFINE-COMPILER-MACRO",
    "DEFPARAMETER",
    "DEFUN",
    "DEFVAR",
    "DESTRUCTURING-BIND",
    "HANDLER-BIND",
    "HANDLER-CASE",
    "RESTART-BIND",
    "RESTART-CASE",
    "WITH-CONDITION-RESTARTS",
];

fn core_marker<'rt>(runtime: &'rt Runtime, operator: &'static str) -> Expansion<'rt> {
    let allocation_error = |source| EvalError::out_of_memory(0, source);
    let (registry, _) = runtime.intern_package(Package::COMMON_LISP, CORE_FORMS_REGISTRY, 0)?;
    let registry = runtime.root(registry);
    let head = runtime.root(
        runtime
            .heap
            .symbol_cell(registry.slot.get(), false)
            .unwrap_or(Word::NIL),
    );
    let mut cursor = head.slot.get();
    while let Some((entry, next)) = runtime.heap.cons_fields(cursor) {
        if let Some((marker, op_sym)) = runtime.heap.cons_fields(entry)
            && runtime.heap.symbol_name(op_sym).as_deref() == Some(operator)
        {
            return Ok(runtime.root(marker));
        }
        cursor = next;
    }
    let (_, marker_word) = runtime.fresh_symbol(operator, 0)?;
    let marker = runtime.root(marker_word);
    let canonical = symbol(runtime, operator)?;
    let entry = runtime
        .cons(&marker, &canonical)
        .map_err(allocation_error)?;
    let updated = runtime.cons(&entry, &head).map_err(allocation_error)?;
    runtime
        .heap
        .set_symbol_cell(registry.slot.get(), false, updated.slot.get());
    Ok(marker)
}

pub(crate) fn core_operator(runtime: &Runtime, name: &Name) -> Option<&'static str> {
    if name.package().is_some() {
        return None;
    }
    let word = name.rooted_word()?;
    let (registry, _) = runtime
        .packages
        .borrow()
        .present(Package::COMMON_LISP, CORE_FORMS_REGISTRY)?;
    let mut cursor = runtime.heap.symbol_cell(registry, false)?;
    while let Some((entry, next)) = runtime.heap.cons_fields(cursor) {
        if let Some((marker, op_sym)) = runtime.heap.cons_fields(entry)
            && marker == word
        {
            let spelling = runtime.heap.symbol_name(op_sym)?;
            return CORE_OPERATORS.iter().copied().find(|&op| op == spelling);
        }
        cursor = next;
    }
    None
}

fn core_expansion<'rt>(
    runtime: &'rt Runtime,
    operator: &'static str,
    forms: &[Root<'rt>],
    minimum: usize,
    maximum: Option<usize>,
) -> Expansion<'rt> {
    if forms.len() < minimum || maximum.is_some_and(|maximum| forms.len() > maximum) {
        return Err(invalid(operator, "has an invalid number of operands"));
    }
    let marker = core_marker(runtime, operator)?;
    let operands = list(runtime, forms.iter())?;
    runtime
        .cons(&marker, &operands)
        .map_err(|error| EvalError::out_of_memory(0, error))
}

fn expand_return<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let nil = runtime.nil();
    match forms {
        [] => call(runtime, "RETURN-FROM", std::iter::once(&nil)),
        [value] => call(runtime, "RETURN-FROM", [&nil, value].into_iter()),
        _ => Err(invalid("RETURN", "expects zero or one value")),
    }
}

fn expand_lambda<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    if forms.is_empty() {
        return Err(invalid("LAMBDA", "expects a lambda list"));
    }
    let lambda = call(runtime, "LAMBDA", forms.iter())?;
    call(runtime, "FUNCTION", std::iter::once(&lambda))
}

fn expand_ignore_errors<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let condition = fresh(runtime, "IGNORE-ERRORS CONDITION")?;
    let body = progn(runtime, forms.to_vec())?;
    let error = symbol(runtime, "ERROR")?;
    let variables = list(runtime, std::iter::once(&condition))?;
    let nil = runtime.nil();
    let report = call(runtime, "VALUES", [&nil, &condition].into_iter())?;
    let clause = list(runtime, [&error, &variables, &report].into_iter())?;
    call(runtime, "HANDLER-CASE", [&body, &clause].into_iter())
}

fn expand_multiple_value_bind<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let [variables, values, body @ ..] = forms else {
        return Err(invalid(
            "MULTIPLE-VALUE-BIND",
            "expects variables, a values form, and body",
        ));
    };
    let variables = proper_list(runtime, variables.clone(), "MULTIPLE-VALUE-BIND")?;
    for variable in &variables {
        let Some(data) = runtime.heap.symbol_attributes(variable.slot.get()) else {
            return Err(invalid("MULTIPLE-VALUE-BIND", "expects variable names"));
        };
        if data.constant {
            return Err(invalid(
                "MULTIPLE-VALUE-BIND",
                "expects non-constant variable names",
            ));
        }
    }
    let optional = symbol(runtime, "&OPTIONAL")?;
    let rest = symbol(runtime, "&REST")?;
    let rest_variable = fresh(runtime, "MULTIPLE-VALUE-BIND REST")?;
    let mut parameters = vec![optional];
    parameters.extend(variables);
    parameters.push(rest);
    parameters.push(rest_variable.clone());
    let parameters = list(runtime, parameters.iter())?;
    let ignore = call(runtime, "IGNORE", std::iter::once(&rest_variable))?;
    let declaration = call(runtime, "DECLARE", std::iter::once(&ignore))?;
    let lambda = call(
        runtime,
        "LAMBDA",
        std::iter::once(&parameters)
            .chain(std::iter::once(&declaration))
            .chain(body.iter()),
    )?;
    let function = call(runtime, "FUNCTION", std::iter::once(&lambda))?;
    call(
        runtime,
        "MULTIPLE-VALUE-CALL",
        [&function, values].into_iter(),
    )
}

fn set_macro_function<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let ([function, name] | [function, name, _]) = args else {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "SET-MACRO-FUNCTION expects a function and a name"),
        ));
    };
    let symbol = name.slot.get();
    if !runtime.heap.is_symbol(symbol) {
        return Err(Fault::type_error(runtime, 
            symbol,
            "SYMBOL",
            EvalError::new(at, "expected a symbol as a macro name"),
        ));
    }
    let value = function.slot.get();
    if value == Word::NIL {
        runtime.heap.set_symbol_cell(symbol, true, Word::NIL);
    } else if crate::vm::is_function(runtime, value) {
        runtime.heap.set_macro_expander(symbol, value);
    } else {
        return Err(Fault::type_error(runtime, 
            value,
            "FUNCTION",
            EvalError::new(at, "expected a function or NIL"),
        ));
    }
    Ok(vec![function.clone()])
}

/// DEFCLASS is preceded by the declaration-name check required by CLHS TYPE.
fn expand_defclass<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    if let Some(name) = forms.first()
        && runtime.heap.is_symbol(name.slot.get())
    {
        crate::compile::declarations::reject_declaration(runtime, name.slot.get(), "DEFCLASS", 0)?;
    }
    defclass::expand(runtime, forms)
}

/// FORMATTER is a macro; the runtime FORMATTER primitive builds the function
/// from the literal control string.
fn expand_formatter<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let [control] = forms else {
        return Err(invalid("FORMATTER", "expects one control string"));
    };
    if !runtime.heap.is_string(control.slot.get()) {
        return Err(invalid("FORMATTER", "requires a literal control string"));
    }
    // Preserve the primitive in the compiled file, where the target runtime
    // can install the macro and primitive without sharing a function cell.
    let formatter = printer_primitive(runtime, "FORMATTER");
    call(runtime, "FUNCALL", [&formatter, control].into_iter())
}

fn expand_step<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let [form] = forms else {
        return Err(invalid("STEP", "expects one form"));
    };
    let lambda = symbol(runtime, "LAMBDA")?;
    let params = runtime.nil();
    let body = list(runtime, [&lambda, &params, form].into_iter())?;
    let function = call(runtime, "FUNCTION", std::iter::once(&body))?;
    let step = runtime.root(runtime.intern_package(Package::ZK, "%DEBUG-STEP", 0)?.0);
    list(runtime, [&step, &function].into_iter())
}

fn expand_declaim<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    core_expansion(runtime, "DECLAIM", forms, 0, None)
}

fn expand_defconstant<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    core_expansion(runtime, "DEFCONSTANT", forms, 2, Some(3))
}

fn expand_defmacro<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    core_expansion(runtime, "DEFMACRO", forms, 2, None)
}

fn expand_define_compiler_macro<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    core_expansion(runtime, "DEFINE-COMPILER-MACRO", forms, 2, None)
}

fn expand_defparameter<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    core_expansion(runtime, "DEFPARAMETER", forms, 2, Some(3))
}

fn expand_defun<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    core_expansion(runtime, "DEFUN", forms, 2, None)
}

fn expand_defvar<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    core_expansion(runtime, "DEFVAR", forms, 1, Some(3))
}

fn expand_destructuring_bind<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    core_expansion(runtime, "DESTRUCTURING-BIND", forms, 2, None)
}

fn expand_handler_bind<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let [bindings, body @ ..] = forms else {
        return Err(invalid("HANDLER-BIND", "requires a list of handlers"));
    };
    let mut initializers = Vec::new();
    let mut prepared_handlers = Vec::new();
    for binding in proper_list(runtime, bindings.clone(), "HANDLER-BIND")? {
        let parts = proper_list(runtime, binding, "HANDLER-BIND")?;
        let [condition, handler] = parts.as_slice() else {
            return Err(invalid("HANDLER-BIND", "expects (condition-type handler)"));
        };
        let variable = fresh(runtime, "HANDLER-FUNCTION")?;
        initializers.push(list(runtime, [&variable, handler].into_iter())?);
        prepared_handlers.push(list(runtime, [condition, &variable].into_iter())?);
    }

    let initializers = list(runtime, initializers.iter())?;
    let progn = call(runtime, "PROGN", body.iter())?;
    if prepared_handlers.is_empty() {
        return call(runtime, "LET", [&initializers, &progn].into_iter());
    }

    // Keep user forms in standard evaluation positions. The private form only
    // installs the prepared cluster and calls the body in its dynamic extent.
    let body_name = fresh(runtime, "HANDLER-BODY")?;
    let body_definition = list(runtime, [&body_name, &runtime.nil(), &progn].into_iter())?;
    let local_functions = list(runtime, std::iter::once(&body_definition))?;
    let body_call = list(runtime, std::iter::once(&body_name))?;
    let handler_cluster = list(runtime, prepared_handlers.iter())?;
    let kernel = core_expansion(
        runtime,
        "HANDLER-BIND",
        &[handler_cluster, body_call],
        1,
        None,
    )?;
    let body = call(runtime, "FLET", [&local_functions, &kernel].into_iter())?;
    call(runtime, "LET", [&initializers, &body].into_iter())
}

fn expand_handler_case<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    core_expansion(runtime, "HANDLER-CASE", forms, 1, None)
}

fn expand_restart_bind<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    core_expansion(runtime, "RESTART-BIND", forms, 1, None)
}

fn expand_restart_case<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    core_expansion(runtime, "RESTART-CASE", forms, 1, None)
}

fn expand_with_condition_restarts<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
) -> Expansion<'rt> {
    core_expansion(runtime, "WITH-CONDITION-RESTARTS", forms, 2, None)
}

fn cl_string(runtime: &Runtime, text: &str) -> Word {
    runtime.heap.string(text).expect("small string")
}

pub(super) fn print_unreadable_object<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
) -> Expansion<'rt> {
    let [spec, body @ ..] = forms else {
        return Err(invalid(
            "PRINT-UNREADABLE-OBJECT",
            "expects a binding list and body",
        ));
    };
    let items = proper_list(runtime, spec.clone(), "PRINT-UNREADABLE-OBJECT")?;
    let [object, stream, options @ ..] = items.as_slice() else {
        return Err(invalid(
            "PRINT-UNREADABLE-OBJECT",
            "expects an object and a stream",
        ));
    };
    let mut show_type = None;
    let mut show_identity = None;
    for pair in options.chunks(2) {
        let key = keyword_name_opt(runtime, &pair[0])
            .ok_or_else(|| invalid("PRINT-UNREADABLE-OBJECT", "expects :TYPE or :IDENTITY"))?;
        let Some(value) = pair.get(1) else {
            return Err(invalid(
                "PRINT-UNREADABLE-OBJECT",
                "has an odd keyword argument list",
            ));
        };
        match key.as_str() {
            "TYPE" => show_type = Some(value),
            "IDENTITY" => show_identity = Some(value),
            _ => {
                return Err(invalid("PRINT-UNREADABLE-OBJECT", "has an unknown keyword"));
            }
        }
    }
    let print_unreadable = printer_primitive(runtime, "%PRINT-UNREADABLE-OBJECT");
    let nil = runtime.nil();
    let thunk = if body.is_empty() {
        nil.clone()
    } else {
        let lambda = call(runtime, "LAMBDA", std::iter::once(&nil).chain(body))?;
        call(runtime, "FUNCTION", std::iter::once(&lambda))?
    };
    call(
        runtime,
        "FUNCALL",
        [
            &print_unreadable,
            object,
            stream,
            show_type.unwrap_or(&nil),
            show_identity.unwrap_or(&nil),
            &thunk,
        ]
        .into_iter(),
    )
}
