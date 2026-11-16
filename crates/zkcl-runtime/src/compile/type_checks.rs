//! Runtime type-check lowering for `THE` and `DECLARE TYPE` assertions.

use crate::core_ir::{
    CoreType, CoreValuesSpec, IntegerRange, ParsedTheType, ftype_signature, parse_the_type,
    parse_type_spec,
};
use crate::heap::Word;
use crate::read::{Form, FormKind};
use crate::symbol::Name;
use crate::vm::{Callee, ConstantTemplate, Immediate, Instruction as Ins, Primitive, Register};
use crate::{EvalError, Root, Runtime, form_value};

use super::{Compiler, Destination, lambda::marker};

pub(super) fn should_emit_runtime_type_check(spec: &Form) -> bool {
    parse_type_spec(spec) != (CoreType::Top, true)
}

/// Positional checks for a direct call covered by an FTYPE declaration.
pub(super) struct CheckedFtype<'f> {
    required: &'f [Form],
    optional: &'f [Form],
    rest: Option<&'f Form>,
    pub(super) result: Option<&'f Form>,
}

impl<'f> CheckedFtype<'f> {
    pub(super) fn for_call(signature: &'f Form, argc: usize) -> Option<Self> {
        let (arguments, result) = ftype_signature(signature)?;
        let result = result.and_then(Self::checked_type);
        if arguments.name().is_some_and(|name| name.operator() == "*") {
            return Some(Self {
                required: &[],
                optional: &[],
                rest: None,
                result,
            });
        }

        let mut positional = arguments.list()?;
        // Keyword value positions depend on the evaluated keys. This path only
        // uses signatures whose checks can be assigned before evaluation.
        let rest = if let Some(index) = positional
            .iter()
            .position(|form| marker(form) == Some("&REST"))
        {
            let [spec] = &positional[index + 1..] else {
                return None;
            };
            if marker(spec).is_some() {
                return None;
            }
            positional = &positional[..index];
            Some(spec)
        } else {
            None
        };
        let split = positional
            .iter()
            .position(|form| marker(form).is_some())
            .unwrap_or(positional.len());
        let (required, tail) = positional.split_at(split);
        let optional = match tail {
            [] => &[],
            [head, optional @ ..]
                if marker(head) == Some("&OPTIONAL")
                    && optional.iter().all(|form| marker(form).is_none()) =>
            {
                optional
            }
            _ => return None,
        };
        if argc < required.len() || (rest.is_none() && argc > required.len() + optional.len()) {
            return None;
        }
        Some(Self {
            required,
            optional,
            rest,
            result,
        })
    }

    fn checked_type(spec: &Form) -> Option<&Form> {
        (!matches!(spec.name().map(Name::operator), Some("*" | "T"))).then_some(spec)
    }

    pub(super) fn arg(&self, index: usize) -> Option<&'f Form> {
        let spec = if index < self.required.len() {
            &self.required[index]
        } else {
            self.optional
                .get(index - self.required.len())
                .or(self.rest)?
        };
        Self::checked_type(spec)
    }
}

#[derive(Clone, Copy)]
pub(super) enum IntegerCheck {
    Fixnum,
    Range(i32, i32),
}

impl IntegerCheck {
    fn for_spec(runtime: &Runtime, spec: &Form) -> Option<Self> {
        let fixnums = IntegerRange::for_target(runtime.compilation_target());
        Self::for_range(fixnums, fixnums.spec(spec)?)
    }

    pub(super) fn for_range(fixnums: IntegerRange, range: IntegerRange) -> Option<Self> {
        if range == fixnums {
            return Some(Self::Fixnum);
        }
        let (low, high) = range.bounds();
        Some(Self::Range(low.try_into().ok()?, high.try_into().ok()?))
    }

    pub(super) fn branch(self, reg: Register, target: usize) -> Ins {
        match self {
            Self::Fixnum => Ins::UnlessFixnum(reg, target),
            Self::Range(low, high) => Ins::UnlessFixnumInRange(reg, low, high, target),
        }
    }
}

pub(super) fn is_function_type_spec(spec: &Form) -> bool {
    if spec
        .name()
        .is_some_and(|name| name.operator() == "FUNCTION")
    {
        return true;
    }
    matches!(
        &spec.kind,
        FormKind::List(items, None)
            if items
                .first()
                .and_then(Form::name)
                .is_some_and(|name| name.operator() == "FUNCTION")
    )
}

