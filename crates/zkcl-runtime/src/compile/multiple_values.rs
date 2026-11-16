use super::{Compiler, Destination, Form, Ins};
use crate::EvalError;

impl Compiler<'_> {
    pub(super) fn emit_multiple_value_call<T>(
        &mut self,
        function: &T,
        forms: &[T],
        dest: Destination,
        at: usize,
        mut emit: impl FnMut(&mut Self, &T, Destination) -> Result<(), EvalError>,
    ) -> Result<(), EvalError> {
        let callee = self.reserve(1).start;
        emit(self, function, Destination::One(callee))?;

        match forms {
            [] => self.select_values(0..0),
            [only] => emit(self, only, Destination::All)?,
            [first, rest @ ..] => {
                emit(self, first, Destination::All)?;
                self.emit(Ins::SaveValues);
                for form in rest {
                    emit(self, form, Destination::All)?;
                    self.emit(Ins::AppendValues);
                }
                self.emit(Ins::RestoreValues);
            }
        }

        self.emit(Ins::CallValues { callee, at });
        self.receive_values(dest);
        Ok(())
    }

    pub(super) fn multiple_value_prog1(
        &mut self,
        first: &Form,
        forms: &[Form],
        dest: Destination,
    ) -> Result<(), EvalError> {
        if forms.is_empty() {
            return self.form(first, dest);
        }

        let finish = match dest {
            Destination::All => {
                self.form(first, Destination::All)?;
                self.emit(Ins::SaveValues);
                Ins::RestoreValues
            }
            Destination::One(out) => {
                let register = self.reserve(1).start;
                self.form(first, Destination::One(register))?;
                Ins::Copy(out, register)
            }
        };

        let mark = self.used;
        for form in forms {
            let ignored = self.reserve(1).start;
            self.form(form, Destination::One(ignored))?;
            self.release(mark);
        }
        self.emit(finish);
        Ok(())
    }
}
