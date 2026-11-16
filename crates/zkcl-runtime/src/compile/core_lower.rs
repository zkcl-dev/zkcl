//! Bridge lowering `Form -> CoreFn -> optimized CoreFn -> bytecode`.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::mem::DropGuard;
use std::ops::ControlFlow;
use std::rc::Rc;

use crate::core_ir::{
    BlockExitTarget, BlockId, CoreBindingTarget, CoreCallee, CoreConst, CoreDefaultedParameter,
    CoreExpr, CoreFn, CoreKeywordParameter, CoreKeywordParameters, CoreKind, CoreParameterBindings,
    CoreType, InlineDeclaration, LocalFunction, LocalFunctionBinding, LocalFunctionId,
    OptimizationQualities, RawLexicalBinding, Tag, TagbodyId, TagbodyTargets, VarId, VariableBinding,
    can_store_inline_candidate, ftype_signature, optimize_core_fn, parse_the_type, parse_type_spec,
};
use crate::heap::Word;
use crate::symbol::Name;

use super::lambda::{DefaultParameter, Parameters, Variable};
use super::tagbody::{TagScope, TagbodyForms};
use super::type_checks::{CheckedFtype, IntegerCheck, should_emit_runtime_type_check};
use super::{
    Arity, BindingLocation, Callee, CaptureKind, Compiler, ConstantTemplate, Destination,
    EvalError, Form, FormKind, Immediate, Ins, Namespace, RegisterBinding, ScopedBinding,
    declaration_items, declarations, eval_when_body, fixnum, function_body, is_special_operator,
    setf_accessor, variable,
};

fn param_type_fact(ty: CoreType) -> zkcl_bytecode::ParamTypeFact {
    match ty {
        CoreType::Fixnum => zkcl_bytecode::ParamTypeFact::Fixnum,
        CoreType::Float => zkcl_bytecode::ParamTypeFact::Float,
        CoreType::Character => zkcl_bytecode::ParamTypeFact::Char,
        CoreType::Cons => zkcl_bytecode::ParamTypeFact::Cons,
        CoreType::NonNilSymbol | CoreType::Keyword => zkcl_bytecode::ParamTypeFact::Symbol,
        CoreType::Function => zkcl_bytecode::ParamTypeFact::Function,
        CoreType::Null => zkcl_bytecode::ParamTypeFact::Nil,
        _ => zkcl_bytecode::ParamTypeFact::Any,
    }
}

fn leading_entry_trusted_type(body: &CoreExpr, vid: VarId) -> CoreType {
    let expr = match &body.kind {
        CoreKind::Block { body, .. } => body,
        _ => body,
    };
    let items = match &expr.kind {
        CoreKind::Progn(items) => items.as_slice(),
        _ => std::slice::from_ref(expr),
    };
    let mut ty = CoreType::Top;
    for item in items {
        match &item.kind {
            CoreKind::The {
                type_spec,
                expr: inner,
                safety: 0,
            } => {
                if matches!(inner.kind, CoreKind::ReadVar(v) if v == vid) {
                    ty = ty.meet(parse_type_spec(type_spec).0);
                }
            }
            _ => break,
        }
    }
    ty
}

const INLINE_CANDIDATE_NODE_LIMIT: usize = 48;

fn source_lambda(
    runtime: &crate::Runtime,
    parameters: &Form,
    body: &[Form],
    block: Option<&Name>,
    at: usize,
) -> Form {
    let mut items = vec![
        Form::new(at, FormKind::Name(Name::common_lisp("LAMBDA"))),
        parameters.clone(),
    ];
    if let Some(block) = block {
        let (declarations, documentation, forms) = function_body(runtime, body);
        if let Some(documentation) = documentation {
            items.push(documentation.clone());
        }
        items.extend(declarations.into_iter().cloned());
        let mut block_body = vec![
            Form::new(at, FormKind::Name(Name::common_lisp("BLOCK"))),
            Form::new(at, FormKind::Name(block.clone())),
        ];
        block_body.extend_from_slice(forms);
        items.push(Form::new(at, FormKind::List(block_body, None)));
    } else {
        items.extend_from_slice(body);
    }
    Form::new(at, FormKind::List(items, None))
}

pub(super) fn application_arity_error(operator: &str, at: usize) -> EvalError {
    EvalError::compile(at, format!("wrong number of arguments to {operator}"))
}

pub(super) fn compile_function<'a>(
    compiler: &mut Compiler<'a>,
    parameters: &Form,
    body: &[Form],
    block: Option<&Name>,
    source_parameters: Option<&Form>,
    at: usize,
) -> Result<(Compiler<'a>, CoreFn), EvalError> {
    let mut function = CoreLowering::build_root_fn(compiler, parameters, body, block, at)?;
    if let Some(source_parameters) = source_parameters {
        function.source_lambda_list = Some(Rc::new(source_parameters.clone()));
    }
    optimize_core_fn(&mut function, &|name| compiler.lookup_inline_core(name));
    let child = compiler.compile_core_lambda(&function, None)?;
    Ok((child, function))
}

pub(super) fn inline_candidate(function: CoreFn) -> Option<Rc<CoreFn>> {
    (function.debug != 3
        && !function.has_captures()
        && function.params.inlineable_fixed_params().is_some()
        && can_store_inline_candidate(&function, INLINE_CANDIDATE_NODE_LIMIT))
    .then(|| Rc::new(function))
}

#[derive(Clone)]
enum ScopedBindingKind {
    Var(VarId),
    Special,
    LocalFn(LocalFunctionId),
    Macro,
    Block(BlockId),
}

#[derive(Clone)]
struct ScopedName {
    name: Name,
    namespace: Namespace,
    kind: ScopedBindingKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TypeDeclarationTarget {
    FrameVar(usize, VarId),
    SpecialBinding(usize, usize),
}

struct CoreLoweringFrame {
    fn_ir: CoreFn,
    scopes: Vec<ScopedName>,
    tagbodies: Vec<TagbodyId>,
    quality_stack: Vec<OptimizationQualities>,
    var_types: Vec<(TypeDeclarationTarget, Form, bool)>,
    captured_parent_vars: HashMap<VarId, VarId>,
    captured_parent_blocks: HashMap<BlockId, BlockId>,
    captured_parent_tagbodies: HashMap<TagbodyId, TagbodyId>,
    captured_parent_functions: HashMap<LocalFunctionId, LocalFunctionId>,
}

struct CoreLowering<'c, 'a> {
    compiler: &'c mut Compiler<'a>,
    frames: Vec<CoreLoweringFrame>,
    outer_bindings: usize,
    outer_declaration_scopes: usize,
}

#[derive(Clone, Copy)]
struct LoweringScopeMark {
    compiler: super::ScopeMark,
    frame: usize,
    scopes: usize,
    tagbodies: usize,
    optimization_depth: usize,
    variable_types: usize,
}

struct FunctionDefinition<'f> {
    parameters: Parameters<'f>,
    declarations: super::Declarations<'f>,
    body: &'f [Form],
    qualities: OptimizationQualities,
}

#[derive(Default)]
struct ParameterScope<'f> {
    vars: Vec<VarId>,
    names: Vec<&'f Name>,
    checks: Vec<CoreExpr>,
    has_special: bool,
    body_safety: u8,
    required_only: bool,
}

impl<'c, 'a> CoreLowering<'c, 'a> {
    fn top(&self) -> &CoreLoweringFrame {
        self.frames.last().expect("active build frame")
    }

    fn top_mut(&mut self) -> &mut CoreLoweringFrame {
        self.frames.last_mut().expect("active build frame")
    }

    fn mark_frame_scope(&self) -> LoweringScopeMark {
        let top = self.top();
        LoweringScopeMark {
            compiler: self.compiler.mark_scope(),
            frame: self.frames.len() - 1,
            scopes: top.scopes.len(),
            tagbodies: top.tagbodies.len(),
            optimization_depth: top.quality_stack.len(),
            variable_types: top.var_types.len(),
        }
    }

    fn restore_frame_scope(&mut self, mark: LoweringScopeMark) {
        self.compiler.restore_scope(mark.compiler);
        let frame = &mut self.frames[mark.frame];
        frame.scopes.truncate(mark.scopes);
        frame.tagbodies.truncate(mark.tagbodies);
        frame.quality_stack.truncate(mark.optimization_depth);
        frame.var_types.truncate(mark.variable_types);
    }

    fn with_frame_scope<T>(&mut self, lower: impl FnOnce(&mut Self, LoweringScopeMark) -> T) -> T {
        let mark = self.mark_frame_scope();
        let mut builder = DropGuard::new(self, move |builder| builder.restore_frame_scope(mark));
        lower(&mut builder, mark)
    }

    fn bind_name(&mut self, name: &Name, namespace: Namespace, kind: ScopedBindingKind) {
        let special = matches!(kind, ScopedBindingKind::Special);
        self.top_mut().scopes.push(ScopedName {
            name: name.clone(),
            namespace,
            kind,
        });
        // Macro expansion looks up names in the compiler's lexical environment.
        if special {
            self.compiler.push_special(name, true);
        } else {
            self.compiler.bindings.push(ScopedBinding {
                name: name.clone(),
                namespace,
                binding: None,
                expander: None,
                used: None,
            });
        }
    }

    fn optimization_qualities(&self) -> OptimizationQualities {
        if let Some(frame) = self.frames.last()
            && let Some(&qualities) = frame.quality_stack.last()
        {
            return qualities;
        }
        self.compiler.optimization_qualities()
    }

    fn resolve_var_target_no_bridge(&self, name: &Name) -> Option<TypeDeclarationTarget> {
        for (lvl, frame) in self.frames.iter().enumerate().rev() {
            if let Some((index, entry)) = frame
                .scopes
                .iter()
                .enumerate()
                .rev()
                .find(|(_, s)| s.namespace == Namespace::Variable && &s.name == name)
            {
                return Some(match entry.kind {
                    ScopedBindingKind::Special => TypeDeclarationTarget::SpecialBinding(lvl, index),
                    ScopedBindingKind::Var(vid) => TypeDeclarationTarget::FrameVar(lvl, vid),
                    _ => unreachable!(),
                });
            }
        }
        None
    }

    fn lookup_var_type_forms(&self, name: &Name, only_free: bool) -> Vec<Form> {
        self.var_type_forms(name, self.resolve_var_target_no_bridge(name), only_free)
    }

    fn var_type_forms(
        &self,
        name: &Name,
        target: Option<TypeDeclarationTarget>,
        only_free: bool,
    ) -> Vec<Form> {
        let Some(target) = target else {
            return self.compiler.current_var_types(name, only_free);
        };
        let special = matches!(target, TypeDeclarationTarget::SpecialBinding(..));
        let mut types: Vec<_> = self
            .core_var_types(target)
            .filter(|(_, is_free)| !only_free || *is_free || special)
            .map(|(spec, _)| spec.clone())
            .collect();
        if special {
            types.extend(self.compiler.proclaimed_var_type(name));
        }
        types
    }

    fn core_var_types(&self, target: TypeDeclarationTarget) -> impl Iterator<Item = (&Form, bool)> {
        self.frames
            .iter()
            .rev()
            .flat_map(|frame| frame.var_types.iter().rev())
            .filter_map(move |(var, spec, is_free)| (*var == target).then_some((spec, *is_free)))
    }

    fn assert_var_types(&self, expr: CoreExpr, types: Vec<Form>, safety: u8) -> CoreExpr {
        types.into_iter().fold(expr, |value, type_spec| {
            if should_emit_runtime_type_check(&type_spec) {
                CoreExpr::new(
                    value.at,
                    CoreKind::The {
                        type_spec,
                        expr: Box::new(value),
                        safety,
                    },
                )
            } else {
                value
            }
        })
    }

    fn record_free_decls(&mut self, decls: &super::Declarations<'_>, bound_names: &[&Name]) {
        if !decls.optimize.is_empty() {
            let qualities = decls.apply_optimize_declarations(self.optimization_qualities());
            self.top_mut().quality_stack.push(qualities);
        }
        self.record_free_types(decls, bound_names);
    }

    fn record_free_types(&mut self, decls: &super::Declarations<'_>, bound_names: &[&Name]) {
        for &(vname, spec) in &decls.types {
            if bound_names.contains(&vname) {
                continue;
            }
            if let Some(target) = self.resolve_var_target_no_bridge(vname) {
                self.top_mut().var_types.push((target, spec.clone(), true));
            }
        }
    }

    fn build_root_fn(
        compiler: &'c mut Compiler<'a>,
        parameters_form: &Form,
        body: &[Form],
        block: Option<&Name>,
        at: usize,
    ) -> Result<CoreFn, EvalError> {
        let mut builder = Self {
            outer_bindings: compiler.bindings.len(),
            outer_declaration_scopes: compiler.declaration_scopes.len(),
            compiler,
            frames: Vec::new(),
        };
        builder.push_and_build_fn(parameters_form, body, block, at)
    }

