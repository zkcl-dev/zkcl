(project "test/embed-app"
  (version "1.0.0")
  (packages "EMBED.APP")
  (dependency "native" (module "test/embed-native") (path "../native") (version "^1"))
  (dependency "asdf-system" (module "test/embed-asdf") (path "../asdf-system")
    (version "^1") (system "embed-asdf") (asd "embed-asdf.asd"))
  (build "generate.lisp" (input "input.txt") (output "generated.lisp"))
  (binary "app" (files "generated.lisp" "main.lisp") (entry "EMBED.APP:MAIN")))
