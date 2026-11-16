(project "examples/units"
  (version "0.1.0")
  (license "Apache-2.0")
  (packages "EXAMPLES.UNITS")
  (build "generate.lisp"
    (input "units.sexp")
    (output "generated-units.lisp"))
  (library
    (files "package.lisp" "generated-units.lisp" "convert.lisp"))
  (binary "units"
    (files "main.lisp")
    (entry "EXAMPLES.UNITS:MAIN"))
  (test
    (files "tests.lisp")
    (suite "EXAMPLES.UNITS::ALL")))
