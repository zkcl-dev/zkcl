use std::io;
use std::path::{Path, PathBuf};

use crate::{EvalError, Runtime, pathname};

impl Runtime {
    /// Override a variable returned by `ZK:GETENV` for this instance only.
    /// `None` hides an inherited process variable. Does not modify the process.
    pub fn set_environment_variable(&self, name: &str, value: Option<&str>) -> io::Result<()> {
        if name.is_empty() || name.contains(['=', '\0']) || value.is_some_and(|v| v.contains('\0'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid environment variable",
            ));
        }
        self.environment
            .borrow_mut()
            .insert(name.to_owned(), value.map(str::to_owned));
        Ok(())
    }

    /// Read the instance override, or the inherited process environment.
    pub fn environment_variable(&self, name: &str) -> Result<Option<String>, std::env::VarError> {
        self.environment.borrow().get(name).cloned().map_or_else(
            || match std::env::var(name) {
                Ok(value) => Ok(Some(value)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(error) => Err(error),
            },
            Ok,
        )
    }

    /// Directory reported by `ZK:GETCWD`. Defaults to the process directory.
    pub fn working_directory(&self) -> io::Result<PathBuf> {
        self.working_directory
            .borrow()
            .clone()
            .map_or_else(std::env::current_dir, Ok)
    }

    /// Set this instance's working directory and `*DEFAULT-PATHNAME-DEFAULTS*`.
    /// Lisp can subsequently rebind the defaults. The process directory is unchanged.
    pub fn set_working_directory(&self, path: &Path) -> Result<(), EvalError> {
        let path = path
            .canonicalize()
            .map_err(|e| EvalError::file_io(0, path, e))?;
        if !path.is_dir() {
            return Err(EvalError::new(0, "working directory is not a directory"));
        }
        let defaults = pathname::Pathname::from_directory_path(&path)
            .map_err(|e| EvalError::from_error(0, e))?;
        self.initialize_packages()?;
        let defaults = defaults.to_lisp_value(self, 0)?;
        let symbol = self
            .lookup_symbol("COMMON-LISP", "*DEFAULT-PATHNAME-DEFAULTS*")?
            .expect("initialized pathname defaults");
        self.heap
            .set_symbol_cell(symbol.slot.get(), false, defaults.slot.get());
        self.working_directory.replace(Some(path));
        Ok(())
    }
}
