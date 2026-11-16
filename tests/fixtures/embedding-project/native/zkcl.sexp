(project "test/embed-native"
  (version "1.0.0")
  (packages "EMBED.NATIVE" "EMBED.HOST")
  (asdf-alias "embed-native" (version "1.0.0"))
  (library (files "package.lisp" "macros.lisp" "api.lisp")))
