use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::mem::DropGuard;
use std::ops::Range;
use std::rc::Rc;
#[cfg(test)]
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;

use self::tagbody::TagScope;
use crate::condition::{Class, Test};
use crate::core_ir::{InlineDeclaration, OptimizationQualities, OptimizationQuality};
use crate::heap::{RootSlot, Word};
use crate::macros::{CallExpander, ExpansionKind, core_operator};
use crate::read::{Form, FormKind};
use crate::symbol::{Name, NameKey, Package};
use crate::vm::{
    Arity, Callee, CaptureKind, CellLocation, ConstantTemplate, FunctionTemplate, Handler,
    HandlerTable, HandlerTarget, Immediate, Instruction as Ins, Primitive, Register, RestartSpec,
    RestartTarget,
};
use crate::{EvalError, Root, Runtime};

pub(crate) mod constants;
mod core_lower;
pub(crate) mod declarations;
mod destructure;
mod diagnostics;
mod lambda;
mod macro_definition;
mod multiple_values;
mod restart;
mod symbol_macros;
mod tagbody;
mod top_level;
mod type_checks;
pub(crate) mod vop;

pub(crate) use crate::macros::EXPANSION_LIMIT;
pub(crate) use diagnostics::{raise_residual_macro_error, residual_macro_error};
pub(crate) use top_level::FileCompiler;

fn enter_macro_expansion(depth: Rc<Cell<usize>>, at: usize) -> Result<impl Drop, EvalError> {
    let previous = depth.get();
    if previous >= EXPANSION_LIMIT {
        return Err(EvalError::macro_expansion(
            at,
            "macro expansion depth exceeded",
        ));
    }
    depth.set(previous + 1);
    Ok(DropGuard::new(depth, move |depth| depth.set(previous)))
}

pub(crate) fn expand_global(
    runtime: &Runtime,
    form: &Form,
    call_expander: Option<&CallExpander<'_>>,
) -> Result<Option<Form>, EvalError> {
    if let Some(form) = crate::form_value::refresh_reader_syntax(runtime, form)? {
        return Ok(Some(form));
    }
    if macro_head(form).is_some_and(|name| has_syntax_binding(name.operator())) {
        return Ok(None);
    }
    let expandable = if let Some(name) = form.name() {
        crate::macros::global_symbol_expander(runtime, name).is_some()
    } else {
        macro_head(form).is_some_and(|name| crate::macros::has_expander(runtime, name))
    };
    if !expandable {
        return Ok(None);
    }
    expand_in_environment(
        runtime,
        form,
        &runtime.nil(),
        ExpansionKind::MacroForm,
        call_expander,
    )
    .map_err(|error| error.during_macro_expansion(form.at))
}

fn macro_head(form: &Form) -> Option<&Name> {
    match &form.kind {
        FormKind::List(forms, _) => forms.first()?.name(),
        _ => None,
    }
}

/// Operators from HyperSpec §3.1.2.1.2.1 that the compiler implements.
pub(crate) fn is_special_operator(operator: &str) -> bool {
    matches!(
        operator,
        "BLOCK"
            | "CATCH"
            | "EVAL-WHEN"
            | "FLET"
            | "FUNCTION"
            | "GO"
            | "IF"
            | "LABELS"
            | "LET"
            | "LET*"
            | "LOAD-TIME-VALUE"
            | "LOCALLY"
            | "MACROLET"
            | "MULTIPLE-VALUE-CALL"
            | "MULTIPLE-VALUE-PROG1"
            | "PROGN"
            | "PROGV"
            | "QUOTE"
            | "RETURN-FROM"
            | "SETQ"
            | "SYMBOL-MACROLET"
            | "TAGBODY"
            | "THE"
            | "THROW"
            | "UNWIND-PROTECT"
    )
}

pub(crate) fn has_syntax_binding(operator: &str) -> bool {
    // These standard macros are lowered directly by the compiler. Their
    // MACRO-FUNCTION expanders exist for MACROEXPAND and introspective callers.
    is_special_operator(operator)
        || matches!(
            operator,
            "DECLAIM"
                | "DEFCONSTANT"
                | "DEFMACRO"
                | "DEFINE-COMPILER-MACRO"
                | "DEFPARAMETER"
                | "DEFUN"
                | "DEFVAR"
                | "DESTRUCTURING-BIND"
                | "HANDLER-BIND"
                | "HANDLER-CASE"
                | "IGNORE-ERRORS"
                | "LAMBDA"
                | "MULTIPLE-VALUE-BIND"
                | "RESTART-BIND"
                | "RESTART-CASE"
                | "RETURN"
                | "WITH-CONDITION-RESTARTS"
        )
}

fn expand_in_environment(
    runtime: &Runtime,
    form: &Form,
    environment: &Root<'_>,
    kind: ExpansionKind,
    call_expander: Option<&CallExpander<'_>>,
) -> Result<Option<Form>, EvalError> {
    let object = crate::form_value::to_value(runtime, form)?;
    let expansion = crate::macros::expand_once_with(
        runtime,
        &object,
        environment,
        form.at,
        kind,
        call_expander,
    )?;
    expansion
        .as_ref()
        .map(|value| {
            crate::form_value::from_value(runtime, value, form.at)
                .map(|expanded| expanded.macro_expansion(form))
        })
        .transpose()
}

fn expand_compiler_macro_in_environment(
    runtime: &Runtime,
    expander: &Root<'_>,
    form: &Form,
    environment: &Root<'_>,
    kind: ExpansionKind,
    call_expander: Option<&CallExpander<'_>>,
) -> Result<Option<Form>, EvalError> {
    let object = crate::form_value::to_value(runtime, form)?;
    let expansion = crate::macros::expand_compiler_macro_once(
        runtime,
        expander,
        &object,
        environment,
        form.at,
        kind,
        call_expander,
    )?;
    expansion
        .as_ref()
        .map(|value| {
            crate::form_value::from_value(runtime, value, form.at)
                .map(|expanded| expanded.macro_expansion(form))
        })
        .transpose()
}

/// A compile-time diagnostic that does not stop compilation. The evaluation
/// and file-compilation entries signal it as a `WARNING` condition in the
/// caller's dynamic environment.
#[derive(Clone)]
pub(crate) struct Warning {
    pub(crate) at: usize,
    pub(crate) message: String,
    pub(crate) style: bool,
}

pub(crate) fn compile(
    runtime: &Runtime,
    form: &Form,
    call_expander: Option<&CallExpander<'_>>,
) -> Result<(FunctionTemplate, Vec<Warning>), EvalError> {
    compile_with_feedback(runtime, form, call_expander, false, None)
}

pub(crate) fn compile_with_feedback(
    runtime: &Runtime,
    form: &Form,
    call_expander: Option<&CallExpander<'_>>,
    warn_undefined_calls: bool,
    document: Option<&crate::SourceDocument>,
) -> Result<(FunctionTemplate, Vec<Warning>), EvalError> {
    let mut compiler = Compiler::new(
        runtime,
        Vec::new(),
        CompilationContext {
            call_expander,
            warn_undefined_calls,
            document: document.cloned(),
            ..CompilationContext::default()
        },
    );
    compiler.record_form_sources(form);
    compiler.is_top_level = true;
    compiler.code.debug.entry = Some(form.at);
    if let Some(package) = runtime.bound_package() {
        compiler.code.debug.reader_package =
            Some(compiler.add_constant(ConstantTemplate::Immediate(package.word())) as u32);
    }
    compiler.debug_level = compiler.optimization_qualities().debug;
    compiler.code.debug.retain_frame = compiler.debug_level == 3;
    compiler.begin_top_level_ltv();
    compiler
        .top_form(form, Destination::All)
        .map_err(|error| error.at_form(form))?;
    compiler.finalize_top_level_ltv()?;
    let warnings = compiler.context.warnings.take();
    Ok((compiler.finish(), warnings))
}

pub(crate) fn compile_lambda(
    runtime: &Runtime,
    parameters: &Form,
    body: &[Form],
    at: usize,
) -> Result<(FunctionTemplate, Vec<Warning>), EvalError> {
    compile_lambda_with(runtime, parameters, body, at, None, &[])
}

pub(crate) fn compile_lambda_with(
    runtime: &Runtime,
    parameters: &Form,
    body: &[Form],
    at: usize,
    call_expander: Option<&CallExpander<'_>>,
    local_macros: &[(Name, Word)],
) -> Result<(FunctionTemplate, Vec<Warning>), EvalError> {
    let outer = local_macros
        .iter()
        .map(|(name, expander)| ScopedBinding {
            name: name.clone(),
            namespace: Namespace::Macro,
            binding: None,
            expander: Some(Rc::new(runtime.root_slot(*expander))),
            used: None,
        })
        .collect();
    let mut compiler = Compiler::new(
        runtime,
        outer,
        CompilationContext {
            call_expander,
            ..CompilationContext::default()
        },
    );
    compiler.record_form_sources(parameters);
    for form in body {
        compiler.record_form_sources(form);
    }
    let function = compiler.lower_function(parameters, body, None, None, at)?;
    function.run_ltv_inits()?;
    let warnings = function.context.warnings.take();
    Ok((function.finish(), warnings))
}

pub(crate) fn generic_required_parameters(
    runtime: &Runtime,
    lambda_list: &Root<'_>,
    at: usize,
) -> Result<Vec<Name>, EvalError> {
    let lambda_list = crate::form_value::from_value(runtime, lambda_list, at)?;
    lambda::generic_required_parameters(&lambda_list)
}

pub(crate) struct CallSignature {
    pub(crate) required: Vec<Name>,
    pub(crate) optional: Vec<Name>,
    pub(crate) rest: Option<Name>,
    pub(crate) keywords: Option<Vec<Name>>,
    pub(crate) allow_other_keys: bool,
}

pub(crate) fn call_signature(
    runtime: &Runtime,
    lambda_list: &Root<'_>,
    at: usize,
) -> Result<CallSignature, EvalError> {
    let lambda_list = crate::form_value::from_value(runtime, lambda_list, at)?;
    lambda::call_signature(&lambda_list)
}

#[derive(Clone, Copy)]
enum Destination {
    One(Register),
    All,
}

/// A register holding a binding and the shared decision to box that binding.
#[derive(Clone)]
struct RegisterBinding {
    // Every visible binding belongs to this function or a lexical ancestor.
    frame_depth: u32,
    reg: Register,
    boxed: Rc<Cell<bool>>,
    capture_kind: CaptureKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Namespace {
    Variable,
    Special,
    SpecialReference,
    SymbolMacro,
    Function,
    Setter,
    Macro,
    Block,
    InlineDeclaration {
        setf: bool,
        declaration: InlineDeclaration,
    },
}

impl Namespace {
    fn is_variable(self) -> bool {
        matches!(
            self,
            Self::Variable | Self::Special | Self::SpecialReference | Self::SymbolMacro
        )
    }

    fn binds_function(self, setf: bool) -> bool {
        if setf {
            self == Self::Setter
        } else {
            matches!(self, Self::Function | Self::Macro)
        }
    }
}

/// A scoped name with its binding, expansion, or declaration.
#[derive(Clone)]
struct ScopedBinding {
    name: Name,
    namespace: Namespace,
    binding: Option<RegisterBinding>,
    expander: Option<Rc<RootSlot>>,
    used: Option<Rc<Cell<bool>>>,
}

#[derive(Clone, Copy)]
struct ScopeMark {
    bindings: usize,
    debug_variables: usize,
    custom_declarations: usize,
    declarations: usize,
}

struct VariableTypeDeclaration {
    name: Name,
    binding_index: Option<usize>,
    spec: Form,
    is_free: bool,
}

struct FunctionTypeDeclaration {
    name: Name,
    setf: bool,
    binding_index: Option<usize>,
    spec: Form,
}

struct DeclarationScope {
    qualities: Option<OptimizationQualities>,
    variable_types: Vec<VariableTypeDeclaration>,
    function_types: Vec<FunctionTypeDeclaration>,
}

#[derive(Clone)]
enum BindingLocation {
    Local(RegisterBinding),
    Capture(usize),
}

struct LoadTimeValueInit {
    cell: Rc<RootSlot>,
    thunk: FunctionTemplate,
    at: usize,
}

pub(crate) type FileConstants = Rc<RefCell<HashMap<NameKey, Form>>>;

#[derive(Default)]
struct GlobalDeclarations {
    constants: FileConstants,
    inline_declarations: RefCell<HashMap<(NameKey, bool), InlineDeclaration>>,
    pending_qualities: Cell<Option<OptimizationQualities>>,
    pending_types: RefCell<HashMap<NameKey, Form>>,
    function_types: RefCell<HashMap<(NameKey, bool), Form>>,
    inline_functions: RefCell<HashMap<NameKey, Rc<crate::core_ir::CoreFn>>>,
    transforms: RefCell<HashMap<NameKey, vop::TransformRule>>,
    aarch64_popcount: RefCell<HashMap<usize, vop::Aarch64Template>>,
}

/// State shared by the compiler for a form and the compilers it creates.
#[derive(Clone, Default)]
struct CompilationContext<'a> {
    call_expander: Option<&'a CallExpander<'a>>,
    warn_undefined_calls: bool,
    specials: Rc<RefCell<HashSet<NameKey>>>,
    declarations: Rc<GlobalDeclarations>,
    warnings: Rc<RefCell<Vec<Warning>>>,
    expansion_depth: Rc<Cell<usize>>,
    source_forms: Rc<RefCell<FxHashMap<usize, crate::read::FormSource>>>,
    document: Option<crate::SourceDocument>,
    #[cfg(test)]
    phase_timings: Option<Rc<FileCompileTimings>>,
}

#[cfg(test)]
#[derive(Default)]
struct FileCompileTimings {
    expansion: Cell<Duration>,
    evaluation: Cell<Duration>,
}

#[cfg(test)]
impl FileCompileTimings {
    fn measure(total: &Cell<Duration>) -> impl Drop + '_ {
        let started = Instant::now();
        DropGuard::new(total, move |total| total.set(total.get() + started.elapsed()))
    }
}

struct Compiler<'a> {
    frame_depth: u32,
    runtime: &'a Runtime,
    context: CompilationContext<'a>,
    ltv_inits: Rc<RefCell<Vec<LoadTimeValueInit>>>,
    ltv_start_pc: Option<usize>,
    is_top_level: bool,
    in_runtime_handler_scope: bool,
    code: FunctionTemplate,
    transform_constants: HashMap<NameKey, (usize, usize)>,
    used: usize,
    bindings: Vec<ScopedBinding>,
    outer: Vec<ScopedBinding>,
    declaration_scopes: Vec<Rc<DeclarationScope>>,
    custom_declarations: Vec<(Name, Rc<RootSlot>)>,
    tags: Vec<TagScope>,
    outer_tags: Vec<TagScope>,
    captures: Vec<RegisterBinding>,
    boxing: Vec<(usize, Rc<Cell<bool>>, Ins)>,
    pending_nops: Vec<usize>,
    debug_level: u8,
    open_debug_variables: Vec<usize>,
    debug_boxing: Vec<(usize, Rc<Cell<bool>>)>,
}

impl<'a> Compiler<'a> {
    fn new(
        runtime: &'a Runtime,
        outer: Vec<ScopedBinding>,
        context: CompilationContext<'a>,
    ) -> Self {
        Self {
            frame_depth: 0,
            runtime,
            context,
            ltv_inits: Rc::default(),
            ltv_start_pc: None,
            is_top_level: false,
            in_runtime_handler_scope: false,
            code: FunctionTemplate {
                name: None,
                debug: Default::default(),
                instructions: Vec::new(),
                constants: Vec::new(),
                lambda_list: None,
                registers: 0,
                arity: Arity::ZERO,
                captures: Vec::new(),
                notinline: Vec::new(),
                vop_calls: Vec::new(),
                handlers: Vec::new(),
                restarts: Vec::new(),
                keywords: Vec::new(),
                jump_tables: Vec::new(),
                facts: Default::default(),
            },
            transform_constants: HashMap::new(),
            used: 0,
            bindings: Vec::new(),
            outer,
            declaration_scopes: Vec::new(),
            custom_declarations: Vec::new(),
            tags: Vec::new(),
            outer_tags: Vec::new(),
            captures: Vec::new(),
            boxing: Vec::new(),
            pending_nops: Vec::new(),
            debug_level: 0,
            open_debug_variables: Vec::new(),
            debug_boxing: Vec::new(),
        }
    }