/// Resolve declared types in the compilation environment (CLHS DEFTYPE).
/// Ordinary TYPEP calls still expand their supplied specifier at run time.
fn runtime_check_type_spec<'rt>(
    runtime: &'rt Runtime,
    spec: &Root<'rt>,
    at: usize,
    depth: usize,
    call: Option<&crate::macros::CallExpander<'_>>,
) -> Result<Option<Root<'rt>>, EvalError> {
    if depth >= crate::vm::type_check::TYPE_NESTING_LIMIT {
        return Err(EvalError::new(
            at,
            "type specifier nesting exceeds the limit",
        ));
    }
    let pair = runtime.heap.cons_fields(spec.slot.get());
    let (name, args) = pair.unwrap_or((spec.slot.get(), Word::NIL));
    if let Some(expander) = runtime.type_expander(name) {
        let expanded = crate::vm::type_check::expand_with(runtime, expander, args, at, call)
            .map_err(|fault| fault.host)?;
        let expanded = runtime.root(expanded);
        return Ok(Some(
            runtime_check_type_spec(runtime, &expanded, at, depth + 1, call)?.unwrap_or(expanded),
        ));
    }
    if pair.is_none() {
        return Ok(None);
    }
    let operator = crate::condition::named_word(runtime, name, crate::symbol::Package::COMMON_LISP);
    match operator.as_deref() {
        Some("FUNCTION") => return Ok(Some(runtime.root(name))),
        Some("OR" | "AND" | "NOT" | "CONS") => {}
        _ => return Ok(None),
    }
    // Only these operators take arguments that are themselves type specifiers.
    // In particular an EQL or MEMBER operand may be an arbitrary circular object.
    let name = runtime.root(name);
    let mut args = crate::vm::proper_list(runtime, args, at)?
        .into_iter()
        .map(|arg| runtime.root(arg))
        .collect::<Vec<_>>();
    let mut changed = false;
    for arg in &mut args {
        if let Some(expanded) = runtime_check_type_spec(runtime, arg, at, depth + 1, call)? {
            *arg = expanded;
            changed = true;
        }
    }
    if !changed {
        return Ok(None);
    }
    let mut tail = runtime.nil();
    for item in args.iter().rev().chain([&name]) {
        tail = runtime
            .cons(item, &tail)
            .map_err(|err| EvalError::out_of_memory(at, err))?;
    }
    Ok(Some(tail))
}

impl<'a> Compiler<'a> {
    pub(super) fn emit_the_checked(
        &mut self,
        type_spec: &Form,
        dest: Destination,
        at: usize,
        known_single_value: bool,
        emit_inner: impl FnOnce(&mut Self, Destination) -> Result<(), EvalError>,
    ) -> Result<(), EvalError> {
        match parse_the_type(type_spec) {
            ParsedTheType::Single { ty, exact } => {
                if ty == CoreType::Top && exact {
                    return emit_inner(self, dest);
                }
                self.emit_fixed_the(
                    std::iter::once((0, type_spec)),
                    dest,
                    at,
                    known_single_value || matches!(dest, Destination::One(_)),
                    emit_inner,
                )
            }
            ParsedTheType::Values(vspec) => {
                let has_non_top_opt = vspec
                    .optional
                    .iter()
                    .any(|(_, ty, exact)| (*ty, *exact) != (CoreType::Top, true));
                let has_non_top_rest = vspec
                    .rest
                    .as_ref()
                    .is_some_and(|(_, ty, exact)| (*ty, *exact) != (CoreType::Top, true));
                if known_single_value || (!has_non_top_opt && !has_non_top_rest) {
                    // Optional or rest checks apply only to values actually returned.
                    let first = if vspec.required.is_empty() && known_single_value {
                        vspec.optional.first().or(vspec.rest.as_ref())
                    } else {
                        None
                    };
                    let checks = vspec.required.iter().chain(first).enumerate().filter_map(
                        |(index, (form, ty, exact))| {
                            ((*ty, *exact) != (CoreType::Top, true)).then_some((index, form))
                        },
                    );
                    return self.emit_fixed_the(checks, dest, at, known_single_value, emit_inner);
                }
                emit_inner(self, Destination::All)?;
                self.emit_values_checker_call(&vspec, at)?;
                self.receive_values(dest);
                Ok(())
            }
        }
    }

