use zkcl_bytecode::{Arity, BytecodeFunction, Immediate, Instruction};
use zkcl_opt::native::native_frame_layout;
use zkcl_opt::{NoSpecializationInputs, SsaOp, lift_code, optimize};

fn code(instructions: Vec<Instruction>) -> BytecodeFunction {
    BytecodeFunction {
        name: None,
        source_file: None,
        debug: Default::default(),
        arity: Arity {
            required: 1,
            maximum: Some(1),
        },
        registers: 1,
        instructions,
        constants: vec![0],
        lambda_list: None,
        captures: vec![],
        notinline: vec![],
        vop_calls: Vec::new(),
        handlers: vec![],
        restarts: vec![],
        keywords: vec![],
        jump_tables: vec![],
        facts: Default::default(),
    }
}

#[test]
fn backedge_poll_reports_the_control_transfer_after_instruction_elimination() {
    for (instructions, pc) in [
        (vec![Instruction::Jump(1), Instruction::Jump(1)], 1),
        (
            vec![
                Instruction::Jump(1),
                Instruction::WriteCell { symbol: 0, src: 0 },
                Instruction::Jump(1),
            ],
            2,
        ),
        (
            vec![
                Instruction::Jump(1),
                Instruction::Load(0, Immediate::Nil),
                Instruction::Unless(0, 1),
            ],
            2,
        ),
    ] {
        let mut function = lift_code(&code(instructions), &NoSpecializationInputs).unwrap();
        optimize(&mut function, &NoSpecializationInputs);
        let frame = native_frame_layout(&function);
        assert!(frame.needs_execution_polls());
        assert!(
            function
                .blocks
                .iter()
                .all(|block| frame.terminal_poll_pc(block.id).is_none())
        );
        let checkpoint_pcs: Vec<_> = function
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .filter(|inst| matches!(inst.op, SsaOp::Checkpoint { .. }))
            .map(|inst| inst.pc.unwrap())
            .collect();
        assert_eq!(checkpoint_pcs, [pc]);

        // SSA assembled without a bytecode checkpoint still polls on a cycle.
        for block in &mut function.blocks {
            block
                .insts
                .retain(|inst| !matches!(inst.op, SsaOp::Checkpoint { .. }));
        }
        let frame = native_frame_layout(&function);
        let terminal_pcs: Vec<_> = function
            .blocks
            .iter()
            .filter_map(|block| frame.terminal_poll_pc(block.id))
            .collect();
        assert_eq!(terminal_pcs, [pc]);
    }
}

#[test]
fn merging_a_jump_chain_keeps_the_final_control_transfer_pc() {
    let mut function = lift_code(
        &code(vec![
            Instruction::Jump(1),
            Instruction::Unless(0, 4),
            Instruction::Jump(3),
            Instruction::Jump(1),
            Instruction::SetValues(0..1),
        ]),
        &NoSpecializationInputs,
    )
    .unwrap();
    let before = function.blocks.len();
    optimize(&mut function, &NoSpecializationInputs);
    assert!(function.blocks.len() < before);
    let merged = function.block(function.pc_to_block[&2]).unwrap();
    assert_eq!(merged.id, function.pc_to_block[&3]);
    assert_eq!(merged.terminator_pc, 3);
    assert!(
        merged
            .insts
            .iter()
            .any(|inst| matches!(inst.op, SsaOp::Checkpoint { .. }) && inst.pc == Some(3))
    );
    assert_eq!(
        native_frame_layout(&function).terminal_poll_pc(merged.id),
        None
    );
}
