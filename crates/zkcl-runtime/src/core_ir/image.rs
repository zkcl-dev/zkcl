//! Typed image representation of already resolved inline definitions.
//!
//! Restoring this IR does not re-read source, expand a macro, or evaluate a
//! load-time value. Every Lisp reference joins the image's shared object graph.

use super::*;
use crate::EvalError;
use crate::image::{ImageReader, ImageWriter, invalid};

struct Encoder<'writer, 'runtime> {
    image: &'writer mut ImageWriter<'runtime>,
    labels: FxHashMap<usize, usize>,
}
impl<'runtime> std::ops::Deref for Encoder<'_, 'runtime> {
    type Target = ImageWriter<'runtime>;
    fn deref(&self) -> &Self::Target {
        self.image
    }
}
impl std::ops::DerefMut for Encoder<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.image
    }
}
struct Decoder<'reader, 'runtime> {
    image: &'reader mut ImageReader<'runtime>,
    labels: Vec<TagLabels>,
}
impl<'runtime> std::ops::Deref for Decoder<'_, 'runtime> {
    type Target = ImageReader<'runtime>;
    fn deref(&self) -> &Self::Target {
        self.image
    }
}
impl std::ops::DerefMut for Decoder<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.image
    }
}

trait Codec: Sized {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError>;
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError>;
}

