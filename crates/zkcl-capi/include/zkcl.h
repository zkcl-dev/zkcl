#ifndef ZKCL_H
#define ZKCL_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct zkcl_runtime zkcl_runtime;
typedef struct zkcl_value zkcl_value;
typedef struct zkcl_values zkcl_values;
typedef struct zkcl_returns zkcl_returns;
typedef struct zkcl_text zkcl_text;
typedef struct zkcl_error zkcl_error;
typedef struct zkcl_cancellation zkcl_cancellation;
typedef struct zkcl_evaluation zkcl_evaluation;
typedef struct zkcl_completion zkcl_completion;
typedef struct zkcl_waker zkcl_waker;
typedef struct zkcl_debug_event zkcl_debug_event;

/* UTF-8 byte views. Input is borrowed for the call. A zero length permits NULL.
 * A returned view is valid until its text or error handle is released. */
typedef struct { const char *data; size_t len; } zkcl_str;
static inline zkcl_str zkcl_utf8(const char *s) {
    zkcl_str view = { s, strlen(s) };
    return view;
}
typedef struct { size_t start, end; } zkcl_range;

/* Fallible functions return NULL on success or an owned error. Output handle
 * slots receive a new owned handle on success and NULL on error. Supply an
 * empty slot. Release each owned handle exactly once with its matching free
 * function. All free functions accept NULL.
 *
 * A runtime, its values, and callback arguments belong to the creating thread.
 * Keep the runtime alive throughout active calls and until all its evaluation
 * handles have been freed.
 * Values can be released after runtime destruction on the owning thread.
 * Cancellation, completion, and waker handles can be used across threads
 * according to their individual contracts below.
 *
 * With allow_foreign_calls=true, Lisp can load native code into this process.
 * The caller vouches for library initialization and destruction, function
 * signatures, pointer lifetimes, and native function preconditions. */
zkcl_error *zkcl_runtime_new(bool allow_foreign_calls, zkcl_runtime **out);

/* Runtime standard streams accept UTF-8 bytes. Read supplies at most capacity
 * bytes, returning the number supplied, 0 for EOF, or -1 for an I/O error.
 * zkcl buffers input and accepts partial reads, including split UTF-8 sequences.
 * Write returns true after accepting all bytes, or false for an I/O error.
 * A write with data=NULL and len=0 requests a flush. An unbuffered sink can
 * return true immediately. Read and write must return normally without calling
 * back into the same runtime. Their byte buffers are borrowed until they return.
 * Callbacks run on the runtime's owning thread. The caller keeps each callback
 * and its userdata valid until the runtime is freed. */
typedef ptrdiff_t (*zkcl_read_callback)(void *userdata, char *buffer, size_t capacity);
typedef bool (*zkcl_write_callback)(void *userdata, const char *data, size_t len);
typedef struct { zkcl_read_callback read; void *userdata; } zkcl_host_input;
typedef struct { zkcl_write_callback write; void *userdata; } zkcl_host_output;

typedef struct {
    size_t heap_size_bytes;
    const uint64_t *step_budget;
    zkcl_host_input input;
    zkcl_host_output output;
    zkcl_host_output error_output;
    bool allow_foreign_calls;
} zkcl_runtime_config;

/* Zero-initialize the configuration and fill the fields you need. NULL config
 * also selects defaults. A zero heap size uses the default. A positive size is
 * raised to at least 1 MiB. This limit applies to managed Lisp allocation.
 * Host allocations and native code are outside it, and GC may use additional
 * evacuation space. A NULL step_budget disables the per-invocation budget.
 * A non-NULL budget is copied before the call returns. An absent I/O callback
 * uses the corresponding process stream. Standard I/O includes terminal input
 * and output. Trace output uses the standard output callback. The same native
 * code preconditions apply if allow_foreign_calls is true. */
zkcl_error *zkcl_runtime_new_with_config(const zkcl_runtime_config *config, zkcl_runtime **out);
void zkcl_runtime_free(zkcl_runtime *rt);
zkcl_error *zkcl_collect(const zkcl_runtime *rt);

typedef struct {
    size_t allocation_limit_bytes;
    size_t committed_bytes;
    size_t large_object_bytes;
    uint64_t collections;
    uint64_t last_collection_ns;
} zkcl_heap_stats;

/* Read a snapshot without triggering GC. Committed bytes include Immix block
 * capacity and large object allocations. The collection count includes explicit
 * and automatic GC. last_collection_ns is zero before the first GC and excludes
 * the time spent destroying foreign payloads. The output is written on success. */
zkcl_error *zkcl_get_heap_stats(const zkcl_runtime *rt, zkcl_heap_stats *out);

typedef struct {
    size_t live_bytes;
    size_t allocated_bytes_since_collection;
    size_t last_reclaimed_bytes;
    size_t last_evacuated_bytes;
    size_t last_evacuated_objects;
    size_t block_count;
    size_t last_evacuation_blocks;
    size_t evacuation_reserve_bytes;
    uint64_t compaction_retries;
    uint64_t total_collection_ns;
} zkcl_gc_stats;

