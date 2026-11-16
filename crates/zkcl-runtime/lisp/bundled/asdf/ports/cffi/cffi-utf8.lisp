;;;; UTF-8 encoding for CFFI's foreign-pointer storage on zkcl.

(in-package #:zkcl-cffi-port)

(defun foreign-utf8-octet-count (string start end maximum)
  ;; Babel's counter uses a nonpositive maximum for an unbounded prefix.
  (zk-ffi:utf8-prefix string start end (and (plusp maximum) maximum)))

(defun encode-foreign-utf8 (string start end pointer byte-offset)
  (zk-ffi:write-utf8 (cffi-sys::native-pointer pointer) string start end byte-offset))

;; CFFI creates its own concrete mappings in strings.lisp. Its UTF-8 aliases
;; share this object; ordinary Babel string/vector mappings are independent.
(let ((mapping (babel-encodings:lookup-mapping cffi::*foreign-string-mappings* :utf-8)))
  (setf (babel-encodings:octet-counter mapping) #'foreign-utf8-octet-count
        (babel-encodings:encoder mapping) #'encode-foreign-utf8))