    fn nested(&self, outer: Vec<ScopedBinding>) -> Self {
        let mut compiler = Self::new(self.runtime, outer, self.context.clone());
        compiler.frame_depth = self.frame_depth + 1;
        compiler
            .custom_declarations
            .clone_from(&self.custom_declarations);
        compiler
            .declaration_scopes
            .clone_from(&self.declaration_scopes);
        compiler
    }

    fn record_form_sources(&self, form: &Form) {
        fn record(form: &Form, sources: &mut FxHashMap<usize, crate::read::FormSource>) {
            if form.source.range.is_some() {
                sources.insert(form.at, form.source.clone());
            }
            if let FormKind::List(items, tail) = &form.kind {
                for child in items.iter().chain(tail.iter().map(Box::as_ref)) {
                    record(child, sources);
                }
            }
        }
        record(form, &mut self.context.source_forms.borrow_mut());
    }

    fn finish(mut self) -> FunctionTemplate {
        let sources = self.context.source_forms.borrow();
        let mut used: Vec<_> = self
            .code
            .instructions
            .iter()
            .filter_map(Ins::source_offset)
            .chain(self.code.debug.entry)
            .collect();
        used.sort_unstable();
        used.dedup();
        self.code.debug.sources = used
            .into_iter()
            .filter_map(|at| sources.get(&at).cloned())
            .collect();
        drop(sources);
        for variable in &mut self.code.debug.variables {
            variable.live.end = variable.live.end.min(self.code.instructions.len());
        }
        for (index, boxed) in self.debug_boxing {
            if let zkcl_bytecode::DebugVariableLocation::Register { boxed: field, .. } =
                &mut self.code.debug.variables[index].location
            {
                *field = boxed.get();
            }
        }
        // A nested compiler can mark a binding after its declaration or earlier uses
        // were emitted. Lower those instructions only when the lexical scope is known.
        for (pc, boxed, instruction) in self.boxing {
            if boxed.get() {
                self.code.instructions[pc] = instruction;
            }
        }
        self.pending_nops.retain(
            |&pc| matches!(&self.code.instructions[pc], Ins::Clear(range) if range.is_empty()),
        );
        if let Some(&first_nop) = self.pending_nops.first() {
            let positions = if self.pending_nops.len() == 1 {
                // The unused LOAD-TIME-VALUE entry slot is usually the only
                // placeholder in a top-level form.
                self.code.instructions.remove(first_nop);
                None
            } else {
                let mut positions = Vec::with_capacity(self.code.instructions.len() + 1);
                // Placeholders are emitted in PC order. Between two of them the
                // relocation is a constant offset, including at the end of code.
                let mut start = 0;
                for (removed, &nop) in self.pending_nops.iter().enumerate() {
                    positions.extend((start..=nop).map(|pc| pc - removed));
                    start = nop + 1;
                }
                let removed = self.pending_nops.len();
                positions.extend((start..=self.code.instructions.len()).map(|pc| pc - removed));
                Some(positions)
            };
            let relocate = |pc: usize| match &positions {
                Some(positions) => positions[pc],
                None => pc - usize::from(pc > first_nop),
            };
            let relocate_instruction = |instruction: &mut Ins| match instruction {
                Ins::Jump(target)
                | Ins::Unless(_, target)
                | Ins::IfPresent(_, target)
                | Ins::UnlessFixnum(_, target)
                | Ins::UnlessFixnumInRange(_, _, _, target)
                | Ins::WhenBound { jump: target, .. }
                | Ins::EnterBlock { resume: target, .. }
                | Ins::EnterCatch { resume: target, .. }
                | Ins::EnterProtect {
                    cleanup: target, ..
                } => {
                    *target = relocate(*target);
                }
                _ => {}
            };
            if let Some(positions) = &positions {
                let mut old_pc = 0;
                self.code.instructions.retain_mut(|instruction| {
                    old_pc += 1;
                    let keep = positions[old_pc - 1] != positions[old_pc];
                    if keep {
                        relocate_instruction(instruction);
                    }
                    keep
                });
            } else {
                self.code
                    .instructions
                    .iter_mut()
                    .for_each(relocate_instruction);
            }
            for table in &mut self.code.handlers {
                for handler in &mut table.handlers {
                    if let HandlerTarget::Case { pc, .. } = &mut handler.target {
                        *pc = relocate(*pc);
                    }
                }
            }
            for table in &mut self.code.restarts {
                for spec in &mut table.restarts {
                    if let RestartTarget::Case { pc, .. } = &mut spec.target {
                        *pc = relocate(*pc);
                    }
                }
            }
            for table in &mut self.code.jump_tables {
                for target in &mut table.targets {
                    *target = relocate(*target);
                }
            }
            for variable in &mut self.code.debug.variables {
                variable.live = relocate(variable.live.start)..relocate(variable.live.end);
            }
            for vop in &mut self.code.vop_calls {
                vop.pc = relocate(vop.pc);
            }
        }
        self.code
    }

    fn emit_nop(&mut self) -> usize {
        let pc = self.emit(Ins::Clear(0..0));
        self.pending_nops.push(pc);
        pc
    }

    fn if_boxed(&mut self, binding: &RegisterBinding, normal: Ins, boxed: Ins) {
        if binding.boxed.get() {
            self.emit(boxed);
        } else {
            let pc = self.emit(normal);
            if binding.capture_kind == CaptureKind::MutableCell {
                self.boxing.push((pc, Rc::clone(&binding.boxed), boxed));
            }
        }
    }

    fn box_binding_if_captured(&mut self, binding: &RegisterBinding, at: usize) {
        if binding.capture_kind == CaptureKind::MutableCell {
            let boxed = Ins::BoxLocal {
                reg: binding.reg,
                at,
            };
            if binding.boxed.get() {
                self.emit(boxed);
            } else {
                let pc = self.emit_nop();
                self.boxing.push((pc, Rc::clone(&binding.boxed), boxed));
            }
        }
    }

    fn declare(
        &mut self,
        name: &Name,
        namespace: Namespace,
        reg: Register,
        at: usize,
    ) -> ScopedBinding {
        let binding = RegisterBinding {
            frame_depth: self.frame_depth,
            reg,
            capture_kind: if namespace == Namespace::Block {
                CaptureKind::ExitToken
            } else {
                CaptureKind::MutableCell
            },
            boxed: Rc::new(Cell::new(false)),
        };
        self.box_binding_if_captured(&binding, at);
        if namespace == Namespace::Variable {
            self.debug_local(name, &binding, at);
        }
        ScopedBinding {
            name: name.clone(),
            namespace,
            binding: Some(binding),
            expander: None,
            used: None,
        }
    }

    fn debug_local(&mut self, name: &Name, binding: &RegisterBinding, at: usize) {
        if self.debug_level == 0 {
            return;
        }
        let symbol = self.symbol(name, at) as u32;
        let index = self.code.debug.variables.len();
        self.open_debug_variables.push(index);
        self.debug_boxing.push((index, Rc::clone(&binding.boxed)));
        self.code
            .debug
            .variables
            .push(zkcl_bytecode::DebugVariable {
                name: symbol,
                live: self.code.instructions.len()..usize::MAX,
                location: zkcl_bytecode::DebugVariableLocation::Register {
                    index: binding.reg,
                    boxed: false,
                },
            });
    }

    fn debug_capture(&mut self, name: &Name, capture: usize, at: usize) {
        use zkcl_bytecode::DebugVariableLocation::Capture;
        if self.debug_level == 0
            || self
                .code
                .debug
                .variables
                .iter()
                .any(|variable| variable.location == Capture(capture))
        {
            return;
        }
        let name = self.symbol(name, at) as u32;
        self.code
            .debug
            .variables
            .push(zkcl_bytecode::DebugVariable {
                name,
                live: 0..usize::MAX,
                location: Capture(capture),
            });
    }

    pub(super) fn is_constant_variable(&self, name: &Name) -> bool {
        constants::is_constant_name(self.runtime, name)
            || self
                .context
                .declarations
                .constants
                .borrow()
                .contains_key(name.key())
    }

    pub(super) fn ensure_writable_variable(
        &self,
        name: &Name,
        at: usize,
        action: &str,
    ) -> Result<(), EvalError> {
        if self.is_constant_variable(name) {
            return Err(EvalError::new(
                at,
                format!("cannot {action} constant variable {}", name.spelling()),
            )
            .with_condition(Class::ProgramError));
        }
        Ok(())
    }

    pub(super) fn begin_top_level_ltv(&mut self) {
        let pc = self.emit_nop();
        self.ltv_start_pc = Some(pc);
    }

    pub(super) fn finalize_top_level_ltv(&mut self) -> Result<(), EvalError> {
        let Some(start_pc) = self.ltv_start_pc.take() else {
            return Ok(());
        };
        let inits = std::mem::take(&mut *self.ltv_inits.borrow_mut());
        if inits.is_empty() {
            return Ok(());
        }
        let done_jump = self.emit(Ins::Jump(0));
        let init_pc = self.code.instructions.len();
        self.code.instructions[start_pc] = Ins::Jump(init_pc);
        let rplaca = Primitive::named("RPLACA").expect("RPLACA primitive");
        let rplaca_id = self.add_constant(ConstantTemplate::Immediate(rplaca));
        let mark = self.used;
        let rplaca_fn = self.reserve(1).start;
        self.emit(Ins::Constant(rplaca_fn, rplaca_id));
        for LoadTimeValueInit { cell, thunk, at } in inits {
            let call_mark = self.used;
            let callee = self.reserve(1).start;
            let val_reg = self.reserve(1).start;
            let thunk_id = self.add_constant(ConstantTemplate::Function(Box::new(thunk)));
            self.emit(Ins::Constant(callee, thunk_id));
            self.call_registers(
                Callee::Value(callee),
                0..0,
                Destination::One(val_reg),
                at,
                false,
                None,
            );
            let pair_args = self.reserve(2);
            let cell_id = self.add_constant(ConstantTemplate::LoadTimeCell(cell));
            self.emit(Ins::Constant(pair_args.start, cell_id));
            self.emit(Ins::Copy(pair_args.start + 1, val_reg));
            let discard = self.reserve(1).start;
            self.call_registers(
                Callee::Value(rplaca_fn),
                pair_args,
                Destination::One(discard),
                at,
                false,
                None,
            );
            self.release(call_mark);
        }
        self.release(mark);
        self.emit(Ins::Jump(start_pc + 1));
        let end_pc = self.code.instructions.len();
        self.code.instructions[done_jump] = Ins::Jump(end_pc);
        Ok(())
    }

    pub(super) fn run_ltv_inits(&self) -> Result<(), EvalError> {
        let inits = std::mem::take(&mut *self.ltv_inits.borrow_mut());
        for LoadTimeValueInit { cell, thunk, at } in inits {
            let value = self.run_during_compilation(thunk, at)?;
            self.runtime.heap.set_cons_car(cell.get(), value);
        }
        Ok(())
    }

    fn run_during_compilation(&self, code: FunctionTemplate, at: usize) -> Result<Word, EvalError> {
        #[cfg(test)]
        let _timing = self
            .context
            .phase_timings
            .as_ref()
            .map(|timings| FileCompileTimings::measure(&timings.evaluation));
        let function = self
            .runtime
            .root(code.instantiate_with_source(self.runtime, self.context.document.as_ref())?);
        if let Some(call) = self.context.call_expander {
            return call(&function, &[], at, ExpansionKind::Other);
        }
        let values = crate::vm::run(
            self.runtime,
            function.slot.get(),
            &crate::heap::RootSlots::empty(),
            0..0,
        )?;
        Ok(values.first().map_or(Word::NIL, |value| value.slot.get()))
    }

    pub(super) fn compile_load_time_value(
        &mut self,
        args: &[Form],
        at: usize,
    ) -> Result<(Rc<RootSlot>, bool), EvalError> {
        let (form, read_only_p) = match args {
            [form] => (form, None),
            [form, read_only_p] => (form, Some(read_only_p)),
            _ => {
                return Err(EvalError::compile(
                    at,
                    "LOAD-TIME-VALUE expects 1 or 2 arguments",
                ));
            }
        };
        let read_only = match read_only_p {
            None => false,
            Some(flag) => {
                !matches!(&flag.kind, FormKind::Name(name) if name.operator() == "NIL")
                    && !matches!(&flag.kind, FormKind::List(items, None) if items.is_empty())
                    && !matches!(&flag.kind, FormKind::Literal(root) if root.get() == Word::NIL)
            }
        };
        let nil = self.runtime.nil();
        let cons = self
            .runtime
            .cons(&nil, &nil)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        let cell = Rc::new(cons.slot);
        // Compile in a null lexical environment while sharing global declarations
        // and file-level inline definitions.
        let mut thunk = Self::new(self.runtime, Vec::new(), self.context.clone());
        thunk.begin_top_level_ltv();
        thunk.form(form, Destination::All)?;
        thunk.finalize_top_level_ltv()?;
        self.ltv_inits.borrow_mut().push(LoadTimeValueInit {
            cell: Rc::clone(&cell),
            thunk: thunk.finish(),
            at,
        });
        Ok((cell, read_only))
    }

    pub(super) fn emit_load_time_value_read(
        &mut self,
        cell: &Rc<RootSlot>,
        dest: Destination,
        at: usize,
    ) {
        let car = Primitive::named("CAR").expect("CAR primitive");
        let car_id = self.add_constant(ConstantTemplate::Immediate(car));
        let cell_id = self.add_constant(ConstantTemplate::LoadTimeCell(Rc::clone(cell)));
        let mark = self.used;
        let callee = self.reserve(1).start;
        let args = self.reserve(1);
        self.emit(Ins::Constant(callee, car_id));
        self.emit(Ins::Constant(args.start, cell_id));
        self.call_registers(Callee::Value(callee), args, dest, at, false, None);
        self.release(mark);
    }

    fn bind_variable(
        &mut self,
        name: &Name,
        reg: Register,
        at: usize,
        special: &[&Name],
        safety: u8,
    ) -> Result<usize, EvalError> {
        self.ensure_writable_variable(name, at, "bind")?;
        if self.is_special_var(name, special) {
            if safety > 0 {
                self.emit_proclaimed_type_check(name, reg, at)?;
            }
            let symbol = self.symbol(name, at);
            self.emit(Ins::BindSpecial { symbol, value: reg });
            self.push_special(name, true);
            Ok(1)
        } else {
            let scope = self.declare(name, Namespace::Variable, reg, at);
            self.bindings.push(scope);
            Ok(0)
        }
    }

    fn is_special_var(&self, name: &Name, local_specials: &[&Name]) -> bool {
        local_specials.contains(&name)
            || self.context.specials.borrow().contains(name.key())
            || self.runtime.is_special(name)
    }

    fn emit_proclaimed_type_check(
        &mut self,
        name: &Name,
        reg: Register,
        at: usize,
    ) -> Result<(), EvalError> {
        if let Some(spec) = self.proclaimed_var_type(name) {
            self.emit_variable_type_checks(reg, [&spec], at)?;
        }
        Ok(())
    }

    fn push_special(&mut self, name: &Name, bound: bool) {
        self.bindings.push(ScopedBinding {
            name: name.clone(),
            namespace: if bound {
                Namespace::Special
            } else {
                Namespace::SpecialReference
            },
            binding: None,
            expander: None,
            used: None,
        });
    }

    fn mark_scope(&self) -> ScopeMark {
        ScopeMark {
            bindings: self.bindings.len(),
            debug_variables: self.open_debug_variables.len(),
            declarations: self.declaration_scopes.len(),
            custom_declarations: self.custom_declarations.len(),
        }
    }

    fn restore_scope(&mut self, mark: ScopeMark) {
        for index in self.open_debug_variables.drain(mark.debug_variables..) {
            self.code.debug.variables[index].live.end = self.code.instructions.len();
        }
        self.bindings.truncate(mark.bindings);
        self.declaration_scopes.truncate(mark.declarations);
        self.custom_declarations.truncate(mark.custom_declarations);
    }