/* Detailed collection counters. live_bytes describes the most recent GC.
 * last_reclaimed_bytes counts blocks and large-object storage returned to the
 * allocator. evacuation_reserve_bytes bounds the extra managed space available
 * during compaction. total_collection_ns excludes foreign payload destruction.
 * The output is written on success and reading it does not trigger GC. */
zkcl_error *zkcl_get_gc_stats(const zkcl_runtime *rt, zkcl_gc_stats *out);

/* Publish a statically linked function or data address as an exact, nonempty,
 * case-sensitive ZK-FFI :HOST name. Registering the same name and address again
 * succeeds. A name already registered with a different address is an error.
 * The runtime must allow foreign calls. The caller keeps the address valid
 * while the runtime can use it. Cast a function pointer to zkcl_foreign_function
 * for registration, and declare its original C prototype in Lisp. */
typedef void (*zkcl_foreign_function)(void);
zkcl_error *zkcl_register_foreign_function(const zkcl_runtime *rt, zkcl_str name,
                                           zkcl_foreign_function address);
zkcl_error *zkcl_register_foreign_data(const zkcl_runtime *rt, zkcl_str name, void *address);

/* Create an owned Lisp handle for a borrowed native address. NULL becomes NIL.
 * The runtime must allow foreign calls. Supply the address's real bounds, access
 * permissions, and ABI in Lisp, and keep it valid through every native use. */
zkcl_error *zkcl_make_foreign_pointer(const zkcl_runtime *rt, void *address, zkcl_value **out);

/* Read a foreign pointer address (Lisp NIL is 0). Keep the owning Lisp handle
 * alive and any referenced storage valid while using the address. out is
 * written on success. */
zkcl_error *zkcl_get_foreign_pointer(const zkcl_runtime *rt, const zkcl_value *value,
                                     uintptr_t *out);

/* Run a native operation on the owning thread, enabling Lisp C callbacks
 * for this runtime until it returns. Retain all callback handles that native
 * code could invoke. Callback results use the declared ABI and any temporary
 * C strings stay valid until run returns. run must return normally. A failed
 * Lisp callback returns zero to C and takes precedence over an owned error
 * returned by run. Calls within the scope share the invocation budget. */
typedef zkcl_error *(*zkcl_foreign_scope)(void *userdata);
zkcl_error *zkcl_with_foreign_callbacks(const zkcl_runtime *rt, zkcl_foreign_scope run,
                                       void *userdata);


/* One form for eval. Load accepts consecutive forms. Load_file accepts a UTF-8
 * path to Lisp source or a compiled zkcl FASL. */
zkcl_error *zkcl_eval(const zkcl_runtime *rt, zkcl_str form, zkcl_values **out);
zkcl_error *zkcl_load(const zkcl_runtime *rt, zkcl_str source);
zkcl_error *zkcl_load_file(const zkcl_runtime *rt, zkcl_str path);
/* Load one compiled FASL from memory. Supply len readable bytes for the
 * duration of the call. The runtime verifies the file and its target before
 * running its top-level forms. */
zkcl_error *zkcl_load_fasl(const zkcl_runtime *rt, const uint8_t *bytes, size_t len);

/* Instance-local configuration. Working directory updates pathname defaults;
 * environment overrides affect ZK:GETENV. Neither changes the process state.
 * NULL value hides an inherited environment variable; an empty view sets "".
 * Modules use Common Lisp REQUIRE and this runtime's provider configuration. */
zkcl_error *zkcl_require_module(const zkcl_runtime *rt, zkcl_str name);
/* Owned JSON string array, preserving the current *MODULES* list order. */
zkcl_error *zkcl_loaded_modules(const zkcl_runtime *rt, zkcl_text **out);
zkcl_error *zkcl_add_module_directory(const zkcl_runtime *rt, zkcl_str path);
zkcl_error *zkcl_set_working_directory(const zkcl_runtime *rt, zkcl_str path);
zkcl_error *zkcl_set_environment_variable(const zkcl_runtime *rt, zkcl_str name, const zkcl_str *value);
zkcl_error *zkcl_set_arguments(const zkcl_runtime *rt, const zkcl_str *arguments, size_t count);
zkcl_error *zkcl_requested_exit_code(const zkcl_runtime *rt, uint8_t *out);

/* FIND-SYMBOL without reader invocation or interning. A missing symbol succeeds
 * with status=0 and *out=NULL. Status 1=internal, 2=external, 3=inherited.
 * A found symbol is an owned value. Unknown package names are errors.
 * Namespace snapshot is owned UTF-8 JSON with packages, relationships and
 * available definitions; taking it runs no Lisp code. */
zkcl_error *zkcl_find_symbol(const zkcl_runtime *rt, zkcl_str package, zkcl_str name,
                             uint32_t *status, zkcl_value **out);
zkcl_error *zkcl_namespace_snapshot(const zkcl_runtime *rt, zkcl_text **out);

/* Prepared projects: build zkcl-capi with the delivery feature. A bundle owns
 * FASLs, ordered startup and declared ASDF resources. Loading verifies its files
 * before execution and does not resolve or fetch dependencies. Keep the bundle
 * directory and output directory available while loaded code can use them.
 * Initialization can have effects before failing; discard a candidate runtime.
 * call returns the binary entry's multiple values without exiting the host. */
