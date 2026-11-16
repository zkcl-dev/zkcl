//! Type membership on the two successors of a conditional.

use super::{
    CoreCallee, CoreConst, CoreExpr, CoreKind, CoreType, InlineDeclaration, IntegerRange, VarId,
};

#[derive(Clone, Copy)]
pub(super) struct TypeTest {
    pub ty: CoreType,
    pub negated: bool,
    pub range: Option<IntegerRange>,
}

impl TypeTest {
    fn member(ty: CoreType) -> Self {
        Self {
            ty,
            negated: false,
            range: None,
        }
    }

    pub fn unary(op: &str) -> Option<Self> {
        use CoreType::*;
        let ty = match op {
            "NULL" | "NOT" => Null,
            "CONSP" | "ATOM" => Cons,
            "LISTP" => List,
            "SYMBOLP" => Symbol,
            "KEYWORDP" => Keyword,
            "NUMBERP" => Number,
            "REALP" => Real,
            "RATIONALP" => Rational,
            "INTEGERP" => Integer,
            "FLOATP" => Float,
            "COMPLEXP" => Complex,
            "CHARACTERP" => Character,
            "STRINGP" => String,
            "VECTORP" => Vector,
            "ARRAYP" => Array,
            "FUNCTIONP" | "COMPILED-FUNCTION-P" => Function,
            _ => return None,
        };
        Some(Self {
            negated: op == "ATOM",
            ..Self::member(ty)
        })
    }

    pub fn call<'a>(
        op: &str,
        args: &'a [CoreExpr],
        fixnums: IntegerRange,
    ) -> Option<(&'a CoreExpr, Self)> {
        match (op, args) {
            ("TYPEP", [arg, spec]) => {
                let test = match &spec.kind {
                    CoreKind::Const(CoreConst::True) => Self::member(CoreType::Top),
                    CoreKind::Const(CoreConst::Nil) => Self::member(CoreType::Empty),
                    CoreKind::Const(CoreConst::Symbol(name)) if name.operator() == "BIT" => Self {
                        range: Some(IntegerRange::BIT),
                        ..Self::member(CoreType::Fixnum)
                    },
                    CoreKind::Const(CoreConst::Symbol(name)) => {
                        let (ty, exact) = CoreType::from_atomic_name(name.operator())?;
                        if !exact {
                            return None;
                        }
                        Self::member(ty)
                    }
                    CoreKind::Const(CoreConst::LiteralForm(form)) => Self {
                        range: Some(fixnums.spec(form)?),
                        ..Self::member(CoreType::Fixnum)
                    },
                    _ => return None,
                };
                Some((arg, test))
            }
            (_, [arg]) => Some((arg, Self::unary(op)?)),
            _ => None,
        }
    }

    pub fn evaluate(self, upper: CoreType) -> Option<bool> {
        if upper == CoreType::Empty {
            return None;
        }
        if self.range.is_none() && upper.is_subtype(self.ty) {
            Some(!self.negated)
        } else if upper.is_disjoint(self.ty) {
            Some(self.negated)
        } else {
            None
        }
    }
}

// The lexical binding keeps its identity, but UNINTERN can change a symbol's home package.
pub(super) fn stable_value_type(ty: CoreType) -> CoreType {
    if ty == CoreType::Keyword {
        CoreType::NonNilSymbol
    } else {
        ty
    }
}

pub(super) fn branch_type_test(
    mut expr: &CoreExpr,
    fixnums: IntegerRange,
) -> Option<(VarId, TypeTest)> {
    let mut inverted = false;
    loop {
        match &expr.kind {
            CoreKind::ReadVar(var) => {
                return Some((
                    *var,
                    TypeTest {
                        negated: !inverted,
                        ..TypeTest::member(CoreType::Null)
                    },
                ));
            }
            CoreKind::Call {
                callee: CoreCallee::Named(name),
                args,
                inline_declaration: None | Some(InlineDeclaration::Inline),
                ..
            } => {
                let op = name.operator();
                if let ("NOT" | "NULL", [arg]) = (op, args.as_slice()) {
                    expr = arg;
                    inverted = !inverted;
                    continue;
                }
                let (arg, mut test) = TypeTest::call(op, args, fixnums)?;
                let CoreKind::ReadVar(var) = arg.kind else {
                    return None;
                };
                test.negated ^= inverted;
                return Some((var, test));
            }
            _ => return None,
        }
    }
}

impl CoreExpr {
    pub(crate) fn inline_integer_test(
        &self,
        fixnums: IntegerRange,
    ) -> Option<(&Self, IntegerRange)> {
        let CoreKind::Call {
            callee: CoreCallee::Named(name),
            args,
            inline_declaration,
            qualities,
            ..
        } = &self.kind
        else {
            return None;
        };
        if name.operator() != "TYPEP"
            || *inline_declaration == Some(InlineDeclaration::NotInline)
            || (*inline_declaration != Some(InlineDeclaration::Inline) && !qualities.favors_speed())
        {
            return None;
        }
        let (arg, test) = TypeTest::call("TYPEP", args, fixnums)?;
        (test.ty == CoreType::Fixnum).then_some((arg, test.range.unwrap_or(fixnums)))
    }
}
