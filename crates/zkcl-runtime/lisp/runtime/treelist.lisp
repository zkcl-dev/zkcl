;;;; Lisp helpers, macros, SETF support, and MAKE-LOAD-FORM methods for ZK-TREELIST.

(cl:defun (cl:setf zk-treelist:ref) (value mtl index)
  "Replace element at INDEX in MUTABLE-TREELIST MTL with VALUE and return VALUE."
  (zk-treelist:set! mtl index value))

(cl:defmacro zk-treelist:with-transient ((var cl:&optional initial) cl:&body body)
  "Bind VAR to a fresh MUTABLE-TREELIST (thawing INITIAL if supplied), run BODY, and freeze into a TREELIST."
  (cl:let ((init-sym (cl:gensym "INITIAL-")))
    `(cl:let* ((,init-sym ,initial)
               (,var (cl:if ,init-sym
                            (zk-treelist:thaw ,init-sym)
                            (zk-treelist:mutable-treelist))))
       ,@body
       (zk-treelist:freeze ,var))))

(cl:defmacro zk-treelist:do-elements ((var seq cl:&optional result) cl:&body body)
  "Iterate VAR across every element of TREELIST or MUTABLE-TREELIST SEQ, then return RESULT."
  (cl:let ((seq-sym (cl:gensym "TREELIST-"))
          (idx-sym (cl:gensym "INDEX-"))
          (len-sym (cl:gensym "LEN-")))
    `(cl:let* ((,seq-sym ,seq)
               (,len-sym (zk-treelist:length ,seq-sym)))
       (cl:dotimes (,idx-sym ,len-sym ,result)
         (cl:let ((,var (zk-treelist:ref ,seq-sym ,idx-sym)))
           ,@body)))))

(cl:defun zk-treelist:equal-p (a b cl:&key (test #'cl:eql))
  "Return T if A and B are treelist sequences of equal length whose elements satisfy TEST."
  (cl:let ((len (zk-treelist:length a)))
    (cl:when (cl:= len (zk-treelist:length b))
      (cl:dotimes (i len cl:t)
        (cl:unless (cl:funcall test (zk-treelist:ref a i) (zk-treelist:ref b i))
          (cl:return cl:nil))))))

(cl:defmethod cl:make-load-form ((object zk-treelist:treelist) cl:&optional environment)
  (cl:declare (cl:ignore environment))
  `(zk-treelist:treelist ,@(cl:loop for item in (zk-treelist:to-list object) collect `',item)))

(cl:defmethod cl:make-load-form ((object zk-treelist:mutable-treelist) cl:&optional environment)
  (cl:declare (cl:ignore environment))
  `(zk-treelist:mutable-treelist ,@(cl:loop for item in (zk-treelist:to-list object) collect `',item)))
