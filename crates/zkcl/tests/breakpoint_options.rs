use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use zkcl::{BreakpointFollow, BreakpointOptions, BreakpointStatus, Runtime, SourceDocument};

mod common;
use common::files::TempDir;

fn counting_debugger(runtime: &Runtime) -> Arc<AtomicUsize> {
    let stops = Arc::new(AtomicUsize::new(0));
    let observed = stops.clone();
    runtime
        .register_function("count-breakpoint", move |_, _| {
            observed.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        })
        .unwrap();
    let frontend = runtime.eval("#'count-breakpoint").unwrap().remove(0);
    runtime.set_debugger(Some(&frontend)).unwrap();
    stops
}

#[test]
fn hit_filters_enable_and_disable_preserve_encounter_counts() {
    let runtime = Runtime::start().unwrap();
    let stops = counting_debugger(&runtime);
    runtime
        .eval("(defun filtered-breakpoint (x) (+ x 1))")
        .unwrap();
    let function = runtime.eval("#'filtered-breakpoint").unwrap().remove(0);
    let breakpoint = runtime
        .add_breakpoint_with_options(
            &function,
            None,
            BreakpointOptions {
                hit_count: Some(2),
                hit_every: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
    runtime
        .eval("(dotimes (x 5) (filtered-breakpoint x))")
        .unwrap();
    assert_eq!(stops.load(Ordering::Relaxed), 2);
    assert_eq!(runtime.breakpoints()[0].hits, 5);
    runtime.enable_breakpoint(breakpoint.id, false).unwrap();
    runtime.eval("(filtered-breakpoint 1)").unwrap();
    assert_eq!(runtime.breakpoints()[0].hits, 5);
    runtime.enable_breakpoint(breakpoint.id, true).unwrap();
    runtime.eval("(filtered-breakpoint 1)").unwrap();
    assert_eq!(stops.load(Ordering::Relaxed), 3);
    assert!(runtime.remove_breakpoint(breakpoint.id));
    assert!(runtime.breakpoints().is_empty());
}

#[test]
fn conditions_read_real_locals_and_logs_keep_side_effects_without_stopping() {
    let runtime = Runtime::start().unwrap();
    let stops = counting_debugger(&runtime);
    runtime.eval("(defparameter *breakpoint-log* nil)").unwrap();
    runtime
        .eval("(defun conditional-breakpoint (x) (declare (optimize (debug 3))) (+ x 10))")
        .unwrap();
    let function = runtime.eval("#'conditional-breakpoint").unwrap().remove(0);
    runtime
        .add_breakpoint_with_options(
            &function,
            None,
            BreakpointOptions {
                condition: Some("(= x 2)".into()),
                log: Some("(progn (push x *breakpoint-log*) x)".into()),
                log_only: true,
                ..Default::default()
            },
        )
        .unwrap();
    let values = runtime
        .eval("(list (conditional-breakpoint 1) (conditional-breakpoint 2) *breakpoint-log*)")
        .unwrap();
    assert_eq!(runtime.preview(&values[0]).unwrap(), "(11 12 (2))");
    assert_eq!(stops.load(Ordering::Relaxed), 0);
    assert_eq!(runtime.breakpoints()[0].hits, 2);
    let id = runtime.breakpoints()[0].id;
    runtime
        .update_breakpoint(
            id,
            BreakpointOptions {
                condition: Some("(= x 3)".into()),
                ..Default::default()
            },
        )
        .unwrap();
    runtime.eval("(conditional-breakpoint 3)").unwrap();
    assert_eq!(stops.load(Ordering::Relaxed), 1);
}

#[test]
fn failed_condition_stops_even_when_configured_as_log_only() {
    let runtime = Runtime::start().unwrap();
    let stops = counting_debugger(&runtime);
    runtime
        .eval("(defun failed-breakpoint (x) (declare (optimize (debug 3))) (+ x 1))")
        .unwrap();
    let function = runtime.eval("#'failed-breakpoint").unwrap().remove(0);
    runtime
        .add_breakpoint_with_options(
            &function,
            None,
            BreakpointOptions {
                condition: Some("(car 42)".into()),
                log_only: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        runtime.eval("(failed-breakpoint 1)").unwrap()[0].as_fixnum(),
        Some(2)
    );
    assert_eq!(stops.load(Ordering::Relaxed), 1);
}

#[test]
fn name_follow_redefinition_keeps_object_breakpoints_on_the_old_object() {
    let runtime = Runtime::start().unwrap();
    let stops = counting_debugger(&runtime);
    runtime.eval("(defun migrating-breakpoint () 1)").unwrap();
    let old = runtime.eval("#'migrating-breakpoint").unwrap().remove(0);
    let name = runtime.eval("'migrating-breakpoint").unwrap().remove(0);
    let object = runtime.add_breakpoint(&old, None).unwrap();
    let following = runtime
        .add_breakpoint_with_options(
            &name,
            None,
            BreakpointOptions {
                follow: BreakpointFollow::Name,
                ..Default::default()
            },
        )
        .unwrap();
    runtime.eval("(defun migrating-breakpoint () 2)").unwrap();
    assert_eq!(
        runtime.eval("(migrating-breakpoint)").unwrap()[0].as_fixnum(),
        Some(2)
    );
    assert_eq!(stops.load(Ordering::Relaxed), 1);
    assert_eq!(runtime.call(&old, &[]).unwrap()[0].as_fixnum(), Some(1));
    assert_eq!(stops.load(Ordering::Relaxed), 2);
    let sites = runtime.breakpoints();
    assert_eq!(sites.iter().find(|b| b.id == object.id).unwrap().hits, 1);
    assert_eq!(sites.iter().find(|b| b.id == following.id).unwrap().hits, 1);
    runtime.eval("(fmakunbound 'migrating-breakpoint)").unwrap();
    assert!(matches!(
        runtime
            .breakpoints()
            .iter()
            .find(|b| b.id == following.id)
            .unwrap()
            .status,
        BreakpointStatus::Pending(_)
    ));
}

#[test]
fn file_breakpoint_is_pending_until_its_real_source_is_loaded() {
    let runtime = Runtime::start().unwrap();
    let directory = TempDir::new("pending-breakpoint");
    let path = directory.write(
        "pending.lisp",
        "(defun pending-breakpoint (x)\n  (+ x 1))\n",
    );
    let breakpoint = runtime
        .add_file_breakpoint(&path, 2, BreakpointOptions::default())
        .unwrap();
    assert!(matches!(breakpoint.status, BreakpointStatus::Pending(_)));
    let stops = counting_debugger(&runtime);
    runtime.load_file(&path).unwrap();
    let loaded = runtime.breakpoints().remove(0);
    assert_eq!(loaded.status, BreakpointStatus::Verified);
    let values = runtime.eval("(pending-breakpoint 4)").unwrap();
    assert_eq!(values[0].as_fixnum(), Some(5));
    assert_eq!(stops.load(Ordering::Relaxed), 1);
    directory.write(
        "pending.lisp",
        "(defun pending-breakpoint (x)\n  (+ x 10))\n",
    );
    runtime.load_file(&path).unwrap();
    assert_eq!(
        runtime.eval("(pending-breakpoint 4)").unwrap()[0].as_fixnum(),
        Some(14)
    );
    assert_eq!(stops.load(Ordering::Relaxed), 2);
    assert_eq!(runtime.breakpoints()[0].id, breakpoint.id);
}

#[test]
fn overlapping_breakpoints_each_keep_their_filter_and_log_behavior() {
    let runtime = Runtime::start().unwrap();
    let stops = counting_debugger(&runtime);
    runtime
        .eval("(defparameter *overlapping-log* nil)")
        .unwrap();
    runtime
        .eval("(defun overlapping-breakpoint (x) (declare (optimize (debug 3))) (+ x 1))")
        .unwrap();
    let function = runtime.eval("#'overlapping-breakpoint").unwrap().remove(0);
    runtime
        .add_breakpoint_with_options(
            &function,
            None,
            BreakpointOptions {
                condition: Some("nil".into()),
                ..Default::default()
            },
        )
        .unwrap();
    runtime
        .add_breakpoint_with_options(
            &function,
            None,
            BreakpointOptions {
                log: Some("(push x *overlapping-log*)".into()),
                log_only: true,
                ..Default::default()
            },
        )
        .unwrap();
    runtime.eval("(overlapping-breakpoint 4)").unwrap();
    assert_eq!(stops.load(Ordering::Relaxed), 0);
    assert_eq!(
        runtime
            .preview(&runtime.eval("*overlapping-log*").unwrap()[0])
            .unwrap(),
        "(4)"
    );
    assert!(runtime.breakpoints().iter().all(|site| site.hits == 1));
}

#[test]
fn logpoints_execute_without_installing_a_debugger_frontend() {
    let runtime = Runtime::start().unwrap();
    runtime
        .eval("(defparameter *frontend-free-log* nil)")
        .unwrap();
    runtime
        .eval("(defun frontend-free-breakpoint (x) (declare (optimize (debug 3))) (+ x 1))")
        .unwrap();
    let function = runtime
        .eval("#'frontend-free-breakpoint")
        .unwrap()
        .remove(0);
    runtime
        .add_breakpoint_with_options(
            &function,
            None,
            BreakpointOptions {
                log: Some("(push x *frontend-free-log*)".into()),
                log_only: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        runtime.eval("(frontend-free-breakpoint 9)").unwrap()[0].as_fixnum(),
        Some(10)
    );
    assert_eq!(
        runtime
            .preview(&runtime.eval("*frontend-free-log*").unwrap()[0])
            .unwrap(),
        "(9)"
    );
}

#[test]
fn condition_writes_the_live_lexical_binding_and_sees_dynamic_bindings() {
    let runtime = Runtime::start().unwrap();
    let stops = counting_debugger(&runtime);
    runtime.eval("(defparameter *condition-delta* 1)").unwrap();
    runtime.eval("(defun writable-condition-breakpoint (x) (declare (optimize (debug 3))) (values x (+ x 1)))").unwrap();
    let function = runtime
        .eval("#'writable-condition-breakpoint")
        .unwrap()
        .remove(0);
    runtime
        .add_breakpoint_with_options(
            &function,
            None,
            BreakpointOptions {
                condition: Some("(progn (incf x *condition-delta*) t)".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let values = runtime
        .eval("(let ((*condition-delta* 5)) (writable-condition-breakpoint 10))")
        .unwrap();
    assert_eq!(
        values.iter().map(|v| v.as_fixnum()).collect::<Vec<_>>(),
        vec![Some(15), Some(16)]
    );
    assert_eq!(stops.load(Ordering::Relaxed), 1);
}

#[test]
fn source_follow_becomes_pending_when_the_requested_line_is_no_longer_executable() {
    let runtime = Runtime::start().unwrap();
    let stops = counting_debugger(&runtime);
    let directory = TempDir::new("invalidated-source-breakpoint");
    let path = directory.write(
        "source.lisp",
        "(defun invalidated-source-breakpoint (x)\n  (+ x 1))\n",
    );
    runtime.load_file(&path).unwrap();
    let breakpoint = runtime
        .add_file_breakpoint(&path, 2, BreakpointOptions::default())
        .unwrap();
    assert_eq!(breakpoint.status, BreakpointStatus::Verified);
    directory.write(
        "source.lisp",
        "(defun invalidated-source-breakpoint (x) (+ x 2))\n",
    );
    runtime.load_file(&path).unwrap();
    assert!(matches!(
        runtime.breakpoints()[0].status,
        BreakpointStatus::Pending(_)
    ));
    assert_eq!(
        runtime.eval("(invalidated-source-breakpoint 3)").unwrap()[0].as_fixnum(),
        Some(5)
    );
    assert_eq!(stops.load(Ordering::Relaxed), 0);
}

#[test]
fn source_follow_maps_a_unique_unchanged_line_into_a_versioned_fragment() {
    let runtime = Runtime::start().unwrap();
    let stops = counting_debugger(&runtime);
    let prefix = "; first line\n; second line\n";
    let original = format!("{prefix}(defun fragment-follow (x)\n  (+ x 1))\n");
    let source = SourceDocument::input(&original)
        .with_uri("file:///fragment-follow.lisp")
        .with_version(4);
    runtime.load_document(&source).unwrap();
    let name = runtime.eval("'fragment-follow").unwrap().remove(0);
    let breakpoint = runtime
        .add_source_breakpoint_with_options(
            &name,
            4,
            BreakpointOptions {
                follow: BreakpointFollow::Source,
                ..Default::default()
            },
        )
        .unwrap();
    let fragment = "(defun fragment-follow (x)\n  (declare (optimize (debug 3)))\n  (+ x 1))\n";
    let next = SourceDocument::input(fragment)
        .with_uri("file:///fragment-follow.lisp")
        .with_version(5)
        .with_base_offset(prefix.len() + 30);
    runtime.load_document(&next).unwrap();
    let migrated = runtime.breakpoints().remove(0);
    assert_eq!(migrated.id, breakpoint.id);
    assert_eq!(migrated.status, BreakpointStatus::Verified);
    assert_eq!(migrated.source.as_ref(), Some(&next));
    assert_eq!(
        runtime.eval("(fragment-follow 4)").unwrap()[0].as_fixnum(),
        Some(5)
    );
    assert_eq!(stops.load(Ordering::Relaxed), 1);
}

#[test]
fn changed_source_fragment_without_a_revision_mapping_stays_pending() {
    let runtime = Runtime::start().unwrap();
    let stops = counting_debugger(&runtime);
    let source = SourceDocument::input("; old prefix\n(defun unmapped-fragment (x)\n  (+ x 1))\n")
        .with_uri("file:///unmapped-fragment.lisp")
        .with_version(1);
    runtime.load_document(&source).unwrap();
    let name = runtime.eval("'unmapped-fragment").unwrap().remove(0);
    runtime
        .add_source_breakpoint_with_options(
            &name,
            3,
            BreakpointOptions {
                follow: BreakpointFollow::Source,
                ..Default::default()
            },
        )
        .unwrap();
    let next = SourceDocument::input("(defun unmapped-fragment (x)\n  (+ x 100))\n")
        .with_uri("file:///unmapped-fragment.lisp")
        .with_version(2)
        .with_base_offset(90);
    runtime.load_document(&next).unwrap();
    assert!(matches!(
        runtime.breakpoints()[0].status,
        BreakpointStatus::Pending(_)
    ));
    assert_eq!(
        runtime.eval("(unmapped-fragment 1)").unwrap()[0].as_fixnum(),
        Some(101)
    );
    assert_eq!(stops.load(Ordering::Relaxed), 0);
}
