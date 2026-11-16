use std::collections::HashMap;
use std::rc::Rc;

use zkcl_bytecode::{Aarch64Popcount, PopcountImplementations};

use super::{
    Compiler, ConstantTemplate, Destination, EvalError, Form, FormKind, Ins, Name, Package,
};
use crate::Runtime;
use crate::heap::RootSlot;
use crate::vm::FunctionTemplate;

#[derive(Clone)]
pub(super) struct FileVop {
    pub(super) binding: Rc<RootSlot>,
    pub(super) implementations: PopcountImplementations,
}

pub(super) fn is_definition(runtime: &Runtime, name: &Name) -> bool {
    name.spelling() == "%DEFINE-VOP"
        && name.package() == Some(Package::SYS)
        && runtime
            .packages
            .borrow()
            .present(Package::SYS, "%DEFINE-VOP")
            .is_some_and(|(symbol, _)| name.rooted_word() == Some(symbol))
}

impl Compiler<'_> {
    pub(super) fn vop_for_call(&self, symbol: usize) -> Option<FileVop> {
        let ConstantTemplate::Symbol(name, _) = &self.code.constants[symbol] else {
            return None;
        };
        self.context
            .declarations
            .vops
            .borrow()
            .get(name.key())
            .cloned()
    }

    pub(super) fn eval_vop_definition(
        &mut self,
        forms: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let definition = Definition::parse(self.runtime, forms, at)?;
        let name = self.symbol(definition.name, at);
        self.scalar(dest, |out| Ins::Constant(out, name));
        Ok(())
    }

    pub(super) fn compile_vop_definition(
        &mut self,
        forms: &[Form],
        at: usize,
        load: bool,
        emit: &mut impl FnMut(FunctionTemplate) -> Result<(), EvalError>,
    ) -> Result<(), EvalError> {
        let definition = Definition::parse(self.runtime, forms, at)?;
        if !load {
            return Ok(());
        }
        let (_, marker) = self.runtime.fresh_symbol("VOP-BINDING", at)?;
        let binding = Rc::new(self.runtime.root_slot(marker));
        let mut installer = self.nested(Vec::new());
        installer.code.debug.entry = Some(at);
        let marker = installer.add_constant(ConstantTemplate::Rooted(Rc::clone(&binding)));
        let symbol = installer.symbol(definition.translate, at);
        let value = installer.reserve(1).start;
        // The function defined by DEFUN is first available at load time. The
        // installer and callers share the private symbol through the literal graph.
        installer.emit(Ins::ReadCell {
            out: value,
            symbol,
            function: true,
            at,
        });
        installer.emit(Ins::WriteCell {
            symbol: marker,
            src: value,
        });
        let name = installer.symbol(definition.name, at);
        installer.scalar(Destination::All, |out| Ins::Constant(out, name));
        emit(installer.finish())?;
        self.context.declarations.vops.borrow_mut().insert(
            definition.translate.key().clone(),
            FileVop {
                binding,
                implementations: definition.implementations,
            },
        );
        Ok(())
    }
}

struct Definition<'a> {
    name: &'a Name,
    translate: &'a Name,
    implementations: PopcountImplementations,
}

