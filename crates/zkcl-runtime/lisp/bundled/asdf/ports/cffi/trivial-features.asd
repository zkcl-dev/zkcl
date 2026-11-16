;;;; zkcl publishes the platform keywords in the initial value of CL:*FEATURES*.
;;;; TRIVIAL-FEATURES has no other runtime API.

(asdf:defsystem "trivial-features"
  :description "Native zkcl platform features"
  :in-order-to ((asdf:test-op (asdf:test-op "trivial-features-tests"))))