    fn with_lexical_scope<T>(&mut self, compile: impl FnOnce(&mut Self, ScopeMark) -> T) -> T {
        let mark = self.mark_scope();
        let mut compiler = DropGuard::new(self, move |compiler| compiler.restore_scope(mark));
        compile(&mut compiler, mark)
    }

    fn optimization_qualities(&self) -> OptimizationQualities {
        self.declaration_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.qualities)
            .or(self.context.declarations.pending_qualities.get())
            .unwrap_or_else(|| self.runtime.optimization_qualities.get())
    }

    fn find_binding_index(
        &self,
        name: &Name,
        in_namespace: impl Fn(Namespace) -> bool,
    ) -> Option<usize> {
        let matches = |scope: &ScopedBinding| &scope.name == name && in_namespace(scope.namespace);
        if let Some(pos) = self.bindings.iter().rposition(matches) {
            Some(self.outer.len() + pos)
        } else {
            self.outer.iter().rposition(matches)
        }
    }

    fn find_var_binding_index(&self, name: &Name) -> Option<usize> {
        self.find_binding_index(name, Namespace::is_variable)
    }

    fn current_var_types(&self, name: &Name, only_free: bool) -> Vec<Form> {
        let target_idx = self.find_var_binding_index(name);
        let special = target_idx.is_none_or(|index| {
            let binding = self.scopes().nth(index).expect("lexical binding index");
            matches!(
                binding.namespace,
                Namespace::Special | Namespace::SpecialReference
            )
        });
        let mut types: Vec<_> = self
            .scoped_var_types(name, target_idx, 0)
            .filter(|decl| !only_free || decl.is_free || special)
            .map(|decl| decl.spec.clone())
            .collect();
        if special {
            types.extend(self.proclaimed_var_type(name));
        }
        types
    }

    fn scoped_var_types<'c>(
        &'c self,
        name: &'c Name,
        binding_index: Option<usize>,
        first_scope: usize,
    ) -> impl Iterator<Item = &'c VariableTypeDeclaration> {
        self.declaration_scopes[first_scope..]
            .iter()
            .rev()
            .flat_map(|scope| scope.variable_types.iter().rev())
            .filter(move |decl| &decl.name == name && decl.binding_index == binding_index)
    }

    fn proclaimed_var_type(&self, name: &Name) -> Option<Form> {
        let key = name.key();
        self.context
            .declarations
            .pending_types
            .borrow()
            .get(key)
            .cloned()
            .or_else(|| self.runtime.proclaimed_types.borrow().get(key).cloned())
    }

    fn current_ftype(&self, name: &Name, setf: bool) -> Option<Form> {
        let binding_index = self.find_binding_index(name, |ns| ns.binds_function(setf));
        if let Some(decl) = self
            .declaration_scopes
            .iter()
            .rev()
            .flat_map(|scope| scope.function_types.iter().rev())
            .find(|decl| {
                &decl.name == name && decl.setf == setf && decl.binding_index == binding_index
            })
        {
            return Some(decl.spec.clone());
        }
        if binding_index.is_some() {
            return None;
        }
        let key = (name.key().clone(), setf);
        self.context
            .declarations
            .function_types
            .borrow()
            .get(&key)
            .cloned()
            .or_else(|| self.runtime.proclaimed_ftypes.borrow().get(&key).cloned())
    }

    fn free_declarations(
        &mut self,
        decls: Declarations<'_>,
        scope_mark: ScopeMark,
    ) -> Result<(), EvalError> {
        if !decls.custom.is_empty() {
            let mut policy = self.optimization_qualities();
            let mut applied = 0;
            for declaration in decls.custom {
                let offset = declaration.preceding_qualities;
                for &(quality, value) in &decls.optimize[applied..offset] {
                    policy.set(quality, value);
                }
                self.process_custom_declaration(declaration.name, declaration.form, policy)?;
                applied = offset;
            }
        }
        let binding_start = scope_mark.bindings;
        for name in decls.special {
            if !self.bindings[binding_start..].iter().any(|scope| {
                &scope.name == name
                    && matches!(scope.namespace, Namespace::Variable | Namespace::Special)
            }) {
                self.push_special(name, false);
            }
        }
        for (name, setf, declaration) in decls.inline_declarations {
            self.push_inline_declaration(name, setf, declaration);
        }
        if decls.optimize.is_empty() && decls.types.is_empty() && decls.ftypes.is_empty() {
            return Ok(());
        }
        let qualities = (!decls.optimize.is_empty()).then(|| {
            let mut qualities = self.optimization_qualities();
            for (quality, value) in decls.optimize {
                qualities.set(quality, value);
            }
            qualities
        });
        let variable_types = decls
            .types
            .into_iter()
            .map(|(name, spec)| {
                let binding_index = self.find_var_binding_index(name);
                let is_free = binding_index.is_none_or(|idx| {
                    idx < self.outer.len() + binding_start
                        || self.bindings[idx - self.outer.len()].namespace
                            == Namespace::SpecialReference
                });
                VariableTypeDeclaration {
                    name: name.clone(),
                    binding_index,
                    spec: spec.clone(),
                    is_free,
                }
            })
            .collect();
        let function_types = decls
            .ftypes
            .into_iter()
            .map(|(name, setf, spec)| FunctionTypeDeclaration {
                name: name.clone(),
                setf,
                binding_index: self.find_binding_index(name, |ns| ns.binds_function(setf)),
                spec: spec.clone(),
            })
            .collect();
        self.declaration_scopes.push(Rc::new(DeclarationScope {
            qualities,
            variable_types,
            function_types,
        }));
        Ok(())
    }

    fn push_inline_declaration(&mut self, name: &Name, setf: bool, declaration: InlineDeclaration) {
        self.bindings.push(ScopedBinding {
            name: name.clone(),
            namespace: Namespace::InlineDeclaration { setf, declaration },
            binding: None,
            expander: None,
            used: None,
        });
    }

    fn inline_declaration(&self, name: &Name, setf: bool) -> Option<InlineDeclaration> {
        for scope in self.scopes().rev().filter(|scope| &scope.name == name) {
            if let Namespace::InlineDeclaration {
                setf: target,
                declaration,
            } = scope.namespace
                && target == setf
            {
                return Some(declaration);
            }
            if scope.namespace.binds_function(setf) {
                return None;
            }
        }
        let file_declaration = self
            .context
            .declarations
            .inline_declarations
            .borrow()
            .get(&(name.key().clone(), setf))
            .copied();
        // Either context can receive a declaration before the other sees it.
        if file_declaration == Some(InlineDeclaration::NotInline) {
            file_declaration
        } else {
            self.runtime
                .proclaimed_inline_declaration(name, setf)
                .or(file_declaration)
        }
    }

    fn is_notinline(&self, name: &Name) -> bool {
        self.inline_declaration(name, false) == Some(InlineDeclaration::NotInline)
    }

    fn call_inline_declaration(&self, name: &Name, args: &[Form]) -> Option<InlineDeclaration> {
        let declaration = self.inline_declaration(name, false);
        // The built-in AREF place expands to FUNCALL with the setter as a literal.
        if declaration != Some(InlineDeclaration::NotInline)
            && name.operator() == "FUNCALL"
            && name.package() == Some(Package::COMMON_LISP)
            && matches!(
                args.first().map(|arg| &arg.kind),
                Some(FormKind::Literal(setter)) if crate::vm::array_write::is_setf_aref(setter.get())
            )
            && (self.is_notinline(&Name::common_lisp("AREF"))
                || self.inline_declaration(&Name::common_lisp("AREF"), true)
                    == Some(InlineDeclaration::NotInline))
        {
            Some(InlineDeclaration::NotInline)
        } else {
            declaration
        }
    }

    fn lookup_inline_core(&self, name: &Name) -> Option<Rc<crate::core_ir::CoreFn>> {
        self.context
            .declarations
            .inline_functions
            .borrow()
            .get(name.key())
            .cloned()
            .or_else(|| self.runtime.inline_def(name))
    }

    fn unbind(&mut self, count: usize) {
        if count != 0 {
            self.emit(Ins::Unbind(count));
        }
    }

    fn reserve(&mut self, len: usize) -> Range<Register> {
        let start = self.used;
        self.used += len;
        self.code.registers = self.code.registers.max(self.used);
        start..self.used
    }

    fn emit(&mut self, instruction: Ins) -> usize {
        let index = self.code.instructions.len();
        self.code.instructions.push(instruction);
        index
    }

    fn release(&mut self, mark: usize) {
        if self.used > mark {
            self.emit(Ins::Clear(mark..self.used));
            self.used = mark;
        }
    }

    fn select_values(&mut self, values: Range<Register>) {
        self.emit(Ins::SetValues(values));
    }

    fn scalar(&mut self, dest: Destination, instruction: impl FnOnce(Register) -> Ins) {
        self.output(dest, |compiler, out| {
            compiler.emit(instruction(out));
        });
    }

    fn top_form(&mut self, form: &Form, dest: Destination) -> Result<(), EvalError> {
        if let Some(expansion) = self.expand_form(form)? {
            return self.with_expansion(form.at, |compiler| compiler.top_form(&expansion, dest));
        }
        if let Some([head, args @ ..]) = form.list()
            && let Some(name) = head.name()
        {
            if let Some(kind) = vop::declaration_kind(self.runtime, name) {
                return self.eval_vop_declaration(kind, args, dest, form.at);
            }
            let operator = core_operator(self.runtime, name).unwrap_or_else(|| name.operator());
            match operator {
                "PROGN" => return self.sequence(args, dest, Self::top_form),
                "EVAL-WHEN" => {
                    return self.sequence(eval_when_body(args, form.at)?, dest, Self::top_form);
                }
                "LOCALLY" => return self.locally(args, dest, Self::top_form),
                "MACROLET" => return self.macrolet(args, dest, Self::top_form, head.at),
                "SYMBOL-MACROLET" => {
                    return self.symbol_macrolet(args, dest, Self::top_form, head.at);
                }
                "DEFVAR" | "DEFPARAMETER" | "DEFCONSTANT" | "DECLAIM" => {
                    self.note_top_level_specials(operator, args, true)?;
                }
                _ => {}
            }
        }
        self.form_unexpanded(form, dest)
    }

    fn apply_declaim_items(&self, decls: &Declarations<'_>) {
        self.context
            .specials
            .borrow_mut()
            .extend(decls.special.iter().map(|name| name.key().clone()));
        {
            let mut declarations = self.context.declarations.inline_declarations.borrow_mut();
            for &(name, setf, declaration) in &decls.inline_declarations {
                declarations.insert((name.key().clone(), setf), declaration);
            }
        }
        if !decls.ftypes.is_empty() {
            let mut ftypes = self.context.declarations.function_types.borrow_mut();
            for &(name, setf, spec) in &decls.ftypes {
                ftypes.insert((name.key().clone(), setf), spec.clone());
            }
        }
    }

    fn note_top_level_specials(
        &self,
        operator: &str,
        args: &[Form],
        pending_execution: bool,
    ) -> Result<(), EvalError> {
        if operator == "DECLAIM" {
            for declaration in args {
                register_declarations(self.runtime, declaration)?;
                let decls = declaration_items(self.runtime, &self.context.warnings, declaration)?;
                self.apply_declaim_items(&decls);
                // EVAL compiles an entire top-level PROGN before running it. The
                // file compiler executes each DECLAIM before processing its successor.
                if pending_execution && !decls.types.is_empty() {
                    let mut types = self.context.declarations.pending_types.borrow_mut();
                    record_proclaimed_types(&mut types, decls.types.iter().copied());
                }
                if pending_execution && !decls.optimize.is_empty() {
                    let pending = &self.context.declarations.pending_qualities;
                    let global = pending
                        .get()
                        .unwrap_or_else(|| self.runtime.optimization_qualities.get());
                    pending.set(Some(decls.apply_optimize_declarations(global)));
                }
            }
        } else if let Some(variable) = args.first() {
            let key = self::variable(variable)?.key().clone();
            if operator == "DEFCONSTANT"
                && let Some(init) = args.get(1)
            {
                self.context
                    .declarations
                    .constants
                    .borrow_mut()
                    .insert(key.clone(), init.clone());
            }
            self.context.specials.borrow_mut().insert(key);
        }
        Ok(())
    }

    fn output(&mut self, dest: Destination, emit: impl FnOnce(&mut Self, Register)) {
        match dest {
            Destination::One(out) => emit(self, out),
            Destination::All => {
                let value = self.reserve(1);
                emit(self, value.start);
                self.select_values(value);
            }
        }
    }

    fn form(&mut self, form: &Form, dest: Destination) -> Result<(), EvalError> {
        self.form_inner(form, dest)
            .map_err(|error| error.at_form(form))
    }

    fn form_inner(&mut self, form: &Form, dest: Destination) -> Result<(), EvalError> {
        if let Some(expansion) = self.expand_form(form)? {
            return self.with_expansion(form.at, |compiler| compiler.form(&expansion, dest));
        }
        self.form_unexpanded(form, dest)
    }

    fn compiler_macro_expander(&self, name: &Name, args: &[Form]) -> Option<Root<'_>> {
        if name.operator() == "FUNCALL" {
            let [op, target] = args.first()?.list()? else {
                return None;
            };
            if op.name()?.operator() != "FUNCTION" {
                return None;
            }
            if let Some(name) = target.name() {
                let expander = self.runtime.global_compiler_macro(name, false)?;
                return (self.lexical_operator(name).is_none() && !self.is_notinline(name))
                    .then_some(expander);
            }
            let accessor = setf_accessor(target)?;
            let expander = self.runtime.global_compiler_macro(accessor, true)?;
            let shadowed = self
                .scopes()
                .any(|scope| scope.namespace == Namespace::Setter && &scope.name == accessor);
            return (!shadowed
                && self.inline_declaration(accessor, true) != Some(InlineDeclaration::NotInline))
            .then_some(expander);
        }
        let expander = self.runtime.global_compiler_macro(name, false)?;
        (!self.is_notinline(name)).then_some(expander)
    }

    fn macro_environment(
        &self,
        type_of: &dyn Fn(&Name) -> Option<Form>,
    ) -> crate::macros::MacroEnvironment {
        let mut bindings = crate::macros::MacroEnvironment {
            file_constants: Some(Rc::clone(&self.context.declarations.constants)),
            declarations: self.custom_declarations.clone(),
            qualities: Some(self.optimization_qualities())
                .filter(|value| *value != self.runtime.optimization_qualities.get()),
            ..Default::default()
        };
        for scope in self.scopes() {
            if scope.namespace.is_variable()
                && let Some(spec) = type_of(&scope.name)
            {
                bindings
                    .variable_types
                    .insert(scope.name.key().clone(), spec);
            }
            let namespace = match scope.namespace {
                Namespace::Function | Namespace::Macro => &mut bindings.operators,
                Namespace::Variable
                | Namespace::Special
                | Namespace::SpecialReference
                | Namespace::SymbolMacro => &mut bindings.variables,
                Namespace::Setter => {
                    bindings.setters.insert(scope.name.key().clone());
                    continue;
                }
                Namespace::Block | Namespace::InlineDeclaration { .. } => continue,
            };
            let expander = scope
                .expander
                .as_ref()
                .map(|root| self.runtime.root_slot(root.get()));
            namespace.insert(scope.name.key().clone(), expander);
        }
        bindings
    }

    fn expand_form(&self, form: &Form) -> Result<Option<Form>, EvalError> {
        self.expand_form_with_types(form, &|name| {
            type_intersection(self.current_var_types(name, false))
        })
    }

    fn expand_form_with_types(
        &self,
        form: &Form,
        type_of: &dyn Fn(&Name) -> Option<Form>,
    ) -> Result<Option<Form>, EvalError> {
        if let Some(form) = crate::form_value::refresh_reader_syntax(self.runtime, form)? {
            self.record_form_sources(&form);
            return Ok(Some(form));
        }
        let kind = if self.in_runtime_handler_scope {
            ExpansionKind::MacroFormInHandler
        } else {
            ExpansionKind::MacroForm
        };
        let mut compiler_macro = None;
        if let Some(name) = form.name() {
            if !self.is_symbol_macro(name) {
                return Ok(None);
            }
        } else {
            let Some(name) = macro_head(form) else {
                return Ok(None);
            };
            match self.lexical_operator(name) {
                Some(scope) if scope.namespace == Namespace::Function => return Ok(None),
                Some(_) => {}
                None => {
                    let operator = name.operator();
                    if has_syntax_binding(operator) {
                        return Ok(None);
                    }
                    if !crate::macros::has_expander(self.runtime, name) {
                        let Some([_, args @ ..]) = form.list() else {
                            return Ok(None);
                        };
                        let Some(expander) = self.compiler_macro_expander(name, args) else {
                            return Ok(None);
                        };
                        compiler_macro = Some(expander);
                    }
                }
            }
        }

        #[cfg(test)]
        let _timing = self
            .context
            .phase_timings
            .as_ref()
            .map(|timings| FileCompileTimings::measure(&timings.expansion));
        let bindings = self.macro_environment(type_of);
        let call_expander = self.context.call_expander;
        let expand = |environment: &Root<'_>| {
            if let Some(expander) = compiler_macro.as_ref() {
                expand_compiler_macro_in_environment(
                    self.runtime,
                    expander,
                    form,
                    environment,
                    kind,
                    call_expander,
                )
            } else {
                expand_in_environment(self.runtime, form, environment, kind, call_expander)
            }
        };
        let result = if bindings.is_empty() {
            expand(&self.runtime.nil())
        } else {
            self.runtime
                .with_macro_environment(bindings, form.at, expand)
        };
        let expansion = result.map_err(|error| error.during_macro_expansion(form.at))?;
        if let Some(expansion) = &expansion {
            self.record_form_sources(expansion);
        }
        if let Some(name) = form.name()
            && let Some(spec) = type_of(name)
            && type_checks::should_emit_runtime_type_check(&spec)
        {
            return Ok(expansion.map(|value| {
                Form::new(
                    value.at,
                    FormKind::List(
                        vec![
                            Form::new(value.at, FormKind::Name(Name::common_lisp("THE"))),
                            spec,
                            value,
                        ],
                        None,
                    ),
                )
            }));
        }
        Ok(expansion)
    }

    fn operator_scopes(&self) -> impl DoubleEndedIterator<Item = &ScopedBinding> {
        self.scopes()
            .filter(|scope| scope.namespace.binds_function(false))
    }

    fn scopes(&self) -> impl DoubleEndedIterator<Item = &ScopedBinding> {
        self.outer.iter().chain(&self.bindings)
    }

    fn lexical_operator(&self, name: &Name) -> Option<&ScopedBinding> {
        self.operator_scopes()
            .rev()
            .find(|scope| &scope.name == name)
    }

    fn with_expansion(
        &mut self,
        at: usize,
        compile: impl FnOnce(&mut Self) -> Result<(), EvalError>,
    ) -> Result<(), EvalError> {
        let _expansion = enter_macro_expansion(Rc::clone(&self.context.expansion_depth), at)?;
        compile(self)
    }

    fn form_unexpanded(&mut self, form: &Form, dest: Destination) -> Result<(), EvalError> {
        let mark = self.used;
        match &form.kind {
            FormKind::Integer(n) => {
                fixnum(form, *n)?;
                self.scalar(dest, |out| Ins::Load(out, Immediate::Fixnum(*n as i64)));
            }
            FormKind::Character(character) => {
                self.scalar(dest, |out| Ins::Load(out, Immediate::Character(*character)));
            }
            FormKind::Name(name) => {
                self.read_var_checked(name, dest, form.at)?;
            }
            FormKind::String(_) | FormKind::Literal(_) => {
                let id = self.constant(form)?;
                self.scalar(dest, |out| Ins::Constant(out, id));
            }
            FormKind::List(args, None) if args.is_empty() => {
                self.scalar(dest, |out| Ins::Load(out, Immediate::Nil));
            }
            FormKind::List(items, None) => {
                let (head, args) = items.split_first().unwrap();
                if let Some(name) = head.name() {
                    if let Some(operator) = core_operator(self.runtime, name) {
                        self.application(&Name::common_lisp(operator), args, dest, head.at)?;
                    } else {
                        self.application(name, args, dest, head.at)?;
                    }
                } else if let Some([op, parameters, body @ ..]) = head.list()
                    && op.name().is_some_and(|name| name.operator() == "LAMBDA")
                {
                    self.call_lambda(parameters, body, args, dest, head.at)?;
                } else {
                    return Err(EvalError::compile(
                        head.at,
                        "expected a function name or lambda",
                    ));
                }
            }
            FormKind::List(_, Some(_)) => {
                return Err(EvalError::compile(form.at, "a form must be a proper list"));
            }
        }
        self.release(mark);
        Ok(())
    }

    // Keep large emission branches separate from recursive dispatch so their
    // temporaries do not occupy every application frame in debug builds.
    fn application(
        &mut self,
        name: &Name,
        args: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error(name.spelling(), at);
        match name.operator() {
            "DEFMACRO" => self.defmacro(args, dest, at),
            "DEFINE-COMPILER-MACRO" => self.define_compiler_macro(args, dest, at),
            "MACROLET" => self.macrolet(args, dest, Self::form, at),
            "SYMBOL-MACROLET" => self.symbol_macrolet(args, dest, Self::form, at),
            // A destructured value that does not fit its pattern is a program
            // error, as it is for a macro lambda list; TYPE-P would report a
            // TYPE-ERROR that callers of DESTRUCTURING-BIND do not see.
            "DESTRUCTURING-BIND" => self.destructuring_bind(args, dest, true),
            "LOAD-TIME-VALUE" => {
                let (cell, _) = self.compile_load_time_value(args, at)?;
                self.emit_load_time_value_read(&cell, dest, at);
                Ok(())
            }
            "QUOTE" => {
                let [quoted] = args else {
                    return Err(arity());
                };
                let constant = self.constant(quoted)?;
                self.scalar(dest, |out| Ins::Constant(out, constant));
                Ok(())
            }
            "IF" => self.if_form(args, dest, at),
            "PROGN" => self.body(args, dest),
            "THE" => self.the_form(args, dest, at),
            "EVAL-WHEN" => self.body(eval_when_body(args, at)?, dest),
            "MULTIPLE-VALUE-CALL" => {
                let Some((callee, operands)) = args.split_first() else {
                    return Err(arity());
                };
                self.emit_multiple_value_call(callee, operands, dest, at, Self::form)
            }
            "MULTIPLE-VALUE-PROG1" => {
                let Some((first, body)) = args.split_first() else {
                    return Err(arity());
                };
                self.multiple_value_prog1(first, body, dest)
            }
            "HANDLER-BIND" => {
                let Some((bindings, body)) = args.split_first() else {
                    return Err(arity());
                };
                self.handler_bind(bindings, body, dest)
            }
            "HANDLER-CASE" => {
                let Some((form, clauses)) = args.split_first() else {
                    return Err(arity());
                };
                self.handler_case(form, clauses, dest)
            }
            "IGNORE-ERRORS" => self.ignore_errors(args, dest),
            "RESTART-BIND" => {
                let Some((bindings, body)) = args.split_first() else {
                    return Err(arity());
                };
                self.restart_bind(bindings, body, dest, at)
            }
            "RESTART-CASE" => {
                let Some((form, clauses)) = args.split_first() else {
                    return Err(arity());
                };
                self.restart_case(form, clauses, dest, at)
            }
            "WITH-CONDITION-RESTARTS" => self.with_condition_restarts(args, dest, at),
            "LOCALLY" => self.locally(args, dest, Self::form),
            "DECLARE" => Err(EvalError::compile(
                at,
                "DECLARE must appear in a declaration context",
            )),
            "DECLAIM" => self.declaim(args, dest),
            "DEFVAR" | "DEFPARAMETER" => self.define_special_variable(name, args, dest, at),
            "DEFCONSTANT" => self.define_constant(args, dest, at),
            "PROGV" => self.progv(args, dest, at),
            "BLOCK" => {
                let Some((label, body)) = args.split_first() else {
                    return Err(arity());
                };
                let name = label
                    .name()
                    .ok_or_else(|| EvalError::compile(label.at, "expected a BLOCK name"))?;
                self.block(name, body, dest, label.at)
            }
            "RETURN-FROM" => {
                let (label, value) = match args {
                    [label] => (label, &[][..]),
                    [label, value] => (label, std::slice::from_ref(value)),
                    _ => return Err(arity()),
                };
                let name = label
                    .name()
                    .ok_or_else(|| EvalError::compile(label.at, "expected a BLOCK name"))?;
                self.return_from(name, value, at)
            }
            "RETURN" => {
                if args.len() > 1 {
                    return Err(arity());
                }
                self.return_from(&Name::common_lisp("NIL"), args, at)
            }
            "CATCH" => self.catch_form(args, dest, at),
            "THROW" => self.throw_form(args, at),
            "UNWIND-PROTECT" => self.unwind_protect(args, dest, at),
            "TAGBODY" => self.tagbody(args, dest, at),
            "GO" => self.go_form(args, at),
            "LET" | "LET*" => {
                let Some((specs, body)) = args.split_first() else {
                    return Err(arity());
                };
                self.let_form(specs, body, dest, name.operator() == "LET*")
            }
            "FUNCTION" => self.function_reference(args, dest, at),
            "LAMBDA" => {
                let [params, body @ ..] = args else {
                    return Err(arity());
                };
                self.function(params, body, None, dest, at)
            }
            "DEFUN" => self.defun(args, dest, at),
            "SETQ" => self.setq(args, dest, at),
            "FLET" | "LABELS" => {
                let Some((specs, body)) = args.split_first() else {
                    return Err(arity());
                };
                self.local_functions(specs, body, dest, name.operator() == "LABELS")
            }
            "MULTIPLE-VALUE-BIND" => self.multiple_value_bind(args, dest, at),
            _ => self.emit_named_application(name, args, dest, at, false),
        }
    }

    fn if_form(&mut self, args: &[Form], dest: Destination, at: usize) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error("IF", at);
        let (test, yes, no) = match args {
            [test, yes] => (test, yes, None),
            [test, yes, no] => (test, yes, Some(no)),
            _ => return Err(arity()),
        };
        let reg = self.reserve(1).start;
        self.form(test, Destination::One(reg))?;
        let choose = self.emit(Ins::Unless(reg, 0));
        self.form(yes, dest)?;
        let done = self.emit(Ins::Jump(0));
        self.code.instructions[choose] = Ins::Unless(reg, self.code.instructions.len());
        self.body(no.map_or(&[], std::slice::from_ref), dest)?;
        self.code.instructions[done] = Ins::Jump(self.code.instructions.len());
        Ok(())
    }

    fn the_form(&mut self, args: &[Form], dest: Destination, at: usize) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error("THE", at);
        let [type_specifier, value] = args else {
            return Err(arity());
        };
        if self.optimization_qualities().safety == 0 {
            self.form(value, dest)?;
        } else {
            self.emit_the_checked(type_specifier, dest, at, false, |compiler, d| {
                compiler.form(value, d)
            })?;
        }
        Ok(())
    }

    fn with_condition_restarts(
        &mut self,
        args: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error("WITH-CONDITION-RESTARTS", at);
        let [condition, restarts, body @ ..] = args else {
            return Err(arity());
        };
        let regs = self.reserve(2);
        self.form(condition, Destination::One(regs.start))?;
        self.form(restarts, Destination::One(regs.start + 1))?;
        self.emit(Ins::AssociateRestarts {
            condition: regs.start,
            list: regs.start + 1,
            at,
        });
        self.body(body, dest)?;
        self.emit(Ins::LeaveRestartAssociation);
        Ok(())
    }

    fn progv(&mut self, args: &[Form], dest: Destination, at: usize) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error("PROGV", at);
        let [symbols, values, body @ ..] = args else {
            return Err(arity());
        };
        let regs = self.reserve(2);
        self.form(symbols, Destination::One(regs.start))?;
        self.form(values, Destination::One(regs.start + 1))?;
        self.emit(Ins::BindProgv {
            symbols: regs.start,
            values: regs.start + 1,
            at,
        });
        self.body(body, dest)?;
        self.unbind(1);
        Ok(())
    }

    fn catch_form(&mut self, args: &[Form], dest: Destination, at: usize) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error("CATCH", at);
        let Some((tag, body)) = args.split_first() else {
            return Err(arity());
        };
        let reg = self.reserve(1).start;
        self.form(tag, Destination::One(reg))?;
        let clear = self.used;
        let entry = self.emit(Ins::EnterCatch {
            tag: reg,
            resume: 0,
            clear,
        });
        self.body(body, Destination::All)?;
        self.emit(Ins::LeaveTarget);
        self.code.instructions[entry] = Ins::EnterCatch {
            tag: reg,
            resume: self.code.instructions.len(),
            clear,
        };
        self.receive_values(dest);
        Ok(())
    }

    fn throw_form(&mut self, args: &[Form], at: usize) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error("THROW", at);
        let [tag, value] = args else {
            return Err(arity());
        };
        let reg = self.reserve(1).start;
        self.form(tag, Destination::One(reg))?;
        self.form(value, Destination::All)?;
        self.emit(Ins::Throw { tag: reg, at });
        Ok(())
    }

    fn unwind_protect(
        &mut self,
        args: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error("UNWIND-PROTECT", at);
        let Some((protected, cleanup)) = args.split_first() else {
            return Err(arity());
        };
        let clear = self.used;
        let entry = self.emit(Ins::EnterProtect { cleanup: 0, clear });
        self.form(protected, Destination::All)?;
        self.emit(Ins::BeginCleanup);
        self.code.instructions[entry] = Ins::EnterProtect {
            cleanup: self.code.instructions.len(),
            clear,
        };
        for form in cleanup {
            let mark = self.used;
            let scratch = self.reserve(1).start;
            self.form(form, Destination::One(scratch))?;
            self.release(mark);
        }
        self.emit(Ins::EndCleanup);
        self.receive_values(dest);
        Ok(())
    }

    fn declaim(&mut self, args: &[Form], dest: Destination) -> Result<(), EvalError> {
        for declaration in args {
            let decls = declaration_items(self.runtime, &self.context.warnings, declaration)?;
            if !decls.optimize.is_empty()
                || !decls.types.is_empty()
                || !decls.ftypes.is_empty()
                || declaration
                    .list()
                    .and_then(|items| items.first())
                    .and_then(Form::name)
                    .is_some_and(|name| name.operator() == "DECLARATION")
            {
                let mark = self.used;
                let callee = self.reserve(1).start;
                let argument = self.reserve(1);
                let primitive = Primitive::named("PROCLAIM").expect("PROCLAIM primitive");
                let operator = self.add_constant(ConstantTemplate::Immediate(primitive));
                let literal = self.constant(declaration)?;
                self.emit(Ins::Constant(callee, operator));
                self.emit(Ins::Constant(argument.start, literal));
                self.call_registers(
                    Callee::Value(callee),
                    argument,
                    Destination::All,
                    declaration.at,
                    false,
                    None,
                );
                self.release(mark);
            }
            for name in decls.special {
                let symbol = self.symbol(name, declaration.at);
                self.emit(Ins::ProclaimSpecial {
                    symbol,
                    at: declaration.at,
                });
            }
            for (name, setf, inline_declaration) in decls.inline_declarations {
                let symbol = self.symbol(name, declaration.at);
                self.emit(Ins::ProclaimInline {
                    symbol,
                    setf,
                    notinline: inline_declaration == InlineDeclaration::NotInline,
                });
            }
        }
        self.scalar(dest, |out| Ins::Load(out, Immediate::Nil));
        Ok(())
    }

    fn define_special_variable(
        &mut self,
        name: &Name,
        args: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error(name.spelling(), at);
        let conditional = name.operator() == "DEFVAR";
        let (var, init, documentation) = match args {
            [var] if conditional => (var, None, None),
            [var, init] => (var, Some(init), None),
            [var, init, doc] => (var, Some(init), Some(doc)),
            _ => return Err(arity()),
        };
        let name = variable(var)?;
        self.ensure_writable_variable(name, var.at, "redefine")?;
        let symbol = self.symbol(name, var.at);
        self.emit(Ins::ProclaimSpecial { symbol, at: var.at });
        if let Some(init) = init {
            let skip = conditional.then(|| self.emit(Ins::WhenBound { symbol, jump: 0 }));
            let mark = self.used;
            let src = self.reserve(1).start;
            self.form(init, Destination::One(src))?;
            if self.optimization_qualities().safety > 0 {
                self.emit_proclaimed_type_check(name, src, var.at)?;
            }
            self.emit(Ins::WriteCell { symbol, src });
            self.release(mark);
            if let Some(pc) = skip {
                self.code.instructions[pc] = Ins::WhenBound {
                    symbol,
                    jump: self.code.instructions.len(),
                };
            }
        }
        if let Some(doc) = documentation {
            match &doc.kind {
                FormKind::String(_) => {}
                FormKind::Literal(root) if self.runtime.heap.is_string(root.get()) => {}
                _ => {
                    return Err(EvalError::compile(
                        doc.at,
                        "expected a documentation string",
                    ));
                }
            }
            let doc = self.constant(doc)?;
            self.emit(Ins::SetVariableDocumentation { symbol, doc });
        }
        self.scalar(dest, |out| Ins::Constant(out, symbol));
        Ok(())
    }

    fn define_constant(
        &mut self,
        args: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error("DEFCONSTANT", at);
        let (var, init, documentation) = match args {
            [var, init] => (var, init, None),
            [var, init, doc] => (var, init, Some(doc)),
            _ => return Err(arity()),
        };
        let name = variable(var)?;
        if constants::is_canonical_constant(self.runtime, name) {
            return Err(EvalError::new(
                var.at,
                format!("cannot redefine built-in constant {}", name.spelling()),
            )
            .with_condition(Class::ProgramError));
        }
        let doc_id = if let Some(doc) = documentation {
            match &doc.kind {
                FormKind::String(_) => {}
                FormKind::Literal(root) if self.runtime.heap.is_string(root.get()) => {}
                _ => {
                    return Err(EvalError::compile(
                        doc.at,
                        "expected a documentation string",
                    ));
                }
            }
            Some(self.constant(doc)?)
        } else {
            None
        };
        self.context
            .declarations
            .constants
            .borrow_mut()
            .insert(name.key().clone(), init.clone());
        self.context
            .specials
            .borrow_mut()
            .insert(name.key().clone());
        let symbol = self.symbol(name, var.at);
        self.emit(Ins::ProclaimSpecial { symbol, at: var.at });
        let mark = self.used;
        let installer = crate::macros::compiled_native(crate::macros::DEFINE_CONSTANT)
            .expect("DEFINE_CONSTANT installer");
        let callee_id = self.add_constant(ConstantTemplate::Immediate(installer));
        let callee = self.reserve(1).start;
        let call_args = self.reserve(3);
        self.emit(Ins::Constant(callee, callee_id));
        self.emit(Ins::Constant(call_args.start, symbol));
        self.form(init, Destination::One(call_args.start + 1))?;
        if let Some(doc_id) = doc_id {
            self.emit(Ins::Constant(call_args.start + 2, doc_id));
        } else {
            self.emit(Ins::Load(call_args.start + 2, Immediate::Nil));
        }
        let discard = self.reserve(1).start;
        self.call_registers(
            Callee::Value(callee),
            call_args,
            Destination::One(discard),
            at,
            false,
            None,
        );
        self.release(mark);
        self.scalar(dest, |out| Ins::Constant(out, symbol));
        Ok(())
    }

    fn function_reference(
        &mut self,
        args: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error("FUNCTION", at);
        let [operand] = args else {
            return Err(arity());
        };
        if let Some(name) = operand.name() {
            if self
                .lexical_operator(name)
                .is_some_and(|scope| scope.namespace == Namespace::Macro)
            {
                return Err(EvalError::compile(
                    operand.at,
                    "FUNCTION requires a function name",
                ));
            }
            self.read_name(name, Namespace::Function, dest, operand.at);
        } else if let Some(accessor) = setf_accessor(operand) {
            if let Some(location) = self.binding(accessor, Namespace::Setter) {
                self.output(dest, |compiler, out| compiler.read_location(location, out));
            } else {
                let args = self.reserve(1);
                let name = self.constant(operand)?;
                self.emit(Ins::Constant(args.start, name));
                let reader = self.symbol(&Name::common_lisp("FDEFINITION"), operand.at);
                self.call_registers(Callee::Named(reader), args, dest, operand.at, false, None);
            }
        } else if let Some([op, params, body @ ..]) = operand.list()
            && op.name().is_some_and(|name| name.operator() == "LAMBDA")
        {
            self.function(params, body, None, dest, operand.at)?;
        } else {
            return Err(EvalError::compile(
                operand.at,
                "expected a function name or lambda",
            ));
        }
        Ok(())
    }

    fn document_definition(&mut self, function: Register, body: &[Form]) -> Result<(), EvalError> {
        let (_, Some(documentation), _) = function_body(self.runtime, body) else {
            return Ok(());
        };
        let args = self.reserve(3);
        let doc = self.constant(documentation)?;
        let t = self.add_constant(ConstantTemplate::Immediate(Word::T));
        self.emit(Ins::Constant(args.start, doc));
        self.emit(Ins::Copy(args.start + 1, function));
        self.emit(Ins::Constant(args.start + 2, t));
        let setter = crate::vm::setf_primitive("DOCUMENTATION").expect("documentation setter");
        let code = self.add_constant(ConstantTemplate::Immediate(setter));
        let callee = self.reserve(1).start;
        self.emit(Ins::Constant(callee, code));
        self.call_registers(
            Callee::Value(callee),
            args,
            Destination::One(callee),
            documentation.at,
            false,
            None,
        );
        Ok(())
    }

    fn defun(&mut self, args: &[Form], dest: Destination, at: usize) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error("DEFUN", at);
        let [naming, params, body @ ..] = args else {
            return Err(arity());
        };
        if let Some(name) = naming.name() {
            let symbol = self.symbol(name, naming.at);
            let reg = self.reserve(1).start;
            let (mut child, function) =
                core_lower::compile_function(self, params, body, Some(name), None, at)?;
            child.code.debug.global_function = true;
            self.emit_closure_for_compiled_child(child, Destination::One(reg), at);
            self.document_definition(reg, body)?;
            if self.outer.is_empty() && !self.bindings.iter().any(|scope| scope.binding.is_some()) {
                if let Some(candidate) = core_lower::inline_candidate(function) {
                    self.context
                        .declarations
                        .inline_functions
                        .borrow_mut()
                        .insert(name.key().clone(), Rc::clone(&candidate));
                    if self.is_top_level {
                        self.runtime.record_inline_def(name, candidate);
                    }
                } else {
                    self.context
                        .declarations
                        .inline_functions
                        .borrow_mut()
                        .remove(name.key());
                    if self.is_top_level {
                        self.runtime.clear_inline_def(name);
                    }
                }
            }
            self.emit(Ins::DefineFunction {
                symbol,
                definition: reg,
                is_macro: false,
                at: naming.at,
            });
            self.scalar(dest, |out| Ins::Constant(out, symbol));
        } else if let Some(accessor) = setf_accessor(naming) {
            let args = self.reserve(2);
            let mut child = self.lower_function(params, body, Some(accessor), None, at)?;
            child.code.debug.global_function = true;
            let name_index = child.form_constant(naming, true)?;
            child.code.debug.function_name = Some(u32::try_from(name_index).map_err(|_| {
                EvalError::compile(at, "function name constant index is out of bounds")
            })?);
            self.emit_closure_for_compiled_child(child, Destination::One(args.start), at);
            self.document_definition(args.start, body)?;
            let name = self.constant(naming)?;
            self.emit(Ins::Constant(args.start + 1, name));
            let setter =
                crate::vm::setf_primitive("FDEFINITION").expect("FDEFINITION setter primitive");
            let constant = self.add_constant(ConstantTemplate::Immediate(setter));
            let callee = self.reserve(1).start;
            self.emit(Ins::Constant(callee, constant));
            let result = Destination::One(args.start);
            self.call_registers(Callee::Value(callee), args, result, naming.at, false, None);
            self.scalar(dest, |out| Ins::Constant(out, name));
        } else {
            return Err(EvalError::compile(naming.at, "expected a function name"));
        }
        Ok(())
    }

    fn setq(&mut self, args: &[Form], dest: Destination, at: usize) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error("SETQ", at);
        let (pairs, remainder) = args.as_chunks::<2>();
        if !remainder.is_empty() {
            return Err(arity());
        }
        let temp = self.reserve(1).start;
        for pair in pairs {
            let var = variable(&pair[0])?;
            if self.is_symbol_macro(var) {
                let assignment = Form::new(
                    pair[0].at,
                    FormKind::List(
                        vec![
                            Form::new(at, FormKind::Name(Name::common_lisp("SETF"))),
                            pair[0].clone(),
                            pair[1].clone(),
                        ],
                        None,
                    ),
                );
                self.form(&assignment, Destination::One(temp))?;
                continue;
            }
            self.form(&pair[1], Destination::One(temp))?;
            self.ensure_writable_variable(var, pair[0].at, "assign")?;
            if self.optimization_qualities().safety > 0 {
                let types = self.current_var_types(var, false);
                self.emit_variable_type_checks(temp, &types, pair[0].at)?;
            }
            if let Some(location) = self.binding(var, Namespace::Variable) {
                self.write_location(location, temp);
            } else {
                let symbol = self.symbol(var, pair[0].at);
                self.emit(Ins::WriteCell { symbol, src: temp });
            }
        }
        self.scalar(dest, |out| Ins::Copy(out, temp));
        Ok(())
    }

    fn multiple_value_bind(
        &mut self,
        args: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error("MULTIPLE-VALUE-BIND", at);
        let [vars, values, body @ ..] = args else {
            return Err(arity());
        };
        let vars = vars
            .list()
            .ok_or_else(|| EvalError::compile(vars.at, "expected a list of variables"))?;
        let regs = self.reserve(vars.len());
        if regs.len() == 1 {
            self.form(values, Destination::One(regs.start))?;
        } else {
            self.form(values, Destination::All)?;
            self.emit(Ins::BindValues(regs.clone()));
        }
        let (decls, body) = declarations(self.runtime, &self.context.warnings, body)?;
        let mark = self.mark_scope();
        let safety = decls
            .apply_optimize_declarations(self.optimization_qualities())
            .safety;
        let mut dynamic = 0;
        for (var, reg) in vars.iter().zip(regs) {
            let name = variable(var)?;
            if safety > 0 {
                self.emit_variable_type_checks(reg, decls.types_for(name), var.at)?;
            }
            dynamic += self.bind_variable(name, reg, var.at, &decls.special, safety)?;
        }
        self.free_declarations(decls, mark)?;
        self.body(body, dest)?;
        self.unbind(dynamic);
        self.restore_scope(mark);
        Ok(())
    }

    fn binding(&mut self, name: &Name, namespace: Namespace) -> Option<BindingLocation> {
        let matches = |scope: &&ScopedBinding| {
            &scope.name == name
                && match namespace {
                    Namespace::Variable => scope.namespace.is_variable(),
                    Namespace::Function => scope.namespace.binds_function(false),
                    _ => scope.namespace == namespace,
                }
        };
        if let Some(scope) = self.bindings.iter().rev().find(matches) {
            if let Some(used) = &scope.used {
                used.set(true);
            }
            return scope.binding.clone().map(BindingLocation::Local);
        }
        let outer_scope = self.outer.iter().rev().find(matches)?;
        if let Some(used) = &outer_scope.used {
            used.set(true);
        }
        let outer = outer_scope.binding.clone()?;
        let capture = self.capture(outer);
        if namespace == Namespace::Variable {
            self.debug_capture(name, capture, 0);
        }
        Some(BindingLocation::Capture(capture))
    }

    fn receive_values(&mut self, dest: Destination) {
        if let Destination::One(reg) = dest {
            self.emit(Ins::BindValues(reg..reg + 1));
        }
    }

    fn handler_test(&mut self, form: &Form) -> Result<Test, EvalError> {
        if let Some(name) = form.name() {
            return Ok(Test::named(name.operator())
                .unwrap_or_else(|| Test::Named(self.symbol(name, form.at))));
        }
        Ok(Test::Named(self.constant(form)?))
    }

    fn handler_table(&mut self, handlers: Vec<Handler>) -> usize {
        let table = self.code.handlers.len();
        self.code.handlers.push(HandlerTable { handlers });
        self.emit(Ins::EnterHandlers { table });
        table
    }

    fn with_handler_extent<T>(&mut self, active: bool, compile: impl FnOnce(&mut Self) -> T) -> T {
        let previous = std::mem::replace(&mut self.in_runtime_handler_scope, active);
        let mut compiler = DropGuard::new(self, move |compiler| {
            compiler.in_runtime_handler_scope = previous;
        });
        compile(&mut compiler)
    }

    fn handler_bind(
        &mut self,
        bindings: &Form,
        body: &[Form],
        dest: Destination,
    ) -> Result<(), EvalError> {
        let bindings = bindings
            .list()
            .ok_or_else(|| EvalError::compile(bindings.at, "expected a list of handlers"))?;
        if bindings.is_empty() {
            return self.body(body, dest);
        }
        let regs = self.reserve(bindings.len());
        let mut handlers = Vec::with_capacity(bindings.len());
        for (binding, reg) in bindings.iter().zip(regs) {
            let Some([test, handler]) = binding.list() else {
                return Err(EvalError::compile(
                    binding.at,
                    "expected (condition-type handler)",
                ));
            };
            handlers.push(Handler {
                test: self.handler_test(test)?,
                target: HandlerTarget::Function(reg),
            });
            self.form(handler, Destination::One(reg))?;
        }
        self.handler_table(handlers);
        self.with_handler_extent(true, |compiler| compiler.body(body, dest))?;
        self.emit(Ins::LeaveHandlers);
        Ok(())
    }

    fn handler_case(
        &mut self,
        form: &Form,
        clauses: &[Form],
        dest: Destination,
    ) -> Result<(), EvalError> {
        let mut cases = Vec::new();
        let mut no_error = None;
        for clause in clauses {
            let Some([kind, params, body @ ..]) = clause.list() else {
                return Err(EvalError::compile(clause.at, "invalid HANDLER-CASE clause"));
            };
            if kind.name().is_some_and(|name| {
                name.package() == Some(Package::KEYWORD) && name.spelling() == "NO-ERROR"
            }) {
                if no_error.replace((params, body)).is_some() {
                    return Err(EvalError::compile(clause.at, "duplicate :NO-ERROR clause"));
                }
            } else {
                let params = params
                    .list()
                    .filter(|params| params.len() <= 1)
                    .ok_or_else(|| {
                        EvalError::compile(params.at, "expected zero or one condition parameter")
                    })?;
                cases.push((self.handler_test(kind)?, params, body));
            }
        }
        let condition = self.reserve(1).start;
        let clear = self.used;
        let table = (!cases.is_empty()).then(|| {
            self.handler_table(
                cases
                    .iter()
                    .map(|&(test, _, _)| Handler {
                        test,
                        target: HandlerTarget::Case {
                            pc: 0,
                            condition,
                            clear,
                        },
                    })
                    .collect(),
            )
        });
        let handler_active = self.in_runtime_handler_scope || table.is_some();
        self.with_handler_extent(handler_active, |compiler| {
            compiler.form(form, Destination::All)
        })?;
        if table.is_some() {
            self.emit(Ins::LeaveHandlers);
        }
        if let Some((parameters, body)) = no_error {
            self.call_with_values(parameters, body)?;
        }
        if let Some(table) = table {
            let mut joins = vec![self.emit(Ins::Jump(0))];
            for (index, (test, params, body)) in cases.into_iter().enumerate() {
                self.code.handlers[table].handlers[index] = Handler {
                    test,
                    target: HandlerTarget::Case {
                        pc: self.code.instructions.len(),
                        condition,
                        clear,
                    },
                };
                self.case_body(params, condition..condition + params.len(), body)?;
                joins.push(self.emit(Ins::Jump(0)));
            }
            let join = self.code.instructions.len();
            for branch in joins {
                self.code.instructions[branch] = Ins::Jump(join);
            }
        }
        self.receive_values(dest);
        Ok(())
    }

    fn call_with_values(&mut self, parameters: &Form, body: &[Form]) -> Result<(), EvalError> {
        let mark = self.used;
        let target = self.reserve(1).start;
        self.function(
            parameters,
            body,
            None,
            Destination::One(target),
            parameters.at,
        )?;
        self.emit(Ins::CallValues {
            callee: target,
            at: parameters.at,
        });
        self.release(mark);
        Ok(())
    }

    fn case_body(
        &mut self,
        params: &[Form],
        regs: Range<Register>,
        body: &[Form],
    ) -> Result<(), EvalError> {
        let (decls, body) = declarations(self.runtime, &self.context.warnings, body)?;
        let mark = self.mark_scope();
        let mut dynamic = 0;
        let mut seen = HashSet::new();
        for (param, reg) in params.iter().zip(regs) {
            let name = variable(param)?;
            if name.spelling().starts_with('&') || !seen.insert(name.key()) {
                return Err(EvalError::compile(
                    param.at,
                    "expected distinct required parameters",
                ));
            }
            let safety = decls
                .apply_optimize_declarations(self.optimization_qualities())
                .safety;
            dynamic += self.bind_variable(name, reg, param.at, &decls.special, safety)?;
        }
        self.free_declarations(decls, mark)?;
        self.body(body, Destination::All)?;
        self.unbind(dynamic);
        self.restore_scope(mark);
        Ok(())
    }

    fn ignore_errors(&mut self, forms: &[Form], dest: Destination) -> Result<(), EvalError> {
        let regs = self.reserve(2);
        let clear = self.used;
        let table = self.handler_table(vec![Handler {
            test: Test::Class(Class::Error),
            target: HandlerTarget::Case {
                pc: 0,
                condition: regs.start + 1,
                clear,
            },
        }]);
        self.with_handler_extent(true, |compiler| compiler.body(forms, Destination::All))?;
        self.emit(Ins::LeaveHandlers);
        let skip = self.emit(Ins::Jump(0));
        self.code.handlers[table].handlers[0] = Handler {
            test: Test::Class(Class::Error),
            target: HandlerTarget::Case {
                pc: self.code.instructions.len(),
                condition: regs.start + 1,
                clear,
            },
        };
        self.emit(Ins::Load(regs.start, Immediate::Nil));
        self.select_values(regs);
        self.code.instructions[skip] = Ins::Jump(self.code.instructions.len());
        self.receive_values(dest);
        Ok(())
    }

    fn block(
        &mut self,
        name: &Name,
        body: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let mark = self.used;
        let token = self.reserve(1).start;
        let exit_pc = self.emit_nop();
        let clear = self.used;
        let entry = self.emit_nop();
        let used = Rc::new(Cell::new(false));
        let mut scope = self.declare(name, Namespace::Block, token, at);
        scope.used = Some(Rc::clone(&used));
        self.with_lexical_scope(|compiler, _| {
            compiler.bindings.push(scope);
            compiler.body(body, Destination::All)
        })?;
        if used.get() {
            self.emit(Ins::LeaveTarget);
            self.code.instructions[exit_pc] = Ins::ExitToken(token, at);
            self.code.instructions[entry] = Ins::EnterBlock {
                token,
                resume: self.code.instructions.len(),
                clear,
            };
        }
        self.receive_values(dest);
        self.release(mark);
        Ok(())
    }

    fn emit_exit_token(&mut self, at: usize) -> RegisterBinding {
        let reg = self.reserve(1).start;
        self.emit(Ins::ExitToken(reg, at));
        RegisterBinding {
            frame_depth: self.frame_depth,
            reg,
            boxed: Rc::new(Cell::new(false)),
            capture_kind: CaptureKind::ExitToken,
        }
    }

    fn return_from(&mut self, name: &Name, value: &[Form], at: usize) -> Result<(), EvalError> {
        let location = self.binding(name, Namespace::Block).ok_or_else(|| {
            EvalError::compile(
                at,
                format!("no lexically enclosing BLOCK {}", name.spelling()),
            )
        })?;
        let token = self.reserve(1).start;
        self.read_location(location, token);
        self.body(value, Destination::All)?;
        self.emit(Ins::ReturnFrom { token, at });
        Ok(())
    }

    fn register_capture(&mut self, binding: RegisterBinding) -> usize {
        if binding.capture_kind == CaptureKind::MutableCell {
            binding.boxed.set(true);
        }
        let index = self.captures.len();
        self.code.captures.push(binding.capture_kind);
        self.captures.push(binding);
        index
    }

    fn capture(&mut self, binding: RegisterBinding) -> usize {
        self.captures
            .iter()
            .position(|other| Rc::ptr_eq(&other.boxed, &binding.boxed))
            .unwrap_or_else(|| self.register_capture(binding))
    }

    fn read_var_checked(
        &mut self,
        name: &Name,
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        if *name != Name::common_lisp("NIL")
            && *name != Name::common_lisp("T")
            && self.optimization_qualities().safety > 0
        {
            let types = self.current_var_types(name, true);
            if types
                .iter()
                .any(type_checks::should_emit_runtime_type_check)
            {
                let out = match dest {
                    Destination::One(reg) => reg,
                    _ => self.reserve(1).start,
                };
                self.read_name(name, Namespace::Variable, Destination::One(out), at);
                self.emit_variable_type_checks(out, &types, at)?;
                if !matches!(dest, Destination::One(reg) if reg == out) {
                    self.scalar(dest, |dst| Ins::Copy(dst, out));
                }
                return Ok(());
            }
        }
        self.read_name(name, Namespace::Variable, dest, at);
        Ok(())
    }

    fn emit_variable_type_checks<'f>(
        &mut self,
        register: Register,
        specs: impl IntoIterator<Item = &'f Form>,
        at: usize,
    ) -> Result<(), EvalError> {
        let checks: Vec<_> = specs
            .into_iter()
            .filter(|spec| type_checks::should_emit_runtime_type_check(spec))
            .map(|spec| (register, spec))
            .collect();
        self.emit_the_values_checks(&checks, at)
    }

    fn declare_bound_var_types<'f>(&mut self, name: &Name, specs: impl Iterator<Item = &'f Form>) {
        let binding_index = Some(self.outer.len() + self.bindings.len() - 1);
        let variable_types: Vec<_> = specs
            .map(|spec| VariableTypeDeclaration {
                name: name.clone(),
                binding_index,
                spec: spec.clone(),
                is_free: false,
            })
            .collect();
        if !variable_types.is_empty() {
            self.declaration_scopes.push(Rc::new(DeclarationScope {
                qualities: None,
                variable_types,
                function_types: Vec::new(),
            }));
        }
    }

    fn read_name(&mut self, name: &Name, namespace: Namespace, dest: Destination, at: usize) {
        if let Some(location) = self.binding(name, namespace) {
            self.output(dest, |compiler, out| compiler.read_location(location, out));
        } else {
            let symbol = self.symbol(name, at);
            self.scalar(dest, |out| Ins::ReadCell {
                out,
                symbol,
                function: namespace == Namespace::Function,
                at,
            });
        }
    }

    fn read_location(&mut self, location: BindingLocation, out: Register) {
        match location {
            BindingLocation::Local(binding) => self.if_boxed(
                &binding,
                Ins::Copy(out, binding.reg),
                Ins::ReadBox {
                    out,
                    cell: CellLocation::Local(binding.reg),
                },
            ),
            BindingLocation::Capture(index)
                if self.code.captures[index] != CaptureKind::MutableCell =>
            {
                self.emit(Ins::Capture(out, index));
            }
            BindingLocation::Capture(index) => {
                self.emit(Ins::ReadBox {
                    out,
                    cell: CellLocation::Capture(index),
                });
            }
        }
    }

    fn write_location(&mut self, location: BindingLocation, src: Register) {
        match location {
            BindingLocation::Local(binding) => self.if_boxed(
                &binding,
                Ins::Copy(binding.reg, src),
                Ins::WriteBox {
                    cell: CellLocation::Local(binding.reg),
                    src,
                },
            ),
            BindingLocation::Capture(index) => {
                self.emit(Ins::WriteBox {
                    cell: CellLocation::Capture(index),
                    src,
                });
            }
        }
    }

    /// Keep the child compiler and core function out of the recursive form dispatcher's frame.
    fn call_lambda(
        &mut self,
        parameters: &Form,
        body: &[Form],
        arguments: &[Form],
        destination: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let target = self.reserve(1).start;
        let (child, function) =
            core_lower::compile_function(self, parameters, body, None, None, at)?;
        self.check_lambda_keywords(&function.params, arguments);
        self.emit_closure_for_compiled_child(child, Destination::One(target), at);
        self.invoke(Callee::Value(target), arguments, destination, at)
    }

    fn function(
        &mut self,
        parameters: &Form,
        body: &[Form],
        block: Option<&Name>,
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let child = self.lower_function(parameters, body, block, None, at)?;
        self.emit_closure_for_compiled_child(child, dest, at);
        Ok(())
    }

    pub(super) fn emit_closure_for_compiled_child(
        &mut self,
        mut child: Self,
        dest: Destination,
        at: usize,
    ) {
        let captures = std::mem::take(&mut child.captures);
        let code = self.add_constant(ConstantTemplate::Function(Box::new(child.finish())));
        if captures.is_empty() {
            self.scalar(dest, |out| Ins::Constant(out, code));
            return;
        }
        // A closure copies its environment before assigning its destination. Local
        // slots already in capture order can be used directly, even if out aliases one.
        let first = captures[0].reg;
        let cells =
            if captures.iter().enumerate().all(|(i, binding)| {
                binding.frame_depth == self.frame_depth && binding.reg == first + i
            }) {
                first..first + captures.len()
            } else {
                let cells = self.reserve(captures.len());
                for (binding, dst) in captures.into_iter().zip(cells.clone()) {
                    if binding.frame_depth == self.frame_depth {
                        self.emit(Ins::Copy(dst, binding.reg));
                    } else {
                        let index = self.capture(binding);
                        self.emit(Ins::Capture(dst, index));
                    }
                }
                cells
            };
        self.scalar(dest, |out| Ins::Closure {
            out,
            code,
            cells,
            at,
        });
    }

    fn lower_function(
        &mut self,
        parameters: &Form,
        body: &[Form],
        block: Option<&Name>,
        source_parameters: Option<&Form>,
        at: usize,
    ) -> Result<Self, EvalError> {
        // Forming a function inside a handler extent does not put its later
        // invocation under that extent.
        let (child, _) = self.with_handler_extent(false, |compiler| {
            core_lower::compile_function(compiler, parameters, body, block, source_parameters, at)
        })?;
        Ok(child)
    }

    fn invoke(
        &mut self,
        callee: Callee,
        args: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        self.emit_call(callee, args, dest, at, false, None)
    }

    fn emit_named_application(
        &mut self,
        name: &Name,
        args: &[Form],
        dest: Destination,
        at: usize,
        associate: bool,
    ) -> Result<(), EvalError> {
        let inline_declaration = self.call_inline_declaration(name, args);
        let callee = if let Some(location) = self.binding(name, Namespace::Function) {
            let reg = self.reserve(1).start;
            self.read_location(location, reg);
            Callee::Value(reg)
        } else {
            if !self.is_top_level {
                self.warn_undefined_call(name, at);
            }
            let symbol = if !associate
                && args.len() == 1
                && inline_declaration != Some(InlineDeclaration::NotInline)
            {
                self.transform_call_symbol(name, at)
            } else {
                self.symbol(name, at)
            };
            Callee::Named(symbol)
        };
        let signature = (self.optimization_qualities().safety == 3)
            .then(|| self.current_ftype(name, false))
            .flatten();
        let Some(types) = signature
            .as_ref()
            .and_then(|signature| type_checks::CheckedFtype::for_call(signature, args.len()))
        else {
            return self.emit_call(callee, args, dest, at, associate, inline_declaration);
        };

        let regs = self.reserve(args.len());
        for (index, (arg, reg)) in args.iter().zip(regs.clone()).enumerate() {
            let dest = Destination::One(reg);
            if let Some(spec) = types.arg(index) {
                self.emit_the_checked(spec, dest, arg.at, false, |compiler, dest| {
                    compiler.form(arg, dest)
                })?;
            } else {
                self.form(arg, dest)?;
            }
        }
        if let Some(spec) = types.result {
            self.emit_the_checked(spec, dest, at, false, |compiler, dest| {
                compiler.call_registers(callee, regs, dest, at, associate, inline_declaration);
                Ok(())
            })
        } else {
            self.call_registers(callee, regs, dest, at, associate, inline_declaration);
            Ok(())
        }
    }

    fn emit_call(
        &mut self,
        callee: Callee,
        args: &[Form],
        dest: Destination,
        at: usize,
        associate: bool,
        inline_declaration: Option<InlineDeclaration>,
    ) -> Result<(), EvalError> {
        let args = self.arguments(args)?;
        self.call_registers(callee, args, dest, at, associate, inline_declaration);
        Ok(())
    }

    fn call_fixnum_arithmetic(
        &mut self,
        op: zkcl_bytecode::FixnumArithmetic,
        symbol: usize,
        args: Range<Register>,
        dest: Destination,
        at: usize,
    ) {
        let scratch = self.reserve(1).start;
        self.emit(Ins::TryFixnumArithmetic {
            op,
            symbol,
            out: scratch,
            args: args.clone(),
        });
        let miss = self.emit(Ins::Unless(scratch, 0));
        match dest {
            Destination::All => self.select_values(scratch..scratch + 1),
            Destination::One(out) => {
                self.emit(Ins::Copy(out, scratch));
                self.select_values(0..0);
            }
        }
        let done = self.emit(Ins::Jump(0));
        self.code.instructions[miss] = Ins::Unless(scratch, self.code.instructions.len());
        // Operand evaluation may have changed the binding. Invoke its current
        // function with the saved arguments and the original value destination.
        self.emit(Ins::ReadCell {
            out: scratch,
            symbol,
            function: true,
            at,
        });
        self.call_registers(
            Callee::Value(scratch),
            args,
            dest,
            at,
            false,
            Some(InlineDeclaration::NotInline),
        );
        self.code.instructions[done] = Ins::Jump(self.code.instructions.len());
    }

    fn call_registers(
        &mut self,
        callee: Callee,
        args: Range<Register>,
        dest: Destination,
        at: usize,
        associate: bool,
        inline_declaration: Option<InlineDeclaration>,
    ) {
        let out = match dest {
            Destination::All => None,
            Destination::One(reg) => Some(reg),
        };
        let notinline = inline_declaration == Some(InlineDeclaration::NotInline);
        if notinline && let Callee::Named(symbol) = callee {
            let symbol = symbol as u32;
            if !self.code.notinline.contains(&symbol) {
                self.code.notinline.push(symbol);
            }
        }
        let specialization = match callee {
            Callee::Named(symbol) if args.len() == 1 && !notinline && !associate => {
                self.specialize_call(symbol)
            }
            _ => None,
        };
        let callee = specialization
            .as_ref()
            .map_or(callee, |call| Callee::Named(call.symbol));
        let pc = self.emit(Ins::Call {
            callee,
            out,
            args,
            at,
            associate,
            notinline,
        });
        if let Some(call) = specialization {
            self.code.vop_calls.push(zkcl_bytecode::VopCall {
                pc,
                binding: call.binding,
                operation: call.operation,
                aarch64_popcount: call.aarch64_popcount,
            });
        }
    }

    fn warn_undefined_call(&self, name: &Name, at: usize) {
        if !self.context.warn_undefined_calls
            || self.in_runtime_handler_scope
            || self.runtime.installing_builtins.get()
            || self.code.name.as_deref() == Some(name.spelling())
            || self
                .runtime
                .find_named_symbol(name)
                .is_some_and(|word| self.runtime.heap.symbol_cell(word, true).is_some())
        {
            return;
        }
        let message = format!("undefined function {}", name.spelling());
        let mut warnings = self.context.warnings.borrow_mut();
        if warnings.iter().all(|warning| warning.message != message) {
            warnings.push(Warning {
                at,
                message,
                style: true,
            });
        }
    }

    fn body(&mut self, body: &[Form], dest: Destination) -> Result<(), EvalError> {
        self.sequence(body, dest, Self::form)
    }

    fn sequence(
        &mut self,
        body: &[Form],
        dest: Destination,
        compile: fn(&mut Self, &Form, Destination) -> Result<(), EvalError>,
    ) -> Result<(), EvalError> {
        let Some((last, earlier)) = body.split_last() else {
            self.scalar(dest, |out| Ins::Load(out, Immediate::Nil));
            return Ok(());
        };
        for form in earlier {
            let mark = self.used;
            let scratch = self.reserve(1).start;
            compile(self, form, Destination::One(scratch))?;
            self.release(mark);
        }
        compile(self, last, dest)
    }

    fn locally(
        &mut self,
        body: &[Form],
        dest: Destination,
        compile: fn(&mut Self, &Form, Destination) -> Result<(), EvalError>,
    ) -> Result<(), EvalError> {
        self.with_local_declarations(body, |compiler, body| {
            compiler.sequence(body, dest, compile)
        })
    }

    fn with_local_declarations<R>(
        &mut self,
        body: &[Form],
        process: impl FnOnce(&mut Self, &[Form]) -> Result<R, EvalError>,
    ) -> Result<R, EvalError> {
        let (decls, body) = declarations(self.runtime, &self.context.warnings, body)?;
        self.with_lexical_scope(|compiler, mark| {
            compiler.free_declarations(decls, mark)?;
            process(compiler, body)
        })
    }

    fn let_form(
        &mut self,
        specs: &Form,
        body: &[Form],
        dest: Destination,
        sequential: bool,
    ) -> Result<(), EvalError> {
        let specs = specs
            .list()
            .ok_or_else(|| EvalError::compile(specs.at, "expected a list of bindings"))?;
        let regs = self.reserve(specs.len());
        let (mut decls, body) = declarations(self.runtime, &self.context.warnings, body)?;
        let mark = self.mark_scope();
        let qualities = decls.apply_optimize_declarations(self.optimization_qualities());
        let mut dynamic = 0;
        let mut parallel = Vec::new();
        for (spec, reg) in specs.iter().zip(regs) {
            let (var, init) = match spec.list() {
                Some([var]) => (var, None),
                Some([var, init]) => (var, Some(init)),
                _ if spec.name().is_some() => (spec, None),
                _ => return Err(EvalError::compile(spec.at, "invalid binding")),
            };
            let name = variable(var)?;
            if let Some(init) = init {
                self.form(init, Destination::One(reg))?;
            } else {
                self.emit(Ins::Load(reg, Immediate::Nil));
            }
            if sequential {
                if qualities.safety > 0 {
                    self.emit_variable_type_checks(reg, decls.types_for(name), var.at)?;
                }
                dynamic +=
                    self.bind_variable(name, reg, var.at, &decls.special, qualities.safety)?;
                self.declare_bound_var_types(name, decls.types_for(name));
            } else {
                parallel.push((name, reg, var.at));
            }
        }
        if qualities.safety > 0 {
            for &(name, reg, at) in &parallel {
                self.emit_variable_type_checks(reg, decls.types_for(name), at)?;
            }
        }
        for (name, reg, at) in parallel {
            dynamic += self.bind_variable(name, reg, at, &decls.special, qualities.safety)?;
        }
        if sequential {
            decls.types.retain(|&(name, _)| {
                !self.bindings[mark.bindings..]
                    .iter()
                    .any(|bound| bound.namespace.is_variable() && &bound.name == name)
            });
        }
        self.free_declarations(decls, mark)?;
        self.body(body, dest)?;
        self.unbind(dynamic);
        self.restore_scope(mark);
        Ok(())
    }

    fn local_functions(
        &mut self,
        specs: &Form,
        body: &[Form],
        dest: Destination,
        recursive: bool,
    ) -> Result<(), EvalError> {
        let specs = specs.list().ok_or_else(|| {
            EvalError::compile(specs.at, "expected a list of function definitions")
        })?;
        let bindings = self.mark_scope();
        let regs = self.reserve(specs.len());
        let mut definitions = Vec::new();

        for (spec, reg) in specs.iter().zip(regs) {
            let Some([name, params, body @ ..]) = spec.list() else {
                return Err(EvalError::compile(
                    spec.at,
                    "invalid local function definition",
                ));
            };
            let (name, namespace) = if let Some(accessor) = setf_accessor(name) {
                (accessor, Namespace::Setter)
            } else if let Some(name) = name.name() {
                (name, Namespace::Function)
            } else {
                return Err(EvalError::compile(name.at, "expected a function name"));
            };
            if definitions
                .iter()
                .any(|(scope, _, _, _): &(ScopedBinding, _, _, _)| {
                    scope.namespace == namespace && &scope.name == name
                })
            {
                return Err(EvalError::compile(spec.at, "duplicate local function"));
            }
            let binding = RegisterBinding {
                frame_depth: self.frame_depth,
                reg,
                boxed: Rc::new(Cell::new(false)),
                capture_kind: if recursive {
                    CaptureKind::MutableCell
                } else {
                    CaptureKind::Value
                },
            };
            if recursive {
                // Make recursive cells before any closure captures them.
                self.box_binding_if_captured(&binding, spec.at);
            }
            let scope = ScopedBinding {
                name: name.clone(),
                namespace,
                binding: Some(binding),
                expander: None,
                used: None,
            };
            if recursive {
                self.bindings.push(scope.clone());
            }
            definitions.push((scope, params, body, spec.at));
        }

        let (decls, body) = declarations(self.runtime, &self.context.warnings, body)?;
        let definition_scope = self.mark_scope();
        if recursive {
            let local = decls.for_local_functions(|name, setf| {
                definitions
                    .iter()
                    .any(|(scope, ..)| &scope.name == name && scope.namespace.binds_function(setf))
            });
            self.free_declarations(local, definition_scope)?;
        }
        let mut flet_bindings = Vec::new();
        for (scope, params, body, at) in definitions {
            let mark = self.used;
            let binding = scope.binding.as_ref().unwrap();
            let reg = binding.reg;
            if recursive {
                let temp = self.reserve(1).start;
                self.function(params, body, Some(&scope.name), Destination::One(temp), at)?;
                self.write_location(BindingLocation::Local(binding.clone()), temp);
            } else {
                self.function(params, body, Some(&scope.name), Destination::One(reg), at)?;
                flet_bindings.push(scope);
            }
            self.release(mark);
        }
        self.restore_scope(definition_scope);
        self.bindings.extend(flet_bindings);
        self.free_declarations(decls, bindings)?;
        self.body(body, dest)?;
        self.restore_scope(bindings);
        Ok(())
    }

    fn arguments(&mut self, args: &[Form]) -> Result<Range<Register>, EvalError> {
        let regs = self.reserve(args.len());
        for (form, reg) in args.iter().zip(regs.clone()) {
            self.form(form, Destination::One(reg))?;
        }
        Ok(regs)
    }

    fn add_constant(&mut self, constant: ConstantTemplate) -> usize {
        let id = self.code.constants.len();
        self.code.constants.push(constant);
        id
    }

    fn symbol(&mut self, name: &Name, at: usize) -> usize {
        self.add_constant(ConstantTemplate::Symbol(name.clone(), at))
    }

    fn constant(&mut self, form: &Form) -> Result<usize, EvalError> {
        self.form_constant(form, false)
    }

    fn form_constant(&mut self, form: &Form, snapshot: bool) -> Result<usize, EvalError> {
        if !snapshot && let Some(root) = form.original_object() {
            return Ok(self.add_constant(ConstantTemplate::Rooted(Rc::clone(root))));
        }
        let datum = match &form.kind {
            FormKind::Integer(n) => ConstantTemplate::Immediate(fixnum(form, *n)?),
            FormKind::Character(character) => {
                ConstantTemplate::Immediate(Word::character(*character))
            }
            FormKind::String(text) => ConstantTemplate::String(text.clone(), form.at),
            FormKind::Literal(root) => ConstantTemplate::Rooted(Rc::clone(root)),
            FormKind::Name(name) => return Ok(self.symbol(name, form.at)),
            FormKind::List(items, tail) => {
                let mut cdr = match tail {
                    Some(tail) => self.form_constant(tail, snapshot)?,
                    None => self.add_constant(ConstantTemplate::Immediate(Word::NIL)),
                };
                for item in items.iter().rev() {
                    let car = self.form_constant(item, snapshot)?;
                    cdr = self.add_constant(ConstantTemplate::Pair(car, cdr));
                }
                return Ok(cdr);
            }
        };
        Ok(self.add_constant(datum))
    }

    fn remember_lambda_list(&mut self, parameters: &Form) -> Result<(), EvalError> {
        // COMPILE and macro expansion may receive caller-owned syntax lists.
        // Keep their shape at compilation time, with literal leaves intact.
        let index = if parameters.list().is_some_and(|items| items.is_empty()) {
            self.add_constant(ConstantTemplate::Immediate(Word::NIL))
        } else {
            self.form_constant(parameters, true)?
        };
        self.code.lambda_list = Some(u32::try_from(index).map_err(|_| {
            EvalError::compile(parameters.at, "lambda list constant index is out of bounds")
        })?);
        Ok(())
    }
}