typedef struct zkcl_bundle zkcl_bundle;
zkcl_error *zkcl_bundle_open(zkcl_str directory, zkcl_bundle **out);
void zkcl_bundle_free(zkcl_bundle *bundle);
zkcl_error *zkcl_bundle_load(const zkcl_bundle *bundle, const zkcl_runtime *rt, zkcl_str output_directory);
zkcl_error *zkcl_bundle_call(const zkcl_bundle *bundle, const zkcl_runtime *rt, zkcl_values **out);

/* Explicit ASDF operations and live metadata (delivery feature). Operations:
 * 0=load, 1=compile, 2=test. system_info may execute a selected .asd definition
 * and component accessors; it returns owned JSON, or JSON null if absent. */
zkcl_error *zkcl_asdf_operate(const zkcl_runtime *rt, zkcl_str system, uint32_t operation);
zkcl_error *zkcl_asdf_system_info(const zkcl_runtime *rt, zkcl_str system, zkcl_text **out);

/* Source projects: build zkcl-capi with the project feature. Opening and info
 * read declarative metadata without running Lisp. Resolve returns an owned
 * S-expression lock graph; it may consult explicitly configured registries but
 * does not fetch source archives, write locks, or initialize a runtime.
 * Loading resolves, acquires sources and initializes on the supplied runtime.
 * Register callbacks/configure I/O first. Lockfile writes are explicit.
 * The runtime and its session belong to one thread. Keep the runtime alive
 * until every project session is freed. Sessions own temporary compilation and
 * ASDF output paths; keep the session alive while code uses those paths.
 * Rebuilding should use a fresh candidate runtime, then switch on success. */
typedef struct zkcl_project zkcl_project;
typedef struct zkcl_project_session zkcl_project_session;
typedef struct { zkcl_str name; const zkcl_str *value; } zkcl_project_environment;
/* Configure each fresh build instance before generator/dependency code runs.
 * May register host functions. Keep callback and userdata alive through load.
 * Return normally with NULL or an owned error. */
typedef zkcl_error *(*zkcl_build_initializer)(const zkcl_runtime *rt, void *userdata);
typedef struct {
    bool locked;
    bool offline;
    uint32_t target_kind; /* 0=library (default), 1=binary, 2=test */
    zkcl_str binary; /* Empty selects the only binary. */
    zkcl_str target; /* Empty selects the native target. */
    const zkcl_str *registries;
    size_t registry_count;
    const zkcl_str *features;
    size_t feature_count;
    zkcl_str output_directory; /* Empty uses the project's target directory. */
    bool allow_build_foreign_calls; /* Same native-code contract as runtime creation. */
    const zkcl_project_environment *build_environment;
    size_t build_environment_count;
    zkcl_build_initializer initialize_build;
    void *build_userdata;
} zkcl_project_options;
zkcl_error *zkcl_project_open(zkcl_str path, zkcl_project **out);
void zkcl_project_free(zkcl_project *project);
zkcl_error *zkcl_project_info(const zkcl_project *project, zkcl_text **out);
zkcl_error *zkcl_project_resolve(const zkcl_project *project, const zkcl_project_options *options,
                                 zkcl_text **out);
zkcl_error *zkcl_project_load(const zkcl_project *project, const zkcl_runtime *rt,
                              const zkcl_project_options *options, zkcl_project_session **out);
void zkcl_project_session_free(zkcl_project_session *session);
zkcl_error *zkcl_project_call(const zkcl_project_session *session, zkcl_values **out);
zkcl_error *zkcl_project_test(const zkcl_project_session *session);
zkcl_error *zkcl_project_write_lock(const zkcl_project_session *session);
zkcl_error *zkcl_project_write_bundle(const zkcl_project_session *session, zkcl_str directory);
/* Project errors use kind 11. Details is borrowed UTF-8 JSON containing the
 * cause chain and, when known, the source path, source ID and UTF-8 byte range.
 * Other errors return an empty view. Available with the delivery feature. */
zkcl_str zkcl_error_details(const zkcl_error *error);

/* Literal, case-sensitive package name/nickname and symbol print name.
 * Missing names return an error. Calling a symbol follows redefinition.
 * Calling a saved function object invokes that particular definition. */
zkcl_error *zkcl_lookup_symbol(const zkcl_runtime *rt, zkcl_str package, zkcl_str name, zkcl_value **out);
zkcl_error *zkcl_resolve_function(const zkcl_runtime *rt, const zkcl_value *target, zkcl_value **out);
zkcl_error *zkcl_call(const zkcl_runtime *rt, const zkcl_value *target,
                      const zkcl_value *const *argv, size_t argc, zkcl_values **out);

enum {
    ZKCL_EVALUATION_READY = 0, ZKCL_EVALUATION_RUNNABLE = 1,
    ZKCL_EVALUATION_WAITING = 2, ZKCL_EVALUATION_WAITING_TIMER = 3,
    ZKCL_EVALUATION_PAUSED = 4
};
typedef struct {
    uint32_t state;
    uint64_t delay_ns;
    zkcl_values *values;
    zkcl_debug_event *debug_event;
} zkcl_evaluation_result;

