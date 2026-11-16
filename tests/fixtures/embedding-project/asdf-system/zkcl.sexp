(project "test/embed-asdf"
  (version "1.0.0")
  (packages "EMBED.ASDF")
  (dependency "native" (module "test/embed-native") (path "../native")
    (version "^1") (system "embed-native"))
  (asdf-system "embed-asdf" "embed-asdf.asd")
  (include "embed-asdf.asd" "package.lisp" "api.lisp" "data.txt"))
