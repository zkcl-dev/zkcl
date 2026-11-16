//! Top-level processing rules for the file compiler (CLHS §3.2.3.1).

use crate::definition_source::definition_debug;
use crate::macros::CallExpander;
use crate::read::Form;
use crate::vm::FunctionTemplate;
use crate::{EvalError, Runtime};
use zkcl_bytecode::DefinitionDebug;

use super::{CompilationContext, Compiler, Destination, EvalWhenSituations, eval_when_parts};

pub(crate) struct FileCompiler<'a> {
    compiler: Compiler<'a>,
}

impl<'a> FileCompiler<'a> {
    pub(crate) fn new(runtime: &'a Runtime) -> Self {
        Self {
            compiler: Compiler::new(runtime, Vec::new(), CompilationContext::default()),
        }
    }

    pub(crate) fn in_document(runtime: &'a Runtime, document: &crate::SourceDocument) -> Self {
        let mut compiler = Self::new(runtime);
        compiler.compiler.context.document = Some(document.clone());
        compiler
    }

    /// Compile-time warnings produced since the previous call.
    pub(crate) fn take_warnings(&self) -> Vec<super::Warning> {
        self.compiler.context.warnings.take()
    }

    #[cfg(test)]
    pub(crate) fn enable_phase_timings(&mut self) {
        self.compiler.context.phase_timings = Some(std::rc::Rc::default());
    }

    #[cfg(test)]
    pub(crate) fn phase_timings(&self) -> (std::time::Duration, std::time::Duration) {
        let timings = self
            .compiler
            .context
            .phase_timings
            .as_ref()
            .expect("phase timings enabled");
        (timings.expansion.get(), timings.evaluation.get())
    }

