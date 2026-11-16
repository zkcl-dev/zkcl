use wasm_encoder::{BlockType, Function, ValType};
use zkcl_bytecode::native_abi::{FixnumComparison, LEAF_ARGUMENTS, LEAF_MISS, LeafGuard};
use zkcl_bytecode::{
    BytecodeFunction, FIXNUM_SHIFT, FixnumArithmetic, LiteralNode, TAG_FIXNUM, TAG_MASK, WORD_NIL,
    WORD_T, fixnum_bounds_for_bits,
};
use zkcl_vop::SelectionError;
use zkcl_vop::leaf::{BitwiseOp, Block, Exit, Operand, Operation, Step, Test};

mod loops;

const ARGUMENTS: u32 = LEAF_ARGUMENTS as u32;

pub struct LoweredLeaf {
    pub body: Function,
    pub guards: Box<[LeafGuard]>,
    /// An entry with bounded loops may need a faster long-running fallback.
    pub has_loop: bool,
}

/// Emit a closed scalar leaf. A failed check returns `LEAF_MISS`.
pub fn compile_leaf(
    bytecode: &BytecodeFunction,
    literals: &[LiteralNode],
) -> Result<LoweredLeaf, SelectionError> {
    let leaf = zkcl_vop::select_compiled(bytecode, literals, 32)?;
    let locals = leaf.locals() as u32;
    let blocks = leaf.blocks();
    let checked_arithmetic = blocks.iter().flat_map(|block| &block.steps).any(|step| {
        matches!(
            step,
            Step::Compute {
                op: Operation::Arithmetic { checked: true, .. },
                ..
            }
        )
    });
    let loop_budget = leaf.loop_budget();
    let flow = loop_budget.map(|_| super::cfg::for_leaf(blocks));
    let has_dispatch = flow
        .as_ref()
        .is_some_and(|flow| flow.dispatch.iter().any(Option::is_some));
    let budget = loop_budget.map(|_| ARGUMENTS + locals);
    let dispatch = has_dispatch.then(|| ARGUMENTS + locals + 1);
    let extra_locals = u32::from(budget.is_some()) + u32::from(has_dispatch);
    let wide = ARGUMENTS + locals + extra_locals;
    let mut function = Function::new([
        (locals + extra_locals, ValType::I32),
        (u32::from(checked_arithmetic), ValType::I64),
    ]);
    if let Some((flow, (budget, count))) = flow.as_ref().zip(budget.zip(loop_budget)) {
        function
            .instructions()
            .i32_const(count as i32)
            .local_set(budget);
        loops::emit(&mut function, blocks, flow, wide, budget, dispatch);
    } else {
        emit_body(&mut function, blocks, wide);
    }
    function.instructions().end();
    Ok(LoweredLeaf {
        body: function,
        guards: leaf.guards().into(),
        has_loop: leaf.loop_budget().is_some(),
    })
}

fn emit_body(function: &mut Function, blocks: &[Block], wide: u32) {
    let mut is_target = vec![false; blocks.len()];
    for (index, block) in blocks.iter().enumerate() {
        block
            .exit
            .visit_targets(|target| is_target[target] |= target != index + 1);
    }
    // Each forward target begins just after its enclosing block. Open the
    // blocks in reverse layout order and use fallthrough for adjacent edges.
    let mut labels = vec![0; blocks.len()];
    let mut depth = 0u32;
    for (target, &is_target) in is_target.iter().enumerate().rev() {
        if is_target {
            labels[target] = depth;
            function.instructions().block(BlockType::Empty);
            depth += 1;
        }
    }
    for (index, block) in blocks.iter().enumerate() {
        if is_target[index] {
            function.instructions().end();
            depth -= 1;
        }
        emit_steps(function, &block.steps, wide, None);
        let next = index + 1;
        let label = |target: usize| depth - 1 - labels[target];
        match block.exit {
            Exit::Return(value) => {
                emit_operand(function, value);
                if next != blocks.len() {
                    function.instructions().return_();
                }
            }
            Exit::Miss => {
                function.instructions().i32_const(LEAF_MISS as i32);
                if next != blocks.len() {
                    function.instructions().return_();
                }
            }
            Exit::Jump(target) => {
                if target != next {
                    function.instructions().br(label(target));
                }
            }
            Exit::Branch {
                value,
                test,
                yes,
                no,
            } => {
                emit_test(function, value, test);
                if yes == next {
                    function.instructions().i32_eqz().br_if(label(no));
                } else {
                    function.instructions().br_if(label(yes));
                    if no != next {
                        function.instructions().br(label(no));
                    }
                }
            }
        }
    }
}