impl Codec for usize {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        image.usize(*self);
        Ok(())
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        image.usize()
    }
}
impl Codec for bool {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        image.bool(*self);
        Ok(())
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        image.bool()
    }
}
impl Codec for String {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        image.string(self);
        Ok(())
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        image.string()
    }
}
macro_rules! unsigned_codec {
    ($($ty:ty),* $(,)?) => {$ (
        impl Codec for $ty {
            fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
                image.usize(*self as usize); Ok(())
            }
            fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
                Self::try_from(image.usize()?).map_err(|_| invalid())
            }
        }
    )*};
}
unsigned_codec!(u8, u32);
impl Codec for i64 {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        image.u64(*self as u64);
        Ok(())
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        Ok(image.u64()? as i64)
    }
}
impl Codec for isize {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        (*self as i64).write(image)
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        Self::try_from(i64::read(image)?).map_err(|_| invalid())
    }
}
impl Codec for BigInt {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        image.string(&self.to_string());
        Ok(())
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        image.string()?.parse().map_err(|_| invalid())
    }
}
impl Codec for char {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        image.usize(*self as usize);
        Ok(())
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        u32::try_from(image.usize()?)
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(invalid)
    }
}
impl Codec for Name {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        image.name(self)
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        image.name()
    }
}
impl Codec for NameKey {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        image.key(self);
        Ok(())
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        image.key()
    }
}
impl Codec for Form {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        image.form(self)
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        image.form()
    }
}
impl Codec for RootSlot {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        image.word(self.get())
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        Ok(image.runtime.root_slot(image.word()?))
    }
}
impl<T: Codec> Codec for Vec<T> {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        image.usize(self.len());
        for value in self {
            value.write(image)?;
        }
        Ok(())
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        let count = image.usize()?;
        let mut values = Vec::new();
        values.try_reserve(count).map_err(|_| invalid())?;
        for _ in 0..count {
            values.push(T::read(image)?);
        }
        Ok(values)
    }
}
impl<T: Codec> Codec for Option<T> {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        image.bool(self.is_some());
        if let Some(value) = self {
            value.write(image)?;
        }
        Ok(())
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        if image.bool()? {
            Ok(Some(T::read(image)?))
        } else {
            Ok(None)
        }
    }
}
impl<T: Codec> Codec for Box<T> {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        (**self).write(image)
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        Ok(Box::new(T::read(image)?))
    }
}
macro_rules! rc_value_codec {
    ($($ty:ty),* $(,)?) => {$ (
        impl Codec for Rc<$ty> {
            fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> { (**self).write(image) }
            fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> { Ok(Rc::new(<$ty as Codec>::read(image)?)) }
        }
    )*};
}
rc_value_codec!(Form, RootSlot);
impl Codec for TagLabels {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        let pointer = Rc::as_ptr(self) as usize;
        if let Some(index) = image.labels.get(&pointer).copied() {
            image.usize(index);
            image.bool(false);
        } else {
            let index = image.labels.len();
            image.labels.insert(pointer, index);
            image.usize(index);
            image.bool(true);
            (**self).write(image)?;
        }
        Ok(())
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        let index = image.usize()?;
        if image.bool()? {
            if index != image.labels.len() {
                return Err(invalid());
            }
            let labels = Rc::new(FxHashMap::<Tag, usize>::read(image)?);
            image.labels.push(Rc::clone(&labels));
            Ok(labels)
        } else {
            image.labels.get(index).cloned().ok_or_else(invalid)
        }
    }
}
impl<A: Codec, B: Codec> Codec for (A, B) {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        self.0.write(image)?;
        self.1.write(image)
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        Ok((A::read(image)?, B::read(image)?))
    }
}
impl<A: Codec, B: Codec, C: Codec> Codec for (A, B, C) {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        self.0.write(image)?;
        self.1.write(image)?;
        self.2.write(image)
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        Ok((A::read(image)?, B::read(image)?, C::read(image)?))
    }
}
impl Codec for FxHashMap<Tag, usize> {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        image.usize(self.len());
        for (tag, label) in self {
            tag.write(image)?;
            label.write(image)?;
        }
        Ok(())
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        let count = image.usize()?;
        let mut labels = Self::default();
        labels.try_reserve(count).map_err(|_| invalid())?;
        for _ in 0..count {
            if labels
                .insert(Tag::read(image)?, usize::read(image)?)
                .is_some()
            {
                return Err(invalid());
            }
        }
        Ok(labels)
    }
}
impl Codec for IntegerRange {
    fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
        let (low, high) = self.bounds();
        low.write(image)?;
        high.write(image)
    }
    fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
        Self::from_bounds(i64::read(image)?, i64::read(image)?).ok_or_else(invalid)
    }
}
macro_rules! id_codec {
    ($($ty:ident),* $(,)?) => {$ (
        impl Codec for $ty {
            fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> { self.0.write(image) }
            fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> { Ok(Self(u32::read(image)?)) }
        }
    )*};
}
id_codec!(VarId, LocalFunctionId, BlockId, TagbodyId);
macro_rules! struct_codec {
    ($ty:ty { $($field:ident),* $(,)? }) => {
        impl Codec for $ty {
            fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
                $(self.$field.write(image)?;)* Ok(())
            }
            fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
                Ok(Self { $($field: Codec::read(image)?,)* })
            }
        }
    };
}
macro_rules! enum_codec {
    ($ty:ty { $( $tag:literal => $variant:ident $( ( $($tuple:ident : $tuple_ty:ty),* ) )? $( { $($field:ident : $field_ty:ty),* } )?, )* }) => {
        impl Codec for $ty {
            fn write(&self, image: &mut Encoder<'_, '_>) -> Result<(), EvalError> {
                match self {
                    $(Self::$variant $(($($tuple),*))? $({$($field),*})? => {
                        image.usize($tag);
                        $($($tuple.write(image)?;)*)?
                        $($($field.write(image)?;)*)?
                    },)*
                }
                Ok(())
            }
            fn read(image: &mut Decoder<'_, '_>) -> Result<Self, EvalError> {
                Ok(match image.usize()? {
                    $($tag => Self::$variant $(($(<$tuple_ty as Codec>::read(image)?),*))? $({$($field: <$field_ty as Codec>::read(image)?),*})?,)*
                    _ => return Err(invalid()),
                })
            }
        }
    };
}

