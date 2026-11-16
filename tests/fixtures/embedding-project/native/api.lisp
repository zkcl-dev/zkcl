(in-package #:embed.native)
(defvar cl-user::*embedded-native-loads* 0)
(incf cl-user::*embedded-native-loads*)
(defun answer () (compile-answer))
