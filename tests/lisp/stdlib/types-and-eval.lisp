(in-package #:zkcl-stdlib-tests)

(defun integer-interval-gen ()
  (zk-pbt:map-gen
   (lambda (pair)
     (let ((a (first pair))
           (b (second pair)))
       (list 'integer (min a b) (max a b))))
   (zk-pbt:tuple (zk-pbt:integers :min -20 :max 20)
                 (zk-pbt:integers :min -20 :max 20))))

(defun type-specifier-gen ()
  (let ((base (zk-pbt:one-of
               (zk-pbt:elements 'nil 't 'null 'cons 'list 'symbol 'keyword
                                'integer 'fixnum 'ratio 'rational 'float
                                'single-float 'double-float 'number
                                'character 'string 'vector 'array 'sequence)
               (integer-interval-gen))))
    (zk-pbt:recursive
     base
     (lambda (sub)
       (zk-pbt:one-of
        (zk-pbt:map-gen (lambda (x) (list 'not x)) sub)
        (zk-pbt:map-gen (lambda (pair) (list 'and (first pair) (second pair))) (zk-pbt:tuple sub sub))
        (zk-pbt:map-gen (lambda (pair) (list 'or (first pair) (second pair))) (zk-pbt:tuple sub sub))))
     :max-depth 2)))

(zk-pbt:defproperty subtypep-lattice-and-soundness
    ((t1 (type-specifier-gen))
     (t2 (type-specifier-gen))
     (sample (zk-pbt:one-of
              (zk-pbt:integers :min -25 :max 25)
              (zk-pbt:ratios :max-bits 16)
              (zk-pbt:floats :format 'double-float)
              (zk-pbt:characters :unicode nil)
              (zk-pbt:strings :max 6 :unicode nil)
              (zk-pbt:keywords :max 6)
              (zk-pbt:just nil)
              (zk-pbt:just '(1 . 2)))))
  (zk-test:is-values (subtypep t1 t1) t t)
  (zk-test:is-values (subtypep nil t1) t t)
  (zk-test:is-values (subtypep t1 t) t t)
  (zk-test:is (subtypep `(and ,t1 ,t2) t1))
  (zk-test:is (subtypep t1 `(or ,t1 ,t2)))
  (multiple-value-bind (sub certain) (subtypep t1 t2)
    (when (and sub certain (typep sample t1))
      (zk-test:is (typep sample t2))))
  (when (typep sample t1)
    (zk-test:is (typep sample (type-of sample))))
  t)

(defun pure-expr-gen ()
  (let ((leaf (zk-pbt:one-of (zk-pbt:integers :min -20 :max 20)
                             (zk-pbt:elements 'x 'y))))
    (zk-pbt:recursive
     leaf
     (lambda (sub)
       (zk-pbt:one-of
        (zk-pbt:map-gen (lambda (p) `(+ ,(first p) ,(second p))) (zk-pbt:tuple sub sub))
        (zk-pbt:map-gen (lambda (p) `(- ,(first p) ,(second p))) (zk-pbt:tuple sub sub))
        (zk-pbt:map-gen (lambda (p) `(* ,(first p) ,(second p))) (zk-pbt:tuple sub sub))
        (zk-pbt:map-gen (lambda (p)
                          (destructuring-bind (a b c) p
                            `(let ((x ,a) (y ,b))
                               (declare (ignorable x y))
                               (if (zerop x) ,c (+ x y)))))
                        (zk-pbt:tuple sub sub sub))))
     :max-depth 2)))

(zk-pbt:defproperty eval-and-compile-agree
    ((expr (pure-expr-gen))
     (x0 (zk-pbt:integers :min -10 :max 10))
     (y0 (zk-pbt:integers :min -10 :max 10)))
  (let* ((form `(let ((x ,x0) (y ,y0))
                  (declare (ignorable x y))
                  (labels ((bump (v) (if (numberp v) (1+ v) 0)))
                    (values (bump ,expr) (numberp ,expr)))))
         (eval-vals (multiple-value-list (eval form)))
         (compiled (compile nil `(lambda () ,form)))
         (comp-vals (multiple-value-list (funcall compiled))))
    (zk-test:is-equal eval-vals comp-vals))
  t)