/* Read one form or prepare a call on the owning thread. Call arguments may be
 * released after start_call returns. One active evaluation leases the runtime.
 * Resume there to execute one slice. Synchronous reader macros, native callbacks
 * and collection may take longer than a slice. A successful READY transfers an
 * owned result tuple, including for zero values. RUNNABLE asks for another slice.
 * WAITING asks the caller to resume after a wake. WAITING_TIMER also requests a
 * resume after delay_ns nanoseconds. Waker is optional and applies to this resume.
 * Supply an output struct with no outstanding values or debug_event. On error
 * both handle members are NULL. Free the handle on the owning thread after any active
 * resume returns. Freeing an unfinished evaluation stops it immediately. To run
 * its Lisp cleanups, request cooperative cancellation and resume to completion. */
zkcl_error *zkcl_start_eval(const zkcl_runtime *rt, zkcl_str form, zkcl_evaluation **out);
zkcl_error *zkcl_start_call(const zkcl_runtime *rt, const zkcl_value *target,
                           const zkcl_value *const *argv, size_t argc, zkcl_evaluation **out);
zkcl_error *zkcl_evaluation_resume(const zkcl_evaluation *evaluation, const zkcl_waker *waker,
                                   zkcl_evaluation_result *out);
void zkcl_evaluation_free(zkcl_evaluation *evaluation);
/* Opt in before resuming to receive PAUSED events. A PAUSED result owns its
 * debug_event and has values=NULL. Release the event with zkcl_debug_event_free.
 * Event snapshots and retained condition objects survive continuing. Do not
 * resume repeatedly to continue a pause: use with_debugger and choose a motion,
 * restart, or frame transfer first. Normal evaluations keep synchronous debugger
 * behavior unless enabled explicitly. */
zkcl_error *zkcl_evaluation_enable_debugging(const zkcl_evaluation *evaluation);
/* Run one debugger operation after resume returned PAUSED. The callback runs
 * synchronously with that task's dynamic context and may use the typed debug
 * APIs below. It may collect, retain values, and inspect objects. Return errors
 * unchanged so control transfers reach the suspended evaluation. Reentering
 * resume/with_debugger for this evaluation returns an error. Do not free the
 * evaluation or its runtime from the callback. */
typedef zkcl_error *(*zkcl_debug_operation)(const zkcl_runtime *rt, void *userdata);
zkcl_error *zkcl_evaluation_with_debugger(const zkcl_evaluation *evaluation,
                                         zkcl_debug_operation callback, void *userdata);
const zkcl_value *zkcl_debug_event_condition(const zkcl_debug_event *event);
zkcl_str zkcl_debug_event_condition_type(const zkcl_debug_event *event);
zkcl_str zkcl_debug_event_message(const zkcl_debug_event *event);
void zkcl_debug_event_free(zkcl_debug_event *event);


/* Notification and release callbacks must return normally and may run on any
 * thread. Notify schedules a later resume on the owning thread. It may run
 * concurrently with other notifications and can precede resume's return.
 * On successful creation the waker retains userdata. A supplied release runs
 * once after the last retained copy and notification are gone. If release is
 * NULL, the caller supplies storage with sufficient lifetime. The waker may be
 * freed once resume returns. Unfinished evaluation holds its own copy. */
typedef void (*zkcl_notify)(void *userdata);
typedef void (*zkcl_release)(void *userdata);
zkcl_error *zkcl_waker_new(zkcl_notify notify, void *userdata, zkcl_release release, zkcl_waker **out);
void zkcl_waker_free(zkcl_waker *waker);

/* A result tuple owns its elements, including a distinct empty tuple for
 * (values). Get returns a borrowed value, or NULL for an out-of-range index.
 * Retain an element to use it after its tuple or callback has finished. */
size_t zkcl_values_len(const zkcl_values *values);
const zkcl_value *zkcl_values_get(const zkcl_values *values, size_t index);
void zkcl_values_free(zkcl_values *values);
zkcl_error *zkcl_value_retain(const zkcl_runtime *rt, const zkcl_value *value, zkcl_value **out);
void zkcl_value_free(zkcl_value *value);

zkcl_error *zkcl_make_i64(const zkcl_runtime *rt, int64_t n, zkcl_value **out);
zkcl_error *zkcl_get_i64(const zkcl_runtime *rt, const zkcl_value *value, int64_t *out);
zkcl_error *zkcl_make_double(const zkcl_runtime *rt, double n, zkcl_value **out);
zkcl_error *zkcl_get_double(const zkcl_runtime *rt, const zkcl_value *value, double *out);
zkcl_error *zkcl_make_string(const zkcl_runtime *rt, zkcl_str text, zkcl_value **out);
zkcl_error *zkcl_get_string(const zkcl_runtime *rt, const zkcl_value *value, zkcl_text **out);
zkcl_error *zkcl_repr(const zkcl_runtime *rt, const zkcl_value *value, zkcl_text **out);
zkcl_str zkcl_text_view(const zkcl_text *text);
void zkcl_text_free(zkcl_text *text);

