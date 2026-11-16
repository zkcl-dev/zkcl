use std::collections::HashSet;
use std::rc::Rc;

use crate::form_value;
use crate::macros;

use super::{
    Class, Compiler, Destination, EvalError, Form, Name, Namespace, ScopedBinding, declarations,
    variable,
};

impl Compiler<'_> {
    pub(super) fn is_symbol_macro(&self, name: &Name) -> bool {
        if let Some(binding) = self
            .scopes()
            .rev()
            .find(|scope| scope.name == *name && scope.namespace.is_variable())
        {
            return binding.namespace == Namespace::SymbolMacro;
        }
        macros::global_symbol_expander(self.runtime, name).is_some()
    }

    pub(super) fn symbol_macrolet(
        &mut self,
        args: &[Form],
        dest: Destination,
        compile: fn(&mut Self, &Form, Destination) -> Result<(), EvalError>,
        at: usize,
    ) -> Result<(), EvalError> {
        self.with_local_symbol_macros(args, at, |compiler, body| {
            compiler.sequence(body, dest, compile)
        })
    }

    pub(super) fn build_symbol_macrolet_scopes(
        &self,
        specs: &Form,
        special: &[&super::Name],
    ) -> Result<Vec<ScopedBinding>, EvalError> {
        let specs = specs
            .list()
            .ok_or_else(|| EvalError::compile(specs.at, "expected a list of symbol macros"))?;
        let mut names = HashSet::new();
        let mut bindings = Vec::with_capacity(specs.len());
        for spec in specs {
            let Some([binding, expansion]) = spec.list() else {
                return Err(EvalError::compile(
                    spec.at,
                    "expected a symbol and its expansion",
                ));
            };
            let name = variable(binding)?;
            if self.is_constant_variable(name) {
                return Err(EvalError::new(
                    binding.at,
                    format!(
                        "cannot bind constant variable {} as a symbol macro",
                        name.spelling()
                    ),
                )
                .with_condition(Class::ProgramError));
            }
            if self.runtime.is_special(name)
                || self.context.specials.borrow().contains(name.key())
                || special.contains(&name)
            {
                return Err(EvalError::new(
                    binding.at,
                    "a special variable cannot be a symbol macro",
                )
                .with_condition(Class::ProgramError));
            }
            if !names.insert(name.key()) {
                return Err(EvalError::compile(binding.at, "duplicate symbol macro"));
            }
            let expansion = form_value::to_value(self.runtime, expansion)?;
            let expander = macros::symbol_expander(self.runtime, &expansion, spec.at)?;
            bindings.push(ScopedBinding {
                name: name.clone(),
                namespace: Namespace::SymbolMacro,
                binding: None,
                expander: Some(Rc::new(expander.slot)),
                used: None,
            });
        }
        Ok(bindings)
    }

    pub(super) fn with_local_symbol_macros(
        &mut self,
        args: &[Form],
        at: usize,
        process: impl FnOnce(&mut Self, &[Form]) -> Result<(), EvalError>,
    ) -> Result<(), EvalError> {
        let Some((specs, body)) = args.split_first() else {
            return Err(EvalError::compile(at, "expected symbol macro bindings"));
        };
        let (decls, body) = declarations(self.runtime, &self.context.warnings, body)?;
        let bindings = self.build_symbol_macrolet_scopes(specs, &decls.special)?;
        self.with_lexical_scope(|compiler, mark| {
            compiler.bindings.extend(bindings);
            compiler.free_declarations(decls, mark)?;
            process(compiler, body)
        })
    }
}