struct_codec!(OptimizationQualities {
    speed,
    safety,
    debug,
    space,
    compilation_speed
});
struct_codec!(VariableBinding {
    name,
    special,
    assigned,
    captured,
    read_count,
    trusted_type,
    dynamic_extent
});
struct_codec!(BlockExitTarget {
    name,
    used,
    escapes,
    crosses_dynamic_scope
});
struct_codec!(TagbodyTargets {
    tags,
    used,
    escapes,
    crosses_dynamic_scope
});
struct_codec!(LocalFunctionBinding {
    name,
    setf,
    call_count,
    escapes,
    recursive,
    has_notinline_call,
    dynamic_extent
});
struct_codec!(CoreDefaultedParameter {
    var,
    init,
    supplied
});
struct_codec!(CoreKeywordParameter { key, param });
struct_codec!(CoreKeywordParameters {
    params,
    allow_other_keys
});
struct_codec!(CoreParameterBindings {
    required_lexicals_only,
    all_params,
    required,
    optional,
    rest,
    keywords,
    aux
});
struct_codec!(LocalFunction {
    binding,
    params,
    source_lambda_list,
    source_expression,
    body,
    at
});
struct_codec!(CoreExpr { kind, at });
struct_codec!(CoreFn {
    fixnums,
    name,
    params,
    source_lambda_list,
    source_expression,
    body,
    vars,
    blocks,
    tagbodies,
    local_functions,
    parent_var_captures,
    parent_block_captures,
    parent_tagbody_captures,
    parent_function_captures,
    external_var_captures,
    external_block_captures,
    external_tagbody_captures,
    external_function_captures,
    at,
    debug,
});
enum_codec!(InlineDeclaration { 0 => Inline, 1 => NotInline, });
enum_codec!(zkcl_bytecode::FixnumArithmetic { 0 => Add, 1 => Sub, 2 => Mul, 3 => Inc, 4 => Dec, });
enum_codec!(CoreType {
    0 => Top, 1 => Empty, 2 => Null, 3 => True, 4 => Boolean, 5 => NonNull,
    6 => Fixnum, 7 => Bignum, 8 => Integer, 9 => Ratio, 10 => Rational,
    11 => Float, 12 => Real, 13 => Complex, 14 => Number, 15 => Character,
    16 => Symbol, 17 => NonNilSymbol, 18 => Keyword, 19 => Cons, 20 => List,
    21 => String, 22 => Vector, 23 => Array, 24 => Function, 25 => HashTable,
    26 => Package, 27 => Pathname, 28 => Readtable, 29 => RandomState,
});
enum_codec!(Tag { 0 => Symbol(key: NameKey), 1 => Integer(value: isize), 2 => Bignum(value: BigInt), });
enum_codec!(CoreConst {
    0 => Nil, 1 => True, 2 => Fixnum(value: i64), 3 => Char(value: char),
    4 => String(value: String), 5 => Symbol(name: Name), 6 => LiteralForm(form: Form),
});
enum_codec!(CoreBindingTarget { 0 => Lexical(id: VarId), 1 => Special(name: Name), });
enum_codec!(CoreCallee { 0 => Named(name: Name), 1 => Local(id: LocalFunctionId), 2 => Expr(expr: Box<CoreExpr>), });
enum_codec!(CoreKind {
    0 => Const(value: CoreConst),
    1 => ReadVar(id: VarId),
    2 => ReadGlobal(name: Name),
    3 => ReadGlobalFn { name: Name, setf: bool },
    4 => ReadLocalFn(id: LocalFunctionId),
    5 => WriteVar { var: VarId, value: Box<CoreExpr> },
    6 => WriteGlobal { name: Name, value: Box<CoreExpr> },
    7 => Progn(forms: Vec<CoreExpr>),
    8 => If { test: Box<CoreExpr>, then_: Box<CoreExpr>, else_: Box<CoreExpr> },
    9 => Let { sequential: bool, bindings: Vec<(CoreBindingTarget, CoreExpr)>, body: Box<CoreExpr> },
    10 => Flet { recursive: bool, fns: Vec<LocalFunctionId>, body: Box<CoreExpr> },
    11 => Block { id: BlockId, body: Box<CoreExpr> },
    12 => ReturnFrom { id: BlockId, value: Box<CoreExpr> },
    13 => Tagbody { id: TagbodyId, segments: Vec<(Option<usize>, Vec<CoreExpr>)> },
    14 => Go { id: TagbodyId, tag_index: usize },
    15 => Call { callee: CoreCallee, args: Vec<CoreExpr>, qualities: OptimizationQualities, inline_declaration: Option<InlineDeclaration>, ftype_ret: CoreType, fixnum_arithmetic: Option<zkcl_bytecode::FixnumArithmetic> },
    16 => CallValues { callee: Box<CoreExpr>, forms: Vec<CoreExpr> },
    17 => Lambda { func: Box<CoreFn> },
    18 => LoadTimeValue { cell: Rc<RootSlot> },
    19 => The { type_spec: Form, expr: Box<CoreExpr>, safety: u8 },
    20 => RawSpecial(form: Form, bindings: Vec<RawLexicalBinding>),
});
enum_codec!(RawLexicalBinding {
    0 => Declaration(name: Name, value: Rc<RootSlot>),
    1 => Optimize(qualities: OptimizationQualities),
    2 => VariableType { name: Name, spec: Form, is_free: bool },
    3 => FunctionType { name: Name, setf: bool, spec: Form },
    4 => Special(name: Name),
    5 => Inline { name: Name, setf: bool, declaration: InlineDeclaration },
    6 => Macro { name: Name, expander: Rc<RootSlot>, symbol_macro: bool },
});