/* Callback arguments and return builder are borrowed for this invocation.
 * Append zero or more results in order. Each append retains its own root.
 * Reentry on the same runtime is supported. Return NULL on success, or
 * transfer an owned error to Lisp. A callback must return normally.
 * The caller owns userdata and keeps it valid until runtime destruction,
 * including after redefinition, since saved functions remain callable. */
typedef zkcl_error *(*zkcl_callback)(const zkcl_runtime *rt, const zkcl_values *args,
                                     zkcl_returns *results, void *userdata);
/* The name uses Lisp function-name syntax, including package prefixes and SETF. */
zkcl_error *zkcl_register_function(const zkcl_runtime *rt, zkcl_str name,
                                   zkcl_callback callback, void *userdata);
zkcl_error *zkcl_return_value(zkcl_returns *results, const zkcl_value *value);

/* Start runs on the owning thread with borrowed arguments. It receives an
 * owned completion and must finish or free it exactly once, including when
 * reporting an error. Copy needed data before returning. Registry userdata
 * follows the same lifetime as for zkcl_register_function. The worker can
 * query cancellation and submit completion without access to a Runtime or Lisp
 * values. Finish or free the completion after the provider has stopped using
 * its resources. Both cooperative cancellation and a start error wait for
 * this acknowledgement. Returning a start error requests cancellation. */
typedef zkcl_error *(*zkcl_async_start)(const zkcl_runtime *rt, const zkcl_values *args,
                                        zkcl_completion *completion, void *userdata);
zkcl_error *zkcl_register_async_function(const zkcl_runtime *rt, zkcl_str name,
                                         zkcl_async_start start, void *userdata);
bool zkcl_completion_is_cancelled(const zkcl_completion *completion);
/* Return true when cancellation was requested. Otherwise retain waker and
 * replace the previous cancellation notification. NULL waker only checks the
 * state. A notification may run before this call returns. Finish or free the
 * completion after the provider has released the operation's resources. */
bool zkcl_completion_poll_cancelled(const zkcl_completion *completion, const zkcl_waker *waker);

/* Finish consumes completion and userdata, including if it returns false
 * because the runtime retired the wait. Materialize, if provided, runs on the
 * owning thread only if execution reaches the reply. Append values or return
 * an owned error. NULL materialize supplies zero values. Release, if supplied,
 * runs once when the reply is consumed or discarded and may run on any thread.
 * Freeing a completion without a reply reports abandonment to an uncancelled
 * waiter. The provider may use completion after the evaluation or runtime has
 * been destroyed. Completion and its userdata are independent of Lisp roots. */
typedef zkcl_error *(*zkcl_materialize)(const zkcl_runtime *rt, zkcl_returns *results, void *userdata);
bool zkcl_completion_finish(zkcl_completion *completion, zkcl_materialize materialize,
                            void *userdata, zkcl_release release);
void zkcl_completion_free(zkcl_completion *completion);

/* NULL disables the per-invocation budget. Zero stops execution at the next checkpoint.
 * Cancel/reset can run on any thread while the handle is live. Cancel with cleanup runs
 * active UNWIND-PROTECT forms. Cancel and budget exhaustion can interrupt those forms.
 * Requests remain set until reset. Free the handle after its last concurrent use. */
zkcl_error *zkcl_set_step_budget(const zkcl_runtime *rt, const uint64_t *budget);
zkcl_error *zkcl_cancellation_new(const zkcl_runtime *rt, zkcl_cancellation **out);
void zkcl_cancel(const zkcl_cancellation *handle);
void zkcl_cancel_with_cleanup(const zkcl_cancellation *handle);
void zkcl_cancel_reset(const zkcl_cancellation *handle);
void zkcl_cancellation_free(zkcl_cancellation *handle);

enum {
    ZKCL_OK = 0, ZKCL_INVALID_ARGUMENT = 1, ZKCL_READ_ERROR = 2,
    ZKCL_COMPILE_ERROR = 3, ZKCL_LISP_CONDITION = 4, ZKCL_HOST_ERROR = 5,
    ZKCL_OUT_OF_MEMORY = 6, ZKCL_CANCELLED = 7, ZKCL_RUNTIME_ERROR = 8,
    ZKCL_PANIC = 9, ZKCL_MACRO_EXPANSION_ERROR = 10, ZKCL_PROJECT_ERROR = 11
};
uint32_t zkcl_error_kind(const zkcl_error *error);
zkcl_str zkcl_error_message(const zkcl_error *error);
/* Half-open UTF-8 byte range of a Lisp diagnostic. Other errors give {0, 0}. */
zkcl_range zkcl_error_range(const zkcl_error *error);
zkcl_error *zkcl_error_new(zkcl_str message);
void zkcl_error_free(zkcl_error *error);