    fn bind_parameter<'f>(
        &mut self,
        var: Variable<'f>,
        decls: &super::Declarations<'_>,
        scope: &mut ParameterScope<'f>,
    ) -> Result<VarId, EvalError> {
        let Variable { name, at: param_at } = var;
        let safety = scope.body_safety;
        self.compiler
            .ensure_writable_variable(name, param_at, "bind")?;
        let top_lvl = self.frames.len() - 1;
        let special = self.compiler.is_special_var(name, &decls.special);
        scope.has_special |= special;
        // The body declaration can describe incoming arguments directly when
        // no initializer runs under the enclosing safety policy.
        let trust_declared_type = !special
            && safety == 0
            && (scope.required_only || self.optimization_qualities().safety == 0);
        let check_declared_type = safety > 0 || (!special && !trust_declared_type);
        let frame = self.top_mut();
        let var_id = frame.fn_ir.alloc_var(VariableBinding {
            name: name.clone(),
            special,
            assigned: false,
            captured: false,
            read_count: 0,
            trusted_type: CoreType::Top,
            dynamic_extent: decls.is_dynamic_extent_var(name),
        });
        scope.vars.push(var_id);
        scope.names.push(name);
        let target = if special {
            TypeDeclarationTarget::SpecialBinding(top_lvl, frame.scopes.len())
        } else {
            TypeDeclarationTarget::FrameVar(top_lvl, var_id)
        };
        let mut append_check = |spec: &Form| {
            if check_declared_type && should_emit_runtime_type_check(spec) {
                let read = if special {
                    CoreExpr::new(param_at, CoreKind::ReadGlobal(name.clone()))
                } else {
                    CoreExpr::new(param_at, CoreKind::ReadVar(var_id))
                };
                scope.checks.push(CoreExpr::new(
                    param_at,
                    CoreKind::The {
                        type_spec: spec.clone(),
                        expr: Box::new(read),
                        safety,
                    },
                ));
            }
        };
        for spec in decls.types_for(name) {
            frame.var_types.push((target, spec.clone(), false));
            if trust_declared_type {
                let binding = &mut frame.fn_ir.vars[var_id.index()];
                binding.trusted_type = binding.trusted_type.meet(parse_type_spec(spec).0);
            } else {
                append_check(spec);
            }
        }
        if special
            && safety > 0
            && let Some(spec) = self.compiler.proclaimed_var_type(name)
        {
            append_check(&spec);
        }
        let kind = if special {
            ScopedBindingKind::Special
        } else {
            ScopedBindingKind::Var(var_id)
        };
        self.bind_name(name, Namespace::Variable, kind);
        Ok(var_id)
    }

    fn lower_default_parameter<'f>(
        &mut self,
        param: &DefaultParameter<'f>,
        decls: &super::Declarations<'_>,
        scope: &mut ParameterScope<'f>,
    ) -> Result<CoreDefaultedParameter, EvalError> {
        let init = param.init.map(|form| self.lower_form(form)).transpose()?;
        let var = self.bind_parameter(param.pattern.ordinary_variable(), decls, scope)?;
        let supplied = param
            .supplied
            .map(|v| self.bind_parameter(v, decls, scope))
            .transpose()?;
        Ok(CoreDefaultedParameter {
            var,
            init,
            supplied,
        })
    }

    fn lower_fn_parameters<'f>(
        &mut self,
        params: &Parameters<'f>,
        decls: &super::Declarations<'_>,
        scope: &mut ParameterScope<'f>,
    ) -> Result<CoreParameterBindings, EvalError> {
        let mut required = Vec::with_capacity(params.required.len());
        for p in &params.required {
            required.push(self.bind_parameter(p.ordinary_variable(), decls, scope)?);
        }
        let mut optional = Vec::with_capacity(params.optional.len());
        for p in &params.optional {
            optional.push(self.lower_default_parameter(p, decls, scope)?);
        }
        let rest = params
            .rest
            .as_ref()
            .map(|p| self.bind_parameter(p.ordinary_variable(), decls, scope))
            .transpose()?;
        let keywords = if let Some(kws) = &params.keywords {
            let mut lowered = Vec::with_capacity(kws.parameters.len());
            for kw in &kws.parameters {
                lowered.push(CoreKeywordParameter {
                    key: kw.name.clone(),
                    param: self.lower_default_parameter(&kw.parameter, decls, scope)?,
                });
            }
            Some(CoreKeywordParameters {
                params: lowered,
                allow_other_keys: kws.allow_other_keys,
            })
        } else {
            None
        };
        let mut aux = Vec::with_capacity(params.aux.len());
        for &(v, init_form) in &params.aux {
            let init = init_form.map(|form| self.lower_form(form)).transpose()?;
            aux.push((self.bind_parameter(v, decls, scope)?, init));
        }
        Ok(CoreParameterBindings::new(
            std::mem::take(&mut scope.vars),
            scope.has_special,
            required,
            optional,
            rest,
            keywords,
            aux,
        ))
    }

    fn function_definition<'f>(
        &self,
        params: &'f Form,
        body: &'f [Form],
    ) -> Result<FunctionDefinition<'f>, EvalError> {
        let (decl_forms, _, body_forms) = function_body(self.compiler.runtime, body);
        let mut decls = super::Declarations::default();
        for decl in decl_forms {
            for spec in super::declaration_specs(decl).expect("leading declaration") {
                decls.extend(declaration_items(
                    self.compiler.runtime,
                    &self.compiler.context.warnings,
                    spec,
                )?);
            }
        }
        let parameters = Parameters::ordinary(params)?;

        let qualities = decls.apply_optimize_declarations(self.optimization_qualities());
        Ok(FunctionDefinition {
            parameters,
            declarations: decls,
            qualities,
            body: body_forms,
        })
    }

    fn push_and_build_fn(
        &mut self,
        parameters_form: &Form,
        body: &[Form],
        block: Option<&Name>,
        at: usize,
    ) -> Result<CoreFn, EvalError> {
        let def = self.function_definition(parameters_form, body)?;
        let mut fn_ir = CoreFn::new(
            self.compiler.runtime.compilation_target(),
            block.cloned(),
            CoreParameterBindings::empty(),
            Box::new(CoreExpr::nil(at)),
            at,
        );
        fn_ir.source_lambda_list = Some(Rc::new(parameters_form.clone()));
        fn_ir.source_expression = Some(Rc::new(source_lambda(
            self.compiler.runtime,
            parameters_form,
            body,
            block,
            at,
        )));
        fn_ir.debug = def.qualities.debug;
        self.frames.push(CoreLoweringFrame {
            fn_ir,
            scopes: Vec::new(),
            tagbodies: Vec::new(),
            quality_stack: Vec::new(),
            var_types: Vec::new(),
            captured_parent_vars: HashMap::new(),
            captured_parent_blocks: HashMap::new(),
            captured_parent_tagbodies: HashMap::new(),
            captured_parent_functions: HashMap::new(),
        });

        let result = self.lower_definition(def, block, at);
        let mut finished = self.frames.pop().expect("balanced build frame").fn_ir;
        let (params, body) = result?;
        finished.params = params;
        *finished.body = body;
        Ok(finished)
    }

    fn lower_definition(
        &mut self,
        def: FunctionDefinition<'_>,
        block: Option<&Name>,
        at: usize,
    ) -> Result<(CoreParameterBindings, CoreExpr), EvalError> {
        self.with_frame_scope(|builder, mark| {
            let mut scope = ParameterScope {
                body_safety: def.qualities.safety,
                required_only: def.parameters.optional.is_empty()
                    && def.parameters.rest.is_none()
                    && def.parameters.keywords.is_none()
                    && def.parameters.aux.is_empty(),
                ..ParameterScope::default()
            };
            let params =
                builder.lower_fn_parameters(&def.parameters, &def.declarations, &mut scope)?;
            // The lambda list sees bound parameter declarations and the enclosing
            // optimization qualities. Free declarations start at the body.
            builder.top_mut().quality_stack.push(def.qualities);
            builder.declare_free_specials(&def.declarations.special, mark.scopes);
            builder.record_free_types(&def.declarations, &scope.names);
            builder
                .compiler
                .free_declarations(def.declarations, mark.compiler)?;

            let lower_body = |builder: &mut Self| {
                let body = builder.lower_sequence(def.body)?;
                if scope.checks.is_empty() {
                    Ok(body)
                } else {
                    scope.checks.push(body);
                    Ok(CoreExpr::new(at, CoreKind::Progn(scope.checks)))
                }
            };
            let body = match block {
                Some(name) => builder.with_block_scope(name, at, lower_body)?,
                None => lower_body(builder)?,
            };
            Ok((params, body))
        })
    }

    fn declare_free_specials(&mut self, names: &[&Name], start: usize) {
        for &name in names {
            if !self.top().scopes[start..]
                .iter()
                .any(|s| s.namespace == Namespace::Variable && &s.name == name)
            {
                self.top_mut().scopes.push(ScopedName {
                    name: name.clone(),
                    namespace: Namespace::Variable,
                    kind: ScopedBindingKind::Special,
                });
            }
        }
    }

    fn with_block_scope(
        &mut self,
        name: &Name,
        at: usize,
        f: impl FnOnce(&mut Self) -> Result<CoreExpr, EvalError>,
    ) -> Result<CoreExpr, EvalError> {
        let id = self.top_mut().fn_ir.alloc_block(BlockExitTarget {
            name: name.clone(),
            used: false,
            escapes: false,
            crosses_dynamic_scope: false,
        });
        let body = self.with_frame_scope(|builder, _| {
            builder.bind_name(name, Namespace::Block, ScopedBindingKind::Block(id));
            f(builder)
        })?;
        Ok(CoreExpr::new(
            at,
            CoreKind::Block {
                id,
                body: Box::new(body),
            },
        ))
    }

    fn lower_sequence(&mut self, forms: &[Form]) -> Result<CoreExpr, EvalError> {
        if forms.is_empty() {
            return Ok(CoreExpr::nil(0));
        }
        if forms.len() == 1 {
            return self.lower_form(&forms[0]);
        }
        let at = forms[0].at;
        let exprs = self.lower_forms(forms)?;
        Ok(CoreExpr::new(at, CoreKind::Progn(exprs)))
    }

    fn lower_forms(&mut self, forms: &[Form]) -> Result<Vec<CoreExpr>, EvalError> {
        let mut lowered = Vec::with_capacity(forms.len());
        for form in forms {
            lowered.push(self.lower_form(form)?);
        }
        Ok(lowered)
    }

    fn lower_form(&mut self, form: &Form) -> Result<CoreExpr, EvalError> {
        if let Some(expansion) = self.compiler.expand_form_with_types(form, &|name| {
            let types = if self.compiler.is_symbol_macro(name) {
                self.compiler.current_var_types(name, false)
            } else {
                self.lookup_var_type_forms(name, false)
            };
            super::type_intersection(types)
        })? {
            return self.expand_recurse(form.at, &expansion);
        }
        self.lower_unexpanded(form)
    }

    fn expand_recurse(&mut self, at: usize, form: &Form) -> Result<CoreExpr, EvalError> {
        let _expansion =
            super::enter_macro_expansion(Rc::clone(&self.compiler.context.expansion_depth), at)?;
        self.lower_form(form)
    }

    fn lower_unexpanded(&mut self, form: &Form) -> Result<CoreExpr, EvalError> {
        let at = form.at;
        match &form.kind {
            FormKind::Integer(n) => {
                fixnum(form, *n)?;
                let (min, max) = self.compiler.runtime.compilation_target().fixnum_bounds();
                let value = if (min..=max).contains(&(*n as i64)) {
                    CoreConst::Fixnum(*n as i64)
                } else {
                    CoreConst::LiteralForm(form.clone())
                };
                Ok(CoreExpr::new(at, CoreKind::Const(value)))
            }
            FormKind::Character(ch) => Ok(CoreExpr::new(at, CoreKind::Const(CoreConst::Char(*ch)))),
            FormKind::String(s) => Ok(CoreExpr::new(
                at,
                CoreKind::Const(CoreConst::String(s.clone())),
            )),
            FormKind::Literal(_) => Ok(CoreExpr::new(
                at,
                CoreKind::Const(CoreConst::LiteralForm(form.clone())),
            )),
            FormKind::Name(name) => Ok(self.resolve_read_var(name, at)),
            FormKind::List(args, None) if args.is_empty() => Ok(CoreExpr::nil(at)),
            FormKind::List(items, None) => {
                let (head, args) = items.split_first().unwrap();
                if let Some(name) = head.name() {
                    self.lower_application(name, args, form, head.at)
                } else {
                    self.lower_lambda_application(head, args, at)
                }
            }
            FormKind::List(_, Some(_)) => {
                Err(EvalError::compile(at, "a form must be a proper list"))
            }
        }
    }

    fn lower_lambda_application(
        &mut self,
        head: &Form,
        args: &[Form],
        at: usize,
    ) -> Result<CoreExpr, EvalError> {
        let (parameters, body) = match head.list() {
            Some([op, parameters, body @ ..])
                if op.name().is_some_and(|name| name.operator() == "LAMBDA") =>
            {
                (parameters, body)
            }
            _ => {
                return Err(EvalError::compile(
                    head.at,
                    "expected a function name or lambda",
                ));
            }
        };
        let func = self.push_and_build_fn(parameters, body, None, head.at)?;
        self.compiler.check_lambda_keywords(&func.params, args);
        let callee = CoreExpr::new(
            head.at,
            CoreKind::Lambda {
                func: Box::new(func),
            },
        );
        let lowered_args = args
            .iter()
            .map(|form| self.lower_form(form))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CoreExpr::new(
            at,
            CoreKind::Call {
                callee: CoreCallee::Expr(Box::new(callee)),
                args: lowered_args,
                qualities: self.optimization_qualities(),
                inline_declaration: None,
                ftype_ret: CoreType::Top,
                fixnum_arithmetic: None,
            },
        ))
    }

    fn lower_quote(&self, quoted: &Form, at: usize) -> CoreExpr {
        if quoted.original_object().is_some() {
            return CoreExpr::new(at, CoreKind::Const(CoreConst::LiteralForm(quoted.clone())));
        }
        let kind = match &quoted.kind {
            FormKind::Name(n) if *n == Name::common_lisp("NIL") => CoreConst::Nil,
            FormKind::Name(n) if *n == Name::common_lisp("T") => CoreConst::True,
            FormKind::Name(n) => CoreConst::Symbol(n.clone()),
            FormKind::List(items, None) if items.is_empty() => CoreConst::Nil,
            _ => CoreConst::LiteralForm(quoted.clone()),
        };
        CoreExpr::new(at, CoreKind::Const(kind))
    }

    // Keep the dispatcher small: every recursive child lowering enters it,
    // including in unoptimized builds where arm-local temporaries need stack space.
    fn lower_application(
        &mut self,
        name: &Name,
        args: &[Form],
        whole_form: &Form,
        at: usize,
    ) -> Result<CoreExpr, EvalError> {
        if let Some(kind) = super::vop::declaration_kind(self.compiler.runtime, name) {
            return Err(EvalError::compile(
                at,
                format!("{} requires a top-level form", kind.name()),
            ));
        }
        let arity = || application_arity_error(name.spelling(), at);
        match name.operator() {
            "QUOTE" => {
                let [quoted] = args else {
                    return Err(arity());
                };
                Ok(self.lower_quote(quoted, at))
            }
            "IF" => self.lower_if(args, at),
            "PROGN" => self.lower_sequence(args),
            "THE" => self.lower_the(args, at),
            "MULTIPLE-VALUE-CALL" => {
                let Some((callee, forms)) = args.split_first() else {
                    return Err(arity());
                };
                let callee = Box::new(self.lower_form(callee)?);
                let forms = forms
                    .iter()
                    .map(|form| self.lower_form(form))
                    .collect::<Result<_, _>>()?;
                Ok(CoreExpr::new(at, CoreKind::CallValues { callee, forms }))
            }
            "EVAL-WHEN" => self.lower_sequence(eval_when_body(args, at)?),
            "LOCALLY" => self.lower_locally(args),
            "LET" | "LET*" => {
                let Some((specs, body)) = args.split_first() else {
                    return Err(arity());
                };
                self.lower_let(specs, body, name.operator() == "LET*", at)
            }
            "BLOCK" => self.lower_block(args, at),
            "RETURN-FROM" => self.lower_return_from(args, at),
            "RETURN" => self.lower_return(args, at),
            "TAGBODY" => self.lower_tagbody(args, at),
            "GO" => {
                let [tag_form] = args else {
                    return Err(arity());
                };
                let tag = Tag::from_form(self.compiler.runtime, tag_form).ok_or_else(|| {
                    EvalError::compile(tag_form.at, "expected a symbol or integer tag")
                })?;
                let (id, tag_index) = self.resolve_tag(&tag).ok_or_else(|| {
                    EvalError::compile(tag_form.at, "no lexically enclosing TAGBODY tag")
                })?;
                Ok(CoreExpr::new(at, CoreKind::Go { id, tag_index }))
            }
            "SETQ" => self.lower_setq(args, at),
            "FLET" | "LABELS" => {
                let Some((specs, body)) = args.split_first() else {
                    return Err(arity());
                };
                self.lower_flet(specs, body, name.operator() == "LABELS", at)
            }
            "LAMBDA" => {
                let [params, body @ ..] = args else {
                    return Err(arity());
                };
                self.lower_lambda(params, body, at)
            }
            "FUNCTION" => self.lower_function(args, at),
            "MACROLET" => {
                let Some((defs, body)) = args.split_first() else {
                    return Err(arity());
                };
                self.lower_macrolet(defs, body)
            }
            "SYMBOL-MACROLET" => {
                let Some((defs, body)) = args.split_first() else {
                    return Err(arity());
                };
                self.lower_symbol_macrolet(defs, body)
            }
            "LOAD-TIME-VALUE" => {
                let (cell, _) = self.compiler.compile_load_time_value(args, at)?;
                Ok(CoreExpr::new(at, CoreKind::LoadTimeValue { cell }))
            }
            op if op == "DECLARE"
                || is_special_operator(op)
                || super::has_syntax_binding(op)
                || super::core_operator(self.compiler.runtime, name).is_some() =>
            {
                Ok(CoreExpr::new(
                    whole_form.at,
                    CoreKind::RawSpecial(whole_form.clone(), self.raw_lexical_bindings()),
                ))
            }
            _ => self.lower_named_call(name, args, at),
        }
    }

    fn lower_if(&mut self, args: &[Form], at: usize) -> Result<CoreExpr, EvalError> {
        let (test, yes, no) = match args {
            [test, yes] => (test, yes, None),
            [test, yes, no] => (test, yes, Some(no)),
            _ => return Err(application_arity_error("IF", at)),
        };
        let test = Box::new(self.lower_form(test)?);
        let then_ = Box::new(self.lower_form(yes)?);
        let else_ = Box::new(match no {
            Some(n) => self.lower_form(n)?,
            None => CoreExpr::nil(at),
        });
        Ok(CoreExpr::new(at, CoreKind::If { test, then_, else_ }))
    }

    fn lower_the(&mut self, args: &[Form], at: usize) -> Result<CoreExpr, EvalError> {
        let [type_spec, value] = args else {
            return Err(application_arity_error("THE", at));
        };
        let expr = Box::new(self.lower_form(value)?);
        Ok(CoreExpr::new(
            at,
            CoreKind::The {
                type_spec: type_spec.clone(),
                expr,
                safety: self.optimization_qualities().safety,
            },
        ))
    }

    fn lower_block(&mut self, args: &[Form], at: usize) -> Result<CoreExpr, EvalError> {
        let Some((label, body)) = args.split_first() else {
            return Err(application_arity_error("BLOCK", at));
        };
        let bname = label
            .name()
            .ok_or_else(|| EvalError::compile(label.at, "expected a BLOCK name"))?;
        self.with_block_scope(bname, at, |builder| builder.lower_sequence(body))
    }

    fn lower_return_from(&mut self, args: &[Form], at: usize) -> Result<CoreExpr, EvalError> {
        let (label, value) = match args {
            [label] => (label, None),
            [label, value] => (label, Some(value)),
            _ => return Err(application_arity_error("RETURN-FROM", at)),
        };
        let bname = label
            .name()
            .ok_or_else(|| EvalError::compile(label.at, "expected a BLOCK name"))?;
        self.lower_return_target(bname, value, at)
    }

    fn lower_return(&mut self, args: &[Form], at: usize) -> Result<CoreExpr, EvalError> {
        if args.len() > 1 {
            return Err(application_arity_error("RETURN", at));
        }
        self.lower_return_target(&Name::common_lisp("NIL"), args.first(), at)
    }

    fn lower_return_target(
        &mut self,
        name: &Name,
        value: Option<&Form>,
        at: usize,
    ) -> Result<CoreExpr, EvalError> {
        let id = self.resolve_block(name).ok_or_else(|| {
            EvalError::compile(
                at,
                format!("no lexically enclosing BLOCK {}", name.spelling()),
            )
        })?;
        let val = match value {
            Some(v) => self.lower_form(v)?,
            None => CoreExpr::nil(at),
        };
        Ok(CoreExpr::new(
            at,
            CoreKind::ReturnFrom {
                id,
                value: Box::new(val),
            },
        ))
    }

    fn lower_lambda(
        &mut self,
        params: &Form,
        body: &[Form],
        at: usize,
    ) -> Result<CoreExpr, EvalError> {
        let func = self.push_and_build_fn(params, body, None, at)?;
        Ok(CoreExpr::new(
            at,
            CoreKind::Lambda {
                func: Box::new(func),
            },
        ))
    }

    fn lower_function(&mut self, args: &[Form], at: usize) -> Result<CoreExpr, EvalError> {
        let [operand] = args else {
            return Err(application_arity_error("FUNCTION", at));
        };
        if let Some(fname) = operand.name() {
            if self
                .compiler
                .lexical_operator(fname)
                .is_some_and(|s| s.namespace == Namespace::Macro)
            {
                return Err(EvalError::compile(
                    operand.at,
                    "FUNCTION requires a function name",
                ));
            }
            Ok(self.resolve_read_fn(fname, false, operand.at))
        } else if let Some(accessor) = setf_accessor(operand) {
            Ok(self.resolve_read_fn(accessor, true, operand.at))
        } else if let Some([op, params, body @ ..]) = operand.list()
            && op.name().is_some_and(|n| n.operator() == "LAMBDA")
        {
            self.lower_lambda(params, body, operand.at)
        } else {
            Err(EvalError::compile(
                operand.at,
                "expected a function name or lambda",
            ))
        }
    }

    fn lower_named_call(
        &mut self,
        name: &Name,
        args: &[Form],
        at: usize,
    ) -> Result<CoreExpr, EvalError> {
        let inline_declaration = self.compiler.call_inline_declaration(name, args);
        let qualities = self.optimization_qualities();
        let mut ftype_ret = CoreType::Top;
        let callee = match self.resolve_local_fn(name, false) {
            Some((id, true)) => CoreCallee::Local(id),
            Some((id, false)) => {
                CoreCallee::Expr(Box::new(CoreExpr::new(at, CoreKind::ReadLocalFn(id))))
            }
            None => {
                if self.top().fn_ir.name.as_ref() != Some(name) {
                    self.compiler.warn_undefined_call(name, at);
                }
                if qualities.safety == 0
                    && let Some(ft_form) = self.compiler.current_ftype(name, false)
                    && let Some((_, Some(result))) = ftype_signature(&ft_form)
                {
                    ftype_ret = parse_the_type(result).primary_type();
                }
                CoreCallee::Named(name.clone())
            }
        };
        let signature = (qualities.safety == 3)
            .then(|| self.compiler.current_ftype(name, false))
            .flatten();
        let checked = signature
            .as_ref()
            .and_then(|signature| CheckedFtype::for_call(signature, args.len()));
        let assertion = |type_spec: &Form, expr: CoreExpr, at| {
            CoreExpr::new(
                at,
                CoreKind::The {
                    type_spec: type_spec.clone(),
                    expr: Box::new(expr),
                    safety: qualities.safety,
                },
            )
        };
        let mut lowered_args = Vec::with_capacity(args.len());
        for (index, arg) in args.iter().enumerate() {
            let expr = self.lower_form(arg)?;
            let expr = match checked.as_ref().and_then(|types| types.arg(index)) {
                Some(spec) => assertion(spec, expr, arg.at),
                None => expr,
            };
            lowered_args.push(expr);
        }
        let call = CoreExpr::new(
            at,
            CoreKind::Call {
                callee,
                args: lowered_args,
                qualities,
                inline_declaration,
                ftype_ret,
                fixnum_arithmetic: None,
            },
        );
        Ok(if let Some(spec) = checked.and_then(|types| types.result) {
            assertion(spec, call, at)
        } else {
            call
        })
    }

    fn lower_locally(&mut self, args: &[Form]) -> Result<CoreExpr, EvalError> {
        let (decls, body) =
            declarations(self.compiler.runtime, &self.compiler.context.warnings, args)?;
        self.with_frame_scope(|builder, mark| {
            builder.declare_free_specials(&decls.special, mark.scopes);
            builder.record_free_decls(&decls, &[]);
            builder.compiler.free_declarations(decls, mark.compiler)?;
            builder.lower_sequence(body)
        })
    }

    fn lower_let(
        &mut self,
        specs_form: &Form,
        body: &[Form],
        sequential: bool,
        at: usize,
    ) -> Result<CoreExpr, EvalError> {
        let specs = specs_form.list().ok_or_else(|| {
            EvalError::compile(specs_form.at, "expected a list of variable bindings")
        })?;
        let mut parsed = Vec::with_capacity(specs.len());
        for spec in specs {
            match &spec.kind {
                FormKind::Name(_) => parsed.push((variable(spec)?, spec.at, None)),
                _ => {
                    let items = spec.list().ok_or_else(|| {
                        EvalError::compile(spec.at, "expected a variable name or (variable value)")
                    })?;
                    let (var_f, init) = match items {
                        [var] => (var, None),
                        [var, value] => (var, Some(value)),
                        _ => {
                            return Err(EvalError::compile(
                                spec.at,
                                "expected a variable name or (variable value)",
                            ));
                        }
                    };
                    parsed.push((variable(var_f)?, var_f.at, init));
                }
            }
        }
        let (decls, body_forms) =
            declarations(self.compiler.runtime, &self.compiler.context.warnings, body)?;
        let f_mark = self.mark_frame_scope();
        let qualities = decls.apply_optimize_declarations(self.optimization_qualities());
        // LET enters the bindings' scope only after every initializer has run.
        let delay_checks = !sequential
            && parsed.len() > 1
            && qualities.safety > 0
            && parsed.iter().any(|&(name, _, _)| {
                decls.types_for(name).any(should_emit_runtime_type_check)
                    || (self.compiler.is_special_var(name, &decls.special)
                        && self
                            .compiler
                            .proclaimed_var_type(name)
                            .as_ref()
                            .is_some_and(should_emit_runtime_type_check))
            });
        let mut initializers = Vec::new();
        let mut bindings = Vec::with_capacity(parsed.len());
        let mut bound_names = Vec::with_capacity(parsed.len());

        for (vname, var_at, init_opt) in parsed {
            self.compiler
                .ensure_writable_variable(vname, var_at, "bind")?;
            let mut rhs = match init_opt {
                Some(form) => self.lower_form(form)?,
                None => CoreExpr::nil(at),
            };
            if delay_checks {
                let temp = self.top_mut().fn_ir.alloc_var(VariableBinding {
                    name: vname.clone(),
                    special: false,
                    assigned: false,
                    captured: false,
                    read_count: 0,
                    trusted_type: CoreType::Top,
                    dynamic_extent: false,
                });
                let read = CoreExpr::new(rhs.at, CoreKind::ReadVar(temp));
                initializers.push((CoreBindingTarget::Lexical(temp), rhs));
                rhs = read;
            }
            let special = self.compiler.is_special_var(vname, &decls.special);
            let proclaimed = if special {
                self.compiler.proclaimed_var_type(vname)
            } else {
                None
            };
            let mut trusted_type = CoreType::Top;
            for spec in decls.types_for(vname).chain(proclaimed.as_ref()) {
                if qualities.safety == 0 && !special {
                    trusted_type = trusted_type.meet(parse_type_spec(spec).0);
                }
                if should_emit_runtime_type_check(spec) {
                    rhs = CoreExpr::new(
                        rhs.at,
                        CoreKind::The {
                            type_spec: spec.clone(),
                            expr: Box::new(rhs),
                            safety: qualities.safety,
                        },
                    );
                }
            }
            let target = if special {
                CoreBindingTarget::Special(vname.clone())
            } else {
                let id = self.top_mut().fn_ir.alloc_var(VariableBinding {
                    name: vname.clone(),
                    special: false,
                    assigned: false,
                    captured: false,
                    read_count: 0,
                    trusted_type,
                    dynamic_extent: decls.is_dynamic_extent_var(vname),
                });
                CoreBindingTarget::Lexical(id)
            };
            if sequential {
                self.bind_let_name(vname, &target, &decls);
            }
            bound_names.push(vname);
            bindings.push((target, rhs));
        }

        if !sequential {
            for (name, (target, _)) in bound_names.iter().zip(&bindings) {
                self.bind_let_name(name, target, &decls);
            }
        }
        self.declare_free_specials(&decls.special, f_mark.scopes);
        self.record_free_decls(&decls, &bound_names);
        self.compiler.free_declarations(decls, f_mark.compiler)?;
        let body_expr = self.lower_sequence(body_forms)?;
        self.restore_frame_scope(f_mark);

        let result = CoreExpr::new(
            at,
            CoreKind::Let {
                sequential,
                bindings,
                body: Box::new(body_expr),
            },
        );
        if delay_checks {
            Ok(CoreExpr::new(
                at,
                CoreKind::Let {
                    sequential: false,
                    bindings: initializers,
                    body: Box::new(result),
                },
            ))
        } else {
            Ok(result)
        }
    }

    fn bind_let_name(
        &mut self,
        name: &Name,
        target: &CoreBindingTarget,
        decls: &super::Declarations<'_>,
    ) {
        let (binding, type_target) = match target {
            CoreBindingTarget::Lexical(id) => (
                ScopedBindingKind::Var(*id),
                TypeDeclarationTarget::FrameVar(self.frames.len() - 1, *id),
            ),
            CoreBindingTarget::Special(_) => (
                ScopedBindingKind::Special,
                TypeDeclarationTarget::SpecialBinding(
                    self.frames.len() - 1,
                    self.top().scopes.len(),
                ),
            ),
        };
        self.bind_name(name, Namespace::Variable, binding);
        for spec in decls.types_for(name) {
            self.top_mut()
                .var_types
                .push((type_target, spec.clone(), false));
        }
    }

    fn lower_tagbody(&mut self, forms: &[Form], at: usize) -> Result<CoreExpr, EvalError> {
        let TagbodyForms { labels, statements } =
            TagbodyForms::parse(self.compiler.runtime, forms)?;
        let has_tags = !labels.is_empty();
        let id = self.top_mut().fn_ir.alloc_tagbody(TagbodyTargets {
            tags: labels,
            used: false,
            escapes: false,
            crosses_dynamic_scope: false,
        });
        let segments = self.with_frame_scope(|builder, _| {
            if has_tags {
                builder.top_mut().tagbodies.push(id);
            }
            statements
                .into_iter()
                .enumerate()
                .map(|(segment, forms)| Ok((segment.checked_sub(1), builder.lower_forms(forms)?)))
                .collect::<Result<_, EvalError>>()
        })?;
        Ok(CoreExpr::new(at, CoreKind::Tagbody { id, segments }))
    }

    fn lower_setq(&mut self, args: &[Form], at: usize) -> Result<CoreExpr, EvalError> {
        let (pairs, remainder) = args.as_chunks::<2>();
        if !remainder.is_empty() {
            return Err(EvalError::compile(at, "wrong number of arguments to SETQ"));
        }
        if pairs.is_empty() {
            return Ok(CoreExpr::nil(at));
        }
        let mut exprs = Vec::with_capacity(pairs.len());
        for pair in pairs {
            let var = variable(&pair[0])?;
            if self.compiler.is_symbol_macro(var) {
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
                exprs.push(self.lower_form(&assignment)?);
                continue;
            }
            self.compiler
                .ensure_writable_variable(var, pair[0].at, "assign")?;
            let rhs = self.lower_form(&pair[1])?;
            let target = self.resolve_var_target_no_bridge(var);
            let safety = self.optimization_qualities().safety;
            let rhs = if safety > 0 {
                self.assert_var_types(rhs, self.var_type_forms(var, target, false), safety)
            } else {
                rhs
            };
            let rhs = Box::new(rhs);
            let expr = match self.resolve_var(var, target) {
                Some(id) => CoreExpr::new(
                    pair[0].at,
                    CoreKind::WriteVar {
                        var: id,
                        value: rhs,
                    },
                ),
                None => CoreExpr::new(
                    pair[0].at,
                    CoreKind::WriteGlobal {
                        name: var.clone(),
                        value: rhs,
                    },
                ),
            };
            exprs.push(expr);
        }
        if exprs.len() == 1 {
            Ok(exprs.pop().unwrap())
        } else {
            Ok(CoreExpr::new(at, CoreKind::Progn(exprs)))
        }
    }

    fn lower_flet(
        &mut self,
        specs_form: &Form,
        body: &[Form],
        recursive: bool,
        at: usize,
    ) -> Result<CoreExpr, EvalError> {
        let specs = specs_form.list().ok_or_else(|| {
            EvalError::compile(
                specs_form.at,
                "expected a list of local function definitions",
            )
        })?;
        let f_mark = self.mark_frame_scope();

        let mut parsed = Vec::with_capacity(specs.len());
        let mut seen_names: Vec<(Namespace, &Name)> = Vec::new();
        for spec in specs {
            let Some([head, params, fbody @ ..]) = spec.list() else {
                return Err(EvalError::compile(
                    spec.at,
                    "expected (name lambda-list . body)",
                ));
            };
            let (namespace, fname, setf) = if let Some(n) = head.name() {
                (Namespace::Function, n, false)
            } else if let Some(acc) = setf_accessor(head) {
                (Namespace::Setter, acc, true)
            } else {
                return Err(EvalError::compile(head.at, "expected a function name"));
            };
            if seen_names
                .iter()
                .any(|(ns, n)| *ns == namespace && *n == fname)
            {
                return Err(EvalError::compile(spec.at, "duplicate local function"));
            }
            seen_names.push((namespace, fname));
            let source_expression = Some(Rc::new(source_lambda(
                self.compiler.runtime,
                params,
                fbody,
                Some(fname),
                spec.at,
            )));
            let id = self.top_mut().fn_ir.alloc_local_fn(LocalFunction {
                binding: LocalFunctionBinding {
                    name: fname.clone(),
                    setf,
                    call_count: 0,
                    escapes: false,
                    recursive: false,
                    has_notinline_call: false,
                    dynamic_extent: false,
                },
                params: CoreParameterBindings::empty(),
                source_lambda_list: Some(Rc::new(params.clone())),
                source_expression,
                body: Box::new(CoreExpr::nil(spec.at)),
                at: spec.at,
            });
            parsed.push((id, namespace, fname, params, fbody, spec.at));
        }

        let (decls, body_forms) =
            declarations(self.compiler.runtime, &self.compiler.context.warnings, body)?;
        for &(id, namespace, fname, ..) in &parsed {
            if decls.is_dynamic_extent_fn(fname, namespace.binds_function(true)) {
                self.top_mut().fn_ir.local_functions[id.index()]
                    .binding
                    .dynamic_extent = true;
            }
        }
        let bind_definitions = |builder: &mut Self| {
            for &(id, ns, name, ..) in &parsed {
                builder.bind_name(name, ns, ScopedBindingKind::LocalFn(id));
            }
        };
        if recursive {
            bind_definitions(self);
        }
        let definition_scope = self.compiler.mark_scope();
        if recursive {
            let local = decls.for_local_functions(|name, setf| {
                parsed.iter().any(|&(_, namespace, function, ..)| {
                    function == name && namespace.binds_function(setf)
                })
            });
            self.compiler.free_declarations(local, definition_scope)?;
        }
        let mut fn_ids = Vec::with_capacity(parsed.len());
        for &(id, _, fname, params, fbody, fat) in &parsed {
            let def = self.function_definition(params, fbody)?;
            let (lf_params, lf_body) = self.lower_definition(def, Some(fname), fat)?;
            let local_function = &mut self.top_mut().fn_ir.local_functions[id.index()];
            local_function.params = lf_params;
            *local_function.body = lf_body;
            fn_ids.push(id);
        }
        self.compiler.restore_scope(definition_scope);

        if !recursive {
            bind_definitions(self);
        }

        self.declare_free_specials(&decls.special, f_mark.scopes);
        self.record_free_decls(&decls, &[]);
        self.compiler.free_declarations(decls, f_mark.compiler)?;
        let body_expr = self.lower_sequence(body_forms)?;
        self.restore_frame_scope(f_mark);

        Ok(CoreExpr::new(
            at,
            CoreKind::Flet {
                recursive,
                fns: fn_ids,
                body: Box::new(body_expr),
            },
        ))
    }

    fn lower_macrolet(&mut self, defs: &Form, body: &[Form]) -> Result<CoreExpr, EvalError> {
        let bindings = self.compiler.build_macrolet_scopes(defs)?;
        let (decls, body_forms) =
            declarations(self.compiler.runtime, &self.compiler.context.warnings, body)?;
        self.with_frame_scope(|builder, mark| {
            for scope in &bindings {
                builder.top_mut().scopes.push(ScopedName {
                    name: scope.name.clone(),
                    namespace: Namespace::Macro,
                    kind: ScopedBindingKind::Macro,
                });
            }
            builder.compiler.bindings.extend(bindings);
            builder.declare_free_specials(&decls.special, mark.scopes);
            builder.record_free_decls(&decls, &[]);
            builder.compiler.free_declarations(decls, mark.compiler)?;
            builder.lower_sequence(body_forms)
        })
    }

    fn lower_symbol_macrolet(&mut self, defs: &Form, body: &[Form]) -> Result<CoreExpr, EvalError> {
        let (decls, body_forms) =
            declarations(self.compiler.runtime, &self.compiler.context.warnings, body)?;
        let bindings = self
            .compiler
            .build_symbol_macrolet_scopes(defs, &decls.special)?;
        self.with_frame_scope(|builder, mark| {
            builder.compiler.bindings.extend(bindings);
            builder.declare_free_specials(&decls.special, mark.scopes);
            builder.record_free_decls(&decls, &[]);
            builder.compiler.free_declarations(decls, mark.compiler)?;
            builder.lower_sequence(body_forms)
        })
    }

    /// Core bindings are emitted directly. Reconstruct the remaining visible
    /// name bindings when a raw form returns to the bytecode compiler.
    fn raw_lexical_bindings(&self) -> Vec<RawLexicalBinding> {
        let mut variables = HashSet::new();
        let mut functions = HashSet::new();
        let mut inline_declarations = HashSet::new();
        let mut bindings: Vec<_> = self.compiler.bindings[self.outer_bindings..]
            .iter()
            .rev()
            .filter_map(|scope| {
                let key = scope.name.key();
                match scope.namespace {
                    Namespace::Variable
                    | Namespace::Special
                    | Namespace::SpecialReference
                    | Namespace::SymbolMacro
                        if variables.insert(key) =>
                    {
                        match scope.namespace {
                            Namespace::SpecialReference => {
                                Some(RawLexicalBinding::Special(scope.name.clone()))
                            }
                            _ => Some(RawLexicalBinding::Macro {
                                name: scope.name.clone(),
                                expander: Rc::clone(scope.expander.as_ref()?),
                                symbol_macro: true,
                            }),
                        }
                    }
                    Namespace::Function | Namespace::Macro if functions.insert((key, false)) => {
                        Some(RawLexicalBinding::Macro {
                            name: scope.name.clone(),
                            expander: Rc::clone(scope.expander.as_ref()?),
                            symbol_macro: false,
                        })
                    }
                    Namespace::Setter => {
                        functions.insert((key, true));
                        None
                    }
                    Namespace::InlineDeclaration { setf, declaration }
                        if !functions.contains(&(key, setf))
                            && inline_declarations.insert((key, setf)) =>
                    {
                        Some(RawLexicalBinding::Inline {
                            name: scope.name.clone(),
                            setf,
                            declaration,
                        })
                    }
                    _ => None,
                }
            })
            .collect();
        bindings.extend(
            self.compiler
                .custom_declarations
                .iter()
                .map(|(name, value)| {
                    RawLexicalBinding::Declaration(name.clone(), Rc::clone(value))
                }),
        );
        bindings.push(RawLexicalBinding::Optimize(self.optimization_qualities()));
        self.raw_type_declarations(&mut bindings);
        let mut functions = HashSet::new();
        for decl in self.compiler.declaration_scopes[self.outer_declaration_scopes..]
            .iter()
            .rev()
            .flat_map(|scope| scope.function_types.iter().rev())
        {
            if self
                .compiler
                .find_binding_index(&decl.name, |ns| ns.binds_function(decl.setf))
                == decl.binding_index
                && functions.insert((decl.name.key(), decl.setf))
            {
                bindings.push(RawLexicalBinding::FunctionType {
                    name: decl.name.clone(),
                    setf: decl.setf,
                    spec: decl.spec.clone(),
                });
            }
        }
        bindings
    }

    fn raw_type_declarations(&self, bindings: &mut Vec<RawLexicalBinding>) {
        let new_scopes = &self.compiler.declaration_scopes[self.outer_declaration_scopes..];
        if new_scopes
            .iter()
            .all(|scope| scope.variable_types.is_empty())
            && self.frames.iter().all(|frame| frame.var_types.is_empty())
        {
            return;
        }
        let mut names = HashSet::new();
        for name in self
            .compiler
            .scopes()
            .filter(|scope| scope.namespace.is_variable())
            .map(|scope| &scope.name)
            .chain(
                new_scopes
                    .iter()
                    .flat_map(|scope| scope.variable_types.iter().map(|decl| &decl.name)),
            )
        {
            if !names.insert(name.key()) {
                continue;
            }
            let mut push = |spec: &Form, is_free| {
                bindings.push(RawLexicalBinding::VariableType {
                    name: name.clone(),
                    spec: spec.clone(),
                    is_free,
                });
            };
            if !self.compiler.is_symbol_macro(name)
                && let Some(target) = self.resolve_var_target_no_bridge(name)
            {
                for (spec, is_free) in self.core_var_types(target) {
                    push(spec, is_free);
                }
            } else {
                let index = self.compiler.find_var_binding_index(name);
                for decl in
                    self.compiler
                        .scoped_var_types(name, index, self.outer_declaration_scopes)
                {
                    push(&decl.spec, decl.is_free);
                }
            }
        }
    }

    fn capture_variable_from_frame(&mut self, mut cur_id: VarId, from_lvl: usize) -> VarId {
        let top = self.frames.len() - 1;
        for child_lvl in (from_lvl + 1)..=top {
            if let Some(&mapped) = self.frames[child_lvl].captured_parent_vars.get(&cur_id) {
                cur_id = mapped;
            } else {
                let binding = self.frames[child_lvl - 1].fn_ir.vars[cur_id.index()].fresh_binding();
                let cid = self.frames[child_lvl].fn_ir.alloc_var(binding);
                self.frames[child_lvl]
                    .captured_parent_vars
                    .insert(cur_id, cid);
                self.frames[child_lvl]
                    .fn_ir
                    .parent_var_captures
                    .push((cid, cur_id));
                cur_id = cid;
            }
        }
        cur_id
    }

    fn resolve_var(&mut self, name: &Name, target: Option<TypeDeclarationTarget>) -> Option<VarId> {
        match target {
            Some(TypeDeclarationTarget::FrameVar(level, id)) => {
                return Some(self.capture_variable_from_frame(id, level));
            }
            Some(TypeDeclarationTarget::SpecialBinding(..)) => return None,
            None => {}
        }
        let ext = self
            .compiler
            .scopes()
            .rev()
            .find(|s| &s.name == name && s.namespace.is_variable())?;
        ext.binding.as_ref()?;
        let root_id = if let Some(&(existing, _)) = self.frames[0]
            .fn_ir
            .external_var_captures
            .iter()
            .find(|(_, n)| n == name)
        {
            existing
        } else {
            let vid = self.frames[0].fn_ir.alloc_var(VariableBinding {
                name: name.clone(),
                special: false,
                assigned: true,
                captured: true,
                read_count: 0,
                trusted_type: CoreType::Top,
                dynamic_extent: false,
            });
            self.frames[0]
                .fn_ir
                .external_var_captures
                .push((vid, name.clone()));
            vid
        };
        Some(self.capture_variable_from_frame(root_id, 0))
    }

    fn resolve_read_var(&mut self, name: &Name, at: usize) -> CoreExpr {
        let target = self.resolve_var_target_no_bridge(name);
        let base = if let Some(id) = self.resolve_var(name, target) {
            CoreExpr::new(at, CoreKind::ReadVar(id))
        } else if *name == Name::common_lisp("NIL") {
            return CoreExpr::new(at, CoreKind::Const(CoreConst::Nil));
        } else if *name == Name::common_lisp("T") {
            return CoreExpr::new(at, CoreKind::Const(CoreConst::True));
        } else {
            CoreExpr::new(at, CoreKind::ReadGlobal(name.clone()))
        };
        self.assert_var_types(
            base,
            self.var_type_forms(name, target, true),
            self.optimization_qualities().safety,
        )
    }

    fn capture_function_from_frame(
        &mut self,
        mut cur_id: LocalFunctionId,
        from_lvl: usize,
    ) -> LocalFunctionId {
        let top = self.frames.len() - 1;
        for child_lvl in (from_lvl + 1)..=top {
            if let Some(&mapped) = self.frames[child_lvl]
                .captured_parent_functions
                .get(&cur_id)
            {
                cur_id = mapped;
            } else {
                let orig = &self.frames[child_lvl - 1].fn_ir.local_functions[cur_id.index()];
                let name = orig.binding.name.clone();
                let setf = orig.binding.setf;
                let at = orig.at;
                let cid = self.frames[child_lvl].fn_ir.alloc_local_fn(LocalFunction {
                    binding: LocalFunctionBinding {
                        name,
                        setf,
                        call_count: 0,
                        escapes: false,
                        recursive: false,
                        has_notinline_call: false,
                        dynamic_extent: false,
                    },
                    params: CoreParameterBindings::empty(),
                    source_lambda_list: None,
                    source_expression: None,
                    body: Box::new(CoreExpr::nil(at)),
                    at,
                });
                self.frames[child_lvl]
                    .captured_parent_functions
                    .insert(cur_id, cid);
                self.frames[child_lvl]
                    .fn_ir
                    .parent_function_captures
                    .push((cid, cur_id));
                cur_id = cid;
            }
        }
        cur_id
    }

    /// Returns `(id, is_same_frame)`.
    fn resolve_local_fn(&mut self, name: &Name, setter: bool) -> Option<(LocalFunctionId, bool)> {
        let top = self.frames.len() - 1;
        let target_ns = if setter {
            Namespace::Setter
        } else {
            Namespace::Function
        };
        for lvl in (0..=top).rev() {
            if let Some(entry) = self.frames[lvl]
                .scopes
                .iter()
                .rev()
                .find(|s| {
                    &s.name == name
                        && (s.namespace == target_ns
                            || (!setter && s.namespace == Namespace::Macro))
                })
                .cloned()
            {
                return match entry.kind {
                    ScopedBindingKind::LocalFn(cur_id) => {
                        Some((self.capture_function_from_frame(cur_id, lvl), lvl == top))
                    }
                    ScopedBindingKind::Macro => None,
                    _ => unreachable!(),
                };
            }
        }
        let ext = self.compiler.scopes().rev().find(|s| {
            &s.name == name
                && (s.namespace == target_ns || (!setter && s.namespace == Namespace::Macro))
        })?;
        if ext.namespace == Namespace::Macro || ext.binding.is_none() {
            return None;
        }
        let root_id = if let Some(&(existing, _, _)) = self.frames[0]
            .fn_ir
            .external_function_captures
            .iter()
            .find(|(_, n, sf)| n == name && *sf == setter)
        {
            existing
        } else {
            let lid = self.frames[0].fn_ir.alloc_local_fn(LocalFunction {
                binding: LocalFunctionBinding {
                    name: name.clone(),
                    setf: setter,
                    call_count: 0,
                    escapes: true,
                    recursive: false,
                    has_notinline_call: false,
                    dynamic_extent: false,
                },
                params: CoreParameterBindings::empty(),
                source_lambda_list: None,
                source_expression: None,
                body: Box::new(CoreExpr::nil(0)),
                at: 0,
            });
            self.frames[0]
                .fn_ir
                .external_function_captures
                .push((lid, name.clone(), setter));
            lid
        };
        Some((self.capture_function_from_frame(root_id, 0), false))
    }

    fn resolve_read_fn(&mut self, name: &Name, setf: bool, at: usize) -> CoreExpr {
        let kind = match self.resolve_local_fn(name, setf) {
            Some((id, _)) => CoreKind::ReadLocalFn(id),
            None => CoreKind::ReadGlobalFn {
                name: name.clone(),
                setf,
            },
        };
        CoreExpr::new(at, kind)
    }

    fn capture_block_from_frame(&mut self, mut cur_id: BlockId, from_lvl: usize) -> BlockId {
        let top = self.frames.len() - 1;
        for child_lvl in (from_lvl + 1)..=top {
            if let Some(&mapped) = self.frames[child_lvl].captured_parent_blocks.get(&cur_id) {
                cur_id = mapped;
            } else {
                let target =
                    self.frames[child_lvl - 1].fn_ir.blocks[cur_id.index()].fresh_binding();
                let cid = self.frames[child_lvl].fn_ir.alloc_block(target);
                self.frames[child_lvl]
                    .captured_parent_blocks
                    .insert(cur_id, cid);
                self.frames[child_lvl]
                    .fn_ir
                    .parent_block_captures
                    .push((cid, cur_id));
                cur_id = cid;
            }
        }
        cur_id
    }

    fn resolve_block(&mut self, name: &Name) -> Option<BlockId> {
        let top = self.frames.len() - 1;
        for lvl in (0..=top).rev() {
            if let Some(entry) = self.frames[lvl]
                .scopes
                .iter()
                .rev()
                .find(|s| s.namespace == Namespace::Block && &s.name == name)
                .cloned()
            {
                let ScopedBindingKind::Block(cur_id) = entry.kind else {
                    unreachable!();
                };
                return Some(self.capture_block_from_frame(cur_id, lvl));
            }
        }
        let ext = self
            .compiler
            .scopes()
            .rev()
            .find(|s| s.namespace == Namespace::Block && &s.name == name)?;
        ext.binding.as_ref()?;
        let root_id = if let Some(&(existing, _)) = self.frames[0]
            .fn_ir
            .external_block_captures
            .iter()
            .find(|(_, n)| n == name)
        {
            existing
        } else {
            let bid = self.frames[0].fn_ir.alloc_block(BlockExitTarget {
                name: name.clone(),
                used: true,
                escapes: true,
                crosses_dynamic_scope: true,
            });
            self.frames[0]
                .fn_ir
                .external_block_captures
                .push((bid, name.clone()));
            bid
        };
        Some(self.capture_block_from_frame(root_id, 0))
    }

    fn capture_tagbody_from_frame(&mut self, mut cur_id: TagbodyId, from_lvl: usize) -> TagbodyId {
        let top = self.frames.len() - 1;
        for child_lvl in (from_lvl + 1)..=top {
            if let Some(&mapped) = self.frames[child_lvl]
                .captured_parent_tagbodies
                .get(&cur_id)
            {
                cur_id = mapped;
            } else {
                let targets =
                    self.frames[child_lvl - 1].fn_ir.tagbodies[cur_id.index()].fresh_binding();
                let cid = self.frames[child_lvl].fn_ir.alloc_tagbody(targets);
                self.frames[child_lvl]
                    .captured_parent_tagbodies
                    .insert(cur_id, cid);
                self.frames[child_lvl]
                    .fn_ir
                    .parent_tagbody_captures
                    .push((cid, cur_id));
                cur_id = cid;
            }
        }
        cur_id
    }

    fn resolve_tag(&mut self, tag: &Tag) -> Option<(TagbodyId, usize)> {
        let top = self.frames.len() - 1;
        for lvl in (0..=top).rev() {
            let frame = &self.frames[lvl];
            for &id in frame.tagbodies.iter().rev() {
                if let Some(&pos) = frame.fn_ir.tagbodies[id.index()].tags.get(tag) {
                    return Some((self.capture_tagbody_from_frame(id, lvl), pos));
                }
            }
        }
        let (labels, pos) = self
            .compiler
            .tags
            .iter()
            .rev()
            .chain(self.compiler.outer_tags.iter().rev())
            .find_map(|scope| {
                scope
                    .labels
                    .get(tag)
                    .map(|&pos| (Rc::clone(&scope.labels), pos))
            })?;
        let root = &mut self.frames[0].fn_ir;
        let root_id = root
            .external_tagbody_captures
            .iter()
            .copied()
            .find(|id| Rc::ptr_eq(&root.tagbodies[id.index()].tags, &labels))
            .unwrap_or_else(|| {
                let id = root.alloc_tagbody(TagbodyTargets {
                    tags: labels,
                    used: true,
                    escapes: true,
                    crosses_dynamic_scope: true,
                });
                root.external_tagbody_captures.push(id);
                id
            });
        Some((self.capture_tagbody_from_frame(root_id, 0), pos))
    }
}

