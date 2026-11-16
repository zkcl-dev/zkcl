use wasm_encoder::{BlockType, Function};
use zkcl_bytecode::native_abi::LEAF_MISS;
use zkcl_vop::leaf::{Block, Exit};

use super::super::cfg::{ControlFlow, Region};
use super::{emit_operand, emit_steps, emit_test};

pub(super) fn emit(
    function: &mut Function,
    blocks: &[Block],
    flow: &ControlFlow,
    wide: u32,
    budget: u32,
    dispatch: Option<u32>,
) {
    let mut emit = Emitter {
        function,
        blocks,
        flow,
        wide,
        budget,
        dispatch,
        labels: vec![u32::MAX; blocks.len()],
        depth: 0,
    };
    let regions = &flow.regions;
    emit.open_regions(regions);
    emit.jump(0, regions.first().map(Region::entry));
    emit.regions(regions);
    emit.function.instructions().unreachable();
}

struct Emitter<'a> {
    function: &'a mut Function,
    blocks: &'a [Block],
    flow: &'a ControlFlow,
    wide: u32,
    budget: u32,
    dispatch: Option<u32>,
    labels: Vec<u32>,
    depth: u32,
}

impl Emitter<'_> {
    fn open_regions(&mut self, regions: &[Region]) {
        for region in regions.iter().rev() {
            self.labels[region.entry()] = self.depth;
            self.function.instructions().block(BlockType::Empty);
            self.depth += 1;
        }
    }

    fn regions(&mut self, regions: &[Region]) {
        for (index, region) in regions.iter().enumerate() {
            self.end();
            let next = regions.get(index + 1).map(Region::entry);
            match region {
                Region::Block(block) => self.block(*block, next),
                Region::Loop { header, body } => {
                    self.open_loop(*header);
                    self.open_regions(body);
                    self.block(*header, body.first().map(Region::entry));
                    self.regions(body);
                    self.end();
                }
                Region::Dispatch(blocks) => self.dispatch_region(blocks),
            }
        }
    }

    fn open_loop(&mut self, header: usize) {
        self.labels[header] = self.depth;
        self.depth += 1;
        self.function.instructions().loop_(BlockType::Empty);
    }

    fn end(&mut self) {
        self.depth -= 1;
        self.function.instructions().end();
    }

    fn dispatch_region(&mut self, blocks: &[usize]) {
        self.open_loop(blocks[0]);
        let cases = blocks.len() as u32;
        for _ in 0..=cases {
            self.function.instructions().block(BlockType::Empty);
            self.depth += 1;
        }
        self.function
            .instructions()
            .local_get(self.dispatch.expect("multi-entry loop dispatcher"))
            .br_table(0..cases, cases);
        for &block in blocks {
            self.end();
            self.block(block, None);
        }
        self.end();
        self.function.instructions().unreachable();
        self.end();
    }

    fn jump(&mut self, target: usize, next: Option<usize>) {
        let anchor = if let Some((anchor, case)) = self.flow.dispatch[target] {
            self.function
                .instructions()
                .i32_const(case as i32)
                .local_set(self.dispatch.unwrap());
            anchor
        } else {
            target
        };
        if Some(anchor) != next {
            self.function
                .instructions()
                .br(self.depth - 1 - self.labels[anchor]);
        }
    }

    fn block(&mut self, index: usize, next: Option<usize>) {
        let block = &self.blocks[index];
        emit_steps(self.function, &block.steps, self.wide, Some(self.budget));
        match block.exit {
            Exit::Return(value) => {
                emit_operand(self.function, value);
                self.function.instructions().return_();
            }
            Exit::Miss => {
                self.function
                    .instructions()
                    .i32_const(LEAF_MISS as i32)
                    .return_();
            }
            Exit::Jump(target) => self.jump(target, next),
            Exit::Branch {
                value,
                test,
                yes,
                no,
            } => {
                emit_test(self.function, value, test);
                let yes_anchor = self.flow.dispatch[yes].map_or(yes, |(anchor, _)| anchor);
                if Some(yes_anchor) == next {
                    self.function.instructions().i32_eqz().if_(BlockType::Empty);
                    self.depth += 1;
                    self.jump(no, None);
                    self.end();
                    self.jump(yes, next);
                } else {
                    self.function.instructions().if_(BlockType::Empty);
                    self.depth += 1;
                    self.jump(yes, None);
                    self.end();
                    self.jump(no, next);
                }
            }
        }
    }
}
