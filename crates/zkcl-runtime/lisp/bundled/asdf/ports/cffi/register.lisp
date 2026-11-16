;;;; Using the zkcl CFFI port (Quicklisp 2026-01-01)
;;;;
;;;; Start a zkcl runtime with foreign calls enabled. In a fresh Lisp image,
;;;; register the existing dependency source directories with ASDF, e.g.:
;;;;
;;;;   (require "asdf")
;;;;   (dolist (dir '(#p"/deps/alexandria-20241012-git/"
;;;;                  #p"/deps/babel-20260101-git/"
;;;;                  #p"/deps/iterate-release-d27d7ff4-git/"
;;;;                  #p"/deps/cl-sqlite-20190813-git/"))
;;;;     (push dir asdf:*central-registry*))
;;;;   (load "/path/to/zkcl/crates/zkcl-runtime/lisp/bundled/asdf/ports/cffi/register.lisp")
;;;;   (zkcl-cffi-port:register #p"/deps/cffi-20260101-git/")
;;;;   (asdf:load-system "sqlite")
;;;;
;;;; For CFFI alone, Alexandria and Babel suffice. After registration use
;;;; (asdf:load-system "cffi"). The two .asd files beside this loader select
;;;; zkcl's backend and its native platform features. The upstream CFFI and
;;;; trivial-features system definitions retain their implementation guards.
;;;; All CFFI frontend components are compiled from the supplied source tree.
;;;; To run the SQLite check, LOAD tests/lisp/ports/cffi/test-sqlite.lisp
;;;; from the repository root in the same image, with a system SQLite library available.
;;;;
;;;; A DEFCALLBACK name owns one native function pointer in this runtime.
;;;; Redefining it with the same canonical signature updates its Lisp body
;;;; for all native registrations. A different signature signals an error
;;;; and the previous definition remains callable. Use a new callback name
;;;; to register a different signature. Native callers can enter Lisp
;;;; synchronously during a foreign call on the runtime's owning thread.
;;;;
;;;; The port registers zkcl's UTF-8 prefix counter and bulk encoder with
;;;; Babel's ENCODER and OCTET-COUNTER accessors for CFFI's foreign buffers.
;;;; A positive limit selects the longest complete UTF-8 prefix that fits.
;;;; CFFI writes the terminator after the encoded bytes. Babel retains its
;;;; own string/vector mappings. LOAD tests/lisp/ports/cffi/test-utf8.lisp
;;;; from the repository root to check both interfaces.
;;;;
;;;; On zkcl, the original Babel 2026-01-01 UTF-8 counter returns (values 3 2)
;;;; for "abc" from 0 to 3 with a 3-byte limit. CFFI interprets the second
;;;; value as an exclusive character end. In a buffer prefilled with #x5a the
;;;; original foreign mapping writes 61 62 5A 00 (hex). The port returns
;;;; (values 3 3) and writes 61 62 63 00. Ordinary BABEL:STRING-TO-OCTETS
;;;; produces #(97 98 99) using Babel's original vector mapping.
;;;;
;;;; For an explicitly bounded native C string, use
;;;; (zk-ffi:write-c-string pointer string capacity). Capacity counts the
;;;; terminating NUL. A capacity of 1 writes only NUL, and 0 signals an error.
;;;; With CFFI:LISP-STRING-TO-FOREIGN, use an empty source range (:end equal
;;;; to :start) for a one-byte UTF-8 destination. CFFI 2026-01-01 passes zero
;;;; both for the one-byte buffer's payload limit and for the unrestricted
;;;; size query in FOREIGN-STRING-ALLOC. Babel defines zero as unrestricted.
;;;; With a nonempty range the write can exceed the caller's logical size.
;;;; zkcl checks each write against a managed allocation's physical capacity.
;;;; For example, with one allocated byte and source "x", the payload fits
;;;; and the separate terminator write signals a bounds error.

(require "asdf")

(defpackage #:zkcl-cffi-port
  (:use #:cl)
  (:export #:register))

(in-package #:zkcl-cffi-port)

(defparameter *port-directory* (uiop:pathname-directory-pathname *load-truename*))
(defvar *cffi-directory*)

(defun register (cffi-source-directory)
  "Select zkcl's ASDF definitions for CFFI and TRIVIAL-FEATURES in this image.
CFFI-SOURCE-DIRECTORY is an unmodified upstream CFFI source distribution."
  #-(and zkcl zk-ffi) (error "This CFFI port requires zkcl with foreign calls enabled.")
  (let ((directory (truename (uiop:ensure-directory-pathname cffi-source-directory))))
    (unless (probe-file (merge-pathnames "src/early-types.lisp" directory))
      (error "Cannot find upstream CFFI source under ~S." directory))
    (setf *cffi-directory* directory))
  (setf asdf:*central-registry*
        (cons *port-directory* (remove *port-directory* asdf:*central-registry* :test #'equal)))
  (values))

(in-package #:cl-user)