// ---------------------------------------------------------------------------
// CoreFn -> Bytecode Codegen
// ---------------------------------------------------------------------------

enum BlockEmitTarget {
    Elided,
    LocalJump {
        dest: Destination,
        mark: usize,
        jumps: Vec<usize>,
    },
    Escaping(BindingLocation),
}

enum TagbodyEmitTarget {
    Unbound,
    LocalJump {
        mark: usize,
        tag_pcs: Vec<Option<usize>>,
        pending_jumps: Vec<(usize, usize)>,
    },
    Escaping(BindingLocation),
}

struct CoreCodegenBindings {
    var_locations: Vec<Option<BindingLocation>>,
    block_targets: Vec<BlockEmitTarget>,
    tagbody_targets: Vec<TagbodyEmitTarget>,
    local_function_locations: Vec<Option<BindingLocation>>,
}

impl<'a> Compiler<'a> {
    fn core_child(&self, fn_ir: &CoreFn) -> (Self, CoreCodegenBindings) {
        let mut outer = self.outer.clone();
        outer.extend(self.bindings.iter().cloned());
        let mut child = self.nested(outer);
        child.ltv_inits = Rc::clone(&self.ltv_inits);
        child
            .outer_tags
            .extend(self.outer_tags.iter().chain(&self.tags).cloned());

        let env = CoreCodegenBindings {
            var_locations: vec![None; fn_ir.vars.len()],
            block_targets: (0..fn_ir.blocks.len())
                .map(|_| BlockEmitTarget::Elided)
                .collect(),
            tagbody_targets: (0..fn_ir.tagbodies.len())
                .map(|_| TagbodyEmitTarget::Unbound)
                .collect(),
            local_function_locations: vec![None; fn_ir.local_functions.len()],
        };

        (child, env)
    }