fn emit_steps(function: &mut Function, steps: &[Step], wide: u32, budget: Option<u32>) {
    for &step in steps {
        match step {
            Step::CheckFixnum(value) => {
                emit_fixnum_test(function, value);
                function.instructions().i32_eqz();
                emit_miss(function);
            }
            Step::CheckLoopBudget => {
                let local = budget.expect("backedges have a loop counter");
                function
                    .instructions()
                    .local_get(local)
                    .i32_const(1)
                    .i32_sub()
                    .local_tee(local)
                    .i32_const(0)
                    .i32_lt_s();
                emit_miss(function);
            }
            Step::Copy { destination, value } => {
                emit_operand(function, value);
                function
                    .instructions()
                    .local_set(ARGUMENTS + destination as u32);
            }
            Step::Compute { destination, op } => {
                emit_operation(function, op, wide);
                function
                    .instructions()
                    .local_set(ARGUMENTS + destination as u32);
            }
        }
    }
}

fn emit_test(function: &mut Function, value: Operand, test: Test) {
    match test {
        Test::Nil => {
            emit_operand(function, value);
            function.instructions().i32_const(WORD_NIL as i32).i32_eq();
        }
        Test::Fixnum => emit_fixnum_test(function, value),
        Test::FixnumInRange(low, high) => {
            emit_fixnum_test(function, value);
            emit_unboxed(function, value);
            function
                .instructions()
                .i32_const(low)
                .i32_sub()
                .i32_const((i64::from(high) - i64::from(low)) as i32)
                .i32_le_u()
                .i32_and();
        }
    }
}

fn emit_operation(function: &mut Function, op: Operation, wide: u32) {
    use FixnumArithmetic::{Add, Dec, Inc, Mul, Sub};
    match op {
        Operation::Complement(value) => {
            emit_operand(function, value);
            function
                .instructions()
                .i32_const(!(TAG_MASK as i32))
                .i32_xor();
        }
        Operation::Bitwise { op, lhs, rhs } => {
            emit_operand(function, lhs);
            emit_operand(function, rhs);
            let mut instructions = function.instructions();
            match op {
                BitwiseOp::And => instructions.i32_and(),
                BitwiseOp::Or => instructions.i32_or(),
                BitwiseOp::Xor => instructions.i32_xor().i32_const(TAG_FIXNUM as i32).i32_or(),
            };
        }
        Operation::Compare { relation, lhs, rhs } => {
            function
                .instructions()
                .i32_const(WORD_T as i32)
                .i32_const(WORD_NIL as i32);
            emit_operand(function, lhs);
            emit_operand(function, rhs);
            let mut instructions = function.instructions();
            match relation {
                FixnumComparison::Eq => instructions.i32_eq(),
                FixnumComparison::Ne => instructions.i32_ne(),
                FixnumComparison::Lt => instructions.i32_lt_s(),
                FixnumComparison::Le => instructions.i32_le_s(),
                FixnumComparison::Gt => instructions.i32_gt_s(),
                FixnumComparison::Ge => instructions.i32_ge_s(),
            }
            .select();
        }
        Operation::Arithmetic {
            op,
            lhs,
            rhs,
            checked,
        } => {
            emit_unboxed(function, lhs);
            if checked {
                function.instructions().i64_extend_i32_s();
            }
            if let Some(rhs) = rhs {
                emit_unboxed(function, rhs);
                if checked {
                    function.instructions().i64_extend_i32_s();
                }
            } else if checked {
                function.instructions().i64_const(1);
            } else {
                function.instructions().i32_const(1);
            }
            let mut instructions = function.instructions();
            match (op, checked) {
                (Add | Inc, false) => instructions.i32_add(),
                (Sub | Dec, false) => instructions.i32_sub(),
                (Mul, false) => instructions.i32_mul(),
                (Add | Inc, true) => instructions.i64_add(),
                (Sub | Dec, true) => instructions.i64_sub(),
                (Mul, true) => instructions.i64_mul(),
            };
            if checked {
                let (low, high) = fixnum_bounds_for_bits(32).unwrap();
                instructions
                    .local_tee(wide)
                    .i64_const(low)
                    .i64_sub()
                    .i64_const(high - low)
                    .i64_gt_u();
                emit_miss(function);
                function.instructions().local_get(wide).i32_wrap_i64();
            }
            function
                .instructions()
                .i32_const(FIXNUM_SHIFT as i32)
                .i32_shl()
                .i32_const(TAG_FIXNUM as i32)
                .i32_or();
        }
    }
}

fn emit_miss(function: &mut Function) {
    function
        .instructions()
        .if_(BlockType::Empty)
        .i32_const(LEAF_MISS as i32)
        .return_()
        .end();
}

fn emit_fixnum_test(function: &mut Function, operand: Operand) {
    emit_operand(function, operand);
    function
        .instructions()
        .i32_const(TAG_MASK as i32)
        .i32_and()
        .i32_const(TAG_FIXNUM as i32)
        .i32_eq();
}

fn emit_unboxed(function: &mut Function, operand: Operand) {
    emit_operand(function, operand);
    function
        .instructions()
        .i32_const(FIXNUM_SHIFT as i32)
        .i32_shr_s();
}

fn emit_operand(function: &mut Function, operand: Operand) {
    let mut instructions = function.instructions();
    match operand {
        Operand::Arg(arg) => instructions.local_get(u32::from(arg)),
        Operand::Local(index) => instructions.local_get(ARGUMENTS + index as u32),
        Operand::Immediate(bits) => instructions.i32_const(bits as i32),
    };
}
