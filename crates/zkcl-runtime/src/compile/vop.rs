use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use zkcl_bytecode::{Aarch64Popcount, WordCountOp};

use super::{
    Compiler, ConstantTemplate, Destination, EvalError, Form, FormKind, Ins, Name, Package, keyword,
};
use crate::condition::Fault;
use crate::heap::{RootSlot, Word};
use crate::image::{ImageReader, ImageWriter};
use crate::vm::{Callee, FunctionTemplate, Primitive};
use crate::{Root, Runtime};

pub(super) struct TransformRule {
    operation: WordCountOp,
    // A compile-time-only declaration can be resolved in a loading runtime
    // only if that runtime has already installed the same semantic rule.
    load_binding: Option<Rc<RootSlot>>,
}

struct CapturedTransform {
    operation: WordCountOp,
    binding: Rc<RootSlot>,
}

#[derive(Clone, Copy)]
pub(super) struct Aarch64Template {
    assembly: Aarch64Popcount,
    cost: isize,
}

const AARCH64_ASSEMBLIES: [Aarch64Popcount; 2] =
    [Aarch64Popcount::AddBytes, Aarch64Popcount::WidenAndAddBytes];
const DEFAULT_TEMPLATE_COST: isize = 8;

impl Aarch64Template {
    fn descriptor(self) -> usize {
        match self.assembly {
            Aarch64Popcount::AddBytes => 0,
            Aarch64Popcount::WidenAndAddBytes => 1,
        }
    }

    fn from_descriptor(descriptor: usize, cost: isize) -> Option<Self> {
        let assembly = *AARCH64_ASSEMBLIES.get(descriptor)?;
        (0..=Word::FIXNUM_MAX)
            .contains(&cost)
            .then_some(Self { assembly, cost })
    }
}

#[derive(Default)]
pub(crate) struct Registry {
    transforms: RefCell<HashMap<usize, CapturedTransform>>,
    // The checked AArch64 template definitions all implement POPCNT.
    aarch64_popcount: RefCell<HashMap<usize, Aarch64Template>>,
}

impl Registry {
    fn install_transform(
        &self,
        runtime: &Runtime,
        id: usize,
        symbol: Word,
        rule: CapturedTransform,
    ) {
        let current = runtime.heap.symbol_cell(symbol, true);
        // A trace wrapper is a different callable. Capture the definition UNTRACE
        // will restore, so the equality test keeps tracing observable.
        let expected = current.map(|function| {
            runtime
                .traced
                .borrow()
                .iter()
                .find(|record| {
                    !record.is_setf
                        && record.symbol.get() == symbol
                        && record.wrapper.get() == function
                })
                .map_or(function, |record| record.original.get())
        });
        // A generic or funcallable instance can change behavior in place.
        let expected = expected.filter(|&function| {
            runtime.heap.function_code(function).is_some()
                || Primitive::compiled_name(function).is_some()
        });
        runtime
            .heap
            .set_cons_car(rule.binding.get(), expected.unwrap_or(Word::NIL));
        let mut transforms = self.transforms.borrow_mut();
        if expected.is_some() {
            transforms.insert(id, rule);
        } else {
            transforms.remove(&id);
        }
    }

    fn operation(&self, runtime: &Runtime, name: &Name) -> Option<WordCountOp> {
        let transforms = self.transforms.borrow();
        if transforms.is_empty() {
            return None;
        }
        let symbol = runtime.find_named_symbol(name)?;
        let id = runtime.heap.symbol_attributes(symbol)?.id;
        transforms.get(&id).map(|rule| rule.operation)
    }

    pub(crate) fn binding(
        &self,
        runtime: &Runtime,
        symbol: Word,
        descriptor: isize,
    ) -> Option<Word> {
        let id = runtime.heap.symbol_attributes(symbol)?.id;
        let transforms = self.transforms.borrow();
        let rule = transforms.get(&id)?;
        if rule.operation.descriptor() as isize != descriptor {
            return None;
        }
        runtime.heap.cons_car(rule.binding.get())
    }