fn setf_accessor(form: &Form) -> Option<&Name> {
    let [operator, accessor] = form.list()? else {
        return None;
    };
    if operator.name()?.operator() == "SETF" {
        accessor.name()
    } else {
        None
    }
}

fn variable(form: &Form) -> Result<&Name, EvalError> {
    match form.name() {
        Some(name) if !name.constant() => Ok(name),
        _ => Err(EvalError::compile(form.at, "expected a variable name")),
    }
}

#[derive(Default)]
struct EvalWhenSituations {
    compile_toplevel: bool,
    load_toplevel: bool,
    execute: bool,
}

fn eval_when_parts(args: &[Form], at: usize) -> Result<(EvalWhenSituations, &[Form]), EvalError> {
    let Some((situations, body)) = args.split_first() else {
        return Err(EvalError::compile(
            at,
            "EVAL-WHEN requires a situation list",
        ));
    };
    let mut flags = EvalWhenSituations::default();
    for situation in situations.list().ok_or_else(|| {
        EvalError::compile(situations.at, "expected a list of EVAL-WHEN situations")
    })? {
        match (keyword(situation), situation.name().map(Name::operator)) {
            (Some("EXECUTE"), _) | (_, Some("EVAL")) => flags.execute = true,
            (Some("COMPILE-TOPLEVEL"), _) | (_, Some("COMPILE")) => flags.compile_toplevel = true,
            (Some("LOAD-TOPLEVEL"), _) | (_, Some("LOAD")) => flags.load_toplevel = true,
            _ => {
                return Err(EvalError::compile(
                    situation.at,
                    "unknown EVAL-WHEN situation",
                ));
            }
        }
    }
    Ok((flags, body))
}

