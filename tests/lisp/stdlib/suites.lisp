(in-package #:zkcl-stdlib-tests)

(zk-test:defsuite numbers-suite
  exact-integer-ring-laws
  integer-division-laws
  gcd-lcm-laws
  bitwise-and-byte-laws
  ratio-canonical-and-field-laws
  float-decode-scale-roundtrip)

(zk-test:defsuite reader-printer-suite
  readable-print-read-roundtrip
  integer-base-radix-roundtrip
  print-circle-preserves-sharing-and-cycles
  malformed-reader-inputs-signal-cleanly)

(zk-test:defsuite equality-and-tables-suite
  equality-refinement-and-equivalence
  sxhash-ignores-sharing-and-package-status
  eq-hash-table-state-machine
  eql-hash-table-state-machine
  equal-hash-table-state-machine
  equalp-hash-table-state-machine)

(zk-test:defsuite sequences-and-arrays-suite
  sort-and-stable-sort-match-oracle
  displaced-and-fill-pointer-sort-isolates-neighbors
  sequence-operations-agree-across-representations
  list-set-algebra-laws
  adjustable-vector-state-machine
  plist-state-machine)

(zk-test:defsuite pathnames-suite
  pathname-namestring-roundtrip
  parse-namestring-slice-matches-subseq
  merge-pathnames-associativity-and-defaults
  pathname-case-common-roundtrip)

(zk-test:defsuite types-and-eval-suite
  subtypep-lattice-and-soundness
  eval-and-compile-agree)

(zk-test:defsuite portable-all
  numbers-suite
  reader-printer-suite
  equality-and-tables-suite
  sequences-and-arrays-suite
  pathnames-suite
  types-and-eval-suite)

#+zkcl
(zk-test:defsuite zkcl-extensions-suite
  variant-match-matches-etypecase
  ematch-signals-match-error-on-miss
  async-task-group-and-cancellation-invariants
  async-timeout-opt-contract
  gray-stream-state-machine)

(zk-test:defsuite stdlib-all
  portable-all
  #+zkcl zkcl-extensions-suite)