    fn inherit_location(&mut self, parent: &Self, location: &BindingLocation) -> BindingLocation {
        let binding = match location {
            BindingLocation::Local(binding) => binding,
            BindingLocation::Capture(index) => &parent.captures[*index],
        };
        BindingLocation::Capture(self.register_capture(binding.clone()))
    }

    fn compile_core_lambda(
        &self,
        fn_ir: &CoreFn,
        outer: Option<&CoreCodegenBindings>,
    ) -> Result<Self, EvalError> {
        let (mut child, mut env) = self.core_child(fn_ir);
        // Bindings from outside the Core IR unit are addressed by name.
        for &(vid, ref name) in &fn_ir.external_var_captures {
            env.var_locations[vid.index()] = child.binding(name, Namespace::Variable);
        }
        for &(bid, ref name) in &fn_ir.external_block_captures {
            if let Some(loc) = child.binding(name, Namespace::Block) {
                env.block_targets[bid.index()] = BlockEmitTarget::Escaping(loc);
            }
        }
        for &tid in &fn_ir.external_tagbody_captures {
            let labels = &fn_ir.tagbodies[tid.index()].tags;
            let scope = child
                .outer_tags
                .iter()
                .rev()
                .find(|scope| Rc::ptr_eq(&scope.labels, labels))
                .expect("captured TAGBODY belongs to an enclosing scope");
            let token = BindingLocation::Capture(child.capture(scope.token.clone()));
            env.tagbody_targets[tid.index()] = TagbodyEmitTarget::Escaping(token);
        }
        for &(lid, ref name, setf) in &fn_ir.external_function_captures {
            let ns = if setf {
                Namespace::Setter
            } else {
                Namespace::Function
            };
            env.local_function_locations[lid.index()] = child.binding(name, ns);
        }
        if let Some(outer) = outer {
            env.inherit_lambda(fn_ir, outer, |loc| child.inherit_location(self, loc));
        }
        child.emit_core_fn(fn_ir, None, &mut env)?;
        Ok(child)
    }

