;; Bootstrap methods keep a VM entry accepting a next-method continuation.
;; METHOD-FUNCTION exposes the AMOP calling convention to portable Lisp.
(defvar zk-mop::*method-call* nil)
(defvar zk-mop::*computing-invocations* nil)
(defvar zk-mop::*method-definition* nil)
(defvar zk-mop::*installing-accessor* nil)
(defvar zk-mop::*updating-method-links* nil)
(defvar zk-mop::*default-method-lambda* nil)
(defvar zk-mop::*file-generics* nil)
(defvar zk-mop::*defining-generic* nil)
(defvar zk-mop::*ensuring-generic* nil)
(defvar zk-mop::*refreshing-generics* nil)

(defun zk-mop:classp (object) (typep object 'class))

(defun zk-mop::%next-object (next)
  (if (functionp next)
      (zk-mop::%synthetic-method
       (lambda (arguments tail) (declare (ignore tail)) (apply next nil nil arguments))
       next)
      next))

(defun zk-mop::%next-methods (entries core)
  (let ((tail (when core
                (list (zk-mop::%synthetic-method
                       (lambda (arguments more) (declare (ignore more)) (funcall core arguments))
                       (lambda (next has &rest args) (declare (ignore next has)) (funcall core args)))))))
    (if entries (append (mapcar #'car entries) tail) tail)))

(defgeneric zk-mop:method-function (method))
(defmethod zk-mop:method-function ((method standard-method))
  (multiple-value-bind (public fast) (zk-mop::%method-entries method)
    (or public
        (zk-mop::%remember-method-function method
         (lambda (arguments next-methods)
          (apply fast
                 (lambda (&rest changed)
                   (let ((actual (or changed arguments)))
                     (if next-methods
                         (funcall (zk-mop:method-function (car next-methods)) actual (cdr next-methods))
                         (apply #'no-next-method (zk-mop:method-generic-function method) method actual))))
                 next-methods arguments))))))

(defun zk-mop::%method-entry (function)
  (lambda (next next-methods &rest arguments)
    (let ((zk-mop::*method-call* (cons arguments next)))
      (funcall function arguments next-methods))))

(defgeneric zk-mop:make-method-lambda (generic-function method lambda-expression environment))
(defmethod zk-mop:make-method-lambda ((generic standard-generic-function) (method standard-method) expression environment)
  (declare (ignore generic method environment))
  (let ((arguments (gensym "ARGS")) (next (gensym "NEXT-METHODS"))
        (continuation (gensym "CONTINUATION")) (changed (gensym "CHANGED")))
    (let ((processed
      `(lambda (,arguments ,next)
         (let ((,continuation (and (eq (car zk-mop::*method-call*) ,arguments) (cdr zk-mop::*method-call*))))
           (flet ((next-method-p () (not (null ,next)))
                  (call-next-method (&rest ,changed)
                    (cond (,continuation (apply ,continuation ,changed))
                          (,next (funcall (zk-mop:method-function (car ,next))
                                          (or ,changed ,arguments) (cdr ,next)))
                          (t (error "No next method.")))))
             (apply (function ,expression) ,arguments))))))
      (when zk-mop::*default-method-lambda*
        (setf (car zk-mop::*default-method-lambda*) expression
              (cdr zk-mop::*default-method-lambda*) processed))
      (values processed nil))))

(defgeneric zk-mop:add-direct-subclass (superclass subclass))
(defmethod zk-mop:add-direct-subclass ((superclass class) (subclass class))
  (zk-mop::%subclass-link superclass subclass t))
(defgeneric zk-mop:remove-direct-subclass (superclass subclass))
(defmethod zk-mop:remove-direct-subclass ((superclass class) (subclass class))
  (zk-mop::%subclass-link superclass subclass nil))
(defun zk-mop::%update-subclass-links (class old new)
  (dolist (parent old)
    (unless (member parent new) (zk-mop:remove-direct-subclass parent class)))
  (dolist (parent new)
    (unless (member parent old) (zk-mop:add-direct-subclass parent class))))

(defclass zk-mop::%method-combination (method-combination)
  ((zk-mop::spec :initarg :spec)))

(defclass zk-mop::%long-method-combination (zk-mop::%method-combination)
  ((zk-mop::arguments :initarg :arguments)))

(defgeneric zk-mop:method-combination-name (combination))
(defmethod zk-mop:method-combination-name ((combination zk-mop::%method-combination))
  (first (slot-value combination 'zk-mop::spec)))

(defgeneric zk-mop:method-combination-arglist (combination))
(defmethod zk-mop:method-combination-arglist ((combination zk-mop::%method-combination))
  (values nil nil))
(defmethod zk-mop:method-combination-arglist ((combination zk-mop::%long-method-combination))
  (values (copy-tree (slot-value combination 'zk-mop::arguments)) t))

(defgeneric zk-mop:find-method-combination (generic-function method-combination-type-name method-combination-options))
(defmethod zk-mop:find-method-combination ((gf standard-generic-function) name options)
  (declare (ignore gf))
  (multiple-value-bind (spec arguments long-p) (zk-mop::%check-combination (cons name (copy-list options)))
    (if long-p
        (make-instance 'zk-mop::%long-method-combination :spec spec :arguments arguments)
        (make-instance 'zk-mop::%method-combination :spec spec))))

(defgeneric zk-mop:generic-function-method-combination (generic-function))
(defmethod zk-mop:generic-function-method-combination ((gf standard-generic-function))
  (or (zk-mop::%gf-combination gf)
      (let ((spec (nth-value 2 (zk-mop::%gf-parameters gf))))
        (zk-mop::%remember-combination gf (zk-mop:find-method-combination gf (car spec) (cdr spec))))))

(defun zk-mop::%class-designator (value)
  (etypecase value (class value) (symbol (find-class value))))

(defgeneric zk-mop:compute-effective-method (generic-function method-combination methods))
(defmethod zk-mop:compute-effective-method ((gf standard-generic-function) (combination method-combination) methods)
  (let ((spec (slot-value combination 'zk-mop::spec)))
    (if (eq (car spec) 'standard)
        (let ((around nil) (before nil) (primary nil) (after nil))
          (dolist (method methods)
            (let ((q (method-qualifiers method)))
              (cond ((null q) (push method primary))
                    ((equal q '(:around)) (push method around))
                    ((equal q '(:before)) (push method before))
                    ((equal q '(:after)) (push method after))
                    (t (error 'program-error)))))
          (unless primary
            (return-from zk-mop:compute-effective-method (values '(error "No primary method.") nil)))
          (setq primary (nreverse primary) around (nreverse around) before (nreverse before))
          (let ((core (if primary `(call-method ,(car primary) ,(cdr primary)) '(error "No primary method."))))
            (when after (setf core `(multiple-value-prog1 ,core ,@(mapcar (lambda (m) `(call-method ,m)) after))))
            (when before (setf core `(progn ,@(mapcar (lambda (m) `(call-method ,m)) before) ,core)))
            (values (if around `(call-method ,(car around) ,(append (cdr around) (list `(make-method ,core)))) core) nil)))
        (values `(zk-mop::%run-selected ',gf ',methods cl::%%zk-em-arguments) nil))))

(defun zk-mop::%run-selected (gf methods argv)
  (multiple-value-bind (target args) (zk-mop::%prepare-generic-call gf argv methods)
    (apply target args)))

(defun zk-mop::%applicable-methods (gf arguments)
  (let* ((n (zk-mop::%check-dispatch gf arguments))
         (classes (loop repeat n for arg in arguments collect (class-of arg))))
    (multiple-value-bind (methods certain) (zk-mop:compute-applicable-methods-using-classes gf classes)
      (if certain methods (compute-applicable-methods gf arguments)))))

(defun zk-mop::%check-next-methods (gf previous arguments)
  (let ((actual (zk-mop::%applicable-methods gf arguments)))
    (unless (equal actual previous) (error 'program-error))
    (zk-mop::%check-dispatch gf arguments actual)))

(defun zk-mop::%effective-argument-form (parameters required positional body)
  ;; CLHS 7.6.6.2 matches required, optional, and rest/key sections independently.
  (let ((bindings nil) (args 'cl::%%zk-em-arguments) (index 0))
    (when (eq (first parameters) '&whole)
      (unless (rest parameters) (error 'program-error))
      (push (list (second parameters) args) bindings)
      (setq parameters (cddr parameters)))
    (loop while (and parameters (not (member (car parameters) lambda-list-keywords)))
          do (push (list (pop parameters) (when (< index required) `(nth ,index ,args))) bindings)
             (incf index))
    (when (eq (car parameters) '&optional)
      (pop parameters)
      (setq index required)
      (loop while (and parameters (not (member (car parameters) lambda-list-keywords)))
            do (let ((parameter (pop parameters)))
                 (destructuring-bind (var &optional init supplied)
                   (if (consp parameter) parameter (list parameter))
                 (let ((present (and (< index positional) `(nthcdr ,index ,args))))
                   (push (list var `(if ,present (nth ,index ,args) ,init)) bindings)
                   (when supplied (push (list supplied `(not (null ,present))) bindings)))))
               (incf index)))
    (when parameters
      (unless (member (car parameters) '(&rest &key &aux)) (error 'program-error))
      (when (and (member '&key parameters) (not (member '&allow-other-keys parameters)))
        (let ((aux (position '&aux parameters)))
          (setq parameters (append (subseq parameters 0 aux) '(&allow-other-keys)
                                   (when aux (nthcdr aux parameters))))))
      (setq body `(apply (function (lambda ,parameters ,body))
                         ,(unless (eq (car parameters) '&aux) `(nthcdr ,positional ,args)))))
    `(let* ,(nreverse bindings) ,body)))

(defun zk-mop::%long-effective-argument-form (gf parameters variables binder body)
  (multiple-value-bind (required known spec positional) (zk-mop::%gf-parameters gf)
    (declare (ignore known spec))
    ;; Bind user names only in the sampler (where defaults can refer to earlier
    ;; parameters), not around BODY, which is evaluated in the null lexical environment.
    `(flet ((,binder (cl::%%zk-em-arguments)
              ,(zk-mop::%effective-argument-form
                parameters required positional `(vector ,@variables))))
       (let ((cl::%%zk-em-argument-values nil))
         (declare (ignorable cl::%%zk-em-argument-values))
         ,body))))

(defun zk-mop::%effective-option-form (gf body options)
  (let ((seen nil) (arguments nil) (generic nil))
    (dolist (option options)
      (when (member (first option) seen) (error 'program-error))
      (push (first option) seen)
      (ecase (first option)
        (:arguments (setq arguments (rest option)))
        (:generic-function (destructuring-bind (name) (rest option) (setq generic name)))))
    (when arguments
      (multiple-value-bind (required known spec positional) (zk-mop::%gf-parameters gf)
        (declare (ignore known spec))
        (setq body (zk-mop::%effective-argument-form arguments required positional body))))
    (if generic `(let ((,generic cl::%%zk-generic)) ,body) body)))

(defun zk-mop::%invoke-protocol (gf arguments)
  (multiple-value-bind (entry methods)
      (let* ((zk-mop::*computing-invocations* (cons gf zk-mop::*computing-invocations*))
             (applicable (zk-mop::%applicable-methods gf arguments)))
        (when applicable
          (zk-mop::%check-dispatch gf arguments applicable)
          (multiple-value-bind (form options)
              (zk-mop:compute-effective-method gf (zk-mop:generic-function-method-combination gf) applicable)
            (values (zk-mop::%compile-effective (zk-mop::%effective-option-form gf form options))
                    applicable))))
    (if entry
        (funcall entry arguments gf methods #'zk-mop::%check-next-methods entry)
        (apply #'no-applicable-method gf arguments))))

(defgeneric zk-mop:compute-discriminating-function (generic-function))
(defmethod zk-mop:compute-discriminating-function ((gf standard-generic-function))
  (let ((function (lambda (&rest args) (zk-mop::%invoke-protocol gf args))))
    (let ((refresh (first zk-mop::*refreshing-generics*)))
      (when (eq gf (car refresh)) (setf (cdr refresh) function)))
    function))

(defun zk-mop::%publish-discriminator (generic function)
  (zk-mop::%install-discriminator generic function)
  ;; A completed nested refresh owns the installed function. Its caller may
  ;; still be computing from an older method table.
  (dolist (older zk-mop::*refreshing-generics* generic)
    (when (eq generic (car older)) (setf (car older) nil))))

(defun zk-mop::%defer-discriminator (generic)
  ;; Run user code once the unwind has completed, so a second nonlocal exit
  ;; from COMPUTE-DISCRIMINATING-FUNCTION cannot replace the original one.
  (zk-mop::%publish-discriminator generic
    (lambda (&rest arguments)
      (if (assoc generic zk-mop::*refreshing-generics* :test #'eq)
          (zk-mop::%invoke-protocol generic arguments)
          (progn (zk-mop::%refresh-generic generic) (apply generic arguments))))))

(defun zk-mop::%refresh-generic (generic)
  (let* ((refresh (list generic))
         (zk-mop::*refreshing-generics* (cons refresh zk-mop::*refreshing-generics*)))
    (unwind-protect
        (let ((fn (zk-mop:compute-discriminating-function generic)))
          (when (car refresh)
            (let ((combination (zk-mop::%gf-combination generic)))
              (zk-mop::%publish-discriminator generic
                (unless (and (eq fn (cdr refresh))
                             (or (null combination)
                                 (let ((class (class-of combination)))
                                   (or (eq class (find-class 'zk-mop::%method-combination))
                                       (eq class (find-class 'zk-mop::%long-method-combination))))))
                  fn)))))
      (when (car refresh) (zk-mop::%defer-discriminator generic)))))

(defmethod shared-initialize ((gf standard-generic-function) slot-names &rest initargs
                              &key (name (zk-mop:generic-function-name gf))
                                   (lambda-list nil lambda-p)
                                   (argument-precedence-order (unless lambda-p (zk-mop:generic-function-argument-precedence-order gf)))
                                   (method-class (zk-mop:generic-function-method-class gf))
                                   (declarations (zk-mop:generic-function-declarations gf))
                                   (documentation (zk-mop:generic-function-documentation gf))
                                   (method-combination nil combination-p))
  (declare (ignore initargs slot-names))
  (zk-mop::%ensure-function-storage gf)
  (let* ((redefining (and zk-mop::*defining-generic* (equal name zk-mop::*defining-generic*)))
         (spec (cond (combination-p method-combination)
                     (redefining '(standard))
                     (t (nth-value 2 (zk-mop::%gf-parameters gf)))))
         (combination (cond ((typep spec 'method-combination) spec)
                            ((and (not combination-p) (not redefining) (zk-mop::%gf-combination gf)))
                            (t (zk-mop:find-method-combination gf (car spec) (cdr spec))))))
    (zk-mop::%configure-generic gf name lambda-list lambda-p argument-precedence-order combination
      (if (typep combination 'zk-mop::%method-combination) (slot-value combination 'zk-mop::spec)
          (nth-value 2 (zk-mop::%gf-parameters gf)))
      (zk-mop::%class-designator method-class) documentation declarations)
    (call-next-method)
    (zk-mop::%refresh-generic gf))
  gf)

(defmethod reinitialize-instance ((gf standard-generic-function) &rest initargs)
  (zk-mop::%ensure-function-storage gf)
  (call-next-method)
  (zk-mop::%notify-dependents gf initargs)
  gf)

(defmethod change-class :before ((gf generic-function) (class class) &rest initargs)
  (declare (ignore class initargs))
  (zk-mop::%ensure-function-storage gf))

(defmethod update-instance-for-different-class :around
    ((previous generic-function) (current generic-function) &rest initargs)
  (declare (ignore previous initargs))
  (zk-mop::%defer-discriminator current)
  (call-next-method))

(defmethod shared-initialize ((m standard-method) names &rest options
                              &key (qualifiers nil) (lambda-list nil lambda-p)
                                   (specializers nil specs-p) (function nil function-p)
                                   documentation slot-definition)
  (declare (ignore options names))
  (unless (and lambda-p specs-p function-p) (error 'program-error))
  (when (and (typep m 'zk-mop:standard-accessor-method) (not (typep slot-definition 'zk-mop:direct-slot-definition)))
    (error 'program-error))
  (call-next-method)
  (let ((definition (and (eq function (first zk-mop::*method-definition*)) zk-mop::*method-definition*)))
    (zk-mop::%configure-method m qualifiers lambda-list specializers function
      (or (second definition) (zk-mop::%method-entry function)) documentation slot-definition (third definition))))

(defmethod reinitialize-instance ((m standard-method) &rest initargs)
  (declare (ignore m initargs)) (error 'program-error))

(defgeneric zk-mop:ensure-generic-function-using-class (generic-function function-name &rest initargs))
(defun zk-mop::%ensure-using (old name initargs)
  (let* ((class (zk-mop::%class-designator (getf initargs :generic-function-class
                                               (if old (class-of old) 'standard-generic-function))))
         (options (loop for (key value) on initargs by #'cddr
                        unless (member key '(:generic-function-class :environment))
                        append (list (if (eq key :declare) :declarations key) value))))
    (unless (subtypep class (find-class 'generic-function)) (error 'program-error))
    (zk-mop::%check-environment (getf initargs :environment))
    (let ((mc (getf options :method-class)))
      (when mc (setf (getf options :method-class) (zk-mop::%class-designator mc))))
    (when (and old (not (eq (class-of old) class)))
      ;; The ANSI entry point permits compatible class changes. Direct calls
      ;; to the MOP generic function require matching classes.
      (unless (and (eq old (car zk-mop::*ensuring-generic*))
                   (equal name (cdr zk-mop::*ensuring-generic*)))
        (error 'program-error))
      (let ((zk-mop::*ensuring-generic* nil)
            (zk-mop::*defining-generic* nil))
        (change-class old class))
      (unless (eq (class-of old) class) (error 'program-error)))
    (let ((spec (getf options :method-combination)))
      (when (consp spec)
        (setf (getf options :method-combination)
              (zk-mop:find-method-combination (or old (zk-mop:class-prototype class)) (car spec) (cdr spec)))))
    (if old (progn (apply #'reinitialize-instance old :name name options) old)
        (apply #'make-instance class :name name options))))

(defmethod zk-mop:ensure-generic-function-using-class ((old null) name &rest options)
  (zk-mop::%ensure-using old name options))
(defmethod zk-mop:ensure-generic-function-using-class ((old generic-function) name &rest options)
  (zk-mop::%ensure-using old name options))

(defun zk-mop::%ensure-generic (name &rest initargs)
  (when (oddp (length initargs)) (error 'program-error))
  (let* ((old (zk-mop::%function-target name))
         (zk-mop::*ensuring-generic* (and old (cons old name))))
    (zk-mop::%install-generic name
      (apply #'zk-mop:ensure-generic-function-using-class old name initargs))))

(defun zk-mop::%defgeneric (name &rest options)
  ;; A removal hook can invoke the old function or leave nonlocally. All
  ;; methods remain congruent with its published signature throughout removal.
  (let* ((gf (zk-mop::%function-target name))
         (definition (and gf (zk-mop::%inline-definition gf nil))))
    (when definition
      (dolist (method (zk-mop:generic-function-methods gf))
        (when (eq definition (nth-value 2 (zk-mop::%method-entries method)))
          (remove-method gf method)
          (when (eq gf (zk-mop:method-generic-function method)) (error 'program-error))))
      (zk-mop::%retire-inline-definition gf definition)))
  (let ((zk-mop::*defining-generic* name))
    (apply #'zk-mop::%ensure-generic name options)))

(defmethod (setf zk-mop:generic-function-name) (name (gf standard-generic-function))
  (reinitialize-instance gf :name name)
  name)

(defun zk-mop::%load-method (name qualifiers lambda-list raw-specializers fast public documentation inline initargs)
  (let* ((gf (or (zk-mop::%function-target name)
                 (zk-mop::%ensure-generic name :lambda-list (zk-mop::%implicit-lambda lambda-list))))
         (method nil)
         (function
           (or public
               (lambda (arguments next-methods)
                 (apply fast (lambda (&rest changed)
                               (let ((actual (or changed arguments)))
                                 (if next-methods
                                     (funcall (zk-mop:method-function (car next-methods)) actual (cdr next-methods))
                                     (apply #'no-next-method gf method actual))))
                        (not (null next-methods)) arguments))))
         (zk-mop::*method-definition* (list function fast (and inline (zk-mop::%inline-definition gf t)))))
    (setf method (apply #'make-instance (zk-mop:generic-function-method-class gf)
                        :qualifiers qualifiers :lambda-list lambda-list
                        :specializers (zk-mop::%resolve-specializers raw-specializers)
                        :function function :documentation documentation initargs))
    (add-method gf method)
    method))

(defun zk-mop::%note-file-generic (name generic-class method-class)
  (push (list name generic-class method-class) zk-mop::*file-generics*))

(defmacro zk-mop::%define-method (name qualifiers lambda-list specializers expression fast-entry doc inline
                                  &optional declared-generic declared-method &environment env)
  (let* ((known (zk-mop::%function-target name))
         (declaration (assoc name zk-mop::*file-generics* :test #'equal))
         (generic-class (or declared-generic (second declaration)))
         (class (if generic-class (find-class generic-class)
                    (if known (class-of known) (find-class 'standard-generic-function))))
         (gf (if (and known (eq (class-of known) class)) known (zk-mop:class-prototype class)))
         (method-name (or declared-method (third declaration)))
         (default (and (not known) (not method-name)
                       (assoc :method-class (zk-mop:class-default-initargs class))))
         (method-class (cond (method-name (find-class method-name))
                             (default (zk-mop::%class-designator (funcall (third default))))
                             (t (zk-mop:generic-function-method-class gf))))
         (method (zk-mop:class-prototype method-class))
         (zk-mop::*default-method-lambda* (cons nil nil)))
    (multiple-value-bind (processed initargs) (zk-mop:make-method-lambda gf method expression env)
      ;; Compile the user's body once. The native entry is equivalent only if
      ;; the default method saw this exact source and its result was returned.
      (let ((native (and (eq expression (car zk-mop::*default-method-lambda*))
                         (eq processed (cdr zk-mop::*default-method-lambda*)))))
        `(zk-mop::%load-method ',name ',qualifiers ',lambda-list ,specializers
                               ,(when native fast-entry) ,(unless native `(function ,processed))
                               ,doc ,inline ',initargs)))))

(defgeneric zk-mop:reader-method-class (class slot &rest initialization-arguments))
(defmethod zk-mop:reader-method-class ((class class) slot &rest options)
  (declare (ignore class slot options)) (find-class 'zk-mop:standard-reader-method))
(defgeneric zk-mop:writer-method-class (class slot &rest initialization-arguments))
(defmethod zk-mop:writer-method-class ((class class) slot &rest options)
  (declare (ignore class slot options)) (find-class 'zk-mop:standard-writer-method))

(defun zk-mop::%install-one-accessor (class slot name writer)
  (let* ((slot-name (zk-mop:slot-definition-name slot))
         (lambda-list (if writer '(new-value object) '(object)))
         (gf (or (zk-mop::%function-target name) (zk-mop::%ensure-generic name :lambda-list lambda-list)))
         (entry (if writer
                    (lambda (next more new object)
                      (declare (ignore next more)) (setf (slot-value object slot-name) new))
                    (lambda (next more object)
                      (declare (ignore next more)) (slot-value object slot-name))))
         (public (if writer
                     (lambda (args next)
                       (declare (ignore next)) (funcall entry nil nil (first args) (second args)))
                     (lambda (args next)
                       (declare (ignore next)) (funcall entry nil nil (first args)))))
         (args (list :slot-definition slot :lambda-list lambda-list
                     :specializers (if writer (list (find-class t) class) (list class))
                     :qualifiers nil :function public))
         (method-class (apply (if writer #'zk-mop:writer-method-class #'zk-mop:reader-method-class) class slot args))
         ;; SHARED-INITIALIZE uses the native entry only if the method
         ;; initializer forwards this exact public function.
         (method (let ((zk-mop::*method-definition* (list public entry nil)))
                   (apply #'make-instance method-class args)))
         (zk-mop::*installing-accessor* (list method gf class slot)))
    (add-method gf method)))


(defun zk-mop:eql-specializer-object (specializer)
  (multiple-value-bind (object boundp) (zk-mop::%eql-specializer-object specializer)
    (cond (boundp object)
          ((typep specializer 'zk-mop:eql-specializer)
           (values (slot-unbound (class-of specializer) specializer 'zk-mop::object)))
          (t (error 'type-error :datum specializer :expected-type 'zk-mop:eql-specializer)))))

(defmethod initialize-instance ((s zk-mop:eql-specializer) &rest options &key (object nil object-p))
  (declare (ignore options))
  (unless object-p (error 'program-error))
  (call-next-method)
  (zk-mop::%initialize-eql-specializer s object))

(defun zk-mop::%current-method-update-p (change)
  (and (third change) (eq (second change) (zk-mop:method-generic-function (first change)))))

(defun zk-mop::%change-direct-method (specializer method adding)
  (let ((change (first zk-mop::*updating-method-links*)))
    ;; A :before or :around method may perform the opposite operation before
    ;; invoking this primary method. Explicit direct-method calls need no owner.
    (unless (and (eq method (first change))
                 (eq adding (not (null (second change))))
                 (not (zk-mop::%current-method-update-p change)))
      (zk-mop::%change-relation specializer method nil adding))))

(defgeneric zk-mop:add-direct-method (specializer method))
(defmethod zk-mop:add-direct-method ((s zk-mop:specializer) (m method)) (zk-mop::%change-direct-method s m t))
(defgeneric zk-mop:remove-direct-method (specializer method))
(defmethod zk-mop:remove-direct-method ((s zk-mop:specializer) (m method)) (zk-mop::%change-direct-method s m nil))
(defgeneric zk-mop:specializer-direct-methods (specializer))
(defmethod zk-mop:specializer-direct-methods ((s zk-mop:specializer)) (zk-mop::%relations s nil))
(defgeneric zk-mop:specializer-direct-generic-functions (specializer))
(defmethod zk-mop:specializer-direct-generic-functions ((s zk-mop:specializer))
  (remove-duplicates (remove nil (mapcar #'zk-mop:method-generic-function (zk-mop:specializer-direct-methods s))) :test #'eq))

(defgeneric zk-mop:add-dependent (metaobject dependent))
(defgeneric zk-mop:remove-dependent (metaobject dependent))
(defgeneric zk-mop:map-dependents (metaobject function))
(defgeneric zk-mop:update-dependent (metaobject dependent &rest initargs))

(defmethod zk-mop:add-dependent ((owner class) dependent) (zk-mop::%change-relation owner dependent t t))
(defmethod zk-mop:add-dependent ((owner generic-function) dependent) (zk-mop::%change-relation owner dependent t t))
(defmethod zk-mop:remove-dependent ((owner class) dependent) (zk-mop::%change-relation owner dependent t nil))
(defmethod zk-mop:remove-dependent ((owner generic-function) dependent) (zk-mop::%change-relation owner dependent t nil))
(defmethod zk-mop:map-dependents ((owner class) fn) (mapc fn (zk-mop::%relations owner t)) nil)
(defmethod zk-mop:map-dependents ((owner generic-function) fn) (mapc fn (zk-mop::%relations owner t)) nil)
(defun zk-mop::%notify-dependents (owner args)
  (zk-mop:map-dependents owner (lambda (d) (apply #'zk-mop:update-dependent owner d args))))

(defmethod reinitialize-instance :after ((owner class) &rest args) (zk-mop::%notify-dependents owner args))

(defun zk-mop::%finish-method-update (gf method specializers adding)
  ;; Owner identity alone cannot distinguish two successive attachments of the
  ;; same method to the same generic function.
  (dolist (older zk-mop::*updating-method-links*)
    (when (eq method (first older)) (setf (third older) nil)))
  (let* ((change (list method (and adding gf) t))
         (zk-mop::*updating-method-links* (cons change zk-mop::*updating-method-links*))
         (finished nil))
    (unwind-protect
        (progn
          (dolist (s specializers)
            (unless (zk-mop::%current-method-update-p change) (return))
            (if adding (zk-mop:add-direct-method s method)
                       (zk-mop:remove-direct-method s method)))
          (setq finished t))
      (when (and (not finished) (zk-mop::%current-method-update-p change))
        (zk-mop::%discard-method gf method)
        ;; A previous discriminator may still close over a removed method, or
        ;; a nested refresh may have observed the addition we just discarded.
        (zk-mop::%defer-discriminator gf)))
    (when (zk-mop::%current-method-update-p change)
      (zk-mop::%refresh-generic gf)
      (when (zk-mop::%current-method-update-p change)
        (zk-mop::%notify-dependents gf (list (if adding 'add-method 'remove-method) method))))))

(defmethod add-method ((gf standard-generic-function) (m standard-method))
  (unless (eq gf (zk-mop:method-generic-function m))
    (unless (zk-mop::%check-method gf m)
      (reinitialize-instance gf :lambda-list (zk-mop::%implicit-lambda (zk-mop:method-lambda-list m))))
    (let* ((specs (zk-mop:method-specializers m))
           (accessor (and (eq m (first zk-mop::*installing-accessor*))
                          (eq gf (second zk-mop::*installing-accessor*))
                          zk-mop::*installing-accessor*)))
      (flet ((check-accessor-slot ()
               ;; A class redefinition in a hook supersedes the outer DEFCLASS.
               (when (and accessor
                          (not (member (fourth accessor) (zk-mop:class-direct-slots (third accessor)))))
                 (return-from add-method gf))))
        (let ((old (find-method gf (method-qualifiers m) specs nil)))
          (check-accessor-slot)
          (when old
            (remove-method gf old)
            (check-accessor-slot))))
      (zk-mop::%attach-method gf m)
      ;; A nested DEFCLASS can now find and remove this accessor from its hooks.
      (when accessor
        (zk-mop::%remember-method (third accessor) m)
        (setq zk-mop::*installing-accessor* nil))
      (zk-mop::%finish-method-update gf m specs t)))
  gf)

(defmethod remove-method ((gf standard-generic-function) (m standard-method))
  (when (eq (zk-mop:method-generic-function m) gf)
    (let ((specs (zk-mop:method-specializers m)))
      (zk-mop::%detach-method gf m)
      (zk-mop::%finish-method-update gf m specs nil)))
  gf)

(defmethod (setf class-name) (name (c standard-class))
  (if (zk-mop::%class-active-p c) (progn (reinitialize-instance c :name name) name) (call-next-method)))
(defmethod (setf class-name) (name (c zk-mop:funcallable-standard-class))
  (if (zk-mop::%class-active-p c) (progn (reinitialize-instance c :name name) name) (call-next-method)))
