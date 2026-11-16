function needsCell(binding) {
  return binding.kind === 'MutableCell' && binding.captured && binding.assigned;
}

// This runs after the enclosing function and every nested function have been
// compiled. Assignments in a later sibling can keep an earlier capture boxed.
export function finishLexicalFrame({ code, localBindings, captureLocations }) {
  const directLocals = new Set();
  for (const [register, binding] of localBindings) {
    if (!needsCell(binding)) directLocals.add(register);
  }
  const directCaptures = new Set();
  for (const [index, { binding }] of captureLocations.entries()) {
    if (binding.kind === 'MutableCell' && !needsCell(binding)) {
      code.captures[index] = 'Value';
      directCaptures.add(index);
    }
  }
  if (!directLocals.size && !directCaptures.size) return;

  const original = code.instructions;
  const instructions = [];
  const positions = new Array(original.length + 1);
  for (const [pc, instruction] of original.entries()) {
    positions[pc] = instructions.length;
    const { BoxLocal, ReadBox, WriteBox } = instruction;
    if (BoxLocal && directLocals.has(BoxLocal.reg)) continue;
    if (ReadBox && directLocals.has(ReadBox.cell.Local)) {
      instructions.push({ Copy: [ReadBox.out, ReadBox.cell.Local] });
    } else if (ReadBox && directCaptures.has(ReadBox.cell.Capture)) {
      instructions.push({ Capture: [ReadBox.out, ReadBox.cell.Capture] });
    } else if (WriteBox && directLocals.has(WriteBox.cell.Local)) {
      instructions.push({ Copy: [WriteBox.cell.Local, WriteBox.src] });
    } else {
      instructions.push(instruction);
    }
  }
  positions[original.length] = instructions.length;
  if (instructions.length !== original.length) relocate(code, instructions, positions);
  code.instructions = instructions;
}

function relocate(code, instructions, positions) {
  for (const [pc, instruction] of instructions.entries()) {
    if (typeof instruction === 'string') continue;
    if (Object.hasOwn(instruction, 'Jump')) {
      instructions[pc] = { Jump: positions[instruction.Jump] };
      continue;
    }
    const branch = instruction.Unless ?? instruction.IfPresent ?? instruction.UnlessFixnum ?? instruction.UnlessFixnumInRange;
    if (branch) branch[branch.length - 1] = positions[branch.at(-1)];
    const target = instruction.EnterBlock ?? instruction.EnterCatch;
    if (target) target.resume = positions[target.resume];
    if (instruction.EnterProtect) instruction.EnterProtect.cleanup = positions[instruction.EnterProtect.cleanup];
    if (instruction.WhenBound) instruction.WhenBound.jump = positions[instruction.WhenBound.jump];
  }
  for (const table of code.jump_tables) table.targets = table.targets.map(pc => positions[pc]);
  for (const tables of [code.handlers, code.restarts]) {
    for (const table of tables) {
      for (const { target } of table.handlers ?? table.restarts) {
        if (target.Case) target.Case.pc = positions[target.Case.pc];
      }
    }
  }
  for (const variable of code.debug.variables) {
    const { start, end } = variable.live;
    variable.live = { start: positions[start], end: positions[end] };
  }
}