    fn emit_core_fn(
        &mut self,
        fn_ir: &CoreFn,
        local: Option<&LocalFunction>,
        env: &mut CoreCodegenBindings,
    ) -> Result<(), EvalError> {
        let (name, params, body, at) = match local {
            Some(lf) => (Some(&lf.binding.name), &lf.params, &*lf.body, lf.at),
            None => (fn_ir.name.as_ref(), &fn_ir.params, &*fn_ir.body, fn_ir.at),
        };
        let b_mark = self.mark_scope();
        self.code.name = name.map(|name| name.spelling().to_owned());
        self.debug_level = fn_ir.debug;
        self.code.debug.entry = Some(at);
        if let Some(package) = self.runtime.bound_package() {
            self.code.debug.reader_package =
                Some(self.add_constant(ConstantTemplate::Immediate(package.word())) as u32);
        }
        self.code.debug.retain_frame = self.debug_level == 3;
        for (variable, location) in fn_ir.vars.iter().zip(&env.var_locations) {
            if let Some(BindingLocation::Capture(capture)) = location {
                self.debug_capture(&variable.name, *capture, at);
            }
        }
        let fixed_only = params.inlineable_fixed_params().is_some();
        let param_facts: Vec<zkcl_bytecode::ParamFact> = params
            .required
            .iter()
            .map(|&vid| {
                let info = &fn_ir.vars[vid.index()];
                let mut trusted = info.trusted_type;
                if trusted == CoreType::Top && !info.special && fixed_only {
                    trusted = leading_entry_trusted_type(body, vid);
                }
                zkcl_bytecode::ParamFact {
                    ty: param_type_fact(trusted),
                    dynamic_extent: info.dynamic_extent,
                }
            })
            .collect();
        if param_facts
            .iter()
            .any(|fact| fact.ty != zkcl_bytecode::ParamTypeFact::Any || fact.dynamic_extent)
        {
            self.code.facts.params = param_facts;
        }
        let mut dynamic;
        if let Some(fixed) = params.inlineable_fixed_params() {
            dynamic = 0;
            let regs = self.reserve(fixed.len());
            self.code.arity = Arity {
                required: fixed.len(),
                maximum: Some(fixed.len()),
            };
            for (&vid, reg) in fixed.iter().zip(regs) {
                self.bind_core_param_var(fn_ir, env, vid, reg, at);
            }
        } else {
            let required = self.reserve(params.required.len());
            let remaining_start = params.optional.len();
            let unbounded = params.rest.is_some() || params.keywords.is_some();
            self.code.arity = Arity {
                required: params.required.len(),
                maximum: (!unbounded).then_some(params.required.len() + remaining_start),
            };
            if let Some(keywords) = &params.keywords {
                let override_key = self.symbol(&Name::keyword("ALLOW-OTHER-KEYS"), at);
                let names = keywords
                    .params
                    .iter()
                    .map(|kw| self.symbol(&kw.key, at))
                    .collect();
                let table = self.code.keywords.len();
                self.code.keywords.push(zkcl_bytecode::KeywordTable {
                    names,
                    allow_other_keys: keywords.allow_other_keys,
                    override_key,
                });
                self.emit(Ins::CheckKeywords {
                    start: remaining_start,
                    table,
                });
            }
            dynamic = 0;
            for (&vid, reg) in params.required.iter().zip(required) {
                dynamic += self.bind_core_param_var(fn_ir, env, vid, reg, at);
            }
            for (index, opt) in params.optional.iter().enumerate() {
                let reg = self.reserve(1).start;
                self.emit(Ins::OptionalArgument(index, reg));
                dynamic += self.emit_core_default_param(fn_ir, opt, reg, env, at)?;
            }
            if let Some(rest_vid) = params.rest {
                let reg = self.reserve(1).start;
                self.emit(Ins::RestArguments {
                    start: remaining_start,
                    out: reg,
                    at,
                });
                dynamic += self.bind_core_param_var(fn_ir, env, rest_vid, reg, at);
            }
            if let Some(keywords) = &params.keywords {
                for (index, kw) in keywords.params.iter().enumerate() {
                    let reg = self.reserve(1).start;
                    self.emit(Ins::KeywordArgument(index, reg));
                    dynamic += self.emit_core_default_param(fn_ir, &kw.param, reg, env, at)?;
                }
            }
            if remaining_start != 0 || unbounded {
                self.emit(Ins::ReleaseArguments);
            }
            for (vid, init) in &params.aux {
                let reg = self.reserve(1).start;
                if let Some(init_expr) = init {
                    self.emit_core_expr(fn_ir, init_expr, Destination::One(reg), env)?;
                } else {
                    self.emit(Ins::Load(reg, Immediate::Nil));
                }
                dynamic += self.bind_core_param_var(fn_ir, env, *vid, reg, at);
            }
        }

        self.emit_core_expr(fn_ir, body, Destination::All, env)?;
        self.unbind(dynamic);
        self.restore_scope(b_mark);
        let lambda_list = match local {
            Some(lf) => lf.source_lambda_list.as_deref(),
            None => fn_ir.source_lambda_list.as_deref(),
        };
        if let Some(lambda_list) = lambda_list {
            self.remember_lambda_list(lambda_list)?;
        }
        let expression = local
            .and_then(|lf| lf.source_expression.as_deref())
            .or_else(|| {
                if local.is_none() {
                    fn_ir.source_expression.as_deref()
                } else {
                    None
                }
            });
        if let Some(expression) = expression {
            let index = self.form_constant(expression, true)?;
            self.code.debug.source_expression = Some(u32::try_from(index).map_err(|_| {
                EvalError::compile(at, "source expression constant index is out of bounds")
            })?);
        }
        if let Some(name) = name {
            let name_form = if local.is_some_and(|local| local.binding.setf) {
                Form::new(
                    at,
                    FormKind::List(
                        vec![
                            Form::new(at, FormKind::Name(Name::common_lisp("SETF"))),
                            Form::new(at, FormKind::Name(name.clone())),
                        ],
                        None,
                    ),
                )
            } else {
                Form::new(at, FormKind::Name(name.clone()))
            };
            self.code.debug.function_name = Some(
                u32::try_from(self.form_constant(&name_form, true)?).map_err(|_| {
                    EvalError::compile(at, "function name constant index is out of bounds")
                })?,
            );
        }
        Ok(())
    }

