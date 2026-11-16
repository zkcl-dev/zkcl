//! Order MAKE-LOAD-FORM code before the first top-level use of each object.
//! Creation dependencies are mandatory. An initialization also precedes a
//! referencing form whenever doing so would not introduce a cycle (CLHS
//! MAKE-LOAD-FORM). Ordinary literals need no executable loader entries.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use rustc_hash::{FxHashMap, FxHashSet};
use zkcl_bytecode::{BytecodeFunction, CompiledFile, LiteralNode};

use crate::EvalError;

pub(super) struct CompiledLoadForms {
    pub(super) node_id: u32,
    pub(super) creation: BytecodeFunction,
    pub(super) initialization: Option<BytecodeFunction>,
}

struct LoadFormEntry {
    node_id: u32,
    is_initialization: bool,
    code: BytecodeFunction,
}

pub(super) fn emit_load_forms(
    file: &mut CompiledFile,
    first_node: usize,
    forms: Vec<CompiledLoadForms>,
) -> Result<(), EvalError> {
    let mut actions = Vec::new();
    let mut creators = FxHashMap::default();
    for form in forms {
        creators.insert(form.node_id, actions.len());
        actions.push(LoadFormEntry {
            node_id: form.node_id,
            is_initialization: false,
            code: form.creation,
        });
        if let Some(code) = form.initialization {
            actions.push(LoadFormEntry {
                node_id: form.node_id,
                is_initialization: true,
                code,
            });
        }
    }

    let mut creation_dependencies = vec![Vec::new(); actions.len()];
    let mut preferred_initializations = Vec::new();
    let mut visited = FxHashSet::default();
    let mut pending = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        if action.is_initialization {
            creation_dependencies[index].push(creators[&action.node_id]);
        }
        visited.clear();
        pending.extend_from_slice(&action.code.constants);
        while let Some(id) = pending.pop() {
            if (id as usize) < first_node || !visited.insert(id) {
                continue;
            }
            if let Some(&creation) = creators.get(&id) {
                creation_dependencies[index].push(creation);
                let initialization = creation + 1;
                if id != action.node_id
                    && actions
                        .get(initialization)
                        .is_some_and(|a| a.is_initialization && a.node_id == id)
                {
                    preferred_initializations.push((index, initialization));
                }
            } else {
                file.nodes[id as usize].push_references(&mut pending);
            }
        }
        creation_dependencies[index].sort_unstable();
        creation_dependencies[index].dedup();
    }

    let mut actions: Vec<_> = actions.into_iter().map(Some).collect();
    for index in load_order(creation_dependencies, preferred_initializations)? {
        let action = actions[index].take().expect("each action is emitted once");
        let LiteralNode::LoadForm {
            creation,
            initialization,
        } = &mut file.nodes[action.node_id as usize]
        else {
            unreachable!("action for an externalized literal")
        };
        if action.is_initialization {
            *initialization = Some(file.entries.len());
        } else {
            *creation = file.entries.len();
        }
        file.entries.push(action.code);
    }
    Ok(())
}

fn load_order(
    mut prerequisites: Vec<Vec<usize>>,
    mut preferred_initializations: Vec<(usize, usize)>,
) -> Result<Vec<usize>, EvalError> {
    let mandatory_counts: Vec<_> = prerequisites.iter().map(Vec::len).collect();
    for &(consumer, initialization) in &preferred_initializations {
        prerequisites[consumer].push(initialization);
    }
    // Most object graphs satisfy every initialization dependency. One sort
    // orders even a long chain without tracing its transitive closure.
    let order = topological_order(&prerequisites);
    if order.len() == prerequisites.len() {
        return Ok(order);
    }
    for (dependencies, count) in prerequisites.iter_mut().zip(mandatory_counts) {
        dependencies.truncate(count);
    }
    let creation_order = topological_order(&prerequisites);
    if creation_order.len() != prerequisites.len() {
        return Err(EvalError::compile(
            0,
            "circular dependency between MAKE-LOAD-FORM creation forms",
        ));
    }

    // In a cyclic graph every preference already satisfied by the mandatory
    // order can be kept together. For the remaining ones, omit an edge only
    // if the initializer already depends on the form that would await it.
    let mut rank = vec![0; prerequisites.len()];
    for (position, action) in creation_order.into_iter().enumerate() {
        rank[action] = position;
    }
    preferred_initializations.retain(|&(consumer, initialization)| {
        if rank[initialization] < rank[consumer] {
            prerequisites[consumer].push(initialization);
            false
        } else {
            true
        }
    });
    let mut visited = vec![usize::MAX; prerequisites.len()];
    let mut pending = Vec::new();
    for (generation, (consumer, initialization)) in
        preferred_initializations.into_iter().enumerate()
    {
        pending.clear();
        pending.push(initialization);
        let mut cycle = false;
        while let Some(action) = pending.pop() {
            if action == consumer {
                cycle = true;
                break;
            }
            if visited[action] != generation {
                visited[action] = generation;
                pending.extend_from_slice(&prerequisites[action]);
            }
        }
        if !cycle {
            prerequisites[consumer].push(initialization);
        }
    }
    Ok(topological_order(&prerequisites))
}

fn topological_order(prerequisites: &[Vec<usize>]) -> Vec<usize> {
    let mut dependents = vec![Vec::new(); prerequisites.len()];
    let mut remaining = Vec::with_capacity(prerequisites.len());
    let mut ready = BinaryHeap::new();
    for (action, dependencies) in prerequisites.iter().enumerate() {
        remaining.push(dependencies.len());
        if dependencies.is_empty() {
            ready.push(Reverse(action));
        }
        for &dependency in dependencies {
            dependents[dependency].push(action);
        }
    }
    let mut order = Vec::with_capacity(prerequisites.len());
    while let Some(Reverse(action)) = ready.pop() {
        order.push(action);
        for &dependent in &dependents[action] {
            remaining[dependent] -= 1;
            if remaining[dependent] == 0 {
                ready.push(Reverse(dependent));
            }
        }
    }
    order
}
