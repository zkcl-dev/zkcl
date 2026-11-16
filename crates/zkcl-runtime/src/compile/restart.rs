use super::{
    Compiler, Destination, EvalError, Form, Ins, Namespace, Register, RestartSpec, RestartTarget,
    keyword,
};

impl<'a> Compiler<'a> {
    fn restart_table(&mut self, specs: Vec<RestartSpec>, at: usize) -> usize {
        let table = self.code.restarts.len();
        self.code
            .restarts
            .push(zkcl_bytecode::RestartTable { restarts: specs });
        self.emit(Ins::EnterRestarts { table, at });
        table
    }

    pub(super) fn restart_bind(
        &mut self,
        bindings: &Form,
        body: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let bindings = bindings.list().ok_or_else(|| {
            EvalError::compile(bindings.at, "expected a list of restart bindings")
        })?;
        if bindings.is_empty() {
            return self.body(body, dest);
        }
        let mut specs = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let Some([name, function, options @ ..]) = binding.list() else {
                return Err(EvalError::compile(
                    binding.at,
                    "expected (restart-name function ...)",
                ));
            };
            let name_index = self.restart_name(name)?;
            let object = self.reserve(1).start;
            let target = self.reserve(1).start;
            self.form(function, Destination::One(target))?;
            let mut test = None;
            let mut interactive = None;
            let mut report = None;
            let (pairs, remainder) = options.as_chunks::<2>();
            for pair in pairs {
                let option = match keyword(&pair[0]) {
                    Some("TEST-FUNCTION") if test.is_none() => &mut test,
                    Some("INTERACTIVE-FUNCTION") if interactive.is_none() => &mut interactive,
                    Some("REPORT-FUNCTION") if report.is_none() => &mut report,
                    _ => {
                        return Err(EvalError::compile(
                            pair[0].at,
                            "unsupported or duplicate RESTART-BIND option",
                        ));
                    }
                };
                let reg = self.reserve(1).start;
                self.form(&pair[1], Destination::One(reg))?;
                *option = Some(reg);
            }
            if !remainder.is_empty() {
                return Err(EvalError::compile(
                    binding.at,
                    "restart options must be keyword/value pairs",
                ));
            }
            specs.push(RestartSpec {
                name: name_index,
                object,
                test,
                interactive,
                report,
                target: RestartTarget::Function(target),
            });
        }
        self.restart_table(specs, at);
        self.body(body, dest)?;
        self.emit(Ins::LeaveRestarts);
        Ok(())
    }

    pub(super) fn restart_case(
        &mut self,
        form: &Form,
        clauses: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        if clauses.is_empty() {
            return self.form(form, dest);
        }
        let mut specs = Vec::with_capacity(clauses.len());
        let mut arms = Vec::with_capacity(clauses.len());
        for clause in clauses {
            let Some([name, params, body @ ..]) = clause.list() else {
                return Err(EvalError::compile(clause.at, "invalid RESTART-CASE clause"));
            };
            let mut body = body;
            let name_index = self.restart_name(name)?;
            let object = self.reserve(1).start;
            let mut test = None;
            let mut interactive = None;
            let mut report = None;
            while let Some(key) = body.first().and_then(keyword) {
                let option = match key {
                    "TEST" => &mut test,
                    "INTERACTIVE" => &mut interactive,
                    "REPORT" => &mut report,
                    _ => break,
                };
                let Some((expr, rest)) = body[1..].split_first() else {
                    return Err(EvalError::compile(
                        body[0].at,
                        "expected a function for the restart option",
                    ));
                };
                if option.is_some() {
                    return Err(EvalError::compile(body[0].at, "duplicate restart option"));
                }
                let reg = self.reserve(1).start;
                if key == "REPORT" {
                    self.restart_report_function(expr, reg)?;
                } else {
                    self.restart_option_function(expr, reg)?;
                }
                *option = Some(reg);
                body = rest;
            }
            specs.push(RestartSpec {
                name: name_index,
                object,
                test,
                interactive,
                report,
                target: RestartTarget::Case { pc: 0, clear: 0 },
            });
            arms.push((params, body));
        }

        let clear = self.used;
        let table = self.restart_table(specs, at);
        self.restartable_form(form)?;
        self.emit(Ins::LeaveRestarts);
        let mut joins = vec![self.emit(Ins::Jump(0))];
        for (index, (params, body)) in arms.into_iter().enumerate() {
            self.code.restarts[table].restarts[index].target = RestartTarget::Case {
                pc: self.code.instructions.len(),
                clear,
            };
            self.call_with_values(params, body)?;
            joins.push(self.emit(Ins::Jump(0)));
        }
        let join = self.code.instructions.len();
        for branch in joins {
            self.code.instructions[branch] = Ins::Jump(join);
        }
        self.receive_values(dest);
        Ok(())
    }

    fn restart_name(&mut self, form: &Form) -> Result<usize, EvalError> {
        let name = form
            .name()
            .ok_or_else(|| EvalError::compile(form.at, "expected a restart name"))?;
        Ok(self.symbol(name, form.at))
    }

    /// A RESTART-CASE report is a report string or a function designator.
    /// Function designators are read from the function namespace; a literal
    /// string is evaluated (and therefore constant) as an ordinary form.
    fn restart_report_function(&mut self, form: &Form, reg: Register) -> Result<(), EvalError> {
        let string = match &form.kind {
            super::FormKind::String(_) => true,
            super::FormKind::Literal(value) => self.runtime.heap.is_string(value.get()),
            _ => false,
        };
        if string {
            return self.form(form, Destination::One(reg));
        }
        self.restart_option_function(form, reg)
    }

    fn restart_option_function(&mut self, form: &Form, reg: Register) -> Result<(), EvalError> {
        if let Some(name) = form.name() {
            self.read_name(name, Namespace::Function, Destination::One(reg), form.at);
            return Ok(());
        }
        if let Some([head, params, body @ ..]) = form.list()
            && head.name().is_some_and(|name| name.operator() == "LAMBDA")
        {
            return self.function(params, body, None, Destination::One(reg), form.at);
        }
        Err(EvalError::compile(
            form.at,
            "expected a function name or lambda",
        ))
    }

    fn restartable_form(&mut self, form: &Form) -> Result<(), EvalError> {
        if let Some(expansion) = self.expand_form(form)? {
            return self.with_expansion(form.at, |compiler| compiler.restartable_form(&expansion));
        }
        if let Some([head, args @ ..]) = form.list()
            && let Some(name) = head.name()
            && matches!(name.operator(), "SIGNAL" | "ERROR" | "CERROR" | "WARN")
            && self.lexical_operator(name).is_none()
        {
            let mark = self.used;
            self.emit_named_application(name, args, Destination::All, head.at, true)?;
            self.release(mark);
            Ok(())
        } else {
            self.form_unexpanded(form, Destination::All)
        }
    }
}