    fn bind_core_param_var(
        &mut self,
        fn_ir: &CoreFn,
        env: &mut CoreCodegenBindings,
        vid: VarId,
        reg: super::Register,
        at: usize,
    ) -> usize {
        let var = &fn_ir.vars[vid.index()];
        if var.special {
            self.bind_core_special(&var.name, reg, at);
            1
        } else {
            self.bind_core_lexical_var(fn_ir, env, vid, reg, at);
            0
        }
    }

    fn bind_core_special(&mut self, name: &Name, reg: super::Register, at: usize) {
        let symbol = self.symbol(name, at);
        self.emit(Ins::BindSpecial { symbol, value: reg });
        self.push_special(name, true);
    }

    fn bind_core_lexical_var(
        &mut self,
        fn_ir: &CoreFn,
        env: &mut CoreCodegenBindings,
        vid: VarId,
        reg: super::Register,
        at: usize,
    ) {
        let vinfo = &fn_ir.vars[vid.index()];
        // Raw forms mark every visible variable assigned during analysis, so
        // only proven immutable bindings can skip speculative boxing here.
        let kind = vinfo.capture_kind();
        let binding = RegisterBinding {
            frame_depth: self.frame_depth,
            reg,
            capture_kind: kind,
            boxed: Rc::new(Cell::new(
                kind == CaptureKind::MutableCell && vinfo.captured,
            )),
        };
        self.box_binding_if_captured(&binding, at);
        self.debug_local(&vinfo.name, &binding, at);
        env.var_locations[vid.index()] = Some(BindingLocation::Local(binding.clone()));
        self.bindings.push(ScopedBinding {
            name: vinfo.name.clone(),
            namespace: Namespace::Variable,
            binding: Some(binding),
            expander: None,
            used: None,
        });
    }

    fn emit_core_default_param(
        &mut self,
        fn_ir: &CoreFn,
        param: &CoreDefaultedParameter,
        reg: super::Register,
        env: &mut CoreCodegenBindings,
        at: usize,
    ) -> Result<usize, EvalError> {
        let flag = param.supplied.map(|vid| (vid, self.reserve(1).start));
        if let Some((_, flag_reg)) = flag {
            self.emit(Ins::Load(flag_reg, Immediate::True));
        }
        let branch = self.emit(Ins::IfPresent(reg, 0));
        if let Some((_, flag_reg)) = flag {
            self.emit(Ins::Load(flag_reg, Immediate::Nil));
        }
        if let Some(ref init) = param.init {
            self.emit_core_expr(fn_ir, init, Destination::One(reg), env)?;
        } else {
            self.emit(Ins::Load(reg, Immediate::Nil));
        }
        self.code.instructions[branch] = Ins::IfPresent(reg, self.code.instructions.len());
        let mut dynamic = self.bind_core_param_var(fn_ir, env, param.var, reg, at);
        if let Some((s_vid, flag_reg)) = flag {
            dynamic += self.bind_core_param_var(fn_ir, env, s_vid, flag_reg, at);
        }
        Ok(dynamic)
    }

