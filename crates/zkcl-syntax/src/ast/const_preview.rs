//! Bounded interpretation of a small set of standard Lisp forms.

use std::fmt::{self, Write};

use num_bigint::{BigInt, Sign};
use num_integer::Integer;
use num_rational::BigRational;
use rustc_hash::FxHashSet;

use super::{AstAtom, AstForm, AstList, AstLiteral, AstReaderForm, Element};
use crate::character_names::{character_name, simple_lowercase, simple_uppercase};
use crate::cl_builtins::{BuiltinKind, builtin_kind, is_external_cl_symbol};
use crate::reader_tokens::{
    NumberLiteralKind, Prefix, SymbolName, exact_number_token, number_literal_kind,
};
use crate::syntax::{SyntaxKind, SyntaxNode};

const MAX_SOURCE_BYTES: u32 = 16 * 1024;
const MAX_DEPTH: usize = 32;
const MAX_WORK: usize = 32 * 1024;
const MAX_NUMBER_BITS: u64 = 4096;
const MAX_RENDER_BYTES: usize = 16 * 1024;

/// Preview a single top-level list using the standard reader and native value representation.
/// Returns `None` when the value is unknown or exceeds the evaluation or rendering budget.
/// The returned string uses standard print settings and can be clipped by the display.
pub fn eval_const_preview(root: &SyntaxNode) -> Option<String> {
    eval_const_preview_with(
        root,
        &|atom, name| {
            atom.symbol()
                .is_some_and(|symbol| symbol.standard_operator() == Some(name))
        },
        PreviewIdentity::Native,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviewIdentity {
    Native,
    #[cfg_attr(not(any(feature = "analysis", test)), allow(dead_code))]
    Portable,
}

pub(crate) fn eval_const_preview_with(
    root: &SyntaxNode,
    is_standard: &impl Fn(&AstAtom, &str) -> bool,
    identity: PreviewIdentity,
) -> Option<String> {
    if u32::from(root.text_range().len()) > MAX_SOURCE_BYTES {
        return None;
    }
    let mut forms = root.children_with_tokens().filter_map(AstForm::cast);
    let AstForm::List(form) = forms.next()? else {
        return None;
    };
    if forms.next().is_some() {
        return None;
    }
    eval_const_form(&form, is_standard, identity)
}

/// Preview a source call site independently of the execution target.
#[cfg(feature = "analysis")]
pub(crate) fn eval_const_form_preview_with(
    form: &AstList,
    is_standard: &impl Fn(&AstAtom, &str) -> bool,
) -> Option<String> {
    eval_const_form(form, is_standard, PreviewIdentity::Portable)
}

fn eval_const_form(
    form: &AstList,
    is_standard: &impl Fn(&AstAtom, &str) -> bool,
    identity: PreviewIdentity,
) -> Option<String> {
    if u32::from(form.text_range().len()) > MAX_SOURCE_BYTES
        || !reader_is_supported(form, is_standard)
    {
        return None;
    }
    let mut preview = Preview {
        values: vec![Value::Nil, Value::True],
        bindings: Vec::new(),
        budget: WorkBudget(MAX_WORK),
        is_standard,
        identity,
    };
    let value = preview.eval_list(form, 0)?;
    preview.render(value)
}

fn reader_is_supported(form: &AstList, is_standard: &impl Fn(&AstAtom, &str) -> bool) -> bool {
    // Reading precedes evaluation even for an expression in a branch that is never taken.
    form.syntax()
        .descendants_with_tokens()
        .all(|element| match element {
            Element::Node(node) => match node.kind() {
                SyntaxKind::Error | SyntaxKind::Vector => false,
                SyntaxKind::List => readable_list(&AstList(node)),
                SyntaxKind::ReaderForm => {
                    let reader = AstReaderForm(node);
                    reader.prefix_kind() == Some(SyntaxKind::Quote)
                        && reader
                            .operands()
                            .next()
                            .is_some_and(|form| !matches!(form, AstForm::Dot(_)))
                }
                _ => true,
            },
            Element::Token(token) => match token.kind() {
                SyntaxKind::UnknownDispatch => false,
                SyntaxKind::Character => AstLiteral(token).character_value().is_some(),
                SyntaxKind::String | SyntaxKind::RawString => {
                    AstLiteral(token).string_value().is_some()
                }
                SyntaxKind::Atom => readable_atom(&AstAtom(token), is_standard),
                _ => true,
            },
        })
}

fn readable_list(list: &AstList) -> bool {
    if !list.is_closed() {
        return false;
    }
    let mut forms = list.forms();
    match forms.next() {
        None => return true,
        Some(AstForm::Dot(_)) => return false,
        Some(_) => {}
    }
    !forms.any(|form| matches!(form, AstForm::Dot(_)))
        || (forms
            .next()
            .is_some_and(|tail| !matches!(tail, AstForm::Dot(_)))
            && forms.next().is_none())
}

fn readable_atom(atom: &AstAtom, is_standard: &impl Fn(&AstAtom, &str) -> bool) -> bool {
    match number_literal_kind(atom.text()) {
        Some(NumberLiteralKind::Ratio) => atom
            .text()
            .rsplit_once('/')
            .is_some_and(|(_, denominator)| denominator.bytes().any(|b| b != b'0')),
        Some(NumberLiteralKind::Integer) => true,
        Some(NumberLiteralKind::Float) => false,
        None => atom.symbol().is_some_and(|symbol| {
            !matches!(symbol.prefix, Prefix::Package { .. })
                || (is_external_cl_symbol(&symbol.name) && is_standard(atom, &symbol.name))
        }),
    }
}

// An arena makes aliases cheap to copy and tracks the identity of each cons or string.
// Values can only point to earlier slots, so the graph is acyclic.
type ValueId = usize;
const NIL: ValueId = 0;
const TRUE: ValueId = 1;

#[derive(Clone, Copy)]
enum Origin {
    Literal,
    Constructed,
}

enum Value {
    Nil,
    True,
    Number(BigRational),
    String(String, Origin),
    Character(char),
    Symbol(SymbolName),
    Cons(ValueId, ValueId, Origin),
}

impl Value {
    fn as_number(&self) -> Option<&BigRational> {
        match self {
            Self::Number(number) => Some(number),
            _ => None,
        }
    }

    fn as_integer(&self) -> Option<&BigInt> {
        let number = self.as_number()?;
        number.is_integer().then_some(number.numer())
    }
}

struct WorkBudget(usize);

impl WorkBudget {
    fn spend(&mut self, work: usize) -> Option<()> {
        self.0 = self.0.checked_sub(work)?;
        Some(())
    }
}

struct Preview<'a> {
    values: Vec<Value>,
    bindings: Vec<(String, ValueId)>,
    budget: WorkBudget,
    is_standard: &'a dyn Fn(&AstAtom, &str) -> bool,
    identity: PreviewIdentity,
}

impl Preview<'_> {
    fn allocate(&mut self, value: Value) -> Option<ValueId> {
        self.budget.spend(1)?;
        let id = self.values.len();
        self.values.push(value);
        Some(id)
    }

    fn number(&mut self, number: BigRational) -> Option<ValueId> {
        check_number_size(&number)?;
        self.allocate(Value::Number(number))
    }

    fn integer(&mut self, integer: impl Into<BigInt>) -> Option<ValueId> {
        self.number(BigRational::from_integer(integer.into()))
    }

    fn standard_symbol(&self, atom: &AstAtom, symbol: &SymbolName) -> bool {
        // The reader preflight has checked every package-qualified symbol.
        matches!(symbol.prefix, Prefix::Package { .. }) || (self.is_standard)(atom, &symbol.name)
    }

    fn constant_symbol(&self, atom: &AstAtom, symbol: &SymbolName) -> Option<ValueId> {
        let name = symbol.name.as_str();
        let value = match name {
            "NIL" => NIL,
            "T" => TRUE,
            _ => return None,
        };
        self.standard_symbol(atom, symbol).then_some(value)
    }

    fn eval_atom(&mut self, atom: &AstAtom, quoted: bool) -> Option<ValueId> {
        self.budget.spend(atom.text().len())?;
        if let Some((numerator, denominator)) = exact_number_token(atom.text()) {
            if let Some(denominator) = denominator {
                let bits = numerator.bits().max(denominator.bits());
                if bits > MAX_NUMBER_BITS {
                    return None;
                }
                self.budget.spend(limb_count(bits).pow(2))?;
                return self.number(BigRational::new(numerator, denominator));
            }
            return self.integer(numerator);
        }
        let symbol = atom.symbol()?;
        if let Some(value) = self.constant_symbol(atom, &symbol) {
            return Some(value);
        }
        match symbol.prefix {
            Prefix::Keyword => self.allocate(Value::Symbol(symbol)),
            Prefix::None | Prefix::Uninterned if quoted => self.allocate(Value::Symbol(symbol)),
            Prefix::None => {
                self.budget.spend(self.bindings.len())?;
                self.bindings
                    .iter()
                    .rev()
                    .find_map(|(name, value)| (name == &symbol.name).then_some(*value))
            }
            _ => None,
        }
    }

    fn eval_literal(&mut self, form: &AstLiteral) -> Option<ValueId> {
        self.budget.spend(form.text().len())?;
        let value = if let Some(character) = form.character_value() {
            Value::Character(character)
        } else {
            Value::String(form.string_value()?, Origin::Literal)
        };
        self.allocate(value)
    }

    fn eval(&mut self, form: &AstForm, depth: usize) -> Option<ValueId> {
        self.budget.spend(1)?;
        if depth > MAX_DEPTH {
            return None;
        }
        match form {
            AstForm::Atom(atom) => self.eval_atom(atom, false),
            AstForm::Literal(literal) => self.eval_literal(literal),
            AstForm::ReaderForm(reader) => self.quote(&reader.operands().next()?, depth + 1),
            AstForm::List(list) => self.eval_list(list, depth),
            _ => None,
        }
    }

    fn eval_list(&mut self, list: &AstList, depth: usize) -> Option<ValueId> {
        if list.is_dotted() {
            return None;
        }
        let mut forms = list.forms();
        let Some(head) = forms.next() else {
            return Some(NIL);
        };
        let head = head.as_atom()?;
        let symbol = head.symbol()?;
        let op = symbol.name.as_str();
        if !self.standard_symbol(head, &symbol) {
            return None;
        }
        let args: Vec<_> = forms.collect();
        self.budget.spend(args.len())?;
        let depth = depth + 1;
        match (op, args.as_slice()) {
            ("QUOTE", [object]) => self.quote(object, depth),
            ("PROGN", body) => self.eval_body(body, depth),
            ("IF", [test, then, otherwise @ ..]) if otherwise.len() <= 1 => {
                if self.eval(test, depth)? != NIL {
                    self.eval(then, depth)
                } else {
                    self.eval_body(otherwise, depth)
                }
            }
            ("WHEN" | "UNLESS", [test, body @ ..]) => {
                if (self.eval(test, depth)? != NIL) == (op == "WHEN") {
                    self.eval_body(body, depth)
                } else {
                    Some(NIL)
                }
            }
            ("AND" | "OR", forms) => {
                let and = op == "AND";
                let mut value = if and { TRUE } else { NIL };
                for form in forms {
                    value = self.eval(form, depth)?;
                    if (value != NIL) != and {
                        break;
                    }
                }
                Some(value)
            }
            ("LET" | "LET*", [bindings, body @ ..]) => {
                let start = self.bindings.len();
                let result = self.eval_let(bindings, body, depth, op == "LET*");
                self.bindings.truncate(start);
                result
            }
            _ => {
                let values = args
                    .iter()
                    .map(|form| self.eval(form, depth))
                    .collect::<Option<Vec<_>>>()?;
                self.call(op, &values)
            }
        }
    }

    fn eval_body(&mut self, body: &[AstForm], depth: usize) -> Option<ValueId> {
        let mut value = NIL;
        for form in body {
            value = self.eval(form, depth)?;
        }
        Some(value)
    }

    fn eval_let(
        &mut self,
        bindings: &AstForm,
        body: &[AstForm],
        depth: usize,
        sequential: bool,
    ) -> Option<ValueId> {
        if let AstForm::Atom(atom) = bindings
            && self.constant_symbol(atom, &atom.symbol()?) == Some(NIL)
        {
            return self.eval_body(body, depth);
        }
        let bindings = bindings.as_list()?;
        if bindings.is_dotted() {
            return None;
        }
        let mut pending = Vec::new();
        let mut seen = FxHashSet::default();
        for spec in bindings.forms() {
            self.budget.spend(1)?;
            let (var, init) = match spec {
                AstForm::Atom(atom) => (atom, None),
                AstForm::List(pair) if !pair.is_dotted() => {
                    let mut pair = pair.forms();
                    let AstForm::Atom(atom) = pair.next()? else {
                        return None;
                    };
                    let init = pair.next();
                    if pair.next().is_some() {
                        return None;
                    }
                    (atom, init)
                }
                _ => return None,
            };
            let symbol = var.symbol()?;
            if symbol.prefix != Prefix::None
                || (builtin_kind(&symbol.name) == Some(BuiltinKind::Constant)
                    && self.standard_symbol(&var, &symbol))
            {
                return None;
            }
            let value = match init {
                Some(init) => self.eval(&init, depth)?,
                None => NIL,
            };
            if sequential {
                self.bindings.push((symbol.name, value));
            } else {
                if !seen.insert(symbol.name.clone()) {
                    return None;
                }
                pending.push((symbol.name, value));
            }
        }
        self.bindings.extend(pending);
        self.eval_body(body, depth)
    }

    fn quote(&mut self, form: &AstForm, depth: usize) -> Option<ValueId> {
        self.budget.spend(1)?;
        if depth > MAX_DEPTH {
            return None;
        }
        match form {
            AstForm::Atom(atom) => self.eval_atom(atom, true),
            AstForm::Literal(literal) => self.eval_literal(literal),
            AstForm::List(list) => {
                let mut forms = list.forms();
                let mut items = Vec::new();
                let tail = loop {
                    match forms.next() {
                        None => break NIL,
                        Some(AstForm::Dot(_)) => break self.quote(&forms.next()?, depth + 1)?,
                        Some(item) => items.push(self.quote(&item, depth + 1)?),
                    }
                };
                self.prepend_items(items.into_iter().rev(), tail, Origin::Literal)
            }
            _ => None,
        }
    }

    fn cons_fields(&self, id: ValueId) -> Option<(ValueId, ValueId)> {
        match self.values[id] {
            Value::Cons(car, cdr, _) => Some((car, cdr)),
            Value::Nil => Some((NIL, NIL)),
            _ => None,
        }
    }

    fn list_items(&mut self, mut id: ValueId) -> Option<Vec<ValueId>> {
        let mut items = Vec::new();
        while id != NIL {
            self.budget.spend(1)?;
            let (car, cdr) = self.cons_fields(id)?;
            items.push(car);
            id = cdr;
        }
        Some(items)
    }

    fn prepend_items(
        &mut self,
        reversed_items: impl IntoIterator<Item = ValueId>,
        mut tail: ValueId,
        origin: Origin,
    ) -> Option<ValueId> {
        for item in reversed_items {
            tail = self.allocate(Value::Cons(item, tail, origin))?;
        }
        Some(tail)
    }

    fn call(&mut self, op: &str, args: &[ValueId]) -> Option<ValueId> {
        match (op, args) {
            ("+" | "-" | "*" | "/", _) => self.arithmetic(op, args),
            ("1+" | "1-", &[arg]) => {
                let number = self.values[arg].as_number()?;
                let step = BigInt::from(if op == "1+" { 1 } else { -1 });
                self.number(number + step)
            }
            ("ABS", &[arg]) => {
                let number = self.values[arg].as_number()?;
                self.number(if number.numer().sign() == Sign::Minus {
                    -number
                } else {
                    number.clone()
                })
            }
            ("MOD" | "REM", &[number, divisor]) => self.remainder(op, number, divisor),
            ("ASH", &[integer, count]) => self.shift(integer, count),
            ("LOGAND" | "LOGIOR" | "LOGXOR", _) => self.bitwise(op, args),
            ("=" | "/=" | "<" | "<=" | ">" | ">=" | "MIN" | "MAX", _) => self.compare(op, args),
            ("ZEROP" | "PLUSP" | "MINUSP", &[arg]) => {
                let sign = self.values[arg].as_number()?.numer().sign();
                Some(bool_value(match op {
                    "PLUSP" => sign == Sign::Plus,
                    "MINUSP" => sign == Sign::Minus,
                    _ => sign == Sign::NoSign,
                }))
            }
            ("EVENP" | "ODDP", &[arg]) => Some(bool_value(
                self.values[arg].as_integer()?.is_even() == (op == "EVENP"),
            )),
            ("NULL" | "NOT", &[arg]) => Some(bool_value(arg == NIL)),
            ("ATOM", &[arg]) => Some(bool_value(!matches!(self.values[arg], Value::Cons(..)))),
            ("CONSP", &[arg]) => Some(bool_value(matches!(self.values[arg], Value::Cons(..)))),
            ("LISTP", &[arg]) => Some(bool_value(matches!(
                self.values[arg],
                Value::Nil | Value::Cons(..)
            ))),
            ("NUMBERP" | "RATIONALP" | "REALP", &[arg]) => {
                Some(bool_value(self.values[arg].as_number().is_some()))
            }
            ("INTEGERP", &[arg]) => Some(bool_value(self.values[arg].as_integer().is_some())),
            ("STRINGP", &[arg]) => Some(bool_value(matches!(self.values[arg], Value::String(..)))),
            ("SYMBOLP", &[arg]) => Some(bool_value(matches!(
                self.values[arg],
                Value::Symbol(_) | Value::Nil | Value::True
            ))),
            ("KEYWORDP", &[arg]) => Some(bool_value(
                matches!(&self.values[arg], Value::Symbol(s) if s.prefix == Prefix::Keyword),
            )),
            ("EQ" | "EQL", &[left, right]) => {
                self.compare_identity(op, left, right).map(bool_value)
            }
            ("EQUAL", &[left, right]) => self.equal(left, right).map(bool_value),
            ("LIST", items) => {
                self.prepend_items(items.iter().rev().copied(), NIL, Origin::Constructed)
            }
            ("CONS", &[head, tail]) => self.allocate(Value::Cons(head, tail, Origin::Constructed)),
            ("CAR" | "FIRST", &[arg]) => Some(self.cons_fields(arg)?.0),
            ("CDR" | "REST", &[arg]) => Some(self.cons_fields(arg)?.1),
            ("SECOND", &[arg]) => Some(self.cons_fields(self.cons_fields(arg)?.1)?.0),
            ("THIRD", &[arg]) => {
                let tail = self.cons_fields(arg)?.1;
                Some(self.cons_fields(self.cons_fields(tail)?.1)?.0)
            }
            ("APPEND", []) => Some(NIL),
            ("APPEND", [lists @ .., tail]) => {
                let mut result = *tail;
                for &list in lists.iter().rev() {
                    let items = self.list_items(list)?;
                    result =
                        self.prepend_items(items.into_iter().rev(), result, Origin::Constructed)?;
                }
                Some(result)
            }
            ("LENGTH" | "REVERSE" | "STRING-UPCASE" | "STRING-DOWNCASE", &[arg]) => {
                self.sequence(op, arg)
            }
            ("SUBSEQ", &[sequence, start]) => self.subseq(sequence, start, NIL),
            ("SUBSEQ", &[sequence, start, end]) => self.subseq(sequence, start, end),
            _ => None,
        }
    }

    fn arithmetic(&mut self, op: &str, args: &[ValueId]) -> Option<ValueId> {
        let (mut result, rest) = match op {
            "+" => (BigRational::from_integer(BigInt::ZERO), args),
            "*" => (BigRational::from_integer(BigInt::from(1)), args),
            _ => {
                let (&first, rest) = args.split_first()?;
                let number = self.values[first].as_number()?;
                if rest.is_empty() {
                    return if op == "-" {
                        self.number(-number)
                    } else if *number.numer() != BigInt::ZERO {
                        self.number(number.recip())
                    } else {
                        None
                    };
                }
                (number.clone(), rest)
            }
        };
        for &arg in rest {
            let number = self.values[arg].as_number()?;
            self.budget.spend(number_work(&result, number))?;
            result = match op {
                "+" => result + number,
                "*" => result * number,
                "-" => result - number,
                "/" if *number.numer() != BigInt::ZERO => result / number,
                _ => return None,
            };
            check_number_size(&result)?;
        }
        self.number(result)
    }

    fn remainder(&mut self, op: &str, number: ValueId, divisor: ValueId) -> Option<ValueId> {
        let number = self.values[number].as_number()?;
        let divisor = self.values[divisor].as_number()?;
        if *divisor.numer() == BigInt::ZERO {
            return None;
        }
        self.budget.spend(number_work(number, divisor))?;
        if number.is_integer() && divisor.is_integer() {
            let result = if op == "MOD" {
                number.numer().mod_floor(divisor.numer())
            } else {
                number.numer() % divisor.numer()
            };
            return self.integer(result);
        }
        let quotient = number / divisor;
        let quotient = if op == "MOD" {
            quotient.numer().div_floor(quotient.denom())
        } else {
            quotient.numer() / quotient.denom()
        };
        self.number(number - divisor * quotient)
    }

    fn shift(&mut self, integer: ValueId, count: ValueId) -> Option<ValueId> {
        let number = self.values[integer].as_integer()?;
        let count = self.values[count].as_integer()?;
        if *number == BigInt::ZERO {
            return Some(integer);
        }
        let magnitude = u64::try_from(count.magnitude()).ok();
        if count.sign() == Sign::Minus {
            self.budget.spend(limb_count(number.bits()))?;
            let result = match magnitude {
                Some(shift) if shift < number.bits() => number >> shift,
                _ => BigInt::from(if number.sign() == Sign::Minus { -1 } else { 0 }),
            };
            return self.integer(result);
        }
        let shift = magnitude?;
        if shift > MAX_NUMBER_BITS - number.bits() {
            return None;
        }
        self.budget.spend(limb_count(shift + number.bits()))?;
        self.integer(number << shift)
    }

    fn bitwise(&mut self, op: &str, args: &[ValueId]) -> Option<ValueId> {
        let mut result = BigInt::from(if op == "LOGAND" { -1 } else { 0 });
        for &arg in args {
            let number = self.values[arg].as_integer()?;
            self.budget
                .spend(limb_count(result.bits().max(number.bits())))?;
            match op {
                "LOGAND" => result &= number,
                "LOGIOR" => result |= number,
                _ => result ^= number,
            }
            if result.bits() > MAX_NUMBER_BITS {
                return None;
            }
        }
        self.integer(result)
    }

    fn compare(&mut self, op: &str, args: &[ValueId]) -> Option<ValueId> {
        let (&first, rest) = args.split_first()?;
        if op == "/=" {
            let mut distinct = FxHashSet::default();
            for &arg in args {
                let arg = self.values[arg].as_number()?;
                self.budget.spend(limb_count(number_bits(arg)))?;
                distinct.insert((arg.numer(), arg.denom()));
            }
            return Some(bool_value(distinct.len() == args.len()));
        }
        let mut left = self.values[first].as_number()?;
        let mut chosen = first;
        let min = op == "MIN";
        let max = op == "MAX";
        let mut comparisons_hold = true;
        for &right_id in rest {
            let right = self.values[right_id].as_number()?;
            self.budget.spend(number_work(left, right))?;
            let order = left.cmp(right);
            if min || max {
                if (min && order.is_gt()) || (max && order.is_lt()) {
                    left = right;
                    chosen = right_id;
                }
            } else {
                comparisons_hold &= match op {
                    "=" => order.is_eq(),
                    "<" => order.is_lt(),
                    "<=" => !order.is_gt(),
                    ">" => order.is_gt(),
                    _ => !order.is_lt(),
                };
                left = right;
            }
        }
        Some(if min || max {
            chosen
        } else {
            bool_value(comparisons_hold)
        })
    }

    fn compare_identity(&mut self, op: &str, left: ValueId, right: ValueId) -> Option<bool> {
        if left == right {
            return Some(true);
        }
        let portable = self.identity == PreviewIdentity::Portable;
        match (&self.values[left], &self.values[right]) {
            (Value::Number(number), Value::Number(_)) if op == "EQ" => {
                if !self.eql(left, right) {
                    return Some(false);
                }
                // The signed fixnum range of a 32-bit Lisp word fits all supported targets.
                // JavaScript compares integer values even outside that range.
                if portable {
                    immediate_integer(number, 32).then_some(true)
                } else {
                    Some(immediate_integer(number, usize::BITS as u8))
                }
            }
            (Value::Character(left), Value::Character(right)) if op == "EQ" && portable => {
                (left != right).then_some(false)
            }
            (Value::String(_, Origin::Literal), Value::String(_, Origin::Literal))
            | (Value::Cons(_, _, Origin::Literal), Value::Cons(_, _, Origin::Literal))
                if portable =>
            {
                // A compiler may coalesce similar literal objects.
                (!self.equal(left, right)?).then_some(false)
            }
            _ => Some(self.eql(left, right)),
        }
    }

    fn eql(&self, left: ValueId, right: ValueId) -> bool {
        if left == right {
            return true;
        }
        match (&self.values[left], &self.values[right]) {
            (Value::Character(left), Value::Character(right)) => left == right,
            (Value::Symbol(left), Value::Symbol(right)) => {
                left.prefix != Prefix::Uninterned && left == right
            }
            (Value::Number(left), Value::Number(right)) => {
                left.numer() == right.numer() && left.denom() == right.denom()
            }
            _ => false,
        }
    }

    fn equal(&mut self, left: ValueId, right: ValueId) -> Option<bool> {
        let mut pending = vec![(left, right)];
        let mut seen = FxHashSet::default();
        while let Some((left, right)) = pending.pop() {
            self.budget.spend(1)?;
            if self.eql(left, right) {
                continue;
            }
            match (&self.values[left], &self.values[right]) {
                (Value::String(left, _), Value::String(right, _)) => {
                    self.budget.spend(left.len().min(right.len()))?;
                    if left != right {
                        return Some(false);
                    }
                }
                (Value::Cons(lcar, lcdr, _), Value::Cons(rcar, rcdr, _)) => {
                    if seen.insert((left, right)) {
                        pending.push((*lcdr, *rcdr));
                        pending.push((*lcar, *rcar));
                    }
                }
                _ => return Some(false),
            }
        }
        Some(true)
    }

    fn sequence(&mut self, op: &str, arg: ValueId) -> Option<ValueId> {
        if let Value::String(text, _) = &self.values[arg] {
            self.budget.spend(text.len())?;
            let text = match op {
                "LENGTH" => return self.integer(text.chars().count()),
                "REVERSE" => text.chars().rev().collect(),
                "STRING-UPCASE" => text.chars().map(simple_uppercase).collect(),
                _ => text.chars().map(simple_lowercase).collect(),
            };
            return self.allocate(Value::String(text, Origin::Constructed));
        }
        match op {
            "LENGTH" => {
                let items = self.list_items(arg)?;
                self.integer(items.len())
            }
            "REVERSE" => {
                let items = self.list_items(arg)?;
                self.prepend_items(items, NIL, Origin::Constructed)
            }
            _ => None,
        }
    }

    fn subseq(&mut self, sequence: ValueId, start: ValueId, end: ValueId) -> Option<ValueId> {
        let start = usize::try_from(self.values[start].as_integer()?).ok()?;
        let end = if end == NIL {
            None
        } else {
            Some(usize::try_from(self.values[end].as_integer()?).ok()?)
        };
        if let Value::String(text, _) = &self.values[sequence] {
            self.budget.spend(text.len())?;
            let offset = |index| {
                text.char_indices()
                    .map(|(byte, _)| byte)
                    .chain(std::iter::once(text.len()))
                    .nth(index)
            };
            let start = offset(start)?;
            let end = match end {
                Some(end) => offset(end)?,
                None => text.len(),
            };
            return self.allocate(Value::String(
                text.get(start..end)?.to_owned(),
                Origin::Constructed,
            ));
        }
        let items = self.list_items(sequence)?;
        let items = items.get(start..end.unwrap_or(items.len()))?;
        self.prepend_items(items.iter().rev().copied(), NIL, Origin::Constructed)
    }

    fn render(&self, root: ValueId) -> Option<String> {
        let mut text = BoundedText(String::new());
        let mut pending = vec![Render::Value(root)];
        while let Some(part) = pending.pop() {
            match part {
                Render::Value(id) => match &self.values[id] {
                    Value::Nil => text.write_str("NIL").ok()?,
                    Value::True => text.write_str("T").ok()?,
                    Value::Number(number) => write!(text, "{number}").ok()?,
                    Value::String(value, _) => write_escaped(&mut text, value, '"').ok()?,
                    Value::Character(character) => {
                        text.write_str("#\\").ok()?;
                        if character.is_control() {
                            text.write_str(&character_name(*character)).ok()?;
                        } else {
                            text.write_char(*character).ok()?;
                        }
                    }
                    Value::Symbol(symbol) => render_symbol(&mut text, symbol).ok()?,
                    Value::Cons(car, cdr, _) => {
                        text.write_char('(').ok()?;
                        pending.push(Render::Tail(*cdr));
                        pending.push(Render::Value(*car));
                    }
                },
                Render::Tail(id) => match &self.values[id] {
                    Value::Nil => text.write_char(')').ok()?,
                    Value::Cons(car, cdr, _) => {
                        text.write_char(' ').ok()?;
                        pending.push(Render::Tail(*cdr));
                        pending.push(Render::Value(*car));
                    }
                    _ => {
                        text.write_str(" . ").ok()?;
                        pending.push(Render::Tail(NIL));
                        pending.push(Render::Value(id));
                    }
                },
            }
        }
        Some(text.0)
    }
}