    fn emit_fixed_the<'f>(
        &mut self,
        checks: impl Iterator<Item = (usize, &'f Form)>,
        dest: Destination,
        at: usize,
        use_single_value: bool,
        emit_inner: impl FnOnce(&mut Self, Destination) -> Result<(), EvalError>,
    ) -> Result<(), EvalError> {
        let mut checks = checks.peekable();
        if checks.peek().is_none() {
            return emit_inner(self, dest);
        }
        let mark = self.used;
        let primary = (use_single_value || matches!(dest, Destination::One(_)))
            .then(|| self.reserve(1).start);
        let inner_dest = match primary {
            Some(reg) if use_single_value => Destination::One(reg),
            _ => Destination::All,
        };
        emit_inner(self, inner_dest)?;

        let checks = checks
            .map(|(index, spec)| {
                let reg = match primary {
                    Some(primary) if index == 0 => primary,
                    _ => {
                        let reg = self.reserve(1).start;
                        if use_single_value {
                            self.emit(Ins::Load(reg, Immediate::Nil));
                        } else {
                            self.emit(Ins::ReadValue(reg, index));
                        }
                        reg
                    }
                };
                (reg, spec)
            })
            .collect::<Vec<_>>();
        if !use_single_value && let Some(primary) = primary {
            self.receive_values(Destination::One(primary));
        }
        // A predicate call may replace the value buffer. Integer checks only branch.
        let save_values = primary.is_none()
            && checks
                .iter()
                .any(|(_, spec)| IntegerCheck::for_spec(self.runtime, spec).is_none());
        if save_values {
            self.emit(Ins::SaveValues);
        }
        self.emit_the_values_checks(&checks, at)?;
        if save_values {
            self.emit(Ins::RestoreValues);
        }
        if let Some(primary) = primary {
            match dest {
                Destination::One(out) => {
                    self.emit(Ins::Copy(out, primary));
                }
                Destination::All => self.select_values(primary..primary + 1),
            }
        }
        self.release(mark);
        Ok(())
    }

