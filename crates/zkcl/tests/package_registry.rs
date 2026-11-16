use zkcl::{Root, Runtime};

#[test]
fn package_queries_reflect_symbol_visibility() {
    let runtime = Runtime::start().unwrap();
    let origin = one(
        &runtime,
        r#"(make-package "PRIMITIVE-ORIGIN" :nicknames '("Primitive Origin"))"#,
    );
    let user = one(
        &runtime,
        r#"(make-package "PRIMITIVE-USER" :use '("Primitive Origin"))"#,
    );
    let nicknames = list(
        &runtime,
        one(&runtime, r#"(package-nicknames "PRIMITIVE-ORIGIN")"#),
    );
    assert_eq!(nicknames.len(), 1);
    assert_eq!(
        runtime.string(&nicknames[0]).as_deref(),
        Some("Primitive Origin")
    );
    assert!(one(&runtime, r#"(package-use-list "PRIMITIVE-ORIGIN")"#).is_nil());
    assert!(
        list(&runtime, one(&runtime, "(list-all-packages)"))
            .iter()
            .any(|package| package.same_identity(&origin))
    );
    let users = list(
        &runtime,
        one(&runtime, r#"(package-used-by-list "Primitive Origin")"#),
    );
    assert_eq!(users.len(), 1);
    assert!(users[0].same_identity(&user));

    runtime
        .load_str(
            r#"
            (make-package "PRIMITIVE-OTHER-USER" :use '("PRIMITIVE-ORIGIN"))
            (export (intern "DUPLICATED" "PRIMITIVE-ORIGIN") "PRIMITIVE-ORIGIN")
            (import 'PRIMITIVE-ORIGIN:DUPLICATED "PRIMITIVE-USER")
            (shadow "LOCAL" "PRIMITIVE-USER")
            (unexport 'PRIMITIVE-ORIGIN:DUPLICATED "PRIMITIVE-ORIGIN")
            "#,
        )
        .unwrap();
    let all_matches = list(
        &runtime,
        one(&runtime, r#"(find-all-symbols "DUPLICATED")"#),
    );
    assert_eq!(all_matches.len(), 1);
    assert!(all_matches[0].same_identity(&one(&runtime, "'PRIMITIVE-USER::DUPLICATED")));
    assert!(runtime.eval("'PRIMITIVE-ORIGIN:DUPLICATED").is_err());
    assert!(one(&runtime, r#"(find-all-symbols "NEVER-INTERNED")"#,).is_nil());
    let hidden = runtime
        .eval(r#"(find-symbol "DUPLICATED" "PRIMITIVE-OTHER-USER")"#)
        .unwrap();
    assert_eq!(hidden.len(), 2);
    assert!(hidden.iter().all(Root::is_nil));
    let shadowing = list(
        &runtime,
        one(&runtime, r#"(package-shadowing-symbols "PRIMITIVE-USER")"#),
    );
    assert_eq!(shadowing.len(), 1);
    assert!(shadowing[0].same_identity(&one(&runtime, "'PRIMITIVE-USER::LOCAL")));
}

#[test]
fn protected_packages_reject_membership_changes() {
    let runtime = Runtime::start().unwrap();
    one(&runtime, r#"(make-package "PRIMITIVE-USER")"#);
    for source in [
        r#"(unintern nil "COMMON-LISP")"#,
        r#"(unintern t "COMMON-LISP")"#,
        r#"(delete-package "COMMON-LISP")"#,
        r#"(use-package "KEYWORD" "PRIMITIVE-USER")"#,
        r#"(use-package "PRIMITIVE-USER" "KEYWORD")"#,
    ] {
        let caught = one(
            &runtime,
            &format!("(handler-case {source} (package-error (condition) t))"),
        );
        assert!(caught.same_identity(&one(&runtime, "t")));
    }
    assert!(
        one(&runtime, "(symbol-package nil)").same_identity(&one(&runtime, "(symbol-package t)"))
    );
}

#[test]
fn continuing_import_rechecks_each_conflict_without_replaying_earlier_imports() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-during-package-restart", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();
    let values = runtime
        .eval(
            r#"(let* ((source (make-package "REPLAY-SOURCE"))
                      (target (make-package "REPLAY-TARGET"))
                      (old-replace (intern "REPLACE" target))
                      (old-keep (intern "KEEP" target))
                      (first (intern "FIRST" source))
                      (replace (intern "REPLACE" source))
                      (keep (intern "KEEP" source))
                      (last (intern "LAST" source))
                      (conflicts 0))
                 (handler-bind ((package-error
                                  (lambda (condition)
                                    (assert (eq target (package-error-package condition)))
                                    (incf conflicts)
                                    (when (= conflicts 1)
                                      (unintern first target)
                                      (unintern old-replace target))
                                    (collect-during-package-restart)
                                    (continue condition))))
                   (import (list first replace keep last) target))
                 (values (= conflicts 2)
                         (null (find-symbol "FIRST" target))
                         (eq replace (find-symbol "REPLACE" target))
                         (eq old-keep (find-symbol "KEEP" target))
                         (eq last (find-symbol "LAST" target))))"#,
        )
        .unwrap();
    assert_eq!(values.len(), 5);
    assert!(values.iter().all(|value| !value.is_nil()));
}

#[test]
fn continuing_export_resumes_at_the_inaccessible_symbol() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-during-export", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();
    let values = runtime
        .eval(
            r#"
        (let* ((target (make-package "EXPORT-TARGET"))
               (user (make-package "EXPORT-USER" :use (list target)))
               (first (intern "FIRST" target))
               (handled (make-symbol "HANDLED"))
               (automatic (make-symbol "AUTOMATIC"))
               (last (intern "LAST" target))
               (signals 0))
          (handler-bind ((package-error
                           (lambda (c)
                             (assert (eq target (package-error-package c)))
                             (incf signals)
                             (when (= signals 1)
                               (unexport first target)
                               (import handled target))
                             (collect-during-export)
                             (continue c))))
            (export (list first handled automatic last) target))
          (values (= signals 2)
                  (null (find-symbol "FIRST" user))
                  (eq handled (find-symbol "HANDLED" user))
                  (eq automatic (find-symbol "AUTOMATIC" user))
                  (eq target (symbol-package automatic))
                  (eq last (find-symbol "LAST" user))))
    "#,
        )
        .unwrap();
    assert_eq!(values.len(), 6);
    assert!(values.iter().all(|value| !value.is_nil()));
}

#[test]
fn continuing_export_reports_import_conflicts_separately() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"
        (let* ((target (make-package "EXPORT-CONFLICTS"))
               (local-replace (intern "REPLACE" target))
               (local-keep (intern "KEEP" target))
               (replace (make-symbol "REPLACE"))
               (keep (make-symbol "KEEP"))
               (last (intern "LAST" target))
               (signals 0))
          (handler-bind ((package-error
                           (lambda (c)
                             (incf signals)
                             (when (= signals 2) (unintern local-replace target))
                             (continue c))))
            (export (list replace keep last) target))
          (values (= signals 4)
                  (equal (multiple-value-list (find-symbol "REPLACE" target))
                         (list replace :external))
                  (equal (multiple-value-list (find-symbol "KEEP" target))
                         (list local-keep :internal))
                  (null (symbol-package keep))
                  (eq (nth-value 1 (find-symbol "LAST" target)) :external)))
    "#,
        )
        .unwrap();
    assert_eq!(values.len(), 5);
    assert!(values.iter().all(|value| !value.is_nil()));
}

#[test]
fn exporting_an_inaccessible_symbol_checks_using_packages_before_importing_it() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"
        (let* ((target (make-package "ATOMIC-EXPORT"))
               (user (make-package "ATOMIC-EXPORT-USER" :use (list target)))
               (local (intern "X" user))
               (wanted (make-symbol "X"))
               (first (intern "FIRST" target))
               (last (intern "LAST" target)))
          (values (handler-case
                      (handler-bind ((package-error
                                       (lambda (c)
                                         (when (eq target (package-error-package c))
                                           (continue c)))))
                        (export (list first wanted last) target))
                    (package-error (c) (eq user (package-error-package c))))
                  (null (find-symbol "X" target))
                  (null (symbol-package wanted))
                  (eq local (find-symbol "X" user))
                  (eq first (find-symbol "FIRST" user))
                  (null (find-symbol "LAST" user))))
    "#,
        )
        .unwrap();
    assert_eq!(values.len(), 6);
    assert!(values.iter().all(|value| !value.is_nil()));
}

#[test]
fn keyword_creation_respects_the_insertion_operation() {
    let runtime = &Runtime::start().unwrap();
    let keyword = one(runtime, r#"(find-package "KEYWORD")"#);
    let internal = one(runtime, ":internal");
    let external = one(runtime, ":external");

    let result = runtime
        .eval(
            r#"(let ((word (intern "PRIMITIVE-KEY" "KEYWORD")))
                 (multiple-value-bind (symbol status) (find-symbol "PRIMITIVE-KEY" "KEYWORD")
                   (values word (symbol-value symbol) status)))"#,
        )
        .unwrap();
    assert_eq!(result.len(), 3);
    assert!(result[0].same_identity(&result[1]));
    assert!(result[2].same_identity(&external));

    let result = runtime
        .eval(
            r#"(let ((imported (make-symbol "PRIMITIVE-IMPORTED-KEY")))
                 (import imported "KEYWORD")
                 (multiple-value-bind (symbol status) (intern "PRIMITIVE-IMPORTED-KEY" "KEYWORD")
                   (values imported symbol status (symbol-package symbol) (boundp symbol))))"#,
        )
        .unwrap();
    assert_eq!(result.len(), 5);
    assert!(result[0].same_identity(&result[1]));
    assert!(result[2].same_identity(&internal));
    assert!(result[3].same_identity(&keyword));
    assert!(result[4].is_nil());

    let result = runtime
        .eval(
            r#"(let ((old (intern "PRIMITIVE-REPLACED-KEY" "KEYWORD"))
                     (replacement (make-symbol "PRIMITIVE-REPLACED-KEY")))
                 (shadowing-import replacement "KEYWORD")
                 (multiple-value-bind (symbol status) (find-symbol "PRIMITIVE-REPLACED-KEY" "KEYWORD")
                   (values replacement symbol status (symbol-package old) (symbol-package symbol))))"#,
        )
        .unwrap();
    assert_eq!(result.len(), 5);
    assert!(result[0].same_identity(&result[1]));
    assert!(result[2].same_identity(&internal));
    assert!(result[3].is_nil() && result[4].is_nil());

    let result = runtime
        .eval(
            r#"(progn
                 (shadow "PRIMITIVE-SHADOWED-KEY" "KEYWORD")
                 (multiple-value-bind (symbol status) (find-symbol "PRIMITIVE-SHADOWED-KEY" "KEYWORD")
                   (values status (symbol-package symbol) (boundp symbol))))"#,
        )
        .unwrap();
    assert_eq!(result.len(), 3);
    assert!(result[0].same_identity(&internal));
    assert!(result[1].same_identity(&keyword));
    assert!(result[2].is_nil());
}

fn one<'rt>(runtime: &'rt Runtime, source: &str) -> Root<'rt> {
    let mut values = runtime
        .eval(source)
        .unwrap_or_else(|error| panic!("{source}: {error}"));
    assert_eq!(values.len(), 1, "{source}");
    values.pop().unwrap()
}

fn list<'rt>(runtime: &'rt Runtime, mut list: Root<'rt>) -> Vec<Root<'rt>> {
    let mut elements = Vec::new();
    while let Some((car, cdr)) = runtime.cons_fields(&list) {
        elements.push(car);
        list = cdr;
    }
    assert!(list.is_nil());
    elements
}