fn bool_value(value: bool) -> ValueId {
    if value { TRUE } else { NIL }
}

fn immediate_integer(number: &BigRational, word_bits: u8) -> bool {
    number.is_integer()
        && i64::try_from(number.numer()).ok().is_some_and(|value| {
            zkcl_target::fixnum_bounds_for_bits(word_bits)
                .is_some_and(|(min, max)| (min..=max).contains(&value))
        })
}

fn number_bits(number: &BigRational) -> u64 {
    number.numer().bits().max(number.denom().bits())
}

fn check_number_size(number: &BigRational) -> Option<()> {
    (number_bits(number) <= MAX_NUMBER_BITS).then_some(())
}

fn limb_count(bits: u64) -> usize {
    bits.div_ceil(64).max(1) as usize
}

fn number_work(left: &BigRational, right: &BigRational) -> usize {
    let limbs = limb_count(number_bits(left).max(number_bits(right)));
    limbs * limbs
}

enum Render {
    Value(ValueId),
    Tail(ValueId),
}

struct BoundedText(String);

impl Write for BoundedText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if text.len() > MAX_RENDER_BYTES - self.0.len() {
            return Err(fmt::Error);
        }
        self.0.push_str(text);
        Ok(())
    }
}

fn write_escaped(text: &mut BoundedText, value: &str, delimiter: char) -> fmt::Result {
    text.write_char(delimiter)?;
    for ch in value.chars() {
        if ch == delimiter
            || ch == '\\'
            || (delimiter == '|'
                && ch.is_ascii_control()
                && !matches!(ch, '\t' | '\n' | '\r' | '\x0c'))
        {
            text.write_char('\\')?;
        }
        text.write_char(ch)?;
    }
    text.write_char(delimiter)
}