impl<'a> Definition<'a> {
    fn parse(runtime: &Runtime, forms: &'a [Form], at: usize) -> Result<Self, EvalError> {
        let Some((name, options)) = forms.split_first() else {
            return Err(invalid(at, "expected a template name"));
        };
        let name = name
            .name()
            .ok_or_else(|| invalid(name.at, "expected a template name"))?;
        let mut clauses = clauses(
            options,
            &[
                "TRANSLATE",
                "ARGS",
                "RESULTS",
                "EFFECTS",
                "LOWER",
                "ASM",
                "FALLBACK",
            ],
        )?;
        let required = |clauses: &mut HashMap<&str, &'a [Form]>, key| {
            clauses
                .remove(key)
                .ok_or_else(|| invalid(at, format!("missing :{key} clause")))
        };
        let translate = single_name(required(&mut clauses, "TRANSLATE")?)
            .filter(|name| {
                name.package()
                    .is_some_and(|package| package != Package::KEYWORD)
            })
            .ok_or_else(|| invalid(at, ":TRANSLATE needs an interned function name"))?;
        if single_name(required(&mut clauses, "FALLBACK")?) != Some(translate) {
            return Err(invalid(
                at,
                ":FALLBACK must name the ordinary function in :TRANSLATE",
            ));
        }
        let input = typed_operand(required(&mut clauses, "ARGS")?, "UNSIGNED-BYTE", &[64])
            .ok_or_else(|| {
                invalid(
                    at,
                    ":ARGS needs one (UNSIGNED-BYTE 64) operand with :U64 representation",
                )
            })?;
        let output = typed_operand(required(&mut clauses, "RESULTS")?, "INTEGER", &[0, 64])
            .filter(|output| *output != input)
            .ok_or_else(|| {
                invalid(
                    at,
                    ":RESULTS needs one distinct (INTEGER 0 64) operand with :U64 representation",
                )
            })?;
        let effects = required(&mut clauses, "EFFECTS")?;
        if effects.len() != 3
            || !["PURE", "NO-GC", "NO-UNWIND"]
                .iter()
                .all(|&effect| effects.iter().any(|form| keyword(form) == Some(effect)))
        {
            return Err(invalid(
                at,
                ":EFFECTS must contain :PURE, :NO-GC, and :NO-UNWIND",
            ));
        }
        let portable = if let Some(forms) = clauses.remove("LOWER") {
            if !matches!(forms, [target, operator] if keyword(target) == Some("PORTABLE")
                && matches!(operator.list(), Some([head, operand]) if keyword(head) == Some("POPCNT")
                    && operand.name() == Some(input)))
            {
                return Err(invalid(at, "expected (:LOWER :PORTABLE (:POPCNT input))"));
            }
            true
        } else {
            false
        };
        let aarch64 = clauses
            .remove("ASM")
            .map(|forms| assembly(runtime, forms, input, output, at))
            .transpose()?;
        if !portable && aarch64.is_none() {
            return Err(invalid(at, "supply a :LOWER or :ASM implementation"));
        }
        Ok(Self {
            name,
            translate,
            implementations: PopcountImplementations { portable, aarch64 },
        })
    }
}

fn invalid(at: usize, message: impl std::fmt::Display) -> EvalError {
    EvalError::compile(at, format!("DEFINE-VOP: {message}"))
}

fn keyword(form: &Form) -> Option<&str> {
    let name = form.name()?;
    (name.package() == Some(Package::KEYWORD)).then(|| name.spelling())
}

fn clauses<'a>(
    forms: &'a [Form],
    allowed: &[&str],
) -> Result<HashMap<&'a str, &'a [Form]>, EvalError> {
    let mut clauses = HashMap::new();
    for form in forms {
        let Some([head, body @ ..]) = form.list() else {
            return Err(invalid(form.at, "expected a nonempty clause"));
        };
        let key = keyword(head)
            .filter(|key| allowed.contains(key))
            .ok_or_else(|| invalid(head.at, "unknown clause"))?;
        if clauses.insert(key, body).is_some() {
            return Err(invalid(head.at, format!("duplicate :{key} clause")));
        }
    }
    Ok(clauses)
}

fn single_name(forms: &[Form]) -> Option<&Name> {
    let [name] = forms else {
        return None;
    };
    name.name()
}

fn typed_operand<'a>(forms: &'a [Form], type_name: &str, bounds: &[isize]) -> Option<&'a Name> {
    let [list] = forms else {
        return None;
    };
    let [operand] = list.list()? else {
        return None;
    };
    let [name, ty, spec, repr, machine] = operand.list()? else {
        return None;
    };
    let [head, args @ ..] = spec.list()? else {
        return None;
    };
    (keyword(ty) == Some("TYPE")
        && keyword(repr) == Some("REPRESENTATION")
        && keyword(machine) == Some("U64")
        && head.name()?.operator() == type_name
        && args.len() == bounds.len()
        && args
            .iter()
            .zip(bounds)
            .all(|(form, &n)| matches!(form.kind, FormKind::Integer(value) if value == n)))
    .then_some(name.name()?)
}