    pub(crate) fn write_image(&self, image: &mut ImageWriter<'_>) -> Result<(), EvalError> {
        let transforms = self.transforms.borrow();
        image.usize(transforms.len());
        for (&id, rule) in transforms.iter() {
            image.usize(id);
            image.usize(rule.operation.descriptor());
            image.word(rule.binding.get())?;
        }
        let templates = self.aarch64_popcount.borrow();
        image.usize(templates.len());
        for (&id, template) in templates.iter() {
            image.usize(id);
            image.usize(template.descriptor());
            image.usize(template.cost as usize);
        }
        Ok(())
    }

    pub(crate) fn read_image(
        runtime: &Runtime,
        image: &mut ImageReader<'_>,
    ) -> Result<Self, EvalError> {
        let mut transforms = HashMap::new();
        for _ in 0..image.usize()? {
            let id = image.usize()?;
            let operation =
                WordCountOp::from_descriptor(image.usize()?).ok_or_else(crate::image::invalid)?;
            let cell = image.word()?;
            if runtime.heap.cons_car(cell).is_none() {
                return Err(crate::image::invalid());
            }
            let binding = Rc::new(runtime.root_slot(cell));
            if transforms
                .insert(id, CapturedTransform { binding, operation })
                .is_some()
            {
                return Err(crate::image::invalid());
            }
        }
        let mut templates = HashMap::new();
        for _ in 0..image.usize()? {
            let id = image.usize()?;
            let descriptor = image.usize()?;
            let cost = isize::try_from(image.usize()?).map_err(|_| crate::image::invalid())?;
            let template = Aarch64Template::from_descriptor(descriptor, cost)
                .ok_or_else(crate::image::invalid)?;
            if templates.insert(id, template).is_some() {
                return Err(crate::image::invalid());
            }
        }
        Ok(Self {
            transforms: RefCell::new(transforms),
            aarch64_popcount: RefCell::new(templates),
        })
    }
}

pub(crate) fn install_transform<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let invalid = || DeclarationKind::Transform.invalid(at, "invalid compiled installer");
    let [symbol, binding, descriptor] = args else {
        return Err(invalid().into());
    };
    let symbol = symbol.slot.get();
    let attributes = runtime.heap.symbol_attributes(symbol).ok_or_else(invalid)?;
    let binding = binding.slot.get();
    let operation = descriptor
        .slot
        .get()
        .as_fixnum()
        .and_then(|number| usize::try_from(number).ok())
        .and_then(WordCountOp::from_descriptor)
        .ok_or_else(invalid)?;
    if runtime.heap.cons_car(binding).is_none() {
        return Err(invalid().into());
    }
    runtime.vops.install_transform(
        runtime,
        attributes.id,
        symbol,
        CapturedTransform {
            binding: Rc::new(runtime.root_slot(binding)),
            operation,
        },
    );
    Ok(vec![runtime.nil()])
}

pub(crate) fn install_vop<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let invalid = || invalid(at, "invalid compiled installer");
    let [name, descriptor, cost] = args else {
        return Err(invalid().into());
    };
    let attributes = runtime
        .heap
        .symbol_attributes(name.slot.get())
        .ok_or_else(invalid)?;
    let descriptor = descriptor
        .slot
        .get()
        .as_fixnum()
        .and_then(|number| usize::try_from(number).ok())
        .ok_or_else(invalid)?;
    let cost = cost.slot.get().as_fixnum().ok_or_else(invalid)?;
    let template = Aarch64Template::from_descriptor(descriptor, cost).ok_or_else(invalid)?;
    runtime
        .vops
        .aarch64_popcount
        .borrow_mut()
        .insert(attributes.id, template);
    Ok(vec![runtime.nil()])
}

#[derive(Clone, Copy)]
pub(super) enum DeclarationKind {
    Transform,
    Vop,
}

impl DeclarationKind {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Transform => "DEFTRANSFORM",
            Self::Vop => "DEFINE-VOP",
        }
    }

    fn invalid(self, at: usize, message: impl std::fmt::Display) -> EvalError {
        EvalError::compile(at, format!("{}: {message}", self.name()))
    }
}