    fn emit_values_checker_call(
        &mut self,
        vspec: &CoreValuesSpec,
        at: usize,
    ) -> Result<(), EvalError> {
        let mut checker = Compiler::new(self.runtime, Vec::new(), self.context.clone());
        checker.code.arity = crate::vm::Arity {
            required: 0,
            maximum: None,
        };

        let all_reg = checker.reserve(1).start;
        checker.emit(Ins::RestArguments {
            start: 0,
            out: all_reg,
            at,
        });

        let mut req_checks: Vec<(Register, &Form)> = Vec::new();
        for (i, (spec, ty, exact)) in vspec.required.iter().enumerate() {
            if *ty == CoreType::Top && *exact {
                continue;
            }
            let reg = checker.reserve(1).start;
            checker.emit(Ins::OptionalArgument(i, reg));
            let present = checker.emit(Ins::IfPresent(reg, 0));
            checker.emit(Ins::Load(reg, Immediate::Nil));
            checker.code.instructions[present] =
                Ins::IfPresent(reg, checker.code.instructions.len());
            req_checks.push((reg, spec));
        }

        let mut opt_checks: Vec<(Register, Register, &Form)> = Vec::new();
        for (i, (spec, ty, exact)) in vspec.optional.iter().enumerate() {
            if *ty == CoreType::Top && *exact {
                continue;
            }
            let pos = vspec.required.len() + i;
            let reg = checker.reserve(1).start;
            let present_reg = checker.reserve(1).start;
            checker.emit(Ins::OptionalArgument(pos, reg));
            checker.emit(Ins::Load(present_reg, Immediate::True));
            let present = checker.emit(Ins::IfPresent(reg, 0));
            checker.emit(Ins::Load(present_reg, Immediate::Nil));
            checker.emit(Ins::Load(reg, Immediate::Nil));
            checker.code.instructions[present] =
                Ins::IfPresent(reg, checker.code.instructions.len());
            opt_checks.push((reg, present_reg, spec));
        }

        let rest_check = if let Some((rest_spec, ty, exact)) = &vspec.rest
            && !(*ty == CoreType::Top && *exact)
        {
            let tail_reg = checker.reserve(1).start;
            checker.emit(Ins::RestArguments {
                start: vspec.required.len() + vspec.optional.len(),
                out: tail_reg,
                at,
            });
            Some((tail_reg, rest_spec))
        } else {
            None
        };

        checker.emit(Ins::ReleaseArguments);

        if !req_checks.is_empty() {
            checker.emit_the_values_checks(&req_checks, at)?;
        }
        for (reg, present_reg, spec) in opt_checks {
            let skip = checker.emit(Ins::Unless(present_reg, 0));
            checker.emit_the_values_checks(&[(reg, spec)], at)?;
            checker.code.instructions[skip] =
                Ins::Unless(present_reg, checker.code.instructions.len());
        }
        if let Some((tail_reg, rest_spec)) = rest_check {
            let car_prim = Primitive::named("CAR").expect("CAR primitive");
            let cdr_prim = Primitive::named("CDR").expect("CDR primitive");
            let car_id = checker.add_constant(ConstantTemplate::Immediate(car_prim));
            let cdr_id = checker.add_constant(ConstantTemplate::Immediate(cdr_prim));
            let car_fn = checker.reserve(1).start;
            let cdr_fn = checker.reserve(1).start;
            checker.emit(Ins::Constant(car_fn, car_id));
            checker.emit(Ins::Constant(cdr_fn, cdr_id));
            let elem_reg = checker.reserve(1).start;

            let loop_pc = checker.code.instructions.len();
            let done_jump = checker.emit(Ins::Unless(tail_reg, 0));

            let car_mark = checker.used;
            let car_arg = checker.reserve(1);
            checker.emit(Ins::Copy(car_arg.start, tail_reg));
            checker.call_registers(
                Callee::Value(car_fn),
                car_arg,
                Destination::One(elem_reg),
                at,
                false,
                None,
            );
            checker.release(car_mark);

            checker.emit_the_values_checks(&[(elem_reg, rest_spec)], at)?;

            let cdr_mark = checker.used;
            let cdr_arg = checker.reserve(1);
            checker.emit(Ins::Copy(cdr_arg.start, tail_reg));
            checker.call_registers(
                Callee::Value(cdr_fn),
                cdr_arg,
                Destination::One(tail_reg),
                at,
                false,
                None,
            );
            checker.release(cdr_mark);

            checker.emit(Ins::Jump(loop_pc));
            checker.code.instructions[done_jump] =
                Ins::Unless(tail_reg, checker.code.instructions.len());
        }

        let vl_prim = Primitive::named("VALUES-LIST").expect("VALUES-LIST primitive");
        let vl_id = checker.add_constant(ConstantTemplate::Immediate(vl_prim));
        let vl_fn = checker.reserve(1).start;
        let vl_arg = checker.reserve(1);
        checker.emit(Ins::Constant(vl_fn, vl_id));
        checker.emit(Ins::Copy(vl_arg.start, all_reg));
        checker.call_registers(
            Callee::Value(vl_fn),
            vl_arg,
            Destination::All,
            at,
            false,
            None,
        );

        let code_id = self.add_constant(ConstantTemplate::Function(Box::new(checker.finish())));
        let mark = self.used;
        let callee = self.reserve(1).start;
        self.emit(Ins::Constant(callee, code_id));
        self.emit(Ins::CallValues { callee, at });
        self.release(mark);
        Ok(())
    }

