;;;; Pattern compiler. Source patterns retain their package-resolved symbols.
(defpackage #:zk-bits
  (:use)
  (:export #:bits #:octets))

(defpackage #:zk-text
  (:use)
  (:export #:snapshot #:span-source #:span-start #:span-end #:span-unit
           #:span-slice #:span-value #:span-origin-byte-range
           #:utf8-encode #:utf8-decode #:utf8-source
           #:unicode-decode-error #:unicode-decode-error-span
           #:normalize #:case-fold #:unicode-property-p #:segment-text))

(defpackage #:zk-lex
  (:use)
  (:export #:lexmatch #:lexscan #:lex #:compile-lex-rules #:run-lex-rules
           #:lex-match-rule #:lex-match-span #:lex-capture #:make-lexscanner
           #:scan-step #:scan-position #:scan-take
           #:scan-error #:scan-error-reason #:scan-error-position))

(defpackage #:zk-pattern-compiler (:use #:cl))
(in-package #:zk-pattern-compiler)
(export '(zk::defvariant zk::defpattern zk::pattern-expand-1 zk::view
          zk::range zk::keys
          zk::match zk::ematch zk::cmatch
          zk::if-match zk::when-match zk::unless-match
          zk::let-match zk::let-match*
          zk::match-error zk::match-error-values zk::match-error-patterns) :zk)

(define-condition zk:match-error (error)
  ((inputs :initarg :inputs :reader zk:match-error-values)
   (patterns :initarg :patterns :reader zk:match-error-patterns))
  (:report (lambda (c s) (format s "No pattern in ~S matched ~S"
                                (zk:match-error-patterns c) (zk:match-error-values c)))))

(define-condition variant-redefinition-warning (style-warning simple-condition) ())

(defvar *families* (make-hash-table :test 'eq))
(defvar *family-options* (make-hash-table :test 'eq))
(defvar *constructors* (make-hash-table :test 'eq))
(defvar *constructor-functions* (make-hash-table :test 'eq))
(defvar *constructor-field-types* (make-hash-table :test 'eq))
(defvar *variant-owned-symbols* (make-hash-table :test 'eq))
(defvar *pending-families* (make-hash-table :test 'eq))
(defvar *expanders* (make-hash-table :test 'eq))
(defvar *special-patterns* (make-hash-table :test 'eq))

(defstruct (special-pattern (:copier nil) (:predicate nil))
  (normalizer nil :read-only t)
  (bindings nil :read-only t)
  (emitter nil :read-only t)
  (user-test-p (lambda (node) (declare (ignore node)) t) :read-only t)
  (opaque-p (lambda (node) (declare (ignore node)) t) :read-only t)
  (specializer nil :read-only t)
  (heads nil :read-only t)
  (vector-widths nil :read-only t))

(defun variable-name-p (x)
  (and (symbolp x) (not (constantp x)) (not (member x lambda-list-keywords))))

(defun arity (pattern count)
  (unless (= (length (cdr pattern)) count) (error "Wrong number of operands in pattern ~S" pattern)))

(defun wildcard-p (x)
  (and (symbolp x) (not (keywordp x)) (string= (symbol-name x) "_")))

(defun builtin-head-p (x)
  (or (member x '(cons list list* vector quote and or type zk:view zk:range zk:keys :const :=))
      (and (gethash x *special-patterns*) t)))

(defun available-head-p (x)
  (and (variable-name-p x) (symbol-package x) (not (builtin-head-p x))
       (not (eq (symbol-package x) (find-package :cl)))))

(defun resolve-constant-form (name env)
  (unless (symbolp name) (error "Expected named constant, got ~S" name))
  (multiple-value-bind (type special shadow declared init)
      (zk::%pattern-variable-info name env)
    (declare (ignore type special shadow))
    (cond
      (declared
       (cond ((and (consp init) (eq (car init) 'quote))
              (values name t (second init)))
             ((or (numberp init) (characterp init) (stringp init)
                  (keywordp init) (member init '(nil t)))
              (values name t init))
             (t (values name nil nil))))
      ((and (constantp name env) (boundp name))
       (values name t (symbol-value name)))
      (t (error "Unknown named constant ~S" name)))))

(defun resolve-constant-value (name env)
  (when (symbolp name)
    (multiple-value-bind (type special shadow declared init)
        (zk::%pattern-variable-info name env)
      (declare (ignore type special shadow))
      (cond
        (declared
         (cond ((and (consp init) (eq (car init) 'quote))
                (values t t (second init)))
               ((or (numberp init) (characterp init) (stringp init)
                    (keywordp init) (member init '(nil t)))
                (values t t init))
               (t (values t nil nil))))
        ((and (constantp name env) (boundp name))
         (values t t (symbol-value name)))))))

(defun reader-name (case conc-name slot)
  (let ((prefix (if (eq conc-name :default)
                    (concatenate 'string (symbol-name case) "-")
                    conc-name)))
    (intern (concatenate 'string prefix (symbol-name slot))
            (symbol-package case))))

(defun parse-variant-header (spec)
  (if (symbolp spec)
      (values spec nil :default)
      (progn
        (unless (and (consp spec) (list-length spec) (symbolp (car spec)))
          (error "Invalid variant header ~S" spec))
        (let ((name (car spec))
              (opts (cdr spec))
              (predicate nil)
              (pred-seen nil)
              (conc-name :default)
              (conc-seen nil))
          (loop while opts do
            (let ((opt (pop opts)) key args)
              (cond ((keywordp opt)
                     (unless opts (error "Missing value for variant option ~S in ~S" opt spec))
                     (setf key opt args (list (pop opts))))
                    ((and (consp opt) (list-length opt) (keywordp (car opt)))
                     (setf key (car opt) args (cdr opt)))
                    (t (error "Invalid variant option ~S in ~S" opt spec)))
              (case key
                (:predicate
                 (when pred-seen (error "Repeated :predicate option in ~S" spec))
                 (setf pred-seen t)
                 (cond ((or (null args) (eq (car args) t))
                        (setf predicate (intern (concatenate 'string (symbol-name name) "-P")
                                                (symbol-package name))))
                       ((null (cdr args))
                        (let ((val (car args)))
                          (unless (or (null val) (variable-name-p val))
                            (error "Invalid variant predicate ~S" val))
                          (setf predicate val)))
                       (t (error "Invalid :predicate option in ~S" spec))))
                (:conc-name
                 (when conc-seen (error "Repeated :conc-name option in ~S" spec))
                 (setf conc-seen t)
                 (cond ((or (null args) (null (car args)))
                        (setf conc-name ""))
                       ((null (cdr args))
                        (let ((val (car args)))
                          (unless (or (symbolp val) (stringp val))
                            (error "Invalid :conc-name option ~S" val))
                          (setf conc-name (string val))))
                       (t (error "Invalid :conc-name option in ~S" spec))))
                (otherwise (error "Unknown variant option ~S in ~S" key spec)))))
          (values name predicate conc-name)))))

(defun parse-variant-cases (cases)
  (loop for spec in cases collect
    (progn
      (unless (and (consp spec) (list-length spec)) (error "Invalid variant case ~S" spec))
      (let ((case (car spec))
            (fields (loop for f in (cdr spec) collect
                      (cond ((symbolp f) (list f t))
                            ((and (consp f) (list-length f) (= (length f) 2) (symbolp (first f)))
                             (list (first f) (second f)))
                            (t (error "Invalid variant field specification ~S in ~S" f spec))))))
        (cons case fields)))))

(defun variant-owned-symbols (predicate conc-name parsed-cases)
  (let ((symbols (when predicate (list predicate))))
    (dolist (spec parsed-cases (nreverse symbols))
      (let ((case (car spec)))
        (push case symbols)
        (dolist (field (cdr spec))
          (push (reader-name case conc-name (first field)) symbols))))))

(defun validate-variant (name predicate conc-name parsed-cases)
  (unless (and (available-head-p name) parsed-cases)
    (error "Invalid or occupied variant type name ~S" name))
  (let* ((existing (or (gethash name *families*) (gethash name *pending-families*)))
         (existing-meta (gethash name *family-options*))
         (owned (gethash name *variant-owned-symbols*)))
    (unless (or existing (not (find-class name nil)))
      (error "Invalid or occupied variant type name ~S" name))
    (when (and (gethash name *families*)
               (or (not (equal existing parsed-cases))
                   (not (equal existing-meta (list predicate conc-name)))))
      (warn 'variant-redefinition-warning
            :format-control "Redefining variant ~S; recompile dependent matches"
            :format-arguments (list name)))
    (let ((functions (make-hash-table :test 'eq)))
      (when predicate
        (when (and (fboundp predicate) (not (member predicate owned)))
          (error "Variant function name already defined: ~S" predicate))
        (setf (gethash predicate functions) t))
      (dolist (spec parsed-cases)
        (let* ((case (car spec))
               (fields (cdr spec))
               (slots (mapcar #'first fields)))
          (unless (and (available-head-p case) (not (eq case name))
                       (or (member case owned)
                           (and (not (find-class case nil)) (not (gethash case *constructors*))))
                       (not (gethash case *expanders*))
                       (every #'variable-name-p slots)
                       (= (length (remove-duplicates slots :test #'string=)) (length slots)))
            (error "Invalid or occupied constructor or fields ~S" spec))
          (dolist (fn (cons case (mapcar (lambda (s) (reader-name case conc-name s)) slots)))
            (when (or (gethash fn functions)
                      (and (fboundp fn) (not (member fn owned))))
              (error "Variant function name already defined: ~S" fn))
            (setf (gethash fn functions) t))))))
  (setf (gethash name *pending-families*) parsed-cases))

(defun publish-variant (name predicate conc-name parsed-cases)
  (let ((old-cases (gethash name *families*))
        (old-owned (gethash name *variant-owned-symbols*))
        (new-owned (variant-owned-symbols predicate conc-name parsed-cases)))
    (dolist (old-spec old-cases)
      (let ((old-case (car old-spec)))
        (remhash old-case *constructors*)
        (remhash old-case *constructor-functions*)
        (remhash old-case *constructor-field-types*)))
    (dolist (sym old-owned)
      (unless (member sym new-owned)
        (when (fboundp sym) (fmakunbound sym))))
    (dolist (spec parsed-cases)
      (let* ((case (car spec))
             (fields (cdr spec))
             (slots (mapcar #'first fields))
             (types (mapcar #'second fields)))
        (setf (gethash case *constructor-functions*) (fdefinition case)
              (gethash case *constructor-field-types*) types
              (gethash case *constructors*)
              (cons name (mapcar (lambda (s) (reader-name case conc-name s)) slots)))))
    (setf (gethash name *families*) parsed-cases
          (gethash name *family-options*) (list predicate conc-name)
          (gethash name *variant-owned-symbols*) new-owned)
    (remhash name *pending-families*)
    name))

(defmacro zk:defvariant (header &rest cases)
  ;; DEFSTRUCT already supplies compile/load/execute semantics. Emitting its
  ;; definitions here puts executable constructors and readers in the FASL.
  (multiple-value-bind (name predicate conc-name) (parse-variant-header header)
    (let ((parsed-cases (parse-variant-cases cases))
          (raw-fn-var (gensym "RAW-CTOR")))
      `(progn
         (eval-when (:compile-toplevel :load-toplevel :execute)
           (validate-variant ',name ',predicate ',conc-name ',parsed-cases))
         (defstruct (,name (:%variant ,(mapcar #'car parsed-cases))
                           (:constructor nil) (:predicate ,predicate) (:copier nil)))
         ,@(loop for (case . fields) in parsed-cases
                 for slots = (mapcar #'first fields)
                 for types = (mapcar #'second fields)
                 for typed-p = (some (lambda (ty) (not (eq ty t))) types)
                 for conc-opt = (unless (eq conc-name :default) `((:conc-name ,conc-name)))
                 collect
                   `(defstruct (,case (:%variant nil) (:include ,name)
                                      ,@conc-opt
                                      (:constructor ,case ,slots)
                                      (:predicate nil) (:copier nil))
                      ,@(loop for slot in slots for ty in types
                              collect (list slot nil :read-only t :type ty)))
                 when typed-p
                   collect
                     `(eval-when (:compile-toplevel :load-toplevel :execute)
                        (let ((,raw-fn-var (fdefinition ',case)))
                          (setf (fdefinition ',case)
                                (lambda ,slots
                                  ,@(loop for slot in slots for ty in types
                                          unless (eq ty t)
                                            collect `(check-type ,slot ,ty))
                                  (funcall ,raw-fn-var ,@slots))))))
         (eval-when (:compile-toplevel :load-toplevel :execute)
           (publish-variant ',name ',predicate ',conc-name ',parsed-cases))))))

(defun register-pattern-expander (name function)
  (setf (gethash name *expanders*) function)
  name)

(defun install-pattern (name function)
  (unless (and (available-head-p name) (not (gethash name *constructors*)))
    (error "Pattern head ~S is reserved or already a constructor" name))
  (register-pattern-expander name function))

(defmacro zk:defpattern (name lambda-list &body forms)
  (let* ((whole (gensym "PATTERN")) (env (gensym "ENV"))
         (rest lambda-list) (args nil) (bindings nil))
    (loop while rest do
      (let ((item (pop rest)))
        (if (member item '(&whole &environment))
            (let ((var (pop rest)))
              (unless (variable-name-p var) (error "Invalid expander binding ~S" var))
              (push (list var (if (eq item '&whole) whole env)) bindings))
            (push item args))))
    `(eval-when (:compile-toplevel :load-toplevel :execute)
       (install-pattern ',name
         (lambda (,whole ,env)
           (declare (ignorable ,env))
           (let ,bindings
             (destructuring-bind ,(nreverse args) (cdr ,whole) ,@forms)))))))

(defun zk:pattern-expand-1 (pattern &optional environment)
  (let ((fn (and (consp pattern) (gethash (car pattern) *expanders*))))
    (if fn (values (funcall fn pattern environment) t) (values pattern nil))))

;;; Built-in structural pattern expanders (LIST, LIST*, ZK:KEYS)
(register-pattern-expander 'list
  (lambda (pattern env)
    (declare (ignore env))
    (reduce (lambda (x acc) (list 'cons x acc)) (cdr pattern) :from-end t :initial-value ''nil)))

(register-pattern-expander 'list*
  (lambda (pattern env)
    (declare (ignore env))
    (let ((args (cdr pattern)))
      (when (null args) (error "Empty LIST* pattern"))
      (let ((tail (car (last args))))
        (dolist (x (reverse (butlast args)) tail)
          (setf tail (list 'cons x tail)))))))

(defun keys-entry-form (entry env)
  (unless (and (consp entry) (list-length entry) (= (length entry) 2))
    (error "Expected (key pattern) in ZK:KEYS, got ~S" entry))
  (destructuring-bind (key child) entry
    (let ((key-form
            (cond ((or (stringp key) (numberp key) (characterp key) (keywordp key)
                       (member key '(nil t)))
                   (list 'quote key))
                  ((and (consp key) (eq (car key) 'quote) (= (length key) 2)) key)
                  ((and (consp key) (eq (car key) :const))
                   (arity key 1)
                   (resolve-constant-form (second key) env))
                  (t (error "Expected constant key, got ~S" key))))
          (table (gensym "TABLE")))
      (if (and (consp child) (eq (car child) :values))
          (progn
            (arity child 2)
            `(zk:view (lambda (,table) (gethash ,key-form ,table)) ,child))
          `(zk:view (lambda (,table) (gethash ,key-form ,table))
                    (:values ,child (type (not null))))))))

(register-pattern-expander 'zk:keys
  (lambda (pattern env)
    `(and (type hash-table)
          ,@(loop for entry in (cdr pattern)
                  collect (keys-entry-form entry env)))))

(defun make-dispatch-index (keys)
  ;; The FASL contains its ordered keys as ordinary Lisp constants.
  ;; Materialize the hash index once in the consuming runtime.
  (let ((table (make-hash-table :test 'eql :size (length keys))))
    (loop for key in keys for ordinal from 0 do (setf (gethash key table) ordinal))
    table))

(defun make-result-table (keys values)
  (let ((table (make-hash-table :test 'eql :size (length keys))))
    (mapc (lambda (key value) (setf (gethash key table) value)) keys values)
    table))

(defun zk-bits:octets (&rest bytes)
  (make-array (length bytes) :element-type '(unsigned-byte 8) :initial-contents bytes))

(defstruct (input-span (:constructor make-input-span
                                      (backing offset start end unit source &optional origin boundaries))
                      (:copier nil) (:predicate input-span-p))
  (backing nil :read-only t)
  (offset 0 :read-only t)
  (start 0 :read-only t)
  (end 0 :read-only t)
  (unit nil :read-only t)
  (source nil :read-only t)
  (origin nil :read-only t)
  (boundaries nil :read-only t))

(defun octet-vector-p (value) (typep value '(vector (unsigned-byte 8))))

(defun vector-span-unit (vector)
  (cond ((stringp vector) :character)
        ((bit-vector-p vector) :bit)
        ((octet-vector-p vector) :byte)
        (t :element)))

(defun slice-vector-span (vector start end)
  (make-input-span (subseq vector start end) 0 start end (vector-span-unit vector) (gensym "SOURCE")))

(defun zk-text:snapshot (input)
  (cond ((input-span-p input) input)
        ((vectorp input) (slice-vector-span input 0 (length input)))
        (t (error "Expected a vector or span, got ~S" input))))

(defun zk-text:span-source (span) (input-span-source span))
(defun zk-text:span-start (span) (input-span-start span))
(defun zk-text:span-end (span) (input-span-end span))
(defun zk-text:span-unit (span) (input-span-unit span))

(defun span-length (span) (- (input-span-end span) (input-span-start span)))

(defun zk-text:span-slice (span start &optional (end (span-length span)))
  (unless (and (integerp start) (integerp end) (<= 0 start end (span-length span)))
    (error "Invalid span range [~S, ~S) for length ~D" start end (span-length span)))
  (make-input-span (input-span-backing span) (+ start (input-span-offset span))
                   (+ start (input-span-start span)) (+ end (input-span-start span))
                   (input-span-unit span) (input-span-source span) (input-span-origin span)
                   (input-span-boundaries span)))

(defun octet-bit (backing position)
  (multiple-value-bind (index bit-index) (floor position 8)
    (ldb (byte 1 (- 7 bit-index)) (aref backing index))))

(defun zk-text:span-value (span)
  (let ((offset (input-span-offset span)) (backing (input-span-backing span))
        (length (span-length span)))
    (if (and (eq (input-span-unit span) :bit) (not (bit-vector-p backing)))
        (let ((result (make-array length :element-type 'bit)))
          (dotimes (index length result)
            (setf (sbit result index) (octet-bit backing (+ offset index)))))
        (subseq backing offset (+ offset length)))))

(defun zk-text:span-origin-byte-range (span)
  (let ((start (input-span-start span)) (end (input-span-end span)))
    (cond ((eq (input-span-unit span) :byte) (values (input-span-source span) start end))
          ((and (eq (input-span-unit span) :bit) (input-span-origin span)
                (zerop (mod start 8)) (zerop (mod end 8)))
           (values (input-span-origin span) (floor start 8) (floor end 8)))
          ((input-span-boundaries span)
           (let* ((offset (input-span-offset span)) (edges (input-span-boundaries span)))
             (values (input-span-origin span) (aref edges offset)
                     (aref edges (+ offset (span-length span))))))
          (t (values nil nil nil)))))

(defun bit-layout-size (source)
  (cond ((input-span-p source)
         (case (input-span-unit source)
           (:bit (span-length source))
           (:byte (* 8 (span-length source)))))
        ((octet-vector-p source) (* 8 (length source)))))

(defun bit-field (source position limit kind width order extract-p)
  (let* ((span-p (input-span-p source))
         (bits-p (and span-p (eq (input-span-unit source) :bit)))
         (backing (if span-p (input-span-backing source) source))
         (offset (if span-p (* (if bits-p 1 8) (input-span-offset source)) 0))
         (absolute (if span-p (* (if bits-p 1 8) (input-span-start source)) 0))
         (end (case kind
                ((:rest :rest-bits) limit)
                (:bytes (when (typep width '(integer 0 *)) (+ position (* width 8))))
                (otherwise (when (typep width '(integer 1 *)) (+ position width)))))
         (byte-field-p (member kind '(:bytes :rest)))
         (start-bit (+ offset position))
         (start-absolute (+ absolute position)))
    (unless (and end (<= end limit)
                 (or span-p (<= end (* 8 (length source))))
                 (or (not (or byte-field-p (eq order :le)))
                     (and (zerop (mod start-absolute 8)) (zerop (mod (- end position) 8)))))
      (return-from bit-field (values nil nil)))
    (values
     (when extract-p
       (cond
         (byte-field-p
          (let ((start (floor start-bit 8)) (stop (floor (+ offset end) 8)))
            (cond ((bit-vector-p backing)
                   (let* ((bytes (make-array (floor (- end position) 8) :element-type '(unsigned-byte 8)))
                          (segment-start (floor start-absolute 8)))
                     (dotimes (index (length bytes))
                       (let ((value 0) (base (+ start-bit (* 8 index))))
                         (dotimes (bit-index 8)
                           (setf value (logior (ash value 1) (bit backing (+ base bit-index)))))
                         (setf (aref bytes index) value)))
                     (if order (make-input-span bytes 0 segment-start (+ segment-start (length bytes))
                                                :byte (input-span-source source)) bytes)))
                  (order
                   (let* ((data (if span-p backing (subseq backing start stop)))
                          (from (floor start-absolute 8)))
                     (make-input-span data (if span-p start 0) from (+ from (- stop start)) :byte
                                      (if span-p (input-span-source source) (gensym "SOURCE")))))
                  (t (subseq backing start stop)))))
         ((eq kind :rest-bits)
          (let* ((start (floor start-bit 8)) (stop (ceiling (+ offset end) 8))
                 (source-id (if span-p (input-span-source source) (gensym "SOURCE")))
                 (data (if span-p backing (subseq backing start stop))))
            (make-input-span data (if span-p start-bit (mod start-bit 8))
                             start-absolute (+ absolute end) :bit source-id
                             (if bits-p (input-span-origin source) source-id))))
         (t (zk::%bit-integer backing start-bit width (eq order :le) (eq kind :i)))))
     end)))

(defun load-match-expander (form environment)
  (let ((*package* (find-package :zk-pattern-compiler))
        (*readtable* (copy-readtable nil))
        (*read-base* 10)
        (*read-suppress* nil)
        (*macroexpand-hook* #'funcall))
    (zk::%load-match-compiler))
  (funcall (macro-function (car form)) form environment))

(dolist (name '(zk:match zk:ematch zk:cmatch
                zk:if-match zk:when-match zk:unless-match
                zk:let-match zk:let-match*
                zk-lex:lexmatch zk-lex:lexscan))
  (setf (macro-function name) #'load-match-expander))

(in-package #:cl-user)