fn render_symbol(text: &mut BoundedText, symbol: &SymbolName) -> fmt::Result {
    match symbol.prefix {
        Prefix::Keyword => text.write_char(':')?,
        Prefix::Uninterned => text.write_str("#:")?,
        _ => {}
    }
    let name = &symbol.name;
    if SymbolName::parse(name).is_some_and(|s| s.prefix == Prefix::None && &s.name == name) {
        text.write_str(name)
    } else {
        write_escaped(text, name, '|')
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::parse_source;

    fn preview(source: &str) -> Option<String> {
        eval_const_preview(&parse_source(source).root())
    }

    #[test]
    fn arithmetic_uses_exact_numbers_and_common_lisp_remainders() {
        for (source, value) in [
            ("(+ -9223372036854775810 3)", "-9223372036854775807"),
            ("(+ 9223372036854775807 1)", "9223372036854775808"),
            ("(ash 4611686018427387904 1)", "9223372036854775808"),
            ("(ash -3 -9000000000000000000000)", "-1"),
            ("(ash 0 9000000000000000000000)", "0"),
            ("(mod 5 -3)", "-1"),
            ("(mod -9223372036854775808 -1)", "0"),
            ("(rem -5 3)", "-2"),
            ("(rem -5/2 3/2)", "-1"),
            ("(mod 5/2 -3/2)", "-1/2"),
            ("(+ #xF #2r-10 #o17 #16rA #b10 1.)", "41"),
            ("(/ #16rA/3 2)", "5/3"),
            (
                "(list (= 3/2 6/4) (integerp 2/2) (/= 3 1/2 2/4))",
                "(T T NIL)",
            ),
            ("(logxor -1 #xFFFFFFFFFFFFFFFF)", "-18446744073709551616"),
        ] {
            assert_eq!(preview(source).as_deref(), Some(value), "{source}");
        }
    }

    #[test]
    fn identity_and_structural_equality_have_distinct_semantics() {
        for (source, value) in [
            ("(eq (list 1) (list 1))", "NIL"),
            ("(eql (list 1) (list 1))", "NIL"),
            ("(equal (list 1) (list 1))", "T"),
            (
                r#"(list (eq "x" "x") (eql "x" "x") (equal "x" "x"))"#,
                "(NIL NIL T)",
            ),
            (
                "(let ((x (list 1))) (list (eq x x) (eq (cdr (cons 0 x)) x) (eq (append nil x) x)))",
                "(T T T)",
            ),
            ("(let ((x (list 1))) (eq (subseq x 0 nil) x))", "NIL"),
            (
                "(list (eq 100 100) (eq 9223372036854775808 9223372036854775808) (eql 9223372036854775808 9223372036854775808))",
                "(T NIL T)",
            ),
            (
                "(let ((x 5/2)) (list (eq x x) (eq x (max x 5/2)) (eq x 5/2) (eql x 10/4)))",
                "(T T NIL T)",
            ),
            (
                r"(list (eq '#:x '#:x) (eq :x :x) (eql #\U+03BB #\λ) (eq nil cl:nil))",
                "(NIL T T T)",
            ),
            ("(equal '(a . b) (cons 'a 'b))", "T"),
        ] {
            assert_eq!(preview(source).as_deref(), Some(value), "{source}");
        }
    }

    #[test]
    fn multiline_preview_preserves_long_unicode_and_printer_escaping() {
        let text = "漢字🙂".repeat(20);
        let source = format!("\n;; a comment\n (string-upcase\n \"{text}\")\n");
        assert_eq!(preview(&source), Some(format!("\"{text}\"")));
        assert_eq!(
            preview(r#"(list (string-upcase "ßéıςﬀ") (subseq "前甲乙后" 1 3) "a\"\\b" '|a b|)"#)
                .as_deref(),
            Some(r#"("ßÉıςﬀ" "甲乙" "a\"\\b" |a b|)"#),
        );
    }

    #[test]
    fn preview_checks_reader_forms_bindings_and_arithmetic_without_running_lisp() {
        for source in [
            "(print 1)",
            "(if t 1 #.(print 2))",
            "(if t 1 #+unknown 2)",
            "(if t 1 1/0)",
            "(if t 1 no-such-pkg:missing)",
            "(if t 1 cl:no-such-export)",
            "(if t 1 '(x . y z))",
            "(if t 1 ')",
            r"(if t 1 #\No-such-character)",
            "(let ((t 1)) t)",
            "(let ((nil 1)) nil)",
            "(let ((pi 1)) pi)",
            "(let ((boole-xor 1)) boole-xor)",
            "(let ((x 1) (x 2)) x)",
            "(let ((x 1) (y x)) y)",
            "(/ 1 0)",
            "(mod 1 0)",
            "(ash 1 9000000000000000000000)",
            "(subseq \"abc\" 2 1)",
            "(+ 1 2.0)",
            "(+ 1",
            "(+ 1 2) (+ 1 2)",
        ] {
            assert_eq!(preview(source), None, "{source}");
        }
        assert_eq!(preview("(if t 1 (print 2))").as_deref(), Some("1"));
        assert_eq!(
            preview("(let* ((x 1) (x (+ x 2))) x)").as_deref(),
            Some("3")
        );
        assert_eq!(preview("(let ())").as_deref(), Some("NIL"));

        let root = parse_source("(list (alias:+ 1 2) alias:nil)").root();
        assert_eq!(eval_const_preview(&root), None);
        let verified_alias = |atom: &AstAtom, name: &str| {
            matches!(
                (atom.text(), name),
                ("list", "LIST") | ("alias:+", "+") | ("alias:nil", "NIL")
            )
        };
        assert_eq!(
            eval_const_preview_with(&root, &verified_alias, PreviewIdentity::Native).as_deref(),
            Some("(3 NIL)")
        );
        assert_eq!(
            eval_const_preview_with(&root, &|_, name| name != "+", PreviewIdentity::Native),
            None
        );
    }

    #[test]
    fn portable_preview_declines_identity_when_targets_can_disagree() {
        let portable = |source| {
            eval_const_preview_with(
                &parse_source(source).root(),
                &|_, _| true,
                PreviewIdentity::Portable,
            )
        };
        for source in [
            "(eq 9223372036854775808 9223372036854775808)",
            "(eq 1/2 2/4)",
            r"(eq #\λ #\λ)",
            r#"(eql "foo" "foo")"#,
            "(eq '(1 2) '(1 2))",
        ] {
            assert_eq!(portable(source), None, "{source}");
        }
        for (source, value) in [
            ("(eq 100 100)", "T"),
            ("(eql 9223372036854775808 9223372036854775808)", "T"),
            (r"(eql #\λ #\λ)", "T"),
            ("(let ((x '(1 2))) (eq x x))", "T"),
            (r#"(eql "foo" "bar")"#, "NIL"),
            ("(eq (list 1 2) (list 1 2))", "NIL"),
        ] {
            assert_eq!(portable(source).as_deref(), Some(value), "{source}");
        }
    }

    #[test]
    fn repeated_aliases_share_values_and_respect_work_and_output_budgets() {
        let sharing = " (x (list x x x x))".repeat(16);
        let result = |body| preview(&format!("(let* ((x (list 1)){sharing}) {body})"));
        assert_eq!(result("(length x)").as_deref(), Some("4"));
        assert_eq!(result("x"), None);

        let copying = " (x (append x x))".repeat(24);
        assert_eq!(
            preview(&format!("(let* ((x (list 1)){copying}) (length x))")),
            None
        );
    }
}