pub(super) fn declaration_kind(runtime: &Runtime, name: &Name) -> Option<DeclarationKind> {
    let spelling = name.spelling();
    let kind = match spelling {
        "%DEFTRANSFORM" => DeclarationKind::Transform,
        "%DEFINE-VOP" => DeclarationKind::Vop,
        _ => return None,
    };
    (name.package() == Some(Package::SYS)
        && runtime
            .packages
            .borrow()
            .present(Package::SYS, spelling)
            .is_some_and(|(symbol, _)| name.rooted_word() == Some(symbol)))
    .then_some(kind)
}

pub(super) struct CallSpecialization {
    pub(super) symbol: usize,
    pub(super) binding: usize,
    pub(super) operation: WordCountOp,
    pub(super) aarch64_popcount: Option<Aarch64Popcount>,
}

impl Compiler<'_> {
    pub(super) fn transform_call_symbol(&mut self, name: &Name, at: usize) -> usize {
        if let Some(&(symbol, _)) = self.transform_constants.get(name.key()) {
            symbol
        } else {
            self.symbol(name, at)
        }
    }

    pub(super) fn specialize_call(&mut self, mut symbol: usize) -> Option<CallSpecialization> {
        let ConstantTemplate::Symbol(name, _) = &self.code.constants[symbol] else {
            return None;
        };
        let (operation, binding) = match self
            .context
            .declarations
            .transforms
            .borrow()
            .get(name.key())
        {
            Some(rule) => {
                let binding = rule.load_binding.as_ref().map_or_else(
                    || operation_constant(rule.operation),
                    |cell| ConstantTemplate::LoadTimeCell(Rc::clone(cell)),
                );
                (rule.operation, binding)
            }
            None => {
                // Separate compilation units resolve the registration at execution.
                let operation = self.runtime.vops.operation(self.runtime, name)?;
                (operation, operation_constant(operation))
            }
        };
        let key = name.key().clone();
        let binding = if let Some(&(previous_symbol, previous_binding)) =
            self.transform_constants.get(&key)
        {
            symbol = previous_symbol;
            if same_binding(&binding, &self.code.constants[previous_binding]) {
                previous_binding
            } else {
                self.add_constant(binding)
            }
        } else {
            self.add_constant(binding)
        };
        self.transform_constants.insert(key, (symbol, binding));
        let aarch64_popcount =
            if operation == WordCountOp::Popcount {
                let pending = self.context.declarations.aarch64_popcount.borrow();
                let loaded = self.runtime.vops.aarch64_popcount.borrow();
                pending
                    .values()
                    .chain(loaded.iter().filter_map(|(id, template)| {
                        (!pending.contains_key(id)).then_some(template)
                    }))
                    // Prefer a fixed checked assembly form on ties independently of symbol IDs.
                    .min_by_key(|template| (template.cost, template.descriptor()))
                    .map(|template| template.assembly)
            } else {
                None
            };
        Some(CallSpecialization {
            symbol,
            binding,
            operation,
            aarch64_popcount,
        })
    }

    pub(super) fn eval_vop_declaration(
        &mut self,
        kind: DeclarationKind,
        forms: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let declaration = Declaration::parse(self.runtime, kind, forms, at)?;
        self.emit_vop_installer(&declaration, dest, at);
        self.remember_vop_declaration(declaration, true);
        Ok(())
    }

    pub(super) fn compile_vop_declaration(
        &mut self,
        kind: DeclarationKind,
        forms: &[Form],
        at: usize,
        evaluate: bool,
        load: bool,
        emit: &mut impl FnMut(FunctionTemplate) -> Result<(), EvalError>,
    ) -> Result<(), EvalError> {
        let declaration = Declaration::parse(self.runtime, kind, forms, at)?;
        if load {
            let mut installer = self.nested(Vec::new());
            installer.code.debug.entry = Some(at);
            installer.emit_vop_installer(&declaration, Destination::All, at);
            emit(installer.finish())?;
        }
        if evaluate {
            match &declaration.body {
                DeclarationBody::Transform(rule) => {
                    let symbol = self.runtime.intern(declaration.name, at)?;
                    self.runtime.vops.install_transform(
                        self.runtime,
                        declaration.id,
                        symbol,
                        CapturedTransform {
                            binding: Rc::clone(&rule.binding),
                            operation: rule.operation,
                        },
                    );
                }
                DeclarationBody::Vop(template) => {
                    self.runtime
                        .vops
                        .aarch64_popcount
                        .borrow_mut()
                        .insert(declaration.id, *template);
                }
            }
        }
        self.remember_vop_declaration(declaration, load);
        Ok(())
    }

    fn remember_vop_declaration(&mut self, declaration: Declaration<'_>, load: bool) {
        match declaration.body {
            DeclarationBody::Transform(rule) => {
                self.context.declarations.transforms.borrow_mut().insert(
                    declaration.name.key().clone(),
                    TransformRule {
                        operation: rule.operation,
                        load_binding: load.then_some(rule.binding),
                    },
                );
            }
            DeclarationBody::Vop(template) => {
                self.context
                    .declarations
                    .aarch64_popcount
                    .borrow_mut()
                    .insert(declaration.id, template);
            }
        }
    }

    fn emit_vop_installer(&mut self, declaration: &Declaration<'_>, dest: Destination, at: usize) {
        let mark = self.used;
        let callee = self.reserve(1).start;
        let args = self.reserve(3);
        let (installer, payload) = match &declaration.body {
            DeclarationBody::Transform(rule) => (
                crate::macros::INSTALL_TRANSFORM,
                [
                    ConstantTemplate::LoadTimeCell(Rc::clone(&rule.binding)),
                    operation_constant(rule.operation),
                ],
            ),
            DeclarationBody::Vop(template) => (
                crate::macros::INSTALL_VOP,
                [
                    fixnum_constant(template.descriptor() as isize),
                    fixnum_constant(template.cost),
                ],
            ),
        };
        let install =
            crate::macros::compiled_native(installer).expect("compiler declaration installer");
        let install = self.add_constant(ConstantTemplate::Immediate(install));
        self.emit(Ins::Constant(callee, install));
        let name = self.symbol(declaration.name, at);
        let [first, second] = payload.map(|constant| self.add_constant(constant));
        for (reg, constant) in args.clone().zip([name, first, second]) {
            self.emit(Ins::Constant(reg, constant));
        }
        self.call_registers(
            Callee::Value(callee),
            args,
            Destination::One(callee),
            at,
            false,
            None,
        );
        self.release(mark);
        self.scalar(dest, |out| Ins::Constant(out, name));
    }
}