impl CoreFn {
    pub(crate) fn write_image(&self, image: &mut ImageWriter<'_>) -> Result<(), EvalError> {
        self.write(&mut Encoder {
            image,
            labels: FxHashMap::default(),
        })
    }
    pub(crate) fn read_image(image: &mut ImageReader<'_>) -> Result<Self, EvalError> {
        let domain = IntegerRange::for_target(image.runtime.compilation_target());
        let function = Self::read(&mut Decoder {
            image,
            labels: Vec::new(),
        })?;
        function.validate_image(None, domain)?;
        Ok(function)
    }
}

fn index(index: usize, count: usize) -> Result<(), EvalError> {
    if index < count {
        Ok(())
    } else {
        Err(invalid())
    }
}
fn qualities(value: OptimizationQualities) -> Result<(), EvalError> {
    if [
        value.speed,
        value.safety,
        value.debug,
        value.space,
        value.compilation_speed,
    ]
    .into_iter()
    .all(|v| v <= 3)
    {
        Ok(())
    } else {
        Err(invalid())
    }
}
impl CoreFn {
    fn validate_image(
        &self,
        parent: Option<&CoreFn>,
        domain: IntegerRange,
    ) -> Result<(), EvalError> {
        if self.fixnums != domain || self.debug > 3 {
            return Err(invalid());
        }
        let var = |id: VarId| index(id.index(), self.vars.len());
        let block = |id: BlockId| index(id.index(), self.blocks.len());
        let tagbody = |id: TagbodyId| index(id.index(), self.tagbodies.len());
        let local = |id: LocalFunctionId| index(id.index(), self.local_functions.len());
        self.validate_params(&self.params, domain)?;
        for function in &self.local_functions {
            self.validate_params(&function.params, domain)?;
            self.validate_expr(&function.body, domain)?;
        }
        for (child, outer) in &self.parent_var_captures {
            var(*child)?;
            index(outer.index(), parent.ok_or_else(invalid)?.vars.len())?;
        }
        for (child, outer) in &self.parent_block_captures {
            block(*child)?;
            index(outer.index(), parent.ok_or_else(invalid)?.blocks.len())?;
        }
        for (child, outer) in &self.parent_tagbody_captures {
            tagbody(*child)?;
            let parent = parent.ok_or_else(invalid)?;
            index(outer.index(), parent.tagbodies.len())?;
            if !Rc::ptr_eq(
                &self.tagbodies[child.index()].tags,
                &parent.tagbodies[outer.index()].tags,
            ) {
                return Err(invalid());
            }
        }
        for (child, outer) in &self.parent_function_captures {
            local(*child)?;
            index(
                outer.index(),
                parent.ok_or_else(invalid)?.local_functions.len(),
            )?;
        }
        for (id, _) in &self.external_var_captures {
            var(*id)?;
        }
        for (id, _) in &self.external_block_captures {
            block(*id)?;
        }
        for id in &self.external_tagbody_captures {
            tagbody(*id)?;
        }
        for (id, _, _) in &self.external_function_captures {
            local(*id)?;
        }
        for targets in &self.tagbodies {
            // Label numbers are dense even when several tags share a segment.
            if targets
                .tags
                .values()
                .any(|&label| label >= targets.tags.len())
            {
                return Err(invalid());
            }
        }
        self.validate_expr(&self.body, domain)
    }
    fn validate_params(
        &self,
        params: &CoreParameterBindings,
        domain: IntegerRange,
    ) -> Result<(), EvalError> {
        let var = |id: VarId| index(id.index(), self.vars.len());
        for id in &params.all_params {
            var(*id)?;
        }
        for id in &params.required {
            var(*id)?;
        }
        let defaulted = |parameter: &CoreDefaultedParameter| -> Result<(), EvalError> {
            var(parameter.var)?;
            if let Some(supplied) = parameter.supplied {
                var(supplied)?;
            }
            if let Some(init) = &parameter.init {
                self.validate_expr(init, domain)?;
            }
            Ok(())
        };
        for parameter in &params.optional {
            defaulted(parameter)?;
        }
        if let Some(id) = params.rest {
            var(id)?;
        }
        if let Some(keywords) = &params.keywords {
            for keyword in &keywords.params {
                defaulted(&keyword.param)?;
            }
        }
        for (id, init) in &params.aux {
            var(*id)?;
            if let Some(init) = init {
                self.validate_expr(init, domain)?;
            }
        }
        if params.required_lexicals_only
            && (!params.optional.is_empty()
                || params.rest.is_some()
                || params.keywords.is_some()
                || !params.aux.is_empty()
                || params
                    .required
                    .iter()
                    .any(|id| self.vars[id.index()].special))
        {
            return Err(invalid());
        }
        Ok(())
    }
    fn validate_expr(&self, expression: &CoreExpr, domain: IntegerRange) -> Result<(), EvalError> {
        let var = |id: VarId| index(id.index(), self.vars.len());
        let block = |id: BlockId| index(id.index(), self.blocks.len());
        let tagbody = |id: TagbodyId| index(id.index(), self.tagbodies.len());
        let local = |id: LocalFunctionId| index(id.index(), self.local_functions.len());
        let child = |expression: &CoreExpr| self.validate_expr(expression, domain);
        match &expression.kind {
            CoreKind::Const(_)
            | CoreKind::ReadGlobal(_)
            | CoreKind::ReadGlobalFn { .. }
            | CoreKind::LoadTimeValue { .. } => {}
            CoreKind::ReadVar(id) => var(*id)?,
            CoreKind::ReadLocalFn(id) => local(*id)?,
            CoreKind::WriteVar { var: id, value } => {
                var(*id)?;
                child(value)?;
            }
            CoreKind::WriteGlobal { value, .. } => child(value)?,
            CoreKind::Progn(forms) => {
                for form in forms {
                    child(form)?;
                }
            }
            CoreKind::If { test, then_, else_ } => {
                child(test)?;
                child(then_)?;
                child(else_)?;
            }
            CoreKind::Let { bindings, body, .. } => {
                for (binding, init) in bindings {
                    if let CoreBindingTarget::Lexical(id) = binding {
                        var(*id)?;
                    }
                    child(init)?;
                }
                child(body)?;
            }
            CoreKind::Flet { fns, body, .. } => {
                for id in fns {
                    local(*id)?;
                }
                child(body)?;
            }
            CoreKind::Block { id, body } => {
                block(*id)?;
                child(body)?;
            }
            CoreKind::ReturnFrom { id, value } => {
                block(*id)?;
                child(value)?;
            }
            CoreKind::Tagbody { id, segments } => {
                tagbody(*id)?;
                for (label, forms) in segments {
                    if let Some(label) = label {
                        if !self.tagbodies[id.index()]
                            .tags
                            .values()
                            .any(|value| value == label)
                        {
                            return Err(invalid());
                        }
                    }
                    for form in forms {
                        child(form)?;
                    }
                }
            }
            CoreKind::Go { id, tag_index } => {
                tagbody(*id)?;
                if !self.tagbodies[id.index()]
                    .tags
                    .values()
                    .any(|label| label == tag_index)
                {
                    return Err(invalid());
                }
            }
            CoreKind::Call {
                callee,
                args,
                qualities: policy,
                ..
            } => {
                qualities(*policy)?;
                match callee {
                    CoreCallee::Named(_) => {}
                    CoreCallee::Local(id) => local(*id)?,
                    CoreCallee::Expr(expression) => child(expression)?,
                }
                for argument in args {
                    child(argument)?;
                }
            }
            CoreKind::CallValues { callee, forms } => {
                child(callee)?;
                for form in forms {
                    child(form)?;
                }
            }
            CoreKind::Lambda { func } => func.validate_image(Some(self), domain)?,
            CoreKind::The { expr, safety, .. } => {
                if *safety > 3 {
                    return Err(invalid());
                }
                child(expr)?;
            }
            CoreKind::RawSpecial(_, bindings) => {
                for binding in bindings {
                    if let RawLexicalBinding::Optimize(policy) = binding {
                        qualities(*policy)?;
                    }
                }
            }
        }
        Ok(())
    }
}
