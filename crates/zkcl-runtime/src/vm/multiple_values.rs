use super::Values;

impl Values {
    pub(super) fn append(&mut self, source: &mut Self) {
        if self.len == 0 {
            std::mem::swap(self, source);
            return;
        }

        let len = self.len + source.len;
        if len > self.capacity {
            let capacity = len.max(self.capacity.saturating_mul(2));
            let slots = self.allocator.alloc(capacity);
            for index in 0..self.len {
                slots.set(index, self.slots.get(index));
            }
            self.slots = slots;
            self.capacity = capacity;
        }
        for index in 0..source.len {
            self.slots.set(self.len + index, source.slots.get(index));
        }
        self.len = len;
        source.resize(0);
    }
}