/* Tooling snapshots own their strings, source documents, and Lisp roots.
 * Getters borrow from their owning snapshot; retain values or sources before
 * freeing it. Snapshots can be freed after runtime destruction on its thread.
 * Source ranges remain relative to the submitted UTF-8 text. base_offset maps
 * them to the original document. Empty path/URI means absent; version may be NULL.
 * Source handles may be released after eval/load/start_eval_document returns. */
typedef struct zkcl_source zkcl_source;
typedef struct zkcl_error_frame zkcl_error_frame;
typedef struct zkcl_definitions zkcl_definitions;
typedef struct zkcl_inspection zkcl_inspection;
typedef struct zkcl_pause_handle zkcl_pause_handle;
typedef struct zkcl_debug_frames zkcl_debug_frame_list;
typedef struct zkcl_debug_variables zkcl_debug_variable_list;
/* Borrowed frame snapshot owned by the event. Do not free it. */
const zkcl_debug_frame_list *zkcl_debug_event_frames(const zkcl_debug_event *event);
typedef struct zkcl_profile zkcl_profile;
zkcl_error *zkcl_source_new(zkcl_str text, zkcl_str path, zkcl_str uri,
                            const int64_t *version, size_t base_offset, zkcl_source **out);
zkcl_error *zkcl_source_retain(const zkcl_source *source, zkcl_source **out);
void zkcl_source_free(zkcl_source *source);
uint64_t zkcl_source_id(const zkcl_source *source);
zkcl_str zkcl_source_text(const zkcl_source *source);
zkcl_str zkcl_source_uri(const zkcl_source *source);
zkcl_error *zkcl_source_path(const zkcl_source *source, zkcl_text **out);
bool zkcl_source_version(const zkcl_source *source, int64_t *out);
size_t zkcl_source_base_offset(const zkcl_source *source);
zkcl_error *zkcl_source_absolute_range(const zkcl_source *source, zkcl_range range, zkcl_range *out);
zkcl_error *zkcl_eval_document(const zkcl_runtime *rt, const zkcl_source *source, zkcl_values **out);
zkcl_error *zkcl_load_document(const zkcl_runtime *rt, const zkcl_source *source);
zkcl_error *zkcl_start_eval_document(const zkcl_runtime *rt, const zkcl_source *source, zkcl_evaluation **out);

/* Error frames and their sources are borrowed until the error is freed. */
/* 0: none, 1: cooperative cancellation, 2: immediate abort, 3: step budget. */
uint32_t zkcl_error_execution_stop(const zkcl_error *error);
zkcl_str zkcl_error_condition_type(const zkcl_error *error);
const zkcl_source *zkcl_error_source(const zkcl_error *error);
size_t zkcl_error_stack_len(const zkcl_error *error);
const zkcl_error_frame *zkcl_error_stack_get(const zkcl_error *error, size_t index);
typedef struct {
    zkcl_str function;
    const zkcl_source *source;
    zkcl_range range;
    size_t bytecode_pc;
    bool has_range, has_bytecode_pc, native;
} zkcl_error_frame_info;
zkcl_error *zkcl_error_frame_view(const zkcl_error_frame *frame, zkcl_error_frame_info *out);

enum {
    ZKCL_DEFINITION_FUNCTION = 0, ZKCL_DEFINITION_MACRO = 1,
    ZKCL_DEFINITION_METHOD = 2, ZKCL_DEFINITION_CLASS = 3,
    ZKCL_DEFINITION_VARIABLE = 4, ZKCL_DEFINITION_CONSTANT = 5,
    ZKCL_DEFINITION_GENERIC_FUNCTION = 6
};
typedef struct {
    zkcl_str name;
    uint32_t kind;
    const zkcl_source *source;
    zkcl_range range;
    zkcl_str lambda_list, declared_type, documentation;
} zkcl_definition;
zkcl_error *zkcl_find_definitions(const zkcl_runtime *rt, zkcl_str name, zkcl_definitions **out);
zkcl_error *zkcl_definition_info(const zkcl_runtime *rt, const zkcl_value *value, zkcl_definitions **out);
size_t zkcl_definitions_len(const zkcl_definitions *items);
zkcl_error *zkcl_definitions_get(const zkcl_definitions *items, size_t index, zkcl_definition *out);
void zkcl_definitions_free(zkcl_definitions *items);

typedef struct {
    size_t index;
    zkcl_str label;
    const zkcl_value *value; /* NULL for unbound slots. */
    bool writable;
} zkcl_inspection_part;
zkcl_error *zkcl_inspect(const zkcl_runtime *rt, const zkcl_value *object,
                         size_t start, size_t count, zkcl_inspection **out);
zkcl_str zkcl_inspection_summary(const zkcl_inspection *inspection);
size_t zkcl_inspection_total(const zkcl_inspection *inspection);
size_t zkcl_inspection_len(const zkcl_inspection *inspection);
zkcl_error *zkcl_inspection_get(const zkcl_inspection *inspection, size_t index, zkcl_inspection_part *out);
void zkcl_inspection_free(zkcl_inspection *inspection);
zkcl_error *zkcl_inspection_replace(const zkcl_runtime *rt, const zkcl_value *object,
                                    size_t index, const zkcl_value *value);

