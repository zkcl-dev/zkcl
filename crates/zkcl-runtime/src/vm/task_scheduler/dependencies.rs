use std::mem::DropGuard;

use rustc_hash::FxHashMap;

use super::TaskId;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Dependency {
    pub(super) task: TaskId,
    pub(super) cancel_before_join: bool,
}

impl Dependency {
    pub(super) fn result(task: TaskId) -> Self {
        Self {
            task,
            cancel_before_join: false,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Requirement {
    Required,
    Alternative,
}

struct WaitNode {
    dependency: Dependency,
    depended_on_by: Option<usize>,
    required: usize,
    needs_alternative: bool,
    target: bool,
}

impl WaitNode {
    fn is_ready(&self) -> bool {
        self.required == 0 && !self.needs_alternative
    }
}

#[derive(Clone, Copy)]
struct WaitEdge {
    dependent: usize,
    requirement: Requirement,
    next: Option<usize>,
}

/// Finds which tasks can finish from the current set of runnable or externally
/// wakeable tasks. Joins and single awaits require all their dependencies.
/// An await-any can proceed after one alternative finishes.
#[derive(Default)]
pub(super) struct WaitGraph {
    ids: FxHashMap<Dependency, usize>,
    nodes: Vec<WaitNode>,
    edges: Vec<WaitEdge>,
    ready: Vec<usize>,
}

impl WaitGraph {
    fn intern(&mut self, dependency: Dependency) -> usize {
        *self.ids.entry(dependency).or_insert_with(|| {
            let index = self.nodes.len();
            self.nodes.push(WaitNode {
                dependency,
                depended_on_by: None,
                required: 0,
                needs_alternative: false,
                target: false,
            });
            index
        })
    }

    fn add_edge(&mut self, parent: usize, dependency: Dependency, requirement: Requirement) {
        let child = self.intern(dependency);
        match requirement {
            Requirement::Required => self.nodes[parent].required += 1,
            Requirement::Alternative => self.nodes[parent].needs_alternative = true,
        }
        let next = self.nodes[child].depended_on_by.replace(self.edges.len());
        self.edges.push(WaitEdge {
            dependent: parent,
            requirement,
            next,
        });
    }

    /// `describe` returns false for a dependency forced to remain blocked. It
    /// emits no edges in that case. All other nodes can proceed once their
    /// emitted requirements are met.
    pub(super) fn any_can_finish(
        &mut self,
        targets: impl IntoIterator<Item = Dependency>,
        mut describe: impl FnMut(Dependency, &mut dyn FnMut(Dependency, Requirement)) -> bool,
    ) -> bool {
        let mut graph = DropGuard::new(self, |graph| {
            graph.ids.clear();
            graph.nodes.clear();
            graph.edges.clear();
            graph.ready.clear();
        });
        for target in targets {
            let index = graph.intern(target);
            graph.nodes[index].target = true;
        }

        let mut index = 0;
        while index < graph.nodes.len() {
            let dependency = graph.nodes[index].dependency;
            let allowed = describe(dependency, &mut |child, requirement| {
                graph.add_edge(index, child, requirement);
            });
            if allowed && graph.nodes[index].is_ready() {
                if graph.nodes[index].target {
                    return true;
                }
                graph.ready.push(index);
            }
            index += 1;
        }

        // Propagate possible completion once per node and edge. A closed
        // cycle has no starting point. One open alternative can break it.
        while let Some(index) = graph.ready.pop() {
            let mut next = graph.nodes[index].depended_on_by;
            while let Some(edge) = next {
                let WaitEdge {
                    dependent,
                    requirement,
                    next: following,
                } = graph.edges[edge];
                next = following;
                let node = &mut graph.nodes[dependent];
                let already_ready = node.is_ready();
                match requirement {
                    Requirement::Required => node.required -= 1,
                    Requirement::Alternative => node.needs_alternative = false,
                }
                if !already_ready && node.is_ready() {
                    if node.target {
                        return true;
                    }
                    graph.ready.push(dependent);
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{Dependency, Requirement, WaitGraph};

    #[test]
    fn completion_reaches_previous_nodes_through_an_open_alternative() {
        let mut graph = WaitGraph::default();
        for (open, expected) in [(false, false), (true, true), (false, false)] {
            let result = graph.any_can_finish([Dependency::result(0)], |dep, add| {
                match dep.task {
                    0 => {
                        add(Dependency::result(1), Requirement::Required);
                        add(Dependency::result(2), Requirement::Required);
                    }
                    1 => {
                        add(Dependency::result(2), Requirement::Alternative);
                        add(Dependency::result(3), Requirement::Alternative);
                    }
                    2 => add(Dependency::result(1), Requirement::Required),
                    3 => return open,
                    _ => unreachable!(),
                }
                true
            });
            assert_eq!(result, expected);
        }
    }

    #[test]
    fn a_deep_wait_chain_uses_bounded_rust_stack_space() {
        let mut graph = WaitGraph::default();
        let result = graph.any_can_finish([Dependency::result(0)], |dep, add| {
            if dep.task < 100_000 {
                add(Dependency::result(dep.task + 1), Requirement::Required);
            } else {
                add(Dependency::result(0), Requirement::Required);
            }
            true
        });
        assert!(!result);
    }
}
