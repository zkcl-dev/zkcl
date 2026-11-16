use std::iter::Peekable;
use std::slice::Iter;

use rustc_hash::FxHashSet;

use crate::Runtime;
use crate::heap::Word;
use crate::symbol::Package;

use super::{
    CallSignature, Compiler, Destination, EvalError, Form, FormKind, Immediate, Ins, Name, NameKey,
    Register, variable,
};

#[derive(Clone, Copy)]
pub(super) struct Variable<'a> {
    pub(super) name: &'a Name,
    pub(super) at: usize,
}

pub(super) enum Pattern<'a> {
    Variable(Variable<'a>),
    List(Box<Parameters<'a>>),
}

impl Pattern<'_> {
    fn at(&self) -> usize {
        match self {
            Self::Variable(variable) => variable.at,
            Self::List(parameters) => parameters.at,
        }
    }
}

impl<'a> Pattern<'a> {
    pub(super) fn ordinary_variable(&self) -> Variable<'a> {
        match self {
            Self::Variable(variable) => *variable,
            Self::List(_) => unreachable!("ordinary lambda lists contain only variables"),
        }
    }
}

pub(super) struct DefaultParameter<'a> {
    pub(super) pattern: Pattern<'a>,
    pub(super) init: Option<&'a Form>,
    pub(super) supplied: Option<Variable<'a>>,
}

pub(super) struct KeywordParameter<'a> {
    pub(super) name: Name,
    pub(super) parameter: DefaultParameter<'a>,
}

pub(super) struct Keywords<'a> {
    pub(super) parameters: Vec<KeywordParameter<'a>>,
    pub(super) allow_other_keys: bool,
}

pub(super) struct Parameters<'a> {
    pub(super) at: usize,
    pub(super) whole: Option<Pattern<'a>>,
    pub(super) required: Vec<Pattern<'a>>,
    pub(super) optional: Vec<DefaultParameter<'a>>,
    pub(super) rest: Option<Pattern<'a>>,
    pub(super) keywords: Option<Keywords<'a>>,
    pub(super) aux: Vec<(Variable<'a>, Option<&'a Form>)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LambdaKind {
    Ordinary,
    Destructuring,
}

impl<'a> Parameters<'a> {
    pub(super) fn destructuring(form: &'a Form) -> Result<Self, EvalError> {
        Self::parse(form, LambdaKind::Destructuring, &mut FxHashSet::default())
    }

    pub(super) fn ordinary(form: &'a Form) -> Result<Self, EvalError> {
        Self::parse(form, LambdaKind::Ordinary, &mut FxHashSet::default())
    }

