(in-package #:zkcl-stdlib-tests)

(zk-pbt:defproperty readable-print-read-roundtrip
    ((value (zk-pbt:one-of
             (zk-pbt:exact-integers)
             (zk-pbt:ratios :max-bits 64)
             (zk-pbt:floats :format 'single-float)
             (zk-pbt:floats :format 'double-float)
             (zk-pbt:complexes)
             (zk-pbt:characters :unicode t)
             (zk-pbt:strings :max 20 :unicode t)
             (zk-pbt:keywords :max 12 :unicode nil)
             (zk-pbt:s-expressions :max-depth 3))))
  (let* ((*print-readably* t)
         (*print-circle* nil)
         (*read-eval* nil)
         (printed (write-to-string value))
         (parsed (read-from-string printed)))
    (zk-test:is-equalp value parsed)
    (when (numberp value)
      (zk-test:is (eql value parsed))))
  t)

(zk-pbt:defproperty integer-base-radix-roundtrip
    ((n (zk-pbt:exact-integers))
     (base (zk-pbt:integers :min 2 :max 36))
     (radix (zk-pbt:booleans)))
  (let* ((*print-base* base)
         (*print-radix* radix)
         (*read-base* 10)
         (text (write-to-string n)))
    (if radix
        (zk-test:is= n (read-from-string text))
        (let ((*read-base* base))
          (zk-test:is= n (read-from-string text)))))
  t)

(zk-pbt:defproperty print-circle-preserves-sharing-and-cycles
    ((tag (zk-pbt:keywords :max 8))
     (n (zk-pbt:exact-integers)))
  (let* ((shared (list tag n))
         (graph (vector shared shared))
         (*print-circle* t)
         (*print-readably* t))
    (setf (cdr (last shared)) shared)
    (let* ((printed (write-to-string graph))
           (parsed (read-from-string printed)))
      (zk-test:is (vectorp parsed))
      (zk-test:is-eq (aref parsed 0) (aref parsed 1))
      (zk-test:is-eq (car (aref parsed 0)) tag)
      (zk-test:is= (cadr (aref parsed 0)) n)
      (zk-test:is-eq (cddr (aref parsed 0)) (aref parsed 0))))
  t)

(zk-pbt:defproperty malformed-reader-inputs-signal-cleanly
    ((bad (zk-pbt:one-of
           (zk-pbt:elements "(" ")" "#(" "#*" "\"unterminated" "|unterminated"
                            "#" "#:" "#1=" "#1#" "#." "#+" "#-" "#\\NoSuchCharName"
                            "1/0" "1e999999" "." "(a . )" "(a b . )")
           (zk-pbt:map-gen (lambda (s) (concatenate 'string "(" s))
                           (zk-pbt:ascii-strings :min 0 :max 12)))))
  (let ((*read-eval* nil))
    (handler-case
        (progn (read-from-string bad) t)
      (error (c)
        (zk-test:is (typep c 'error))))))
