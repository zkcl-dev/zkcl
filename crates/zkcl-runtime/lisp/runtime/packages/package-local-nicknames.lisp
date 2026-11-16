(in-package #:cl-user)

(export '(zk::add-package-local-nickname) :zk)

(define-condition zk::local-nickname-error (simple-condition package-error) ())

(defun zk::add-local-nickname-entry (entries nickname actual owner)
  (let* ((name (string nickname))
         (target (zk::%global-package actual))
         (old (assoc name entries :test #'string=)))
    (when (member name '("CL" "COMMON-LISP" "KEYWORD") :test #'string=)
      (cerror "Use the reserved name as a local nickname."
              'zk::local-nickname-error :package target
              :format-control "~S is a reserved package name." :format-arguments (list name)))
    (when (or (string= name (package-name owner))
              (member name (package-nicknames owner) :test #'string=))
      (cerror "Use the package's own name as a local nickname."
              'zk::local-nickname-error :package owner
              :format-control "~S already names ~A." :format-arguments (list name owner)))
    (when old
      (when (eq target (cdr old)) (return-from zk::add-local-nickname-entry entries))
      (restart-case
          (error 'zk::local-nickname-error :package target
                 :format-control "~S in ~A already names ~A." :format-arguments (list name owner (cdr old)))
        (zk::keep-old () :report "Keep the existing nickname."
          (return-from zk::add-local-nickname-entry entries))
        (zk::change-nick () :report "Replace the existing nickname.")))
    (acons name target (if old (remove old entries) entries))))

(defun zk:add-package-local-nickname (nickname actual &optional (package *package*))
  (let ((owner (find-package package)))
    (unless (and owner (package-name owner))
      (error 'package-error :package package))
    (zk::%replace-local-nicknames owner
      (zk::add-local-nickname-entry (zk:package-local-nicknames owner) nickname actual owner))))

(defun zk::%define-local-nicknames (owner specs)
  (let ((entries nil))
    (dolist (spec specs)
      (setf entries (zk::add-local-nickname-entry entries (first spec) (second spec) owner)))
    (zk::%replace-local-nicknames owner entries)))