    fn parse(
        form: &'a Form,
        kind: LambdaKind,
        variables: &mut FxHashSet<&'a NameKey>,
    ) -> Result<Self, EvalError> {
        let (forms, tail) = match &form.kind {
            FormKind::List(forms, Some(tail)) if kind == LambdaKind::Destructuring => {
                (forms.as_slice(), Some(tail.as_ref()))
            }
            _ => (
                form.list()
                    .ok_or_else(|| EvalError::compile(form.at, "expected a lambda list"))?,
                None,
            ),
        };
        let mut params = forms.iter().peekable();
        let whole = if kind == LambdaKind::Destructuring && take_marker(&mut params, "&WHOLE") {
            let pattern = params.next().ok_or_else(|| {
                EvalError::compile(form.at, "&WHOLE requires a destructuring pattern")
            })?;
            Some(parameter_pattern(pattern, kind, variables)?)
        } else {
            None
        };
        let required = section(&mut params)
            .map(|parameter| parameter_pattern(parameter, kind, variables))
            .collect::<Result<Vec<_>, _>>()?;
        let optional = if take_marker(&mut params, "&OPTIONAL") {
            section(&mut params)
                .map(|parameter| default_parameter(parameter, kind, variables))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        let explicit_rest = take_marker(&mut params, "&REST")
            || (kind == LambdaKind::Destructuring && take_marker(&mut params, "&BODY"));
        let rest = if explicit_rest {
            let pattern = params.next().ok_or_else(|| {
                EvalError::compile(form.at, "&REST or &BODY requires a parameter")
            })?;
            Some(parameter_pattern(pattern, kind, variables)?)
        } else {
            None
        };
        let keywords = if take_marker(&mut params, "&KEY") {
            let parameters = section(&mut params)
                .map(|parameter| keyword_parameter(parameter, kind, variables))
                .collect::<Result<Vec<_>, _>>()?;
            Some(Keywords {
                parameters,
                allow_other_keys: take_marker(&mut params, "&ALLOW-OTHER-KEYS"),
            })
        } else {
            None
        };
        let has_aux = take_marker(&mut params, "&AUX");
        let aux = if has_aux {
            section(&mut params)
                .map(|parameter| {
                    let (name, init, _) = parameter_parts(parameter, false)?;
                    // &AUX binds sequentially like LET*: its initializer can
                    // refer to an earlier binding of the same name.
                    Ok((bindable_variable(name)?, init))
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        if let Some(parameter) = params.next() {
            return Err(EvalError::compile(
                parameter.at,
                "unexpected lambda list parameter or keyword",
            ));
        }
        let rest = if let Some(tail) = tail {
            if rest.is_some() || keywords.is_some() || has_aux {
                return Err(EvalError::compile(
                    tail.at,
                    "dotted tail must follow required or optional parameters",
                ));
            }
            Some(Pattern::Variable(parameter_variable(tail, variables)?))
        } else {
            rest
        };

        Ok(Self {
            at: form.at,
            whole,
            required,
            optional,
            rest,
            keywords,
            aux,
        })
    }
}

pub(super) fn generic_required_parameters(form: &Form) -> Result<Vec<Name>, EvalError> {
    let parameters = Parameters::ordinary(form)?;
    let keywords = parameters
        .keywords
        .iter()
        .flat_map(|keys| &keys.parameters)
        .map(|keyword| &keyword.parameter);
    if parameters
        .optional
        .iter()
        .chain(keywords)
        .any(|parameter| parameter.init.is_some() || parameter.supplied.is_some())
    {
        return Err(EvalError::compile(
            form.at,
            "generic lambda list parameters cannot have initializers or supplied-p parameters",
        ));
    }
    if form
        .list()
        .is_some_and(|forms| forms.iter().any(|form| marker(form) == Some("&AUX")))
    {
        return Err(EvalError::compile(
            form.at,
            "&AUX is not a generic lambda list keyword",
        ));
    }
    Ok(parameters
        .required
        .into_iter()
        .map(|parameter| parameter.ordinary_variable().name.clone())
        .collect())
}

pub(super) fn call_signature(form: &Form) -> Result<CallSignature, EvalError> {
    let parameters = Parameters::ordinary(form)?;
    let name = |pattern: Pattern<'_>| pattern.ordinary_variable().name.clone();
    let allow_other_keys = parameters
        .keywords
        .as_ref()
        .is_some_and(|keywords| keywords.allow_other_keys);
    Ok(CallSignature {
        required: parameters.required.into_iter().map(name).collect(),
        optional: parameters
            .optional
            .into_iter()
            .map(|parameter| name(parameter.pattern))
            .collect(),
        rest: parameters.rest.map(name),
        keywords: parameters.keywords.map(|keywords| {
            keywords
                .parameters
                .into_iter()
                .map(|keyword| {
                    if keyword.name.package() == Some(Package::KEYWORD) {
                        Name::keyword(keyword.name.spelling())
                    } else {
                        keyword.name
                    }
                })
                .collect()
        }),
        allow_other_keys,
    })
}

pub(super) fn marker(form: &Form) -> Option<&str> {
    let name = form.name()?.operator();
    match name {
        "&OPTIONAL" | "&REST" | "&KEY" | "&ALLOW-OTHER-KEYS" | "&AUX" | "&BODY" | "&WHOLE"
        | "&ENVIRONMENT" => Some(name),
        _ => None,
    }
}

fn take_marker(params: &mut Peekable<Iter<'_, Form>>, name: &str) -> bool {
    params
        .next_if(|parameter| marker(parameter) == Some(name))
        .is_some()
}

fn section<'a, 'p>(
    params: &'p mut Peekable<Iter<'a, Form>>,
) -> impl Iterator<Item = &'a Form> + 'p {
    std::iter::from_fn(|| params.next_if(|parameter| marker(parameter).is_none()))
}

fn bindable_variable(form: &Form) -> Result<Variable<'_>, EvalError> {
    if marker(form).is_some() {
        return Err(EvalError::compile(form.at, "expected a parameter variable"));
    }
    Ok(Variable {
        name: variable(form)?,
        at: form.at,
    })
}

fn parameter_variable<'a>(
    form: &'a Form,
    variables: &mut FxHashSet<&'a NameKey>,
) -> Result<Variable<'a>, EvalError> {
    let variable = bindable_variable(form)?;
    if !variables.insert(variable.name.key()) {
        return Err(EvalError::compile(form.at, "duplicate parameter variable"));
    }
    Ok(variable)
}

fn parameter_pattern<'a>(
    form: &'a Form,
    kind: LambdaKind,
    variables: &mut FxHashSet<&'a NameKey>,
) -> Result<Pattern<'a>, EvalError> {
    if kind == LambdaKind::Destructuring
        && (matches!(form.kind, FormKind::List(_, _)) || form.list().is_some())
    {
        Parameters::parse(form, kind, variables)
            .map(Box::new)
            .map(Pattern::List)
    } else {
        parameter_variable(form, variables).map(Pattern::Variable)
    }
}