fn asm_register<'a>(forms: &'a [Form], class: &str) -> Option<&'a Name> {
    let [form] = forms else {
        return None;
    };
    let [name, register] = form.list()? else {
        return None;
    };
    (keyword(register) == Some(class)).then_some(name.name()?)
}

fn assembly(
    runtime: &Runtime,
    forms: &[Form],
    input: &Name,
    output: &Name,
    at: usize,
) -> Result<Aarch64Popcount, EvalError> {
    let Some((target, forms)) = forms.split_first() else {
        return Err(invalid(at, ":ASM needs a target"));
    };
    if keyword(target) != Some("AARCH64") {
        return Err(invalid(target.at, ":ASM currently accepts :AARCH64"));
    }
    let Some(instruction_start) = forms.len().checked_sub(4) else {
        return Err(invalid(at, "expected four AArch64 POPCOUNT instructions"));
    };
    let (options, instructions) = forms.split_at(instruction_start);
    let options = clauses(options, &["REQUIRES", "IN", "OUT", "TEMP"])?;
    if !matches!(options.get("REQUIRES").copied(), Some([feature]) if keyword(feature) == Some("ASIMD"))
        || options
            .get("IN")
            .and_then(|forms| asm_register(forms, "GPR64"))
            != Some(input)
        || options
            .get("OUT")
            .and_then(|forms| asm_register(forms, "GPR64"))
            != Some(output)
    {
        return Err(invalid(
            at,
            "AArch64 POPCOUNT needs :ASIMD and matching :GPR64 input and output",
        ));
    }
    let temp = options
        .get("TEMP")
        .and_then(|forms| asm_register(forms, "SIMD128"))
        .filter(|temp| *temp != input && *temp != output)
        .ok_or_else(|| invalid(at, "AArch64 POPCOUNT needs one distinct :SIMD128 temporary"))?;
    let incoming = format!("{{{}}}", input.spelling());
    let outgoing = format!("{{{}}}", output.spelling());
    let double = format!("{{{}:d}}", temp.spelling());
    let lanes = format!("{{{}:v}}.8b", temp.spelling());
    let half = format!("{{{}:h}}", temp.spelling());
    let byte = format!("{{{}:b}}", temp.spelling());
    let [load, count, sum, store] = instructions else {
        return Err(invalid(at, "expected four AArch64 POPCOUNT instructions"));
    };
    if !instruction(runtime, load, "fmov", &double, &incoming)
        || !instruction(runtime, count, "cnt", &lanes, &lanes)
        || !instruction(runtime, store, "fmov", &outgoing, &double)
    {
        return Err(invalid(
            at,
            "AArch64 POPCOUNT needs FMOV, CNT, UADDLV or ADDV, and FMOV with the declared registers",
        ));
    }
    if instruction(runtime, sum, "uaddlv", &half, &lanes) {
        Ok(Aarch64Popcount::WidenAndAddBytes)
    } else if instruction(runtime, sum, "addv", &byte, &lanes) {
        Ok(Aarch64Popcount::AddBytes)
    } else {
        Err(invalid(
            sum.at,
            "expected UADDLV with the :H view or ADDV with the :B view",
        ))
    }
}

fn instruction(
    runtime: &Runtime,
    form: &Form,
    opcode: &str,
    destination: &str,
    source: &str,
) -> bool {
    let matches = |text: &str| {
        let Some((mnemonic, operands)) = text.trim().split_once(char::is_whitespace) else {
            return false;
        };
        let Some((left, right)) = operands.split_once(',') else {
            return false;
        };
        mnemonic.eq_ignore_ascii_case(opcode)
            && left.trim().eq_ignore_ascii_case(destination)
            && right.trim().eq_ignore_ascii_case(source)
    };
    match &form.kind {
        FormKind::String(text) => matches(text),
        FormKind::Literal(root) => {
            runtime.heap.with_string_slice(root.get(), matches) == Some(true)
        }
        _ => false,
    }
}