pub(crate) fn eval_when_body(args: &[Form], at: usize) -> Result<&[Form], EvalError> {
    let (situations, body) = eval_when_parts(args, at)?;
    Ok(if situations.execute { body } else { &[] })
}

fn keyword(form: &Form) -> Option<&str> {
    form.name()
        .filter(|name| name.package() == Some(Package::KEYWORD))
        .map(Name::spelling)
}

#[derive(Default)]
struct Declarations<'a> {
    custom: Vec<CustomDeclaration<'a>>,
    special: Vec<&'a Name>,
    dynamic_extent_vars: Vec<&'a Name>,
    dynamic_extent_fns: Vec<(&'a Name, bool)>,
    inline_declarations: Vec<(&'a Name, bool, InlineDeclaration)>,
    optimize: Vec<(OptimizationQuality, u8)>,
    types: Vec<(&'a Name, &'a Form)>,
    ftypes: Vec<(&'a Name, bool, &'a Form)>,
}

struct CustomDeclaration<'a> {
    name: &'a Name,
    form: &'a Form,
    preceding_qualities: usize,
}

impl<'a> Declarations<'a> {
    fn apply_optimize_declarations(
        &self,
        mut qualities: OptimizationQualities,
    ) -> OptimizationQualities {
        for &(quality, value) in &self.optimize {
            qualities.set(quality, value);
        }
        qualities
    }

