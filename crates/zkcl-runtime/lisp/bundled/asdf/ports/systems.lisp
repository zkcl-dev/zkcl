;;;; ASDF still selects the distribution (including explicitly supplied .asd
;;;; files). Install a backend only for the definition it was written for.
;;;; New upstream versions and users' native definitions run as supplied.
(in-package #:cl-user)

(export 'zk::*use-asdf-ports* :zk)
(defvar zk:*use-asdf-ports* t
  "Adapt supported upstream ASDF definitions for this runtime.")

(defparameter zk::*asdf-port-definitions*
  ;; System, byte count and FNV-1a of the Quicklisp 2026-01-01 definition.
  '(("trivial-features" 2698 #xc0fc75ace8ec2b69 "cffi/trivial-features.asd")
    ("trivial-garbage" 1119 #x14327ebf28474570 "trivial-garbage/trivial-garbage.asd"
     ("trivial-garbage.lisp" 18781 #xf35246b9d59300d0))
    ("bordeaux-threads" 5260 #x4d91f82d1631355c "bordeaux-threads/bordeaux-threads.asd"
     ("version.sexp" 24 #x490fabf7af3b0ac0))
    ;; Gray Streams selects its implementation in package.lisp. A distribution
    ;; can supply its own zkcl branch without changing the .asd.
    ("trivial-gray-streams" 375 #xea4f67a12bcc46d1 "trivial-gray-streams/trivial-gray-streams.asd"
     ("package.lisp" 3512 #x0616876fbe03288e))
    ("trivial-cltl2" 256 #x6648818f5bd08aad "package-backend.lisp"
     ("cltl2.lisp" 797 #x2795b8ee4fdd12dd))
    ("trivial-method-combinations" 354 #x653734d5e233812d "package-backend.lisp"
     ("trivial-method-combinations.lisp" 2656 #xa041c022f0800e40))
    #+zk-ffi ("cffi" 4244 #x14c6d76b8c316f2f "cffi/upstream.lisp")))

(defun zk::matching-asdf-definition-p (pathname size checksum)
  (with-open-file (input pathname :element-type '(unsigned-byte 8))
    (and (= size (file-length input))
         (= checksum (loop with hash = #xcbf29ce484222325
                           for byte = (read-byte input nil nil) while byte
                           do (setf hash (ldb (byte 64 0) (* (logxor hash byte) #x100000001b3)))
                           finally (return hash))))))

(defun zk::port-source-pathname (name)
  ;; ASDF compiles the embedded backends as ordinary source components.
  ;; A content-dependent pathname leaves cached sources and FASLs usable
  ;; across fresh runtimes, including concurrently running executables.
  (let* ((source (zk::%asdf-port-source name))
         (path (merge-pathnames name
                 (uiop:xdg-cache-home "zkcl" "ports" (format nil "~X/" (sxhash source))))))
    (unless (probe-file path)
      (ensure-directories-exist path)
      (uiop:with-staging-pathname (output path)
        (with-open-file (stream output :direction :output :if-exists :supersede
                                       :external-format :utf-8)
          (write-string source stream))))
    path))

(defun zk::load-asdf-port (name pathname)
  ;; Keep the selected .asd as SYSTEM-SOURCE-FILE. Relative component paths,
  ;; version files, secondary systems, and upstream tests belong to that tree.
  (let ((port (and zk:*use-asdf-ports*
                   (assoc name zk::*asdf-port-definitions* :test #'equal))))
    (when port
      (destructuring-bind (size checksum source &rest additional-files) (cdr port)
        (when (and (zk::matching-asdf-definition-p pathname size checksum)
                   (every (lambda (entry)
                            (destructuring-bind (relative size checksum) entry
                              (let ((file (probe-file (merge-pathnames relative pathname))))
                                (and file (zk::matching-asdf-definition-p file size checksum)))))
                          additional-files))
          (let ((*load-pathname* pathname)
                (*load-truename* (truename pathname)))
            (with-input-from-string (input (zk::%asdf-port-source source))
              (uiop:eval-input input)))
          t)))))

(defun zk::sysdef-native-features (name)
  ;; Platform keywords are present at runtime initialization, even in hosts
  ;; without foreign calls. TRIVIAL-FEATURES has no further runtime API.
  (when (and zk:*use-asdf-ports* (equal name "trivial-features"))
    (or (asdf/system-registry:registered-system name)
        (make-instance 'asdf:system :name name
          :in-order-to '((asdf:test-op (asdf:test-op "trivial-features-tests")))))))

(unless (member 'zk::sysdef-native-features asdf:*system-definition-search-functions*)
  (setf asdf:*system-definition-search-functions*
        (append asdf:*system-definition-search-functions* '(zk::sysdef-native-features))))