fn parameter_parts(
    form: &Form,
    allow_supplied: bool,
) -> Result<(&Form, Option<&Form>, Option<&Form>), EvalError> {
    if form.name().is_some() {
        return Ok((form, None, None));
    }
    match form.list() {
        Some([name]) => Ok((name, None, None)),
        Some([name, init]) => Ok((name, Some(init), None)),
        Some([name, init, supplied]) if allow_supplied => Ok((name, Some(init), Some(supplied))),
        _ => Err(EvalError::compile(
            form.at,
            "invalid lambda list parameter specifier",
        )),
    }
}

fn default_parameter<'a>(
    form: &'a Form,
    kind: LambdaKind,
    variables: &mut FxHashSet<&'a NameKey>,
) -> Result<DefaultParameter<'a>, EvalError> {
    let (name, init, supplied) = parameter_parts(form, true)?;
    let pattern = if form.name().is_some() {
        Pattern::Variable(parameter_variable(name, variables)?)
    } else {
        parameter_pattern(name, kind, variables)?
    };
    let supplied = supplied
        .map(|form| parameter_variable(form, variables))
        .transpose()?;
    Ok(DefaultParameter {
        pattern,
        init,
        supplied,
    })
}

fn keyword_parameter<'a>(
    form: &'a Form,
    kind: LambdaKind,
    variables: &mut FxHashSet<&'a NameKey>,
) -> Result<KeywordParameter<'a>, EvalError> {
    let (name, init, supplied) = parameter_parts(form, true)?;
    let (name, pattern) = if name.name().is_some() {
        let variable = parameter_variable(name, variables)?;
        (
            Name::keyword(variable.name.spelling()),
            Pattern::Variable(variable),
        )
    } else {
        let Some([keyword, variable]) = name.list() else {
            return Err(EvalError::compile(
                name.at,
                "expected a variable or a keyword name and variable",
            ));
        };
        let keyword = keyword
            .name()
            .ok_or_else(|| EvalError::compile(keyword.at, "expected a symbol as a keyword name"))?;
        (
            keyword.clone(),
            parameter_pattern(variable, kind, variables)?,
        )
    };
    let supplied = supplied
        .map(|form| parameter_variable(form, variables))
        .transpose()?;
    Ok(KeywordParameter {
        name,
        parameter: DefaultParameter {
            pattern,
            init,
            supplied,
        },
    })
}

