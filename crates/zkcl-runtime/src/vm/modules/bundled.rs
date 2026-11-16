//! Source supplied by zkcl for named modules and ASDF compatibility ports.

#[cfg(feature = "asdf-ports")]
use super::{allocate_string, arity_error};
#[cfg(feature = "asdf-ports")]
use crate::condition::{Class, Fault};
#[cfg(feature = "asdf-ports")]
use crate::{EvalError, Runtime, vm};

#[cfg(feature = "asdf-ports")]
const ASDF_SOURCE: &str = concat!(
    include_str!("../../../lisp/bundled/asdf/asdf.lisp"),
    "\n",
    include_str!("../../../lisp/bundled/asdf/ports/systems.lisp"),
    "\n",
    include_str!("../../../lisp/bundled/asdf/ports/closer-mop.lisp"),
    "\n",
    include_str!("../../../lisp/bundled/asdf/bundled-systems.lisp"),
);

#[cfg(all(feature = "asdf", not(feature = "asdf-ports")))]
const ASDF_SOURCE: &str = concat!(
    include_str!("../../../lisp/bundled/asdf/asdf.lisp"),
    "\n",
    include_str!("../../../lisp/bundled/asdf/bundled-systems.lisp"),
);

/// Find the source and the name it registers with `PROVIDE`.
pub(super) fn find(name: &str) -> Option<(&'static str, &'static str)> {
    [
        #[cfg(feature = "asdf")]
        ("asdf", ASDF_SOURCE),
        #[cfg(feature = "asdf")]
        ("uiop", ASDF_SOURCE),
        #[cfg(feature = "syntax-editing")]
        (
            "ZK-SYNTAX",
            include_str!("../../../lisp/runtime/syntax/syntax.lisp"),
        ),
        (
            "ZK-GRAY",
            concat!(
                include_str!("../../../lisp/runtime/streams/gray-streams.lisp"),
                "\n",
                include_str!("../../../lisp/runtime/streams/gray-compound-streams.lisp")
            ),
        ),
        (
            "zk-test",
            include_str!("../../../lisp/bundled/systems/zk-test/zk-test.lisp"),
        ),
        (
            "zk-pbt",
            concat!(
                "(require \"zk-test\")\n",
                include_str!("../../../lisp/bundled/systems/zk-pbt/generators.lisp"),
                "\n",
                include_str!("../../../lisp/bundled/systems/zk-pbt/corpus.lisp"),
                "\n",
                include_str!("../../../lisp/bundled/systems/zk-pbt/runner.lisp"),
                "\n",
                include_str!("../../../lisp/bundled/systems/zk-pbt/machines.lisp")
            ),
        ),
    ]
    .into_iter()
    .find(|(registered, _)| registered.eq_ignore_ascii_case(name))
}

// ASDF loads these components alongside frontends in the user's distribution.
#[cfg(feature = "asdf-ports")]
fn asdf_port_source(name: &str) -> Option<&'static str> {
    macro_rules! sources {
        ($($(#[$feature:meta])* $path:literal),* $(,)?) => {
            match name {
                $($(#[$feature])* $path => Some(include_str!(concat!("../../../lisp/bundled/asdf/ports/", $path))),)*
                _ => None,
            }
        };
    }
    sources! {
        "cffi/trivial-features.asd",
        #[cfg(feature = "ffi")]
        "cffi/upstream.lisp",
        #[cfg(feature = "ffi")]
        "cffi/cffi-zkcl.lisp",
        #[cfg(feature = "ffi")]
        "cffi/cffi-utf8.lisp",
        #[cfg(feature = "ffi")]
        "cffi/cffi-aggregate.lisp",
        "trivial-garbage/trivial-garbage.asd",
        "trivial-garbage/zkcl.lisp",
        "bordeaux-threads/bordeaux-threads.asd",
        "bordeaux-threads/types-v1.lisp",
        "bordeaux-threads/impl-zkcl.lisp",
        "trivial-gray-streams/trivial-gray-streams.asd",
        "trivial-gray-streams/package.lisp",
        "trivial-gray-streams/zkcl.lisp",
        "package-backend.lisp",
        "trivial-cltl2/zkcl.lisp",
        "trivial-method-combinations/zkcl.lisp",
    }
}

#[cfg(feature = "asdf-ports")]
pub(super) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    vm::install_host_function(
        runtime,
        "ZK::%ASDF-PORT-SOURCE",
        std::sync::Arc::new(|runtime, args, at| {
            let [name] = args else {
                return Err(arity_error(at, "%ASDF-PORT-SOURCE expects one name"));
            };
            let name = runtime.string(name).ok_or_else(|| {
                Fault::type_error(runtime, 
                    name.slot.get(),
                    "STRING",
                    EvalError::new(at, "expected a port source name"),
                )
            })?;
            let source = asdf_port_source(&name).ok_or_else(|| {
                Fault::plain(
                    Class::Error,
                    EvalError::new(at, format!("unknown ASDF port source {name:?}")),
                )
            })?;
            Ok(vec![runtime.root(allocate_string(runtime, source, at)?)])
        }),
    )
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "asdf-ports"))]
    use crate::Runtime;

    use super::*;

    #[cfg(not(feature = "asdf"))]
    #[test]
    fn asdf_is_absent_without_asdf_feature() {
        assert!(find("asdf").is_none());
        let runtime = Runtime::start().unwrap();
        assert!(runtime.eval("(require :asdf)").is_err());
    }

    #[cfg(all(feature = "asdf", not(feature = "asdf-ports")))]
    #[test]
    fn asdf_loads_without_compatibility_ports() {
        assert!(find("asdf").is_some());
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                let runtime = Runtime::start().unwrap();
                runtime.eval("(require :asdf)").unwrap();
                assert!(runtime.eval("(fboundp 'zk::load-asdf-port)").unwrap()[0].is_nil());
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[cfg(feature = "asdf-ports")]
    #[test]
    fn asdf_and_ports_are_present_with_asdf_ports_feature() {
        assert!(find("asdf").is_some());
        assert!(asdf_port_source("cffi/trivial-features.asd").is_some());
        for source in [
            "cffi/upstream.lisp",
            "cffi/cffi-zkcl.lisp",
            "cffi/cffi-utf8.lisp",
            "cffi/cffi-aggregate.lisp",
        ] {
            assert_eq!(asdf_port_source(source).is_some(), cfg!(feature = "ffi"));
        }
    }
}
