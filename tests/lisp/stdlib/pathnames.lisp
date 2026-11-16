(in-package #:zkcl-stdlib-tests)

(defun path-segment-gen ()
  (zk-pbt:one-of
   (zk-pbt:elements "src" "lib" "alpha" "beta" "项目" "data")
   (zk-pbt:such-that
    (lambda (s)
      (and (plusp (length s))
           (not (find #\/ s))
           (not (find #\\ s))
           (not (find #\* s))
           (not (find #\? s))
           (not (find #\. s))
           (not (find (code-char 0) s))))
    (zk-pbt:ascii-strings :min 1 :max 8)
    :attempts 20)))

(defun posix-pathname-gen ()
  (zk-pbt:map-gen
   (lambda (parts)
     (destructuring-bind (abs-p dirs name type) parts
       (make-pathname
        :directory (when (or abs-p dirs)
                     (cons (if abs-p :absolute :relative) dirs))
        :name name
        :type (when name type)
        :version (when name :newest))))
   (zk-pbt:tuple (zk-pbt:booleans)
                 (zk-pbt:list-of (path-segment-gen) :min 0 :max 3)
                 (zk-pbt:one-of (zk-pbt:just nil) (path-segment-gen))
                 (zk-pbt:one-of (zk-pbt:just nil) (zk-pbt:elements "lisp" "txt" "fasl")))))

(zk-pbt:defproperty pathname-namestring-roundtrip ((p (posix-pathname-gen)))
  (let* ((text (namestring p))
         (reparsed (pathname text)))
    (zk-test:is (pathnamep reparsed))
    (zk-test:is (equal p reparsed))
    (zk-test:is-equal text (namestring reparsed)))
  t)

(zk-pbt:defproperty parse-namestring-slice-matches-subseq
    ((p (zk-pbt:such-that (lambda (x) (plusp (length (namestring x))))
                          (posix-pathname-gen)
                          :attempts 20))
     (prefix (zk-pbt:elements "" "前" "abc:"))
     (suffix (zk-pbt:elements "" "后" "xyz")))
  (let* ((core (namestring p))
         (wrapped (concatenate 'string prefix core suffix))
         (start (length prefix))
         (end (+ start (length core))))
    (multiple-value-bind (parsed pos)
        (parse-namestring wrapped nil *default-pathname-defaults* :start start :end end)
      (zk-test:is-equal (pathname core) parsed)
      ;; SBCL 2.6.9 returns slice-relative position when :start > 0 instead of bounding index END.
      #-sbcl (zk-test:is= end pos)
      #+sbcl (zk-test:is (or (= pos end) (= pos (- end start))))
      ))
  t)

(zk-pbt:defproperty merge-pathnames-associativity-and-defaults
    ((a (posix-pathname-gen))
     (b (posix-pathname-gen))
     (c (posix-pathname-gen)))
  (let ((left (merge-pathnames (merge-pathnames a b nil) c nil))
        (right (merge-pathnames a (merge-pathnames b c nil) nil)))
    (zk-test:is-equal left right)
    (zk-test:is (pathname-match-p a a))
    (when (pathname-name a)
      (zk-test:is-equal (pathname-name a)
                        (pathname-name (merge-pathnames a b nil))))
    (when (and (not (pathname-name a)) (pathname-name b))
      (zk-test:is-equal (pathname-name b)
                        (pathname-name (merge-pathnames a b nil)))))
  t)

(zk-pbt:defproperty pathname-case-common-roundtrip
    ((seg (zk-pbt:elements "FOO" "foo" "Mixed" "report" "LISP")))
  (let* ((p (make-pathname :name seg :case :local))
         (common (pathname-name p :case :common))
         (rebuilt (make-pathname :name common :case :common)))
    (zk-test:is-equal seg (pathname-name rebuilt :case :local)))
  t)