    fn types_for<'s>(&'s self, name: &'s Name) -> impl Iterator<Item = &'a Form> + 's {
        self.types
            .iter()
            .filter_map(move |&(var, spec)| (var == name).then_some(spec))
    }

    fn is_dynamic_extent_var(&self, name: &Name) -> bool {
        self.dynamic_extent_vars.iter().any(|&v| v == name)
    }

    fn is_dynamic_extent_fn(&self, name: &Name, setf: bool) -> bool {
        self.dynamic_extent_fns
            .iter()
            .any(|&(fn_name, fn_setf)| fn_name == name && fn_setf == setf)
    }

    fn extend(&mut self, other: Self) {
        let offset = self.optimize.len();
        self.custom
            .extend(other.custom.into_iter().map(|mut declaration| {
                declaration.preceding_qualities += offset;
                declaration
            }));
        self.special.extend(other.special);
        self.dynamic_extent_vars.extend(other.dynamic_extent_vars);
        self.dynamic_extent_fns.extend(other.dynamic_extent_fns);
        self.inline_declarations.extend(other.inline_declarations);
        self.optimize.extend(other.optimize);
        self.types.extend(other.types);
        self.ftypes.extend(other.ftypes);
    }

    /// The declarations bound by LABELS have the same scope as its functions.
    fn for_local_functions(&self, defines: impl Fn(&Name, bool) -> bool) -> Self {
        Self {
            dynamic_extent_fns: self
                .dynamic_extent_fns
                .iter()
                .copied()
                .filter(|&(name, setf)| defines(name, setf))
                .collect(),
            inline_declarations: self
                .inline_declarations
                .iter()
                .copied()
                .filter(|&(name, setf, _)| defines(name, setf))
                .collect(),
            ftypes: self
                .ftypes
                .iter()
                .copied()
                .filter(|&(name, setf, _)| defines(name, setf))
                .collect(),
            ..Self::default()
        }
    }
}