    fn emit_core_expr(
        &mut self,
        fn_ir: &CoreFn,
        expr: &CoreExpr,
        dest: Destination,
        env: &mut CoreCodegenBindings,
    ) -> Result<(), EvalError> {
        let mark = self.used;
        match &expr.kind {
            CoreKind::Const(c) => match c {
                CoreConst::Nil => self.scalar(dest, |out| Ins::Load(out, Immediate::Nil)),
                CoreConst::True => self.scalar(dest, |out| Ins::Load(out, Immediate::True)),
                CoreConst::Fixnum(v) => {
                    self.scalar(dest, |out| Ins::Load(out, Immediate::Fixnum(*v)))
                }
                CoreConst::Char(ch) => {
                    self.scalar(dest, |out| Ins::Load(out, Immediate::Character(*ch)))
                }
                CoreConst::String(s) => {
                    let id = self.add_constant(ConstantTemplate::String(s.clone(), expr.at));
                    self.scalar(dest, |out| Ins::Constant(out, id));
                }
                CoreConst::Symbol(sym) => {
                    let id = self.symbol(sym, expr.at);
                    self.scalar(dest, |out| Ins::Constant(out, id));
                }
                CoreConst::LiteralForm(form) => {
                    let id = self.constant(form)?;
                    self.scalar(dest, |out| Ins::Constant(out, id));
                }
            },
            CoreKind::ReadVar(var) => {
                let loc = env.var_locations[var.index()]
                    .clone()
                    .expect("bound lexical var in Core IR");
                self.output(dest, |compiler, out| compiler.read_location(loc, out));
            }
            CoreKind::ReadGlobal(name) => {
                let symbol = self.symbol(name, expr.at);
                self.scalar(dest, |out| Ins::ReadCell {
                    out,
                    symbol,
                    function: false,
                    at: expr.at,
                });
            }
            CoreKind::ReadGlobalFn { name, setf } => {
                if *setf {
                    let setf_sym = self.symbol(&Name::common_lisp("SETF"), expr.at);
                    let acc_sym = self.symbol(name, expr.at);
                    let nil_id = self.add_constant(ConstantTemplate::Immediate(Word::NIL));
                    let cdr = self.add_constant(ConstantTemplate::Pair(acc_sym, nil_id));
                    let list_id = self.add_constant(ConstantTemplate::Pair(setf_sym, cdr));
                    let arg_reg = self.reserve(1);
                    self.emit(Ins::Constant(arg_reg.start, list_id));
                    let fdef = self.symbol(&Name::common_lisp("FDEFINITION"), expr.at);
                    self.call_registers(
                        Callee::Named(fdef),
                        arg_reg,
                        dest,
                        expr.at,
                        false,
                        self.inline_declaration(&Name::common_lisp("FDEFINITION"), false),
                    );
                } else {
                    let symbol = self.symbol(name, expr.at);
                    self.scalar(dest, |out| Ins::ReadCell {
                        out,
                        symbol,
                        function: true,
                        at: expr.at,
                    });
                }
            }
            CoreKind::ReadLocalFn(id) => {
                let loc = env.local_function_locations[id.index()]
                    .clone()
                    .expect("bound local function in Core IR");
                self.output(dest, |compiler, out| compiler.read_location(loc, out));
            }
            CoreKind::WriteVar { var, value } => {
                let val_reg = self.reserve(1).start;
                self.emit_core_expr(fn_ir, value, Destination::One(val_reg), env)?;
                let loc = env.var_locations[var.index()]
                    .clone()
                    .expect("bound lexical var for WriteVar");
                self.write_location(loc, val_reg);
                if let Destination::One(out) = dest {
                    self.emit(Ins::Copy(out, val_reg));
                } else {
                    self.select_values(val_reg..val_reg + 1);
                }
            }
            CoreKind::WriteGlobal { name, value } => {
                let val_reg = self.reserve(1).start;
                self.emit_core_expr(fn_ir, value, Destination::One(val_reg), env)?;
                let symbol = self.symbol(name, expr.at);
                self.emit(Ins::WriteCell {
                    symbol,
                    src: val_reg,
                });
                if let Destination::One(out) = dest {
                    self.emit(Ins::Copy(out, val_reg));
                } else {
                    self.select_values(val_reg..val_reg + 1);
                }
            }
            CoreKind::Progn(es) => {
                if let Some((last, earlier)) = es.split_last() {
                    self.emit_core_statements(fn_ir, earlier, env)?;
                    self.emit_core_expr(fn_ir, last, dest, env)?;
                } else {
                    self.scalar(dest, |out| Ins::Load(out, Immediate::Nil));
                }
            }
            CoreKind::If { test, then_, else_ } => {
                let reg = self.reserve(1).start;
                let fixnums = fn_ir.fixnums;
                let guard = test
                    .inline_integer_test(fixnums)
                    .and_then(|(arg, range)| Some((arg, IntegerCheck::for_range(fixnums, range)?)));
                let operand = guard.map_or(test.as_ref(), |(arg, _)| arg);
                let branch = |target| match guard {
                    Some((_, check)) => check.branch(reg, target),
                    None => Ins::Unless(reg, target),
                };
                self.emit_core_expr(fn_ir, operand, Destination::One(reg), env)?;
                let choose = self.emit(branch(0));
                self.emit_core_expr(fn_ir, then_, dest, env)?;
                let done = self.emit(Ins::Jump(0));
                self.code.instructions[choose] = branch(self.code.instructions.len());
                self.emit_core_expr(fn_ir, else_, dest, env)?;
                self.code.instructions[done] = Ins::Jump(self.code.instructions.len());
            }
            CoreKind::Let {
                sequential,
                bindings,
                body,
            } => {
                let b_mark = self.mark_scope();
                let dynamic =
                    self.emit_core_let_bindings(fn_ir, env, *sequential, bindings, expr.at)?;
                self.emit_core_expr(fn_ir, body, dest, env)?;
                for (target, _) in bindings {
                    if let CoreBindingTarget::Lexical(vid) = target {
                        env.var_locations[vid.index()] = None;
                    }
                }
                self.unbind(dynamic);
                self.restore_scope(b_mark);
            }
            CoreKind::Flet {
                recursive,
                fns,
                body,
            } => {
                let b_mark = self.mark_scope();
                // A sibling FLET must see its own enclosing bindings again.
                let saved_lf_env: Vec<_> = fns
                    .iter()
                    .map(|&id| (id, env.local_function_locations[id.index()].clone()))
                    .collect();
                self.emit_local_function_bindings(fn_ir, env, fns, *recursive)?;
                self.emit_core_expr(fn_ir, body, dest, env)?;
                for (id, loc) in saved_lf_env {
                    env.local_function_locations[id.index()] = loc;
                }
                self.restore_scope(b_mark);
            }
            CoreKind::Block { id, body } => {
                self.emit_core_block(fn_ir, env, *id, body, dest, expr.at)?;
            }
            CoreKind::ReturnFrom { id, value } => {
                self.emit_core_return_from(fn_ir, env, *id, value, expr.at)?;
            }
            CoreKind::Tagbody { id, segments } => {
                self.emit_core_tagbody(fn_ir, env, *id, segments, expr.at)?;
                self.scalar(dest, |out| Ins::Load(out, Immediate::Nil));
            }
            CoreKind::Go { id, tag_index } => {
                self.emit_core_go(env, *id, *tag_index, expr.at);
            }
            CoreKind::Call {
                callee,
                args,
                inline_declaration,
                fixnum_arithmetic,
                ..
            } => {
                let compiled_callee = match callee {
                    CoreCallee::Named(name) => {
                        let symbol = if args.len() == 1
                            && *inline_declaration != Some(InlineDeclaration::NotInline)
                        {
                            self.transform_call_symbol(name, expr.at)
                        } else {
                            self.symbol(name, expr.at)
                        };
                        Callee::Named(symbol)
                    }
                    CoreCallee::Local(lf_id) => {
                        let loc = env.local_function_locations[lf_id.index()]
                            .clone()
                            .expect("bound local fn in Call");
                        let reg = self.reserve(1).start;
                        self.read_location(loc, reg);
                        Callee::Value(reg)
                    }
                    CoreCallee::Expr(callee_expr) => {
                        let reg = self.reserve(1).start;
                        self.emit_core_expr(fn_ir, callee_expr, Destination::One(reg), env)?;
                        Callee::Value(reg)
                    }
                };
                let arg_regs = self.reserve(args.len());
                for (arg, reg) in args.iter().zip(arg_regs.clone()) {
                    self.emit_core_expr(fn_ir, arg, Destination::One(reg), env)?;
                }
                if let (Some(op), Callee::Named(symbol)) = (fixnum_arithmetic, compiled_callee) {
                    self.call_fixnum_arithmetic(*op, symbol, arg_regs, dest, expr.at);
                } else {
                    self.call_registers(
                        compiled_callee,
                        arg_regs,
                        dest,
                        expr.at,
                        false,
                        *inline_declaration,
                    );
                }
            }
            CoreKind::Lambda { func } => {
                let child = self.compile_core_lambda(func, Some(env))?;
                self.emit_closure_for_compiled_child(child, dest, func.at);
            }
            CoreKind::CallValues { callee, forms } => {
                self.emit_multiple_value_call(
                    callee.as_ref(),
                    forms,
                    dest,
                    expr.at,
                    |compiler, expr, dest| compiler.emit_core_expr(fn_ir, expr, dest, env),
                )?;
            }
            CoreKind::LoadTimeValue { cell } => {
                self.emit_load_time_value_read(cell, dest, expr.at);
            }
            CoreKind::The {
                type_spec,
                expr: inner,
                safety,
            } => {
                if *safety == 0 {
                    self.emit_core_expr(fn_ir, inner, dest, env)?;
                } else {
                    self.emit_the_checked(
                        type_spec,
                        dest,
                        expr.at,
                        crate::core_ir::is_single_value_expr(inner),
                        |compiler, d| compiler.emit_core_expr(fn_ir, inner, d, env),
                    )?;
                }
            }
            CoreKind::RawSpecial(form, bindings) => {
                let scope = self.mark_scope();
                // Replayed syntax (including inlined code) must see the
                // declaration environment in which it was lowered.
                let outer_declarations = std::mem::take(&mut self.custom_declarations);
                let mut qualities = None;
                let mut variable_types = Vec::new();
                let mut function_types = Vec::new();
                for binding in bindings {
                    match binding {
                        RawLexicalBinding::Declaration(name, value) => {
                            self.custom_declarations
                                .push((name.clone(), Rc::clone(value)));
                        }
                        RawLexicalBinding::Optimize(settings) => qualities = Some(*settings),
                        RawLexicalBinding::VariableType {
                            name,
                            spec,
                            is_free,
                        } => {
                            variable_types.push(super::VariableTypeDeclaration {
                                name: name.clone(),
                                binding_index: self.find_var_binding_index(name),
                                spec: spec.clone(),
                                is_free: *is_free,
                            });
                        }
                        RawLexicalBinding::FunctionType { name, setf, spec } => {
                            function_types.push(super::FunctionTypeDeclaration {
                                name: name.clone(),
                                setf: *setf,
                                binding_index: self
                                    .find_binding_index(name, |ns| ns.binds_function(*setf)),
                                spec: spec.clone(),
                            });
                        }
                        RawLexicalBinding::Special(name) => self.push_special(name, false),
                        RawLexicalBinding::Inline {
                            name,
                            setf,
                            declaration,
                        } => {
                            self.push_inline_declaration(name, *setf, *declaration);
                        }
                        RawLexicalBinding::Macro {
                            name,
                            expander,
                            symbol_macro,
                        } => {
                            self.bindings.push(ScopedBinding {
                                name: name.clone(),
                                namespace: if *symbol_macro {
                                    Namespace::SymbolMacro
                                } else {
                                    Namespace::Macro
                                },
                                binding: None,
                                expander: Some(Rc::clone(expander)),
                                used: None,
                            });
                        }
                    }
                }
                let qualities =
                    qualities.filter(|settings| *settings != self.optimization_qualities());
                if qualities.is_some() || !variable_types.is_empty() || !function_types.is_empty() {
                    variable_types.reverse();
                    self.declaration_scopes
                        .push(Rc::new(super::DeclarationScope {
                            qualities,
                            variable_types,
                            function_types,
                        }));
                }
                let result = self.form_unexpanded(form, dest);
                self.restore_scope(scope);
                self.custom_declarations = outer_declarations;
                result?;
            }
        }
        self.release(mark);
        Ok(())
    }

    fn emit_core_block(
        &mut self,
        fn_ir: &CoreFn,
        env: &mut CoreCodegenBindings,
        id: BlockId,
        body: &CoreExpr,
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let info = &fn_ir.blocks[id.index()];
        if !info.used {
            self.emit_core_expr(fn_ir, body, dest, env)?;
        } else if info.is_local_jump() {
            env.block_targets[id.index()] = BlockEmitTarget::LocalJump {
                dest,
                mark: self.used,
                jumps: Vec::new(),
            };
            self.emit_core_expr(fn_ir, body, dest, env)?;
            let end_pc = self.code.instructions.len();
            if let BlockEmitTarget::LocalJump { ref jumps, .. } = env.block_targets[id.index()] {
                for &jpc in jumps {
                    self.code.instructions[jpc] = Ins::Jump(end_pc);
                }
            }
            env.block_targets[id.index()] = BlockEmitTarget::Elided;
        } else {
            let b_mark = self.mark_scope();
            let token = self.emit_exit_token(at);
            let token_reg = token.reg;
            let clear = self.used;
            let entry = self.emit(Ins::EnterBlock {
                token: token_reg,
                resume: 0,
                clear,
            });
            env.block_targets[id.index()] =
                BlockEmitTarget::Escaping(BindingLocation::Local(token.clone()));
            self.bindings.push(ScopedBinding {
                name: info.name.clone(),
                namespace: Namespace::Block,
                binding: Some(token),
                expander: None,
                used: None,
            });
            self.emit_core_expr(fn_ir, body, Destination::All, env)?;
            self.restore_scope(b_mark);
            env.block_targets[id.index()] = BlockEmitTarget::Elided;
            self.emit(Ins::LeaveTarget);
            self.code.instructions[entry] = Ins::EnterBlock {
                token: token_reg,
                resume: self.code.instructions.len(),
                clear,
            };
            self.receive_values(dest);
        }
        Ok(())
    }

    fn emit_core_return_from(
        &mut self,
        fn_ir: &CoreFn,
        env: &mut CoreCodegenBindings,
        id: BlockId,
        value: &CoreExpr,
        at: usize,
    ) -> Result<(), EvalError> {
        match env.block_targets[id.index()] {
            BlockEmitTarget::Elided => unreachable!("used block cannot be Elided"),
            BlockEmitTarget::LocalJump {
                dest: blk_dest,
                mark: blk_mark,
                ..
            } => {
                self.emit_core_expr(fn_ir, value, blk_dest, env)?;
                if self.used > blk_mark {
                    self.emit(Ins::Clear(blk_mark..self.used));
                }
                let jpc = self.emit(Ins::Jump(0));
                if let BlockEmitTarget::LocalJump { ref mut jumps, .. } =
                    env.block_targets[id.index()]
                {
                    jumps.push(jpc);
                }
            }
            BlockEmitTarget::Escaping(ref loc) => {
                let tok_loc = loc.clone();
                let tok_reg = self.reserve(1).start;
                self.read_location(tok_loc, tok_reg);
                self.emit_core_expr(fn_ir, value, Destination::All, env)?;
                self.emit(Ins::ReturnFrom { token: tok_reg, at });
            }
        }
        Ok(())
    }