fn same_binding(a: &ConstantTemplate, b: &ConstantTemplate) -> bool {
    match (a, b) {
        (ConstantTemplate::Immediate(a), ConstantTemplate::Immediate(b)) => a == b,
        (ConstantTemplate::LoadTimeCell(a), ConstantTemplate::LoadTimeCell(b)) => Rc::ptr_eq(a, b),
        _ => false,
    }
}

fn fixnum_constant(value: isize) -> ConstantTemplate {
    ConstantTemplate::Immediate(
        Word::fixnum(value).expect("validated compiler declaration argument"),
    )
}

fn operation_constant(operation: WordCountOp) -> ConstantTemplate {
    fixnum_constant(operation.descriptor() as isize)
}

struct Declaration<'a> {
    name: &'a Name,
    id: usize,
    body: DeclarationBody,
}

enum DeclarationBody {
    Transform(CapturedTransform),
    Vop(Aarch64Template),
}

impl<'a> Declaration<'a> {
    fn parse(
        runtime: &Runtime,
        kind: DeclarationKind,
        forms: &'a [Form],
        at: usize,
    ) -> Result<Self, EvalError> {
        let (head, forms) = forms
            .split_first()
            .ok_or_else(|| kind.invalid(at, "missing declaration name"))?;
        let name = head
            .name()
            .filter(|name| {
                name.package()
                    .is_some_and(|package| package != Package::KEYWORD)
            })
            .ok_or_else(|| kind.invalid(head.at, "expected an interned symbol name"))?;
        let symbol = runtime.intern(name, head.at)?;
        let id = runtime
            .heap
            .symbol_attributes(symbol)
            .expect("interned declaration name")
            .id;
        let body = match kind {
            DeclarationKind::Transform => {
                DeclarationBody::Transform(parse_transform(runtime, forms, at)?)
            }
            DeclarationKind::Vop => DeclarationBody::Vop(parse_template(forms, at)?),
        };
        Ok(Self { name, id, body })
    }
}