/* A debugger callback runs synchronously on the owning thread, with one
 * condition argument. Use the callback's dynamic extent to inspect frames,
 * change locals, evaluate, choose motion, or invoke restarts. A frame index is
 * valid only for that pause. Rooted variable snapshots survive continuing.
 * Returning normally propagates an unhandled condition; at a resumable stop,
 * choosing motion permits execution to continue. Propagate every returned error
 * unchanged, including nonlocal restart/frame-transfer errors, to the callback
 * caller. Do not free a transferred error. NULL callback disables the debugger.
 * Callback/userdata lifetime and reentry follow zkcl_register_function. */
zkcl_error *zkcl_set_debugger(const zkcl_runtime *rt, zkcl_callback callback, void *userdata);
/* Pause requests are independent of cancellation and may be used from any
 * thread. Request is consumed at a debugger checkpoint. Free after concurrent
 * uses finish. Install a synchronous debugger or enable incremental debugging
 * on the running evaluation to receive the stop. */
zkcl_error *zkcl_pause_new(const zkcl_runtime *rt, zkcl_pause_handle **out);
void zkcl_pause(const zkcl_pause_handle *handle);
void zkcl_pause_reset(const zkcl_pause_handle *handle);
bool zkcl_pause_pending(const zkcl_pause_handle *handle);
void zkcl_pause_free(zkcl_pause_handle *handle);
typedef struct {
    size_t index;
    zkcl_str function, path;
    size_t line, column; /* One-based; zero when unavailable. */
    zkcl_str source_text;
    size_t bytecode_pc;
    bool has_bytecode_pc, native, locals_available;
    zkcl_str locals_reason;
    bool can_return, can_restart;
} zkcl_debug_frame;
zkcl_error *zkcl_debug_frames(const zkcl_runtime *rt, zkcl_debug_frame_list **out);
size_t zkcl_debug_frames_len(const zkcl_debug_frame_list *frames);
zkcl_error *zkcl_debug_frames_get(const zkcl_debug_frame_list *frames, size_t index, zkcl_debug_frame *out);
void zkcl_debug_frames_free(zkcl_debug_frame_list *frames);
typedef struct {
    const zkcl_value *name, *value;
    zkcl_str unavailable;
    bool writable;
} zkcl_debug_variable;
zkcl_error *zkcl_debug_variables(const zkcl_runtime *rt, size_t frame, zkcl_debug_variable_list **out);
size_t zkcl_debug_variables_len(const zkcl_debug_variable_list *variables);
zkcl_error *zkcl_debug_variables_get(const zkcl_debug_variable_list *variables, size_t index, zkcl_debug_variable *out);
void zkcl_debug_variables_free(zkcl_debug_variable_list *variables);
zkcl_error *zkcl_debug_local(const zkcl_runtime *rt, size_t frame, const zkcl_value *name, zkcl_value **out);
zkcl_error *zkcl_debug_set_local(const zkcl_runtime *rt, size_t frame, const zkcl_value *name, const zkcl_value *value);
zkcl_error *zkcl_debug_eval(const zkcl_runtime *rt, size_t frame, zkcl_str source, zkcl_values **out);
enum { ZKCL_DEBUG_CONTINUE = 0, ZKCL_DEBUG_INTO = 1, ZKCL_DEBUG_NEXT = 2,
       ZKCL_DEBUG_OUT = 3, ZKCL_DEBUG_ABORT = 4 };
zkcl_error *zkcl_debug_motion(const zkcl_runtime *rt, uint32_t motion, bool *resumable);
zkcl_error *zkcl_debug_restarts(const zkcl_runtime *rt, zkcl_values **out);
zkcl_error *zkcl_debug_invoke_restart(const zkcl_runtime *rt, const zkcl_value *restart,
                                      const zkcl_value *const *argv, size_t argc, zkcl_values **out);
zkcl_error *zkcl_debug_return_frame(const zkcl_runtime *rt, size_t frame,
                                    const zkcl_value *const *argv, size_t argc);
zkcl_error *zkcl_debug_restart_frame(const zkcl_runtime *rt, size_t frame);
zkcl_error *zkcl_add_breakpoint(const zkcl_runtime *rt, const zkcl_value *function,
                                const size_t *pc, uint64_t *out);
zkcl_error *zkcl_add_source_breakpoint(const zkcl_runtime *rt, const zkcl_value *function,
                                       size_t line, uint64_t *out);
zkcl_error *zkcl_remove_breakpoint(const zkcl_runtime *rt, uint64_t id, bool *removed);

enum { ZKCL_BREAKPOINT_OBJECT = 0, ZKCL_BREAKPOINT_NAME = 1, ZKCL_BREAKPOINT_SOURCE = 2 };
typedef struct {
    bool enabled;
    zkcl_str condition;
    uint64_t hit_count, hit_every; /* Zero selects the default of 1. */
    zkcl_str log;
    bool log_only;
    uint32_t follow;
} zkcl_breakpoint_options;
typedef struct zkcl_breakpoint_list zkcl_breakpoint_list;
typedef struct {
    uint64_t id;
    zkcl_str function;
    size_t bytecode_pc;
    const zkcl_source *source;
    size_t offset;
    bool has_offset;
    zkcl_breakpoint_options options;
    uint64_t hits;
    bool verified;
    zkcl_str pending_reason;
} zkcl_breakpoint;
/* NULL options selects enabled object identity mode. A supplied options struct
 * copies all text before returning. Name/source follow requires a symbol target.
 * location is an instruction or one-based source line according to source_line. */