    pub(super) fn emit_the_values_checks(
        &mut self,
        checks: &[(Register, &Form)],
        at: usize,
    ) -> Result<(), EvalError> {
        if checks.is_empty() {
            return Ok(());
        }
        let mark = self.used;
        let mut typep_sym = None;
        let mut functionp_id = None;
        let mut ok_reg = None;
        let err_val = self.reserve(1).start;
        let err_spec = self.reserve(1).start;
        let mut fail_jumps = Vec::with_capacity(checks.len());

        for &(reg, spec_form) in checks {
            if let Some(check) = IntegerCheck::for_spec(self.runtime, spec_form) {
                let spec_id = self.constant(spec_form)?;
                let fj = self.emit(check.branch(reg, 0));
                fail_jumps.push((fj, reg, spec_id));
                continue;
            }
            let ok_reg = *ok_reg.get_or_insert_with(|| self.reserve(1).start);
            if is_function_type_spec(spec_form) {
                let fn_id = *functionp_id.get_or_insert_with(|| {
                    let primitive = Primitive::named("FUNCTIONP").expect("FUNCTIONP primitive");
                    self.add_constant(ConstantTemplate::Immediate(primitive))
                });
                let spec_id = self.symbol(&Name::common_lisp("FUNCTION"), at);
                let call_mark = self.used;
                let callee = self.reserve(1).start;
                let args = self.reserve(1);
                self.emit(Ins::Constant(callee, fn_id));
                self.emit(Ins::Copy(args.start, reg));
                self.call_registers(
                    Callee::Value(callee),
                    args,
                    Destination::One(ok_reg),
                    at,
                    false,
                    None,
                );
                self.release(call_mark);
                let fj = self.emit(Ins::Unless(ok_reg, 0));
                fail_jumps.push((fj, reg, spec_id));
            } else {
                let spec = form_value::to_value(self.runtime, spec_form)?;
                let normalized = runtime_check_type_spec(
                    self.runtime,
                    &spec,
                    at,
                    0,
                    self.context.call_expander,
                )?;
                let reported = self.constant(spec_form)?;
                let spec_id = match normalized {
                    Some(spec) => {
                        self.add_constant(ConstantTemplate::Rooted(std::rc::Rc::new(spec.slot)))
                    }
                    None => reported,
                };
                let typep_sym =
                    *typep_sym.get_or_insert_with(|| self.symbol(&Name::common_lisp("TYPEP"), at));
                let call_mark = self.used;
                let args = self.reserve(2);
                self.emit(Ins::Copy(args.start, reg));
                self.emit(Ins::Constant(args.start + 1, spec_id));
                self.call_registers(
                    Callee::Named(typep_sym),
                    args,
                    Destination::One(ok_reg),
                    at,
                    false,
                    None,
                );
                self.release(call_mark);
                let fj = self.emit(Ins::Unless(ok_reg, 0));
                fail_jumps.push((fj, reg, reported));
            }
        }

        let done_jump = self.emit(Ins::Jump(0));

        let mut to_common = Vec::with_capacity(fail_jumps.len());
        for (fj, reg, spec_id) in fail_jumps {
            let fail_pc = self.code.instructions.len();
            match &mut self.code.instructions[fj] {
                Ins::Unless(_, target)
                | Ins::UnlessFixnum(_, target)
                | Ins::UnlessFixnumInRange(_, _, _, target) => *target = fail_pc,
                _ => unreachable!("expected a type-check branch"),
            }
            self.emit(Ins::Copy(err_val, reg));
            self.emit(Ins::Constant(err_spec, spec_id));
            to_common.push(self.emit(Ins::Jump(0)));
        }

        let fail_common = self.code.instructions.len();
        for j in to_common {
            self.code.instructions[j] = Ins::Jump(fail_common);
        }
        let err_args = self.reserve(5);
        let type_err_sym = self.symbol(&Name::common_lisp("TYPE-ERROR"), at);
        let kw_datum = self.symbol(&Name::keyword("DATUM"), at);
        let kw_expected = self.symbol(&Name::keyword("EXPECTED-TYPE"), at);
        self.emit(Ins::Constant(err_args.start, type_err_sym));
        self.emit(Ins::Constant(err_args.start + 1, kw_datum));
        self.emit(Ins::Copy(err_args.start + 2, err_val));
        self.emit(Ins::Constant(err_args.start + 3, kw_expected));
        self.emit(Ins::Copy(err_args.start + 4, err_spec));
        let err_sym = self.symbol(&Name::common_lisp("ERROR"), at);
        let dummy_out = self.reserve(1).start;
        self.call_registers(
            Callee::Named(err_sym),
            err_args,
            Destination::One(dummy_out),
            at,
            false,
            None,
        );

        let done_pc = self.code.instructions.len();
        self.code.instructions[done_jump] = Ins::Jump(done_pc);
        self.release(mark);
        Ok(())
    }
}