fn function_decl_name_setf(form: &Form) -> Option<(&Name, bool)> {
    if let Some(name) = form.name() {
        Some((name, false))
    } else {
        setf_accessor(form).map(|acc| (acc, true))
    }
}

fn parse_optimize_level(form: &Form) -> Result<u8, EvalError> {
    let level = match &form.kind {
        FormKind::Integer(n) => Some(*n),
        FormKind::Literal(root) => root.get().as_fixnum(),
        _ => None,
    };
    level
        .filter(|n| (0..=3).contains(n))
        .map(|n| n as u8)
        .ok_or_else(|| {
            EvalError::compile(
                form.at,
                "OPTIMIZE quality value must be an integer between 0 and 3",
            )
        })
}

fn optimization_quality(name: &Name) -> Option<OptimizationQuality> {
    match name.operator() {
        "SPEED" => Some(OptimizationQuality::Speed),
        "SAFETY" => Some(OptimizationQuality::Safety),
        "DEBUG" => Some(OptimizationQuality::Debug),
        "SPACE" => Some(OptimizationQuality::Space),
        "COMPILATION-SPEED" => Some(OptimizationQuality::CompilationSpeed),
        _ => None,
    }
}

fn emit_decl_warning(warnings: &Rc<RefCell<Vec<Warning>>>, at: usize, message: impl Into<String>) {
    warnings.borrow_mut().push(Warning {
        style: false,
        at,
        message: message.into(),
    });
}

fn push_typed_variables<'a>(
    decls: &mut Declarations<'a>,
    type_spec: &'a Form,
    vars: &'a [Form],
) -> Result<(), EvalError> {
    for var_form in vars {
        decls.types.push((variable(var_form)?, type_spec));
    }
    Ok(())
}