zkcl_error *zkcl_add_breakpoint_with_options(const zkcl_runtime *rt, const zkcl_value *function,
                                              size_t location, bool source_line,
                                              const zkcl_breakpoint_options *options, uint64_t *out);
zkcl_error *zkcl_update_breakpoint(const zkcl_runtime *rt, uint64_t id, const zkcl_breakpoint_options *options);
zkcl_error *zkcl_enable_breakpoint(const zkcl_runtime *rt, uint64_t id, bool enabled);
zkcl_error *zkcl_breakpoints(const zkcl_runtime *rt, zkcl_breakpoint_list **out);
size_t zkcl_breakpoints_len(const zkcl_breakpoint_list *items);
zkcl_error *zkcl_breakpoints_get(const zkcl_breakpoint_list *items, size_t index, zkcl_breakpoint *out);
void zkcl_breakpoints_free(zkcl_breakpoint_list *items);
typedef struct zkcl_location_list zkcl_location_list;
typedef struct {
    size_t bytecode_pc;
    const zkcl_source *source;
    size_t offset;
    bool has_offset;
    size_t line, column;
} zkcl_executable_location;
zkcl_error *zkcl_executable_locations(const zkcl_runtime *rt, const zkcl_value *function, zkcl_location_list **out);
size_t zkcl_executable_locations_len(const zkcl_location_list *items);
zkcl_error *zkcl_executable_locations_get(const zkcl_location_list *items, size_t index, zkcl_executable_location *out);
void zkcl_executable_locations_free(zkcl_location_list *items);

/* Image operations require an idle runtime. Restore into a fresh runtime to
 * preserve a running session if restoration fails. Nonserializable resources
 * are rejected; these files do not preserve an active stack. */
zkcl_error *zkcl_save_image(const zkcl_runtime *rt, zkcl_str path);
/* Restore requires exclusive access: no live evaluation handles, active calls,
 * or callbacks into this runtime. It replaces runtime state. */
zkcl_error *zkcl_restore_image(zkcl_runtime *rt, zkcl_str path);

/* Profiling counts aligned managed allocation bytes; host allocations are
 * excluded. Time sampling takes place at runtime safe points. Function times exclude
 * scheduler waits and other tasks; report elapsed time covers the session.
 * native_calls counts native activations within calls. Durations are
 * saturated nanoseconds. A report remains valid after subsequent profiling. */
zkcl_error *zkcl_profile_start(const zkcl_runtime *rt, uint64_t sample_interval_ns);
zkcl_error *zkcl_profile_report(const zkcl_runtime *rt, bool stop, zkcl_profile **out);
zkcl_error *zkcl_profile_reset(const zkcl_runtime *rt);
typedef struct {
    bool running;
    uint64_t elapsed_ns;
    zkcl_str sampling_mode;
    uint64_t samples, allocated_bytes, collections, gc_time_ns;
    size_t function_count;
} zkcl_profile_info;
typedef struct {
    uint64_t id;
    zkcl_str name;
    uint64_t calls, native_calls, total_time_ns, self_time_ns, samples, allocated_bytes, self_allocated_bytes;
} zkcl_function_profile;
zkcl_error *zkcl_profile_view(const zkcl_profile *profile, zkcl_profile_info *out);
zkcl_error *zkcl_profile_function(const zkcl_profile *profile, size_t index, zkcl_function_profile *out);
void zkcl_profile_free(zkcl_profile *profile);

/* Installed native SDK. All module data and entry points must be statically
 * linked, immutable, and remain valid until process exit. Only call the runner
 * once. Generated code and SDK must come from the same toolchain. */
typedef void (*zkcl_native_entry)(void *, uintptr_t, const uintptr_t *, size_t, void *);
typedef struct {
    uint32_t index;
    size_t required, maximum, root_slots;
    bool variadic;
    bool native_debug;
    zkcl_native_entry entry;
    uintptr_t (*leaf)(uintptr_t, uintptr_t, uintptr_t, uintptr_t);
    zkcl_str leaf_guards;
} zkcl_native_code;
typedef struct {
    zkcl_str id;
    const unsigned char *fasl;
    size_t fasl_len;
    const zkcl_native_code *codes;
    size_t code_count;
    uint8_t pointer_width;
    uint32_t word_layout_version;
    uint64_t bytecode_contract_version;
    uint32_t continuation_abi;
} zkcl_native_module;
/* plan is UTF-8 JSON emitted by zkcl build, describing ordered startup.
 * argv includes the program name. Returns the requested Lisp exit status,
 * or 1 after printing a runtime error. */
int zkcl_native_run(const zkcl_native_module *modules, size_t module_count,
                    zkcl_str plan, int argc, const char *const *argv);

#ifdef __cplusplus
}
#endif
#endif