    fn emit_core_tagbody(
        &mut self,
        fn_ir: &CoreFn,
        env: &mut CoreCodegenBindings,
        id: TagbodyId,
        segments: &[(Option<usize>, Vec<CoreExpr>)],
        at: usize,
    ) -> Result<(), EvalError> {
        let tb_info = &fn_ir.tagbodies[id.index()];
        if !tb_info.used || tb_info.tags.is_empty() {
            for (_, es) in segments {
                self.emit_core_statements(fn_ir, es, env)?;
            }
        } else if tb_info.is_local_jump() {
            env.tagbody_targets[id.index()] = TagbodyEmitTarget::LocalJump {
                mark: self.used,
                tag_pcs: vec![None; tb_info.tags.len()],
                pending_jumps: Vec::new(),
            };
            for (tag_opt, es) in segments {
                if let Some(tag_idx) = tag_opt {
                    let pc = self.code.instructions.len();
                    if let TagbodyEmitTarget::LocalJump {
                        ref mut tag_pcs, ..
                    } = env.tagbody_targets[id.index()]
                    {
                        tag_pcs[*tag_idx] = Some(pc);
                    }
                }
                self.emit_core_statements(fn_ir, es, env)?;
            }
            if let TagbodyEmitTarget::LocalJump {
                ref tag_pcs,
                ref pending_jumps,
                ..
            } = env.tagbody_targets[id.index()]
            {
                for &(jpc, tag_idx) in pending_jumps {
                    let target_pc = tag_pcs[tag_idx].expect("valid tag pc");
                    self.code.instructions[jpc] = Ins::Jump(target_pc);
                }
            }
            env.tagbody_targets[id.index()] = TagbodyEmitTarget::Unbound;
        } else {
            let t_mark = self.tags.len();
            let (table_index, token) = self.emit_tagbody_entry(tb_info.tags.len(), at);
            self.tags.push(TagScope {
                labels: Rc::clone(&tb_info.tags),
                token: token.clone(),
            });
            env.tagbody_targets[id.index()] =
                TagbodyEmitTarget::Escaping(BindingLocation::Local(token));
            for (tag_opt, es) in segments {
                if let Some(tag_idx) = tag_opt {
                    self.code.jump_tables[table_index].targets[*tag_idx] =
                        self.code.instructions.len();
                }
                self.emit_core_statements(fn_ir, es, env)?;
            }
            self.emit(Ins::LeaveTarget);
            self.tags.truncate(t_mark);
            env.tagbody_targets[id.index()] = TagbodyEmitTarget::Unbound;
        }
        Ok(())
    }

    fn emit_core_go(
        &mut self,
        env: &mut CoreCodegenBindings,
        id: TagbodyId,
        tag_index: usize,
        at: usize,
    ) {
        match &mut env.tagbody_targets[id.index()] {
            TagbodyEmitTarget::Unbound => unreachable!("active tagbody expected for GO"),
            TagbodyEmitTarget::LocalJump {
                mark: tb_mark,
                tag_pcs,
                pending_jumps,
            } => {
                if self.used > *tb_mark {
                    self.emit(Ins::Clear(*tb_mark..self.used));
                }
                if let Some(target_pc) = tag_pcs[tag_index] {
                    self.emit(Ins::Jump(target_pc));
                } else {
                    let jpc = self.emit(Ins::Jump(0));
                    pending_jumps.push((jpc, tag_index));
                }
            }
            TagbodyEmitTarget::Escaping(loc) => {
                let tok_loc = loc.clone();
                let tok_reg = self.reserve(1).start;
                self.read_location(tok_loc, tok_reg);
                self.emit(Ins::Go {
                    token: tok_reg,
                    label: tag_index,
                    at,
                });
            }
        }
    }

    fn emit_local_function_bindings(
        &mut self,
        fn_ir: &CoreFn,
        env: &mut CoreCodegenBindings,
        ids: &[LocalFunctionId],
        recursive: bool,
    ) -> Result<(), EvalError> {
        let mut bindings = Vec::with_capacity(ids.len());
        for &id in ids {
            let info = &fn_ir.local_functions[id.index()].binding;
            if recursive || info.call_count > 0 || info.escapes {
                let binding = RegisterBinding {
                    frame_depth: self.frame_depth,
                    reg: self.reserve(1).start,
                    boxed: Rc::new(Cell::new(recursive)),
                    capture_kind: if recursive {
                        CaptureKind::MutableCell
                    } else {
                        CaptureKind::Value
                    },
                };
                bindings.push((id, binding));
            }
        }

        if recursive {
            // LABELS closures capture cells for the whole group, including themselves.
            for (id, binding) in &bindings {
                let local = &fn_ir.local_functions[id.index()];
                self.emit(Ins::BoxLocal {
                    reg: binding.reg,
                    at: local.at,
                });
                self.install_core_local_function(local, env, *id, binding.clone());
            }
            for (id, binding) in bindings {
                let mark = self.used;
                let temp = self.reserve(1).start;
                self.emit_local_fn_closure(fn_ir, id, Destination::One(temp), env)?;
                self.write_location(BindingLocation::Local(binding), temp);
                self.release(mark);
            }
        } else {
            // FLET definitions see only the enclosing function bindings.
            for (id, binding) in &bindings {
                let mark = self.used;
                let reg = binding.reg;
                self.emit_local_fn_closure(fn_ir, *id, Destination::One(reg), env)?;
                self.release(mark);
            }
            for (id, binding) in bindings {
                let local = &fn_ir.local_functions[id.index()];
                self.install_core_local_function(local, env, id, binding);
            }
        }
        Ok(())
    }

    fn install_core_local_function(
        &mut self,
        local: &LocalFunction,
        env: &mut CoreCodegenBindings,
        id: LocalFunctionId,
        binding: RegisterBinding,
    ) {
        env.local_function_locations[id.index()] = Some(BindingLocation::Local(binding.clone()));
        self.bindings.push(ScopedBinding {
            name: local.binding.name.clone(),
            namespace: if local.binding.setf {
                Namespace::Setter
            } else {
                Namespace::Function
            },
            binding: Some(binding),
            expander: None,
            used: None,
        });
    }

    fn emit_core_statements(
        &mut self,
        fn_ir: &CoreFn,
        statements: &[CoreExpr],
        env: &mut CoreCodegenBindings,
    ) -> Result<(), EvalError> {
        for expr in statements {
            let mark = self.used;
            let scratch = self.reserve(1).start;
            self.emit_core_expr(fn_ir, expr, Destination::One(scratch), env)?;
            self.release(mark);
        }
        Ok(())
    }

    fn emit_core_let_bindings(
        &mut self,
        fn_ir: &CoreFn,
        env: &mut CoreCodegenBindings,
        sequential: bool,
        bindings: &[(CoreBindingTarget, CoreExpr)],
        at: usize,
    ) -> Result<usize, EvalError> {
        let regs = self.reserve(bindings.len());
        let mut dynamic = 0;
        for ((target, init), reg) in bindings.iter().zip(regs.clone()) {
            self.emit_core_expr(fn_ir, init, Destination::One(reg), env)?;
            if sequential {
                dynamic += self.bind_core_let_target(fn_ir, env, target, reg, at);
            }
        }
        if !sequential {
            // Parallel LET makes all bindings visible only after the last initform.
            for ((target, _), reg) in bindings.iter().zip(regs) {
                dynamic += self.bind_core_let_target(fn_ir, env, target, reg, at);
            }
        }
        Ok(dynamic)
    }

    fn bind_core_let_target(
        &mut self,
        fn_ir: &CoreFn,
        env: &mut CoreCodegenBindings,
        target: &CoreBindingTarget,
        reg: super::Register,
        at: usize,
    ) -> usize {
        match target {
            CoreBindingTarget::Lexical(vid) => {
                self.bind_core_lexical_var(fn_ir, env, *vid, reg, at);
                0
            }
            CoreBindingTarget::Special(name) => {
                self.bind_core_special(name, reg, at);
                1
            }
        }
    }

    fn emit_local_fn_closure(
        &mut self,
        parent_fn: &CoreFn,
        lf_id: LocalFunctionId,
        dest: Destination,
        env: &CoreCodegenBindings,
    ) -> Result<(), EvalError> {
        let lf = &parent_fn.local_functions[lf_id.index()];
        let (mut child, mut bindings) = self.core_child(parent_fn);

        let refs = BindingReferences::for_local_function(lf, parent_fn);

        // Core references use binding IDs. Raw forms resolve names through the
        // child compiler's lexical environment and capture them on demand.
        for (idx, binding) in env.var_locations.iter().enumerate() {
            let id = VarId(idx as u32);
            if let Some(binding) = binding
                && refs.vars.contains(&id)
                && !lf.params.all_params.contains(&id)
            {
                bindings.var_locations[idx] = Some(child.inherit_location(self, binding));
            }
        }
        for (idx, target) in env.block_targets.iter().enumerate() {
            let id = BlockId(idx as u32);
            if let BlockEmitTarget::Escaping(loc) = target
                && parent_fn.blocks[idx].used
                && refs.blocks.contains(&id)
            {
                bindings.block_targets[idx] =
                    BlockEmitTarget::Escaping(child.inherit_location(self, loc));
            }
        }
        for (idx, target) in env.tagbody_targets.iter().enumerate() {
            let id = TagbodyId(idx as u32);
            if let TagbodyEmitTarget::Escaping(loc) = target
                && parent_fn.tagbodies[idx].used
                && refs.tagbodies.contains(&id)
            {
                bindings.tagbody_targets[idx] =
                    TagbodyEmitTarget::Escaping(child.inherit_location(self, loc));
            }
        }
        for (idx, binding) in env.local_function_locations.iter().enumerate() {
            let id = LocalFunctionId(idx as u32);
            if let Some(binding) = binding
                && refs.local_functions.contains(&id)
            {
                bindings.local_function_locations[idx] =
                    Some(child.inherit_location(self, binding));
            }
        }
        child.emit_core_fn(parent_fn, Some(lf), &mut bindings)?;
        self.emit_closure_for_compiled_child(child, dest, lf.at);
        Ok(())
    }
}

impl CoreCodegenBindings {
    fn inherit_lambda(
        &mut self,
        func: &CoreFn,
        env: &CoreCodegenBindings,
        mut capture: impl FnMut(&BindingLocation) -> BindingLocation,
    ) {
        for &(child, parent) in &func.parent_var_captures {
            let binding = env.var_locations[parent.index()]
                .as_ref()
                .expect("outer variable binding");
            self.var_locations[child.index()] = Some(capture(binding));
        }
        for &(child, parent) in &func.parent_block_captures {
            if func.blocks[child.index()].used {
                let BlockEmitTarget::Escaping(loc) = &env.block_targets[parent.index()] else {
                    unreachable!("captured block has an exit token")
                };
                self.block_targets[child.index()] = BlockEmitTarget::Escaping(capture(loc));
            }
        }
        for &(child, parent) in &func.parent_tagbody_captures {
            if func.tagbodies[child.index()].used {
                let TagbodyEmitTarget::Escaping(loc) = &env.tagbody_targets[parent.index()] else {
                    unreachable!("captured tagbody has an exit token")
                };
                self.tagbody_targets[child.index()] = TagbodyEmitTarget::Escaping(capture(loc));
            }
        }
        for &(child, parent) in &func.parent_function_captures {
            let binding = env.local_function_locations[parent.index()]
                .as_ref()
                .expect("outer function binding");
            self.local_function_locations[child.index()] = Some(capture(binding));
        }
    }
}

#[derive(Default)]
struct BindingReferences {
    vars: HashSet<VarId>,
    blocks: HashSet<BlockId>,
    tagbodies: HashSet<TagbodyId>,
    local_functions: HashSet<LocalFunctionId>,
}

impl BindingReferences {
    fn for_local_function(local: &LocalFunction, owner: &CoreFn) -> Self {
        let mut refs = Self::default();
        for expr in local.params.initializers_and_body(&local.body) {
            let _ = expr.walk_in_function(Some(&owner.local_functions), &mut |node| {
                match &node.kind {
                    CoreKind::ReadVar(id) | CoreKind::WriteVar { var: id, .. } => {
                        refs.vars.insert(*id);
                    }
                    CoreKind::ReadLocalFn(id)
                    | CoreKind::Call {
                        callee: CoreCallee::Local(id),
                        ..
                    } => {
                        refs.local_functions.insert(*id);
                    }
                    CoreKind::ReturnFrom { id, .. } => {
                        refs.blocks.insert(*id);
                    }
                    CoreKind::Go { id, .. } => {
                        refs.tagbodies.insert(*id);
                    }
                    CoreKind::Lambda { func } => {
                        refs.vars
                            .extend(func.parent_var_captures.iter().map(|&(_, parent)| parent));
                        refs.blocks
                            .extend(func.parent_block_captures.iter().map(|&(_, parent)| parent));
                        refs.tagbodies.extend(
                            func.parent_tagbody_captures
                                .iter()
                                .map(|&(_, parent)| parent),
                        );
                        refs.local_functions.extend(
                            func.parent_function_captures
                                .iter()
                                .map(|&(_, parent)| parent),
                        );
                    }
                    _ => {}
                }
                ControlFlow::Continue(())
            });
        }
        refs
    }
}