fn parse_optimize_declaration<'a>(
    decls: &mut Declarations<'a>,
    warnings: &Rc<RefCell<Vec<Warning>>>,
    qualities: &'a [Form],
) -> Result<(), EvalError> {
    for item in qualities {
        let (name, at, level) = if let Some(name) = item.name() {
            (name, item.at, 3)
        } else if let Some([quality_form, level_form]) = item.list()
            && let Some(name) = quality_form.name()
        {
            (name, quality_form.at, parse_optimize_level(level_form)?)
        } else {
            return Err(EvalError::compile(
                item.at,
                "expected an OPTIMIZE quality or (quality value)",
            ));
        };
        if let Some(quality) = optimization_quality(name) {
            decls.optimize.push((quality, level));
        } else {
            emit_decl_warning(
                warnings,
                at,
                format!("unknown OPTIMIZE quality {}", name.spelling()),
            );
        }
    }
    Ok(())
}

fn declaration_items<'a>(
    runtime: &Runtime,
    warnings: &Rc<RefCell<Vec<Warning>>>,
    declaration: &'a Form,
) -> Result<Declarations<'a>, EvalError> {
    let Some([head, operands @ ..]) = declaration.list() else {
        return Err(EvalError::compile(
            declaration.at,
            "expected a declaration specifier",
        ));
    };
    let mut decls = Declarations::default();
    let Some(head_name) = head.name() else {
        emit_decl_warning(
            warnings,
            head.at,
            "declaration specifier is not a symbol name",
        );
        return Ok(decls);
    };
    match (head_name.operator(), operands) {
        ("SPECIAL", names) => {
            decls.special = names.iter().map(variable).collect::<Result<_, _>>()?;
        }
        (operator @ ("INLINE" | "NOTINLINE"), names) => {
            let declaration = if operator == "NOTINLINE" {
                InlineDeclaration::NotInline
            } else {
                InlineDeclaration::Inline
            };
            for form in names {
                let (name, setf) = function_decl_name_setf(form)
                    .ok_or_else(|| EvalError::compile(form.at, "expected a function name"))?;
                decls.inline_declarations.push((name, setf, declaration));
            }
        }
        ("TYPE", [type_spec, vars @ ..]) => {
            push_typed_variables(&mut decls, type_spec, vars)?;
        }
        ("FTYPE", [ftype_spec, fns @ ..]) => {
            for fform in fns {
                let (name, setf) = function_decl_name_setf(fform)
                    .ok_or_else(|| EvalError::compile(fform.at, "expected a function name"))?;
                decls.ftypes.push((name, setf, ftype_spec));
            }
        }
        ("OPTIMIZE", qualities) => {
            parse_optimize_declaration(&mut decls, warnings, qualities)?;
        }
        ("DYNAMIC-EXTENT", items) => {
            for form in items {
                if let Some([head, fname]) = form.list()
                    && head.name().is_some_and(|n| n.operator() == "FUNCTION")
                {
                    let (name, setf) = function_decl_name_setf(fname)
                        .ok_or_else(|| EvalError::compile(fname.at, "expected a function name"))?;
                    decls.dynamic_extent_fns.push((name, setf));
                } else {
                    decls.dynamic_extent_vars.push(variable(form)?);
                }
            }
        }
        ("IGNORE" | "IGNORABLE" | "DECLARATION", _) => {}
        (_, names) => {
            let symbol = runtime.find_named_symbol(head_name);
            if let Some(word) = symbol {
                if runtime
                    .declaration_handlers
                    .borrow()
                    .contains_key(head_name.key())
                {
                    decls.custom.push(CustomDeclaration {
                        name: head_name,
                        form: declaration,
                        preceding_qualities: 0,
                    });
                    return Ok(decls);
                }
                if declarations::is_declaration(runtime, word) {
                    return Ok(decls);
                }
            }
            let is_type = match symbol {
                Some(word) => declarations::names_a_type(runtime, word, head.at)
                    .map_err(crate::condition::Fault::into_eval_error)?,
                None => false,
            } || crate::core_ir::CoreType::from_atomic_name(head_name.operator())
                .is_some();
            if is_type {
                push_typed_variables(&mut decls, head, names)?;
                return Ok(decls);
            }
            // ANSI leaves an unrecognized specifier's consequences undefined; a
            // warning keeps the form compilable and still reports the mistake.
            emit_decl_warning(
                warnings,
                head.at,
                format!("unknown declaration specifier {}", head_name.spelling()),
            );
        }
    }
    Ok(decls)
}

fn declarations<'a>(
    runtime: &Runtime,
    warnings: &Rc<RefCell<Vec<Warning>>>,
    mut body: &'a [Form],
) -> Result<(Declarations<'a>, &'a [Form]), EvalError> {
    let mut decls = Declarations::default();
    while let Some((form, rest)) = body.split_first() {
        let Some(specs) = declaration_specs(form) else {
            break;
        };
        for spec in specs {
            decls.extend(declaration_items(runtime, warnings, spec)?);
        }
        body = rest;
    }
    Ok((decls, body))
}

fn register_declarations(runtime: &Runtime, declaration: &Form) -> Result<(), EvalError> {
    let Some([head, names @ ..]) = declaration.list() else {
        return Ok(());
    };
    if !head
        .name()
        .is_some_and(|name| name.operator() == "DECLARATION")
    {
        return Ok(());
    }
    for name_form in names {
        let name = variable(name_form)?;
        let word = runtime.intern(name, name_form.at)?;
        declarations::register(runtime, word, name_form.at)
            .map_err(crate::condition::Fault::into_eval_error)?;
    }
    Ok(())
}

fn declaration_specs(form: &Form) -> Option<&[Form]> {
    let [head, specs @ ..] = form.list()? else {
        return None;
    };
    (head.name()?.operator() == "DECLARE").then_some(specs)
}

fn function_body<'a>(
    runtime: &Runtime,
    mut body: &'a [Form],
) -> (Vec<&'a Form>, Option<&'a Form>, &'a [Form]) {
    let mut declarations = Vec::new();
    let mut documentation = None;
    while let Some((form, rest)) = body.split_first() {
        if declaration_specs(form).is_some() {
            declarations.push(form);
        } else if documentation.is_none()
            && !rest.is_empty()
            && match &form.kind {
                FormKind::String(_) => true,
                FormKind::Literal(root) => runtime.heap.is_string(root.get()),
                _ => false,
            }
        {
            documentation = Some(form);
        } else {
            break;
        }
        body = rest;
    }
    (declarations, documentation, body)
}

fn fixnum(form: &Form, n: isize) -> Result<Word, EvalError> {
    Word::fixnum(n)
        .ok_or_else(|| EvalError::compile(form.at, "integer exceeds the supported fixnum range"))
}

fn type_intersection(mut types: Vec<Form>) -> Option<Form> {
    match types.len() {
        0 => None,
        1 => types.pop(),
        _ => {
            let at = types[0].at;
            types.insert(0, Form::new(at, FormKind::Name(Name::common_lisp("AND"))));
            Some(Form::new(at, FormKind::List(types, None)))
        }
    }
}

pub(crate) fn constantp(
    runtime: &Runtime,
    value: Word,
    _env: Word,
    at: usize,
) -> Result<bool, EvalError> {
    if let Some((head, tail)) = runtime.heap.cons_fields(value) {
        let quote = runtime.intern(&Name::common_lisp("QUOTE"), at)?;
        return Ok(head == quote
            && runtime
                .heap
                .cons_fields(tail)
                .is_some_and(|(_, cdr)| cdr == Word::NIL));
    }
    if runtime.heap.is_symbol(value) {
        if runtime.heap.symbol_macro_expander(value).is_some() {
            return Ok(false);
        }
        return Ok(constants::is_constant(runtime, value));
    }
    Ok(true)
}

pub(crate) fn proclaim(
    runtime: &Runtime,
    spec: Word,
    at: usize,
) -> Result<(), crate::condition::Fault> {
    let form = crate::form_value::from_value(runtime, &runtime.root(spec), at)?;
    let Some(items) = form.list() else {
        if let FormKind::List(_, Some(tail)) = &form.kind {
            // An improper declaration specifier, such as (FTYPE . FOO), is a
            // type error rather than a mere malformed declaration.
            let datum = crate::form_value::to_value(runtime, tail)?;
            return Err(crate::condition::Fault::type_error(runtime, 
                datum.slot.get(),
                "LIST",
                EvalError::new(at, "a declaration specifier must be a proper list"),
            ));
        }
        return Err(EvalError::compile(form.at, "expected a declaration specifier").into());
    };
    if items
        .first()
        .and_then(Form::name)
        .is_some_and(|name| name.operator() == "DECLARATION")
    {
        for name_form in &items[1..] {
            let name = variable(name_form)?;
            let word = runtime.intern(name, name_form.at)?;
            declarations::register(runtime, word, name_form.at)?;
        }
    }
    let warnings = Rc::default();
    let decls = declaration_items(runtime, &warnings, &form)?;
    if !decls.optimize.is_empty() {
        runtime
            .optimization_qualities
            .set(decls.apply_optimize_declarations(runtime.optimization_qualities.get()));
    }
    for name in decls.special {
        let word = runtime.intern(name, at)?;
        runtime.proclaim_special(word, at)?;
    }
    for (name, setf, declaration) in decls.inline_declarations {
        let word = runtime.intern(name, at)?;
        runtime.proclaim_inline(word, setf, declaration);
    }
    if !decls.ftypes.is_empty() {
        let mut types = runtime.proclaimed_ftypes.borrow_mut();
        for (name, setf, spec) in decls.ftypes {
            types.insert((name.key().clone(), setf), spec.clone());
        }
    }
    if !decls.types.is_empty() {
        record_proclaimed_types(&mut runtime.proclaimed_types.borrow_mut(), decls.types);
    }
    Ok(())
}

fn record_proclaimed_types<'f>(
    types: &mut HashMap<NameKey, Form>,
    declarations: impl IntoIterator<Item = (&'f Name, &'f Form)>,
) {
    for (name, spec) in declarations {
        types.insert(name.key().clone(), spec.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

    use super::{Cell, CompilationContext, Compiler, Ins, Rc, core_lower, enter_macro_expansion};
    use crate::{Runtime, read};

    #[test]
    fn immutable_core_locals_need_no_deferred_boxing() {
        let runtime = Runtime::start().unwrap();
        runtime.initialize_packages().unwrap();
        let form = read::read(
            &runtime,
            "(lambda (test left right)
               (let ((result (if test left right)))
                 (values result left right)))",
        )
        .unwrap();
        let [_, params, body @ ..] = form.list().unwrap() else {
            panic!("lambda form");
        };
        let mut compiler = Compiler::new(&runtime, Vec::new(), CompilationContext::default());
        let (child, _) =
            core_lower::compile_function(&mut compiler, params, body, None, None, form.at).unwrap();
        assert!(child.boxing.is_empty());
        assert!(child.pending_nops.is_empty());
        assert!(
            child
                .finish()
                .instructions
                .iter()
                .all(|ins| !matches!(ins, Ins::BoxLocal { .. }))
        );
    }

    #[test]
    fn unreachable_local_functions_do_not_allocate_inline_bindings() {
        let runtime = Runtime::start().unwrap();
        runtime.initialize_packages().unwrap();
        let form = read::read(
            &runtime,
            "(lambda (x)
               (labels ((first-local (n) (if n (second-local nil) 1))
                        (second-local (n) (if n (first-local nil) 2)))
                 (+ x 1)))",
        )
        .unwrap();
        let [_, params, body @ ..] = form.list().unwrap() else {
            panic!("lambda form");
        };
        let mut compiler = Compiler::new(&runtime, Vec::new(), CompilationContext::default());
        let (_, function) =
            core_lower::compile_function(&mut compiler, params, body, None, None, form.at).unwrap();
        assert_eq!(function.local_functions.len(), 2);
        assert_eq!(function.vars.len(), 3);
    }

    #[test]
    fn macro_expansion_depth_is_restored_during_unwinding() {
        let depth = Rc::new(Cell::new(3));
        let active_depth = Cell::new(0);
        let unwind = catch_unwind(AssertUnwindSafe(|| {
            let _expansion = enter_macro_expansion(Rc::clone(&depth), 0).unwrap();
            active_depth.set(depth.get());
            resume_unwind(Box::new("macro expander panic"));
        }));
        assert!(unwind.is_err());
        assert_eq!(active_depth.get(), 4);
        assert_eq!(depth.get(), 3);
    }

    #[test]
    fn dynamic_extent_declarations_are_retained_on_core_bindings() {
        let runtime = Runtime::start().unwrap();
        runtime.initialize_packages().unwrap();
        let form = read::read(
            &runtime,
            "(lambda (p)
               (declare (dynamic-extent p) (notinline helper))
               (let ((x (cons 1 p)))
                 (declare (dynamic-extent x))
                 (flet ((helper () x))
                   (declare (dynamic-extent (function helper)) (notinline helper))
                   (cons p (helper)))))",
        )
        .unwrap();
        let [_, params, body @ ..] = form.list().unwrap() else {
            panic!("lambda form");
        };
        let mut compiler = Compiler::new(&runtime, Vec::new(), CompilationContext::default());
        let (_, function) =
            core_lower::compile_function(&mut compiler, params, body, None, None, form.at).unwrap();
        let warning_msgs: Vec<_> = compiler
            .context
            .warnings
            .borrow()
            .iter()
            .map(|w| w.message.clone())
            .collect();
        assert!(
            warning_msgs.is_empty(),
            "unexpected warnings: {warning_msgs:?}"
        );
        assert!(
            function
                .vars
                .iter()
                .any(|v| v.name.spelling() == "P" && v.dynamic_extent),
            "parameter P should retain dynamic_extent"
        );
        assert!(
            function
                .vars
                .iter()
                .any(|v| v.name.spelling() == "X" && v.dynamic_extent),
            "let binding X should retain dynamic_extent"
        );
        assert!(
            function
                .local_functions
                .iter()
                .any(|lf| lf.binding.name.spelling() == "HELPER" && lf.binding.dynamic_extent),
            "flet HELPER should retain dynamic_extent"
        );
    }

    #[test]
    fn compiled_flet_nesting_tree_detects_hoistable_inner_functions() {
        let runtime = Runtime::start().unwrap();
        runtime.initialize_packages().unwrap();
        let form = read::read(
            &runtime,
            "(lambda (x)
               (flet ((outer (y)
                        (declare (ignorable y))
                        (flet ((leaf (z) (+ z 1)))
                          (declare (notinline leaf))
                          (+ x (funcall #'leaf x)))))
                 (declare (notinline outer))
                 (funcall #'outer 10)))",
        )
        .unwrap();
        let [_, params, body @ ..] = form.list().unwrap() else {
            panic!("lambda form");
        };
        let mut compiler = Compiler::new(&runtime, Vec::new(), CompilationContext::default());
        let (_, function) =
            core_lower::compile_function(&mut compiler, params, body, None, None, form.at).unwrap();
        let tree = crate::core_ir::nesting::CoreNestingTree::compute(&function);
        let leaf_idx = function
            .local_functions
            .iter()
            .position(|lf| lf.binding.name.spelling() == "LEAF")
            .expect("LEAF local function");
        let leaf_id = crate::core_ir::LocalFunctionId(leaf_idx as u32);
        assert!(tree.has_no_captures(leaf_id));
        assert!(tree.is_hoistable_to_root(leaf_id));
    }

    #[test]
    fn trusted_safety_zero_param_facts_populate_function_template() {
        let runtime = Runtime::start().unwrap();
        runtime.initialize_packages().unwrap();
        let form = read::read(
            &runtime,
            "(lambda (x y)
               (declare (optimize (safety 0)) (type fixnum x) (dynamic-extent y))
               (+ x (if y 1 0)))",
        )
        .unwrap();
        let [_, params, body @ ..] = form.list().unwrap() else {
            panic!("lambda form");
        };
        let mut compiler = Compiler::new(&runtime, Vec::new(), CompilationContext::default());
        let (child, _) =
            core_lower::compile_function(&mut compiler, params, body, None, None, form.at).unwrap();
        let template = child.finish();
        assert_eq!(
            template.facts.params,
            vec![
                zkcl_bytecode::ParamFact {
                    ty: zkcl_bytecode::ParamTypeFact::Fixnum,
                    dynamic_extent: false,
                },
                zkcl_bytecode::ParamFact {
                    ty: zkcl_bytecode::ParamTypeFact::Any,
                    dynamic_extent: true,
                },
            ]
        );
    }
}