fn literal_symbol(runtime: &Runtime, form: &Form) -> Option<Word> {
    let (datum, quoted) = match form.list() {
        Some([operator, datum]) if operator.name()?.operator() == "QUOTE" => (datum, true),
        _ => (form, false),
    };
    match &datum.kind {
        FormKind::Name(name)
            if quoted
                || name.package() == Some(Package::KEYWORD)
                || matches!(name.operator(), "T" | "NIL") =>
        {
            runtime.find_named_symbol(name)
        }
        FormKind::Literal(value) if runtime.heap.is_symbol(value.get()) => Some(value.get()),
        FormKind::List(items, None) if items.is_empty() => Some(Word::NIL),
        _ => None,
    }
}

impl<'a> Compiler<'a> {
    /// Diagnose a provably invalid call without evaluating its argument forms.
    /// An unknown key expression might provide :ALLOW-OTHER-KEYS itself.
    pub(super) fn check_lambda_keywords(
        &self,
        parameters: &crate::core_ir::CoreParameterBindings,
        arguments: &[Form],
    ) {
        let Some(keywords) = &parameters.keywords else {
            return;
        };
        let positional = parameters.required.len() + parameters.optional.len();
        if keywords.allow_other_keys || arguments.len() <= positional {
            return;
        }
        let (pairs, leftover) = arguments[positional..].as_chunks::<2>();
        if !leftover.is_empty() {
            return;
        }

        let override_key = self
            .runtime
            .packages
            .borrow()
            .present(Package::KEYWORD, "ALLOW-OTHER-KEYS")
            .map(|(symbol, _)| symbol);
        let mut override_seen = false;
        let mut unknown = None;
        for [key, value] in pairs {
            let Some(symbol) = literal_symbol(self.runtime, key) else {
                return;
            };
            if Some(symbol) == override_key {
                if !override_seen && literal_symbol(self.runtime, value) != Some(Word::NIL) {
                    return;
                }
                override_seen = true;
            } else if unknown.is_none()
                && !keywords
                    .params
                    .iter()
                    .any(|param| self.runtime.find_named_symbol(&param.key) == Some(symbol))
            {
                unknown = Some((key.at, symbol));
            }
        }
        if let Some((at, symbol)) = unknown {
            let name = self
                .runtime
                .heap
                .symbol_name(symbol)
                .expect("literal symbol");
            self.context.warnings.borrow_mut().push(super::Warning {
                style: false,
                at,
                message: format!("unrecognized keyword argument {name} in lambda call"),
            });
        }
    }

    pub(super) fn keyword_table(&mut self, keywords: &Keywords<'_>, at: usize) -> usize {
        let override_key = self.symbol(&Name::keyword("ALLOW-OTHER-KEYS"), at);
        let names = keywords
            .parameters
            .iter()
            .map(|keyword| self.symbol(&keyword.name, keyword.parameter.pattern.at()))
            .collect();
        let table = self.code.keywords.len();
        self.code.keywords.push(zkcl_bytecode::KeywordTable {
            names,
            allow_other_keys: keywords.allow_other_keys,
            override_key,
        });
        table
    }

    pub(super) fn initialize_default<'p>(
        &mut self,
        init: Option<&Form>,
        supplied: Option<Variable<'p>>,
        register: Register,
    ) -> Result<Option<(Variable<'p>, Register)>, EvalError> {
        let flag = supplied.map(|variable| (variable, self.reserve(1).start));
        if let Some((_, flag_register)) = flag {
            self.emit(Ins::Load(flag_register, Immediate::True));
        }
        let branch = self.emit(Ins::IfPresent(register, 0));
        if let Some((_, flag_register)) = flag {
            self.emit(Ins::Load(flag_register, Immediate::Nil));
        }
        self.parameter_initial_value(init, register)?;
        self.code.instructions[branch] = Ins::IfPresent(register, self.code.instructions.len());

        Ok(flag)
    }

    pub(super) fn parameter_initial_value(
        &mut self,
        init: Option<&Form>,
        register: Register,
    ) -> Result<(), EvalError> {
        if let Some(init) = init {
            self.form(init, Destination::One(register))
        } else {
            self.emit(Ins::Load(register, Immediate::Nil));
            Ok(())
        }
    }
}