    /// Process one form before the reader advances to the next.
    pub(crate) fn compile_top_level_form(
        &mut self,
        form: &Form,
        call_expander: Option<&CallExpander<'_>>,
        emit: &mut impl FnMut(FunctionTemplate) -> Result<(), EvalError>,
    ) -> Result<(), EvalError> {
        self.compiler.record_form_sources(form);
        let mut definitions = Vec::new();
        let result = if let Some(call_expander) = call_expander {
            // Keep declarations and warnings shared across the file, while the
            // active Lisp call is only available while compiling this form.
            let context = CompilationContext {
                call_expander: Some(call_expander),
                ..self.compiler.context.clone()
            };
            let mut compiler = Compiler::new(self.compiler.runtime, Vec::new(), context);
            compiler.process_top_level_form(
                form,
                TopLevelMode::NotCompileTime,
                emit,
                &mut definitions,
            )
        } else {
            self.compiler.process_top_level_form(
                form,
                TopLevelMode::NotCompileTime,
                emit,
                &mut definitions,
            )
        };
        result.map_err(|error| error.at_form(form))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TopLevelMode {
    NotCompileTime,
    CompileTimeToo,
    /// The compile-time evaluation action for EVAL-WHEN.
    EvaluateAtCompileTime,
}

impl TopLevelMode {
    fn eval_when(self, situations: EvalWhenSituations) -> Option<Self> {
        if self == Self::EvaluateAtCompileTime {
            return situations.execute.then_some(Self::EvaluateAtCompileTime);
        }
        let evaluate =
            situations.compile_toplevel || situations.execute && self == Self::CompileTimeToo;
        match (situations.load_toplevel, evaluate) {
            (true, true) => Some(Self::CompileTimeToo),
            (true, false) => Some(Self::NotCompileTime),
            (false, true) => Some(Self::EvaluateAtCompileTime),
            (false, false) => None,
        }
    }
}

impl Compiler<'_> {
    fn process_top_level_form(
        &mut self,
        form: &Form,
        mode: TopLevelMode,
        emit: &mut impl FnMut(FunctionTemplate) -> Result<(), EvalError>,
        pending_definitions: &mut Vec<DefinitionDebug>,
    ) -> Result<(), EvalError> {
        if let Some(expansion) = self.expand_form(form)? {
            let previous = pending_definitions.len();
            if mode != TopLevelMode::EvaluateAtCompileTime {
                let definitions = definition_debug(self.runtime, form)?;
                if !definitions.is_empty() {
                    let expanded_definitions = definition_debug(self.runtime, &expansion)?;
                    pending_definitions.extend(definitions.into_iter().filter(|definition| {
                        !expanded_definitions.iter().any(|expanded| {
                            expanded.kind == definition.kind
                                && expanded.name == definition.name
                                && expanded.package == definition.package
                                && expanded.range == definition.range
                        })
                    }));
                }
            }
            let result = self.with_expansion(form.at, |compiler| {
                compiler.process_top_level_form(&expansion, mode, emit, pending_definitions)
            });
            // Definitions that produced no loadable entry belong to this
            // expansion and must never be attached to the following form.
            pending_definitions.truncate(previous);
            return result;
        }
        let mut defining = false;
        if let Some([head, body @ ..]) = form.list()
            && let Some(name) = head.name()
        {
            if let Some(kind) = super::vop::declaration_kind(self.runtime, name) {
                return self.compile_vop_declaration(
                    kind,
                    body,
                    form.at,
                    mode != TopLevelMode::NotCompileTime,
                    mode != TopLevelMode::EvaluateAtCompileTime,
                    emit,
                );
            }
            let operator =
                super::core_operator(self.runtime, name).unwrap_or_else(|| name.operator());
            let nested = |compiler: &mut Self, forms: &[Form]| {
                compiler.process_top_level_body(forms, mode, emit, pending_definitions)
            };
            match operator {
                "PROGN" => {
                    return self.process_top_level_body(body, mode, emit, pending_definitions);
                }
                "LOCALLY" => return self.with_local_declarations(body, nested),
                "MACROLET" => return self.with_local_macros(body, head.at, nested),
                "SYMBOL-MACROLET" => {
                    return self.with_local_symbol_macros(body, head.at, nested);
                }
                "EVAL-WHEN" => {
                    let (situations, forms) = eval_when_parts(body, form.at)?;
                    if let Some(mode) = mode.eval_when(situations) {
                        self.process_top_level_body(forms, mode, emit, pending_definitions)?;
                    }
                    return Ok(());
                }
                "DEFMACRO" | "DEFINE-COMPILER-MACRO" => defining = true,
                "DECLAIM" => {
                    self.note_top_level_specials("DECLAIM", body, false)?;
                    defining = true;
                }
                "DEFVAR" | "DEFPARAMETER" | "DEFCONSTANT" => {
                    self.note_top_level_specials(operator, body, false)?;
                }
                _ => {}
            }
        }

        let scopes = self.scopes().cloned().collect();
        let mut form_compiler = self.nested(scopes);
        form_compiler.code.debug.entry = Some(form.at);
        if let Some(package) = self.runtime.bound_package() {
            form_compiler.code.debug.reader_package = Some(
                form_compiler.add_constant(super::ConstantTemplate::Immediate(package.word()))
                    as u32,
            );
        }
        form_compiler.begin_top_level_ltv();
        form_compiler
            .form_unexpanded(form, Destination::All)
            .map_err(|error| error.at_form(form))?;
        form_compiler.finalize_top_level_ltv()?;
        let mut code = form_compiler.finish();
        code.debug
            .definitions
            .extend(definition_debug(self.runtime, form)?);
        if mode == TopLevelMode::EvaluateAtCompileTime {
            self.run_during_compilation(code, form.at)?;
        } else {
            code.debug.definitions.append(pending_definitions);
            if mode == TopLevelMode::CompileTimeToo || defining {
                self.run_during_compilation(code.clone(), form.at)?;
            }
            emit(code)?;
        }
        Ok(())
    }

    fn process_top_level_body(
        &mut self,
        forms: &[Form],
        mode: TopLevelMode,
        emit: &mut impl FnMut(FunctionTemplate) -> Result<(), EvalError>,
        pending_definitions: &mut Vec<DefinitionDebug>,
    ) -> Result<(), EvalError> {
        for form in forms {
            self.process_top_level_form(form, mode, emit, pending_definitions)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::read;

    #[test]
    fn declaration_handler_failure_restores_top_level_lexical_scope() {
        let runtime = Runtime::start().unwrap();
        runtime
            .eval(
                "(progn
                    (zk:define-declaration compiler-note (spec env)
                      (declare (ignore spec env))
                      (values :declare '(compiler-note . seen)))
                    (zk:define-declaration bad-compiler-note (spec env)
                      (declare (ignore spec env))
                      (error \"failed at compile time\")))",
            )
            .unwrap();
        let mut compiler = FileCompiler::new(&runtime);
        let form = read(
            &runtime,
            "(symbol-macrolet ((replacement 9))
               (declare (compiler-note) (bad-compiler-note))
               replacement)",
        )
        .unwrap();
        let error = compiler
            .compile_top_level_form(&form, None, &mut |_| Ok(()))
            .unwrap_err();
        assert!(
            error.to_string().contains("failed at compile time"),
            "{error}"
        );
        assert!(compiler.compiler.bindings.is_empty());
        assert!(compiler.compiler.custom_declarations.is_empty());
    }
}
