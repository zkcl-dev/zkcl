use super::{ControlScope, ControlStack, ScopeKind, Values};

impl ControlStack<'_> {
    pub(crate) fn save_values(&mut self, frame: usize, values: &mut Values) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::SavedValues(values.take()),
        });
    }

    pub(crate) fn append_values(&mut self, values: &mut Values) {
        let ScopeKind::SavedValues(saved) = &mut self.scopes.last_mut().unwrap().kind else {
            unreachable!("appending to a saved multiple-value scope");
        };
        saved.append(values);
    }

    pub(crate) fn restore_values(&mut self, values: &mut Values) {
        let ScopeKind::SavedValues(saved) = self.scopes.pop().unwrap().kind else {
            unreachable!("restoring a saved multiple-value scope");
        };
        *values = saved;
    }
}
