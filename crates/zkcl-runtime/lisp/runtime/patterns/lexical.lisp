(defun lexical-input-p (input domain)
  (let ((span-p (input-span-p input)))
    (ecase domain
      (:text (if span-p (eq (input-span-unit input) :character) (stringp input)))
      (:bytes (if span-p (eq (input-span-unit input) :byte) (octet-vector-p input))))))

(defun input-region (input domain start end)
  (let ((span-p (input-span-p input)))
    (unless (lexical-input-p input domain) (error "Expected ~S input, got ~S" domain input))
    (let* ((size (if span-p (span-length input) (length input)))
           (end (if (null end) size end))
           (offset (if span-p (input-span-offset input) 0)))
      (unless (and (integerp start) (integerp end) (<= 0 start end size))
        (error "Invalid input range [~S, ~S) for length ~D" start end size))
      (values (if span-p (input-span-backing input) input)
              (+ offset start) (+ offset end) end))))

(defun lexical-binding (name style)
  (unless (or (wildcard-p name) (variable-name-p name))
    (error "Invalid lexical binding ~S" name))
  (unless (member style '(:value :unit :span)) (error "Unknown capture style ~S" style))
  (list name style))

(defun around-binding (form)
  (cond ((consp form)
         (destructuring-bind (name option style) form
           (unless (and (eq option :as) (eq style :span))
             (error "Expected (name :as :span), got ~S" form))
           (lexical-binding name style)))
        (t (lexical-binding form :value))))

(defun lexical-rule-data (source env expand)
  (let ((groups nil) (next 0) (names nil) (before nil) (after nil))
    (labels ((expand-rule (form depth)
               (when (> depth 128) (error "Lexical rule nesting or expansion exceeds 128 forms"))
               (if (and expand (consp form) (symbolp (car form)) (not (keywordp (car form))))
                   (multiple-value-bind (result expanded) (zk:pattern-expand-1 form env)
                     (if expanded (expand-rule result (1+ depth)) form))
                   form))
             (register (name style)
               (let ((spec (lexical-binding name style)))
                 (unless (wildcard-p name)
                   (when (member name names) (error "Duplicate lexical capture ~S" name))
                   (push name names))
                 spec))
             (walk (form depth)
               (let ((form (expand-rule form depth)))
                 (when (and (consp form) (null (list-length form))) (error "Cyclic lexical rule"))
                 (if (atom form) form
                     (case (car form)
                       (:capture
                        (destructuring-bind (head name body &key (as :value)) form
                          (declare (ignore head))
                          (let* ((slot next) (binding (register name as)))
                            (incf next)
                            (push binding groups)
                            (list :capture slot (walk body (1+ depth)) as))))
                       ((:seq :or)
                        (cons (car form) (mapcar (lambda (part) (walk part (1+ depth))) (cdr form))))
                       ((:* :+ :?)
                        (arity form 1) (list (car form) (walk (second form) (1+ depth))))
                       (:repeat
                        (arity form 3) (list :repeat (second form) (third form) (walk (fourth form) (1+ depth))))
                       (otherwise form))))))
      (let* ((form (expand-rule source 0))
             (with-p (and (consp form) (eq (car form) :with)))
             (rule (if with-p
                       (progn
                         (unless (and (list-length form) (cdr form))
                           (error "Missing rule in ~S" form))
                         (second form))
                       form)))
        (when with-p
          (loop for (option value . remaining) on (cddr form) by #'cddr do
            (when (and (null value) (null remaining)) (error "Missing lexical binding after ~S" option))
            (let ((binding (around-binding value)))
              (case option
                (:before (when before (error "Repeated :before")) (setf before binding))
                (:after (when after (error "Repeated :after")) (setf after binding))
                (otherwise (error "Unknown lexical binding option ~S" option))))))
        (let ((normalized (walk rule 0)))
          (dolist (binding (list before after))
            (when binding (register (first binding) (second binding))))
          (values normalized (list (nreverse groups) before after) (nreverse names)))))))

(defstruct (lexical-plan (:constructor make-lexical-plan (program bindings domain around-bindings-p))
                         (:copier nil))
  (program nil :read-only t)
  (bindings #() :read-only t)
  (domain :text :read-only t)
  (around-bindings-p nil :read-only t))

(defun assemble-lexical-plan (rules bindings domain mode)
  (make-lexical-plan (zk::%lex-compile rules domain mode)
                     (coerce bindings 'vector) domain
                     (some (lambda (spec) (or (second spec) (third spec))) bindings)))

(defun zk-lex:compile-lex-rules (rules &key (domain :text) (mode :full))
  (unless (list-length rules) (error "Expected a finite list of lexical rules"))
  (let ((compiled nil) (bindings nil))
    (dolist (rule rules)
      (multiple-value-bind (data rule-bindings) (lexical-rule-data rule nil nil)
        (push data compiled)
        (push rule-bindings bindings)))
    (assemble-lexical-plan (nreverse compiled) (nreverse bindings) domain mode)))

(defstruct (lexical-match (:constructor make-lexical-match (rule span captures)) (:copier nil))
  (rule 0 :read-only t)
  (span nil :read-only t)
  (captures nil :read-only t))

(defun make-match-record (plan result slice &optional region-end (materialize-span-p t))
  (let* ((ordinal (aref result 0)) (start (aref result 1)) (end (aref result 2))
         (span (when materialize-span-p (funcall slice start end :span)))
         (metadata (aref (lexical-plan-bindings plan) ordinal))
         (captures nil))
    (flet ((save (spec low high)
             (when (and spec (not (wildcard-p (first spec))))
               (push (list (first spec) (second spec)
                           (and low (funcall slice low high (second spec))))
                     captures))))
      (loop for spec in (first metadata) for index from 3 by 2 do
        (save spec (aref result index) (aref result (1+ index))))
      (save (second metadata) 0 start)
      (save (third metadata) end region-end))
    (make-lexical-match ordinal span captures)))

(defun array-lexical-slice (input start end style unit source)
  (ecase style
    (:unit (aref input start))
    (:value (subseq input start end))
    (:span (make-input-span (subseq input start end) 0 start end unit
                            (or source (gensym "SOURCE"))))))

(defun lexical-slicer (input start domain)
  (if (input-span-p input)
      (lambda (left right style)
        (declare (ignore style))
        (zk-text:span-slice input (+ start left) (+ start right)))
      (let ((kind (if (eq domain :text) :character :byte)) (source nil))
        (lambda (left right style)
          (array-lexical-slice input (+ start left) (+ start right) style kind
                               (when (eq style :span)
                                 (or source (setf source (gensym "SOURCE")))))))))

(defun run-lex-rules-internal (plan input start end materialize-span-p)
  (let ((domain (lexical-plan-domain plan)))
    (multiple-value-bind (backing low high local-end) (input-region input domain start end)
      (let ((result (zk::%lex-run (lexical-plan-program plan) backing low high)))
        (when result
          (make-match-record plan result (lexical-slicer input start domain)
                             (- local-end start) materialize-span-p))))))

(defun zk-lex:run-lex-rules (plan input &key (start 0) end)
  (run-lex-rules-internal plan input start end t))

(defun zk-lex:lex-match-rule (record) (lexical-match-rule record))
(defun zk-lex:lex-match-span (record) (lexical-match-span record))

(defun lexical-capture-value (payload style)
  (if (input-span-p payload)
      (ecase style
        (:span payload)
        (:unit (aref (input-span-backing payload) (input-span-offset payload)))
        (:value (zk-text:span-value payload)))
      payload))

(defun slice-lexical-binding (input start left right style domain &optional source)
  (let ((left (+ start left)) (right (+ start right)))
    (if (input-span-p input)
        (lexical-capture-value (zk-text:span-slice input left right) style)
        (array-lexical-slice input left right style
                             (if (eq domain :text) :character :byte) source))))

(defun zk-lex:lex-capture (record name)
  (let ((entry (assoc name (lexical-match-captures record))))
    (unless entry (error "Undeclared lexical capture ~S" name))
    (destructuring-bind (name style payload) entry
      (declare (ignore name))
      (if (null payload) (values nil nil)
          (values (lexical-capture-value payload style) t)))))

(define-condition zk-lex:scan-error (error)
  ((reason :initarg :reason :reader zk-lex:scan-error-reason)
   (position :initarg :position :reader zk-lex:scan-error-position))
  (:report (lambda (condition stream)
             (format stream "Lexical scan ~S at ~D" (zk-lex:scan-error-reason condition)
                     (zk-lex:scan-error-position condition)))))

(defstruct (lexical-scanner (:constructor make-lexical-scanner (domain cursor refill)) (:copier nil))
  (domain :text :read-only t)
  (cursor nil :read-only t)
  (refill nil :read-only t)
  (source (gensym "SOURCE") :read-only t)
  (position 0)
  (filled 0)
  (head nil)
  (tail nil)
  (eof nil)
  (busy nil))

(defun scan-append (scanner chunk)
  (multiple-value-bind (backing start end) (input-region chunk (lexical-scanner-domain scanner) 0 nil)
    (declare (ignore backing))
    (when (< start end)
      (let* ((length (- end start))
             (snapshot (zk-text:snapshot chunk))
             (base (lexical-scanner-filled scanner))
             (end (+ base length))
             (stable (make-input-span (input-span-backing snapshot) (input-span-offset snapshot)
                                      base end (input-span-unit snapshot) (lexical-scanner-source scanner)
                                      (input-span-origin snapshot) (input-span-boundaries snapshot)))
             (cell (list stable)))
        (zk::%cursor-append (lexical-scanner-cursor scanner) (input-span-backing stable)
                            (input-span-offset stable) (+ (input-span-offset stable) length))
        (if (lexical-scanner-tail scanner)
            (setf (cdr (lexical-scanner-tail scanner)) cell)
            (setf (lexical-scanner-head scanner) cell))
        (setf (lexical-scanner-tail scanner) cell (lexical-scanner-filled scanner) end)
        t))))

(defun channel-refill-fn (channel)
  (lambda ()
    (multiple-value-bind (chunk ok) (zk-async:channel-recv channel)
      (if ok (values chunk :chunk) (values nil :eof)))))

(defun zk-lex:make-lexscanner (source &key (domain :text))
  (let* ((refill (cond ((functionp source) source)
                       ((zk-async:channelp source) (channel-refill-fn source))))
         (scanner (make-lexical-scanner domain (zk::%lex-cursor domain) refill)))
    (unless refill
      (scan-append scanner source)
      (setf (lexical-scanner-eof scanner) t))
    scanner))

(defun zk-lex:scan-position (scanner) (lexical-scanner-position scanner))

(defun scan-refill (scanner)
  (if (lexical-scanner-eof scanner) :eof
      (multiple-value-bind (chunk status) (funcall (lexical-scanner-refill scanner))
        (ecase status
          (:chunk (unless (scan-append scanner chunk) (error "A scanner chunk must not be empty")))
          (:eof (setf (lexical-scanner-eof scanner) t))
          (:need-more nil))
        status)))

(defmacro with-scanner-use (scanner &body body)
  `(progn
     (when (lexical-scanner-busy ,scanner) (error "Recursive use of the same scanner from its source"))
     (setf (lexical-scanner-busy ,scanner) t)
     (unwind-protect (progn ,@body) (setf (lexical-scanner-busy ,scanner) nil))))

(defun scanner-commit (scanner end)
  (zk::%cursor-commit (lexical-scanner-cursor scanner) end)
  (setf (lexical-scanner-position scanner) end)
  (loop while (and (lexical-scanner-head scanner)
                   (<= (input-span-end (car (lexical-scanner-head scanner))) end)) do
    (pop (lexical-scanner-head scanner)))
  (unless (lexical-scanner-head scanner) (setf (lexical-scanner-tail scanner) nil)))

(defun empty-span-backing (kind)
  (if (eq kind :character) "" (zk-bits:octets)))

(defun scan-slice (scanner start end)
  (let* ((kind (if (eq (lexical-scanner-domain scanner) :text) :character :byte))
         (source (lexical-scanner-source scanner))
         (chunks (lexical-scanner-head scanner)))
    (loop while (and chunks (if (= start end) (< (input-span-end (car chunks)) start)
                                (<= (input-span-end (car chunks)) start))) do (pop chunks))
    (when (null chunks)
      (return-from scan-slice (make-input-span (empty-span-backing kind) 0 start end kind source)))
    (let ((first (car chunks)))
      (when (<= end (input-span-end first))
        (return-from scan-slice (zk-text:span-slice first (- start (input-span-start first))
                                                    (- end (input-span-start first))))))
    (let* ((length (- end start))
           (data (make-array length :element-type (if (eq kind :character) 'character '(unsigned-byte 8))))
           (copied 0) (origin nil) (edges nil) (aligned t))
      (loop for chunk in chunks while (< copied length)
            for left = (max start (input-span-start chunk))
            for right = (min end (input-span-end chunk))
            when (< left right) do
        (let* ((from (+ (input-span-offset chunk) (- left (input-span-start chunk))))
               (count (- right left))
               (boundaries (input-span-boundaries chunk)))
          (replace data (input-span-backing chunk) :start1 copied :end1 (+ copied count)
                   :start2 from :end2 (+ from count))
          (when (and aligned (eq kind :character))
            (cond ((and (zerop copied) boundaries)
                   (setf edges (make-array (1+ length)) origin (input-span-origin chunk)))
                  ((or (null boundaries) (null edges) (not (eq origin (input-span-origin chunk)))
                       (/= (aref edges copied) (aref boundaries from)))
                   (setf aligned nil edges nil origin nil)))
            (when edges
              (replace edges boundaries :start1 copied :end1 (+ copied count 1)
                       :start2 from :end2 (+ from count 1))))
          (incf copied count)))
      (make-input-span data 0 start end kind source origin edges))))

(defun scan-step-internal (scanner plan materialize-span-p)
  (when (lexical-plan-around-bindings-p plan)
    (error ":before and :after bindings require an in-memory range"))
  (with-scanner-use scanner
    (loop
      (multiple-value-bind (status matched)
          (zk::%cursor-probe (lexical-scanner-cursor scanner) (lexical-plan-program plan) (lexical-scanner-eof scanner))
        (case status
          (:match
           (let ((record (make-match-record plan matched
                                            (lambda (start end style)
                                              (declare (ignore style))
                                              (scan-slice scanner start end))
                                            nil materialize-span-p)))
             (scanner-commit scanner (aref matched 2))
             (return (values :match record))))
          (:need-more (when (eq (scan-refill scanner) :need-more) (return (values :need-more nil))))
          (otherwise (return (values status nil))))))))

(defun zk-lex:scan-step (scanner plan)
  (scan-step-internal scanner plan t))

(defun zk-lex:scan-take (scanner count)
  (unless (eq (lexical-scanner-domain scanner) :bytes) (error "SCAN-TAKE requires a byte scanner"))
  (check-type count (integer 0 *))
  (with-scanner-use scanner
    (let ((start (lexical-scanner-position scanner)))
      (when (zerop count) (return-from zk-lex:scan-take (values :ok (scan-slice scanner start start) 0)))
      (zk::%cursor-reset (lexical-scanner-cursor scanner))
      (let ((end (+ start count)))
        (loop
          (when (<= end (lexical-scanner-filled scanner))
            (let ((view (scan-slice scanner start end)))
              (scanner-commit scanner end)
              (return (values :ok view count))))
          (case (scan-refill scanner)
            (:eof (return (values :truncated nil (- (lexical-scanner-filled scanner) start))))
            (:need-more (return (values :need-more nil (- (lexical-scanner-filled scanner) start))))))))))

(define-condition zk-text:unicode-decode-error (error)
  ((span :initarg :span :reader zk-text:unicode-decode-error-span))
  (:report (lambda (condition stream)
             (let ((span (zk-text:unicode-decode-error-span condition)))
               (format stream "Invalid UTF-8 at bytes [~D, ~D)"
                       (zk-text:span-start span) (zk-text:span-end span))))))

(defun zk-text:utf8-encode (input)
  (multiple-value-bind (backing low high) (input-region input :text 0 nil)
    (zk::%utf8-encode backing low high)))

(defun offset-edges (edges base)
  (unless (zerop base)
    (dotimes (index (length edges)) (incf (aref edges index) base)))
  edges)

(defun zk-text:utf8-decode (input &key (on-error :error) (as :value))
  (let ((replace-p (ecase on-error (:error nil) (:replace t))))
    (unless (member as '(:value :span)) (error "Invalid UTF-8 output style ~S" as))
    (multiple-value-bind (backing low high) (input-region input :bytes 0 nil)
      (multiple-value-bind (text edges consumed failure) (zk::%utf8-decode backing low high t replace-p)
        (when failure
          (error 'zk-text:unicode-decode-error :span
                 (if (input-span-p input) (zk-text:span-slice input consumed failure)
                     (slice-vector-span input consumed failure))))
        (if (eq as :value) text
            (let ((origin (if (input-span-p input) (input-span-source input) (gensym "BYTE-SOURCE")))
                  (base (if (input-span-p input) (input-span-start input) 0)))
              (make-input-span text 0 0 (length text) :character (gensym "TEXT-SOURCE")
                               origin (offset-edges edges base))))))))

(defun zk-text:utf8-source (octet-source &key (on-error :error))
  (let ((octet-source (cond ((functionp octet-source) octet-source)
                            ((zk-async:channelp octet-source) (channel-refill-fn octet-source))
                            (t (error "Expected a function or channel as UTF-8 octet source, got ~S"
                                      octet-source))))
        (replace-p (ecase on-error (:error nil) (:replace t)))
        (origin (gensym "BYTE-SOURCE")) (text-source (gensym "TEXT-SOURCE"))
        (pending (zk-bits:octets)) (byte-position 0) (text-position 0) (eof nil) (failure nil))
    (lambda ()
      (when failure (error 'zk-text:unicode-decode-error :span failure))
      (loop
        (when (plusp (length pending))
          (multiple-value-bind (text edges consumed bad-end)
              (zk::%utf8-decode pending 0 (length pending) eof replace-p)
            (when bad-end
              (setf failure (make-input-span (subseq pending consumed bad-end) 0 (+ byte-position consumed)
                                            (+ byte-position bad-end) :byte origin)))
            (unless (zerop consumed)
              (let* ((end (+ text-position (length text)))
                     (span (make-input-span text 0 text-position end :character text-source
                                            origin (offset-edges edges byte-position))))
                (setf text-position end
                      byte-position (+ byte-position consumed)
                      pending (if failure (zk-bits:octets) (subseq pending consumed)))
                (return (values span :chunk))))
            (when failure (error 'zk-text:unicode-decode-error :span failure))))
        (when eof (return (values nil :eof)))
        (multiple-value-bind (chunk status) (funcall octet-source)
          (ecase status
            (:eof (setf eof t))
            (:need-more (return (values nil :need-more)))
            (:chunk
             (multiple-value-bind (backing low high) (input-region chunk :bytes 0 nil)
               (unless (< low high) (error "A scanner chunk must not be empty"))
               (if (zerop (length pending))
                   (setf pending (subseq backing low high))
                   (let ((buffer (make-array (+ (length pending) (- high low)) :element-type '(unsigned-byte 8))))
                     (replace buffer pending)
                     (replace buffer backing :start1 (length pending) :start2 low :end2 high)
                     (setf pending buffer)))))))))))

(defun text-value (input)
  (if (input-span-p input)
      (progn (unless (eq (input-span-unit input) :character) (error "Expected a text span"))
             (zk-text:span-value input))
      input))

(defun zk-text:normalize (input form) (zk::%text-normalize (text-value input) form))
(defun zk-text:case-fold (input) (zk::%text-case-fold (text-value input)))

(defun zk-text:unicode-property-p (character property &optional (value nil supplied))
  (if supplied (zk::%unicode-property-p character property value)
      (zk::%unicode-property-p character property)))

(defun zk-text:segment-text (input mode)
  (let* ((span (zk-text:snapshot input)) (edges (zk::%text-segment (text-value span) mode))
         (result (make-array (1- (length edges)))))
    (dotimes (index (length result) result)
      (setf (aref result index) (zk-text:span-slice span (aref edges index) (aref edges (1+ index)))))))
