
  (lambda (form environment)
    (declare (ignore environment))
    (labels
        ((short-p (rest)
           (or (null rest)
               (and (symbolp (first rest)) (not (null (first rest))))))
         (short-options (options operator documentation identity)
           (tagbody
            next
              (when options
                (let ((key (first options)))
                  (unless (keywordp key)
                    (error "DEFINE-METHOD-COMBINATION: invalid option ~S" key))
                  (unless (rest options)
                    (error "DEFINE-METHOD-COMBINATION: missing value for ~S" key))
                  (let ((value (second options)))
                    (cond ((eq key :operator)
                           (unless (symbolp value)
                             (error "DEFINE-METHOD-COMBINATION: :operator must name an operator"))
                           (setq operator value))
                          ((eq key :documentation)
                           (unless (stringp value)
                             (error "DEFINE-METHOD-COMBINATION: :documentation must be a string"))
                           (setq documentation value))
                          ((eq key :identity-with-one-argument)
                           (setq identity value))
                          (t
                           (error "DEFINE-METHOD-COMBINATION: unknown option ~S" key))))
                  (setq options (cddr options))
                  (go next))))
           (list operator documentation identity))
         (group-option (value)
           (and (keywordp value)
                (member value '(:description :order :required))))
         (split-patterns (items patterns)
           (tagbody
            next
              (when (and items (not (group-option (first items))))
                (setq patterns (cons (first items) patterns))
                (setq items (rest items))
                (go next)))
           (values (nreverse patterns) items))
         (group-options (items order required description)
           (tagbody
            next
              (when items
                (let ((key (first items)))
                  (unless (keywordp key)
                    (error "DEFINE-METHOD-COMBINATION: invalid method group option ~S" key))
                  (unless (rest items)
                    (error "DEFINE-METHOD-COMBINATION: missing value for ~S" key))
                  (let ((value (second items)))
                    (cond ((eq key :order) (setq order value))
                          ((eq key :required) (setq required value))
                          ((eq key :description) (setq description value))
                          (t
                           (error "DEFINE-METHOD-COMBINATION: unknown method group option ~S" key))))
                  (setq items (cddr items))
                  (go next))))
           (list order required description))
         (group-info (spec)
           (unless (and (consp spec) (symbolp (first spec)))
             (error "DEFINE-METHOD-COMBINATION: invalid method group specifier ~S" spec))
           (multiple-value-bind (patterns options)
               (split-patterns (rest spec) nil)
             (let ((parsed (group-options options :most-specific-first nil nil)))
               (values (first spec)
                       (list 'list
                             (list 'quote (first spec))
                             (cons 'list
                                   (mapcar (lambda (pattern) (list 'quote pattern))
                                           patterns))
                             (first parsed)
                             (list 'quote (second parsed))
                             (list 'quote (third parsed)))))))
         (special-options (items arguments generic-function)
           (tagbody
            next
              (when (and (consp (first items))
                         (keywordp (first (first items)))
                         (member (first (first items)) '(:arguments :generic-function)))
                (let ((option (first items)))
                  (if (eq (first option) :arguments)
                      (setq arguments (rest option))
                      (setq generic-function (second option)))
                  (setq items (rest items))
                  (go next))))
           (values arguments generic-function items))
         (split-declarations (items)
           (let ((declarations nil))
             (tagbody
              next
                (when (and (consp (first items))
                           (eq (first (first items)) 'declare))
                  (push (first items) declarations)
                  (setq items (rest items))
                  (go next)))
             (values (nreverse declarations) items)))
         (split-documentation (items)
           (if (and (stringp (first items)) (rest items))
               (values (first items) (rest items))
               (values nil items)))
          (argument-variables (parameters)
            (let ((mode :required)
                  (variables nil))
              (dolist (parameter parameters)
                (case parameter
                  ((&whole &rest &allow-other-keys))
                  (&optional (setq mode :optional))
                  (&key (setq mode :key))
                  (&aux (setq mode :aux))
                  (otherwise
                   (if (atom parameter)
                       (push parameter variables)
                       (let ((name (first parameter)))
                         (push (if (and (eq mode :key) (consp name))
                                   (second name)
                                   name)
                               variables)
                         (when (and (member mode '(:optional :key)) (third parameter))
                           (push (third parameter) variables)))))))
              (nreverse variables)))
          (argument-form-bindings (variables binder)
            (loop for variable in variables
                  for index from 0
                  collect
                  (list variable
                        `'(svref (or cl::%%zk-em-argument-values
                                    (setf cl::%%zk-em-argument-values
                                          (,binder cl::%%zk-em-arguments)))
                                 ,index))))
         (group-bindings (variables)
           (let ((index 0)
                 (bindings nil))
             (dolist (variable variables)
               (push (list variable
                           (list 'nth index 'cl::%%zk-group-values))
                     bindings)
               (setq index (1+ index)))
             (nreverse bindings)))
          (long-expansion (name rest)
           (let ((lambda-list (first rest))
                 (specs (second rest)))
             (unless (listp lambda-list)
               (error "DEFINE-METHOD-COMBINATION: the lambda list must be a list"))
             (unless (listp specs)
               (error "DEFINE-METHOD-COMBINATION: the method group specifiers must be a list"))
             (multiple-value-bind (arguments generic-function tail-options)
                 (special-options (cddr rest) nil nil)
               (multiple-value-bind (declarations forms)
                   (split-declarations tail-options)
                 (multiple-value-bind (documentation body)
                     (split-documentation forms)
                   (let ((variables nil)
                         (descriptors nil))
                     (dolist (spec specs)
                       (multiple-value-bind (variable descriptor)
                           (group-info spec)
                         (push variable variables)
                         (push descriptor descriptors)))
                      (let* ((bindings (group-bindings (nreverse variables)))
                             (argument-vars (argument-variables arguments))
                             (binder (and argument-vars (gensym "BIND-EFFECTIVE-ARGUMENTS")))
                             (argument-forms (argument-form-bindings argument-vars binder))
                             (generic-binding
                              (when generic-function
                                (list (list generic-function 'cl::%%zk-generic-function))))
                             (effective-form `(progn ,@body))
                             (effective-method-builder
                              `(lambda (cl::%%zk-group-values cl::%%zk-generic-function)
                                 (declare (ignorable cl::%%zk-generic-function))
                                 ,@declarations
                                 (let (,@bindings ,@argument-forms ,@generic-binding)
                                   ,(if argument-vars
                                        `(zk-mop::%long-effective-argument-form
                                          cl::%%zk-generic-function
                                          ',arguments ',argument-vars ',binder ,effective-form)
                                        effective-form)))))
                        `(cl::%zk-define-long-method-combination
                          ',name
                          (function
                           (lambda (cl::%%zk-options)
                             (apply (function
                                     (lambda ,lambda-list
                                       ,@declarations
                                       (list (list ,@(nreverse descriptors))
                                             (function ,effective-method-builder))))
                                    cl::%%zk-options)))
                          ',documentation ',arguments)))))))))
      (unless (symbolp (second form))
        (error "DEFINE-METHOD-COMBINATION: expects a symbol name"))
      (if (short-p (cddr form))
          (let* ((parsed (short-options (cddr form) (second form) nil nil))
                 (operator (first parsed))
                 (documentation (second parsed))
                 (identity (third parsed)))
            `(cl::%zk-define-short-method-combination
              ',(second form) ',documentation ',operator ',identity))
          (long-expansion (second form) (cddr form)))))
