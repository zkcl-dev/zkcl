use super::lambda::{DefaultParameter, Parameters, Pattern};
use super::{Compiler, Destination, EvalError, Form, Ins, Name, Register, declarations};

impl Compiler<'_> {
    pub(super) fn destructuring_bind(
        &mut self,
        args: &[Form],
        dest: Destination,
        macro_call: bool,
    ) -> Result<(), EvalError> {
        let [parameters, expression, body @ ..] = args else {
            return Err(EvalError::compile(
                args.first().map_or(0, |form| form.at),
                "expected a destructuring lambda list and expression",
            ));
        };
        let parameters = Parameters::destructuring(parameters)?;
        let (decls, body) = declarations(self.runtime, &self.context.warnings, body)?;
        let source = self.reserve(1).start;
        self.form(expression, Destination::One(source))?;
        self.with_lexical_scope(|compiler, mark| {
            let safety = decls
                .apply_optimize_declarations(compiler.optimization_qualities())
                .safety;
            let dynamic =
                compiler.destructure(parameters, source, &decls.special, safety, macro_call)?;
            compiler.free_declarations(decls, mark)?;
            compiler.body(body, dest)?;
            compiler.unbind(dynamic);
            Ok(())
        })
    }

    fn destructure(
        &mut self,
        params: Parameters<'_>,
        source: Register,
        special: &[&Name],
        safety: u8,
        macro_call: bool,
    ) -> Result<usize, EvalError> {
        let cursor = if params.whole.is_some() {
            let cursor = self.reserve(1).start;
            self.emit(Ins::Copy(cursor, source));
            cursor
        } else {
            source
        };
        let mut dynamic = 0;
        if let Some(whole) = params.whole {
            dynamic += self.bind_pattern(whole, source, special, safety, macro_call)?;
        }
        for pattern in params.required {
            let value = self.reserve(1).start;
            self.emit(Ins::DestructureNext {
                cursor,
                out: value,
                required: true,
                macro_call,
                at: params.at,
            });
            dynamic += self.bind_pattern(pattern, value, special, safety, macro_call)?;
        }
        for parameter in params.optional {
            let value = self.reserve(1).start;
            self.emit(Ins::DestructureNext {
                cursor,
                out: value,
                required: false,
                macro_call,
                at: params.at,
            });
            dynamic +=
                self.bind_destructured_default(parameter, value, special, safety, macro_call)?;
        }
        if let Some(rest) = params.rest {
            // A rest variable can become a capture cell. Keep the original
            // cursor available for the keyword scan at this same level.
            let value = if params.keywords.is_some() {
                let value = self.reserve(1).start;
                self.emit(Ins::Copy(value, cursor));
                value
            } else {
                cursor
            };
            dynamic += self.bind_pattern(rest, value, special, safety, macro_call)?;
        } else if params.keywords.is_none() {
            self.emit(Ins::DestructureEnd {
                cursor,
                macro_call,
                at: params.at,
            });
        }
        if let Some(keywords) = params.keywords {
            let count = keywords.parameters.len();
            let values = self.reserve(count);
            let table = self.keyword_table(&keywords, params.at);
            self.emit(Ins::DestructureKeywords {
                cursor,
                table,
                out: values.clone(),
                macro_call,
                at: params.at,
            });
            for (keyword, value) in keywords.parameters.into_iter().zip(values) {
                dynamic += self.bind_destructured_default(
                    keyword.parameter,
                    value,
                    special,
                    safety,
                    macro_call,
                )?;
            }
        }
        for (variable, init) in params.aux {
            let value = self.reserve(1).start;
            self.parameter_initial_value(init, value)?;
            dynamic += self.bind_variable(variable.name, value, variable.at, special, safety)?;
        }
        Ok(dynamic)
    }

    fn bind_pattern(
        &mut self,
        pattern: Pattern<'_>,
        source: Register,
        special: &[&Name],
        safety: u8,
        macro_call: bool,
    ) -> Result<usize, EvalError> {
        match pattern {
            Pattern::Variable(variable) => {
                self.bind_variable(variable.name, source, variable.at, special, safety)
            }
            Pattern::List(params) => self.destructure(*params, source, special, safety, macro_call),
        }
    }

    fn bind_destructured_default(
        &mut self,
        parameter: DefaultParameter<'_>,
        value: Register,
        special: &[&Name],
        safety: u8,
        macro_call: bool,
    ) -> Result<usize, EvalError> {
        let flag = self.initialize_default(parameter.init, parameter.supplied, value)?;
        let mut dynamic =
            self.bind_pattern(parameter.pattern, value, special, safety, macro_call)?;
        if let Some((variable, flag)) = flag {
            dynamic += self.bind_variable(variable.name, flag, variable.at, special, safety)?;
        }
        Ok(dynamic)
    }
}
