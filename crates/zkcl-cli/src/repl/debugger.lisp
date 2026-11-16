(cl:lambda (get-command &optional terminal)
  (cl:lambda (condition)
    (cl:block leave
      (cl:let ((fault (zk::%debug-fault))
               (message cl:nil)
               (frames (zk::%debug-frames))
               (selected 0)
               (restarts (cl:compute-restarts condition)))
        (cl:labels
            ((condition-message (condition)
               (cl:or
                (cl:ignore-errors
                  (cl:case (cl:type-of condition)
                    (cl:unbound-variable
                     (cl:format cl:nil "unbound variable ~S" (cl:cell-error-name condition)))
                    (cl:undefined-function
                     (cl:format cl:nil "undefined function ~S" (cl:cell-error-name condition)))
                    (cl:type-error
                     (cl:let ((cl:*print-level* 2) (cl:*print-length* 6)
                              (cl:*print-circle* cl:t) (cl:*print-readably* cl:nil))
                       (cl:format cl:nil "expected ~S, got ~S"
                                  (cl:type-error-expected-type condition) (cl:type-error-datum condition))))
                    (cl:t (cl:princ-to-string condition))))
                (cl:string (cl:type-of condition))))
             (print-frame (index)
               (cl:destructuring-bind (name &optional file line column text pc native &rest capabilities)
                   (cl:nth index frames)
                 (cl:declare (cl:ignore text capabilities))
                 (cl:format cl:*debug-io* "~&~A ~D  ~A~@[ at ~A~]~@[~A~]~@[~A~]~@[ [pc ~D]~]~@[ [native]~]~%"
                            (cl:if (cl:= index selected) ">" " ") index name file
                            (cl:when line (cl:format cl:nil ":~D" line))
                            (cl:when column (cl:format cl:nil ":~D" column)) pc native)))
             (select-frame (index)
               (cl:if (cl:and index (cl:<= 0 index) (cl:< index (cl:length frames)))
                      (cl:progn (cl:setq selected index) (print-frame selected))
                      (cl:format cl:*debug-io* "No such frame.~%")))
             (bindings ()
               (cl:when frames (zk::%debug-locals selected)))
             (evaluate-values (text)
               (cl:let* ((cl:*package* (cl:or (cl:and frames (zk::%debug-eval-package selected)) cl:*package*))
                         (form (cl:read-from-string text))
                         (variables (cl:mapcar
                                     (cl:lambda (entry)
                                       (cl:let ((name (cl:car entry)))
                                         (cl:list name (cl:list 'zk::%debug-local selected (cl:list 'cl:quote name)))))
                                     (bindings)))
                         (values (cl:multiple-value-list
                                  (cl:eval (cl:list 'cl:symbol-macrolet variables form)))))
                 values))
             (evaluate (text)
               (cl:let ((values (evaluate-values text)))
                 (cl:if values
                        (cl:loop for value in values for index from 0
                                 do (cl:format cl:*debug-io* "~A ~S~%" (cl:if (cl:= index 0) "=>" "  ") value))
                        (cl:format cl:*debug-io* "No values.~%"))))
             (show-source ()
               (cl:let* ((frame (cl:nth selected frames))
                         (text (cl:fifth frame)))
                 (cl:when frame (print-frame selected))
                 (cl:if text
                        (cl:format cl:*debug-io* "    ~A~%~VT^~%" text (cl:+ 3 (cl:fourth frame)))
                        (cl:format cl:*debug-io* "No source text is available for this frame.~%"))))
             (resume-motion (motion)
               (cl:when (zk::%debug-motion motion) (cl:return-from leave cl:nil))
               (cl:let ((restart (cl:find-restart 'cl:continue condition)))
                 (cl:if restart
                        (cl:progn (cl:invoke-restart restart) (show-restarts))
                        (cl:progn
                          (zk::%debug-motion :continue)
                          (cl:format cl:*debug-io* "No CONTINUE restart is active.~%")))))
             (show-restarts (&optional (refresh cl:t))
               (cl:when refresh (cl:setq restarts (cl:compute-restarts condition)))
               (cl:loop for restart in restarts for number from 1
                        for name = (cl:restart-name restart)
                        do (cl:format cl:*debug-io* "  ~D. ~@[[~A] ~]~A~%" number name restart)))
             (restart-command (command)
               (cl:let* ((number (cl:ignore-errors (cl:parse-integer command)))
                         (restart
                           (cl:if number
                                  (cl:when (cl:<= 1 number (cl:length restarts))
                                    (cl:nth (cl:1- number) restarts))
                                  (cl:loop for candidate in restarts
                                           for name = (cl:restart-name candidate)
                                           when (cl:and name (cl:string-equal command (cl:symbol-name name)))
                                             return candidate))))
                 (cl:if restart
                        (cl:progn (cl:invoke-restart-interactively restart) (show-restarts))
                        (cl:format cl:*debug-io* "Choose a listed restart by number or name, or enter h for help.~%"))))
             (help ()
               (cl:format cl:*debug-io*
                          "  bt             show the backtrace~%  frame N        select a frame~%  up, down       select a caller or callee~%  locals         show lexical variables and availability~%  source         show the selected source line~%  p FORM         evaluate in the selected frame (SETQ edits retained locals)~%  return FORM    leave the selected frame with FORM's multiple values~%  restart-frame [N]  restart a DEBUG 3 frame with its original arguments~%  step, next, out   move to a source location~%  d              describe the condition~%  N or NAME      invoke a restart~%  c, continue    resume normal execution~%  a, q, Enter     abort this evaluation~%")))
          (cl:when (cl:and terminal (cl:second fault) (cl:null restarts))
            (cl:return-from leave cl:nil))
          (cl:setq message (cl:or (cl:first fault) (condition-message condition)))
          (cl:format cl:*debug-io* "~&~A~A~%"
                     (cl:if (cl:typep condition 'cl:error) "error: " "") message)
          (cl:when frames (print-frame selected))
          (cl:progn
            (show-restarts cl:nil)
            (cl:when terminal (cl:format cl:*debug-io* "  Enter: return to REPL   h: help~%"))
            (cl:loop
              (cl:let ((raw (cl:funcall get-command message frames restarts selected
                                        (cl:handler-case (bindings) (cl:error () cl:nil)))))
                (cl:unless raw
                  (zk::%debug-motion :abort)
                  (cl:return-from leave cl:nil))
                (cl:let* ((line (cl:string-trim '(#\Space #\Tab #\Return) raw))
                          (end (cl:position-if (cl:lambda (c) (cl:find c '(#\Space #\Tab))) line))
                          (command (cl:string-downcase (cl:subseq line 0 end)))
                          (args (cl:when end (cl:string-trim '(#\Space #\Tab) (cl:subseq line end)))))
                  (cl:handler-case
                      (cl:cond
                        ((cl:member command '("" "q" ":q" "quit" ":quit" "a" "abort" ":abort") :test #'cl:string=)
                         (zk::%debug-motion :abort)
                         (cl:let ((abort (cl:find-restart 'cl:abort condition)))
                           (cl:when abort (cl:invoke-restart abort)))
                         (cl:return-from leave cl:nil))
                        ((cl:member command '("h" "?" "help" ":help") :test #'cl:string=) (help))
                        ((cl:member command '("bt" "backtrace") :test #'cl:string=)
                         (cl:loop for index below (cl:length frames) do (print-frame index)))
                        ((cl:string= command "frame") (select-frame (cl:and args (cl:parse-integer args :junk-allowed cl:t))))
                        ((cl:string= command "up") (select-frame (cl:1+ selected)))
                        ((cl:string= command "down") (select-frame (cl:1- selected)))
                        ((cl:member command '("locals" "list-locals") :test #'cl:string=)
                         (cl:let ((entries (zk::%debug-variables selected)))
                           (cl:if entries
                                  (cl:dolist (entry entries)
                                    (cl:if (cl:stringp entry)
                                           (cl:format cl:*debug-io* "  Locals unavailable: ~A~%" entry)
                                           (cl:format cl:*debug-io* "  ~S = ~S~A~%" (cl:first entry) (cl:second entry)
                                                      (cl:if (cl:third entry) "" " [read-only]"))))
                                  (cl:format cl:*debug-io* "No locals in this frame.~%"))))
                        ((cl:string= command "source") (show-source))
                        ((cl:member command '("d" "describe" ":describe") :test #'cl:string=)
                         (cl:describe condition cl:*debug-io*))
                        ((cl:member command '("c" "cont" ":cont" "continue" ":continue") :test #'cl:string=)
                         (resume-motion :continue))
                        ((cl:string= command "step") (resume-motion :into))
                        ((cl:string= command "next") (resume-motion :next))
                        ((cl:string= command "out") (resume-motion :out))
                        ((cl:string= command "return")
                         (cl:if args (zk::%debug-return selected (evaluate-values args))
                                (cl:format cl:*debug-io* "Usage: return FORM~%")))
                        ((cl:string= command "restart-frame")
                         (zk::%debug-restart-frame (cl:if args (cl:parse-integer args) selected)))
                        ((cl:string= command "p")
                         (cl:if args (evaluate args)
                                (cl:format cl:*debug-io* "Usage: p FORM~%")))
                        ((cl:char= (cl:char line 0) #\() (evaluate line))
                        (cl:t (restart-command line)))
                    (cl:error (error)
                      (cl:format cl:*debug-io* "error: ~A~%" (condition-message error)))))))))))))