fn parse_transform(
    runtime: &Runtime,
    forms: &[Form],
    at: usize,
) -> Result<CapturedTransform, EvalError> {
    let kind = DeclarationKind::Transform;
    let [parameters, expression] = forms else {
        return Err(kind.invalid(
            at,
            "expected an operand list and a unary operation expression",
        ));
    };
    let parameter = match parameters.list() {
        Some([parameter]) => parameter.name().filter(|parameter| !parameter.constant()),
        _ => None,
    }
    .ok_or_else(|| {
        kind.invalid(
            parameters.at,
            "expected an operand list with one variable name",
        )
    })?;
    let Some([head, operand]) = expression.list() else {
        return Err(kind.invalid(expression.at, "expected a unary operation expression"));
    };
    if operand.name() != Some(parameter) {
        return Err(kind.invalid(operand.at, "expected the declared operand"));
    }
    let operation = word_count_operation(head)
        .ok_or_else(|| kind.invalid(head.at, "expected :POPCNT, :CLZ, or :CTZ"))?;
    let binding = runtime
        .heap
        .cons(|| (Word::NIL, Word::NIL))
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(CapturedTransform {
        operation,
        binding: Rc::new(runtime.root_slot(binding)),
    })
}

fn parse_template(forms: &[Form], at: usize) -> Result<Aarch64Template, EvalError> {
    let Some((operation, options)) = forms.split_first() else {
        return Err(invalid(at, "expected an operation and an :ASM clause"));
    };
    if word_count_operation(operation) != Some(WordCountOp::Popcount) {
        return Err(invalid(
            operation.at,
            "expected :POPCNT for an :AARCH64 template",
        ));
    }
    let mut cost = None;
    let mut assembly = None;
    for option in options {
        let Some([head, args @ ..]) = option.list() else {
            return Err(invalid(option.at, "expected a nonempty clause"));
        };
        match keyword(head) {
            Some("COST") if cost.is_none() => {
                cost = Some(parse_cost(args, option.at)?);
            }
            Some("ASM") if assembly.is_none() => {
                assembly = Some(parse_assembly(args, option.at)?);
            }
            Some(key @ ("COST" | "ASM")) => {
                return Err(invalid(head.at, format!("duplicate :{key} clause")));
            }
            _ => return Err(invalid(head.at, "unknown clause")),
        }
    }
    Ok(Aarch64Template {
        assembly: assembly.ok_or_else(|| invalid(at, "missing :ASM clause"))?,
        cost: cost.unwrap_or(DEFAULT_TEMPLATE_COST),
    })
}

fn parse_cost(forms: &[Form], at: usize) -> Result<isize, EvalError> {
    let cost = match forms {
        [form] => match &form.kind {
            FormKind::Integer(value) => Some(*value),
            FormKind::Literal(root) => root.get().as_fixnum(),
            _ => None,
        },
        _ => None,
    };
    cost.filter(|n| (0..=Word::FIXNUM_MAX).contains(n))
        .ok_or_else(|| invalid(at, ":COST requires a nonnegative fixnum"))
}

fn parse_assembly(forms: &[Form], at: usize) -> Result<Aarch64Popcount, EvalError> {
    if let [target, reduction] = forms
        && keyword(target) == Some("AARCH64")
    {
        match keyword(reduction) {
            Some("ADDV") => return Ok(Aarch64Popcount::AddBytes),
            Some("UADDLV") => return Ok(Aarch64Popcount::WidenAndAddBytes),
            _ => {}
        }
    }
    Err(invalid(
        at,
        "expected (:ASM :AARCH64 :UADDLV) or (:ASM :AARCH64 :ADDV)",
    ))
}

fn word_count_operation(form: &Form) -> Option<WordCountOp> {
    match keyword(form)? {
        "POPCNT" => Some(WordCountOp::Popcount),
        "CLZ" => Some(WordCountOp::LeadingZeros),
        "CTZ" => Some(WordCountOp::TrailingZeros),
        _ => None,
    }
}

fn invalid(at: usize, message: impl std::fmt::Display) -> EvalError {
    DeclarationKind::Vop.invalid(at, message)
}
