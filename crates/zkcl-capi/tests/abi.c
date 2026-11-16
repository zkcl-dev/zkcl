#include "zkcl.h"
#include <assert.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

static void ok(zkcl_error *error) {
    if (error) {
        zkcl_str text = zkcl_error_message(error);
        fwrite(text.data, 1, text.len, stderr);
        fputc('\n', stderr);
        abort();
    }
}

static void expect_error(zkcl_error *error, uint32_t kind) {
    assert(error && zkcl_error_kind(error) == kind);
    assert(zkcl_error_message(error).len > 0);
    zkcl_error_free(error);
}

static void test_compiled_asset(const zkcl_runtime *runtime, const char *path) {
    FILE *file = fopen(path, "rb");
    assert(file && fseek(file, 0, SEEK_END) == 0);
    long length = ftell(file);
    assert(length > 1 && fseek(file, 0, SEEK_SET) == 0);
    size_t size = (size_t)length;
    uint8_t *bytes = malloc(size);
    assert(bytes && fread(bytes, 1, size, file) == size);
    fclose(file);

    expect_error(zkcl_load_fasl(runtime, NULL, size), ZKCL_INVALID_ARGUMENT);
    expect_error(zkcl_load_fasl(runtime, bytes, size - 1), ZKCL_RUNTIME_ERROR);
    ok(zkcl_load(runtime, zkcl_utf8("(assert (not (fboundp 'asset-value)))")));
    ok(zkcl_load_fasl(runtime, bytes, size));
    free(bytes);
    ok(zkcl_collect(runtime));
    ok(zkcl_load(runtime, zkcl_utf8("(assert (equal (multiple-value-list (asset-value)) '(42 nil)))")));
}

struct input {
    const char *next;
    bool fail;
    bool oversized;
};

static ptrdiff_t read_byte(void *userdata, char *buffer, size_t capacity) {
    struct input *input = userdata;
    assert(capacity > 0);
    if (input->fail) return -1;
    if (input->oversized) return (ptrdiff_t)capacity + 1;
    if (!*input->next) return 0;
    *buffer = *input->next++;
    return 1;
}

struct output {
    char bytes[64];
    size_t used;
    unsigned flushes;
    bool fail;
};

static bool write_bytes(void *userdata, const char *data, size_t length) {
    struct output *output = userdata;
    if (output->fail) return false;
    if (!data) {
        assert(length == 0);
        output->flushes++;
    } else {
        assert(length <= sizeof output->bytes - output->used);
        memcpy(output->bytes + output->used, data, length);
        output->used += length;
    }
    return true;
}

static void test_runtime_config(void) {
    const size_t mib = 1024 * 1024;
    zkcl_runtime *runtime = NULL, *other = NULL;
    zkcl_heap_stats stats;
    zkcl_gc_stats gc;
    zkcl_runtime_config config = { .heap_size_bytes = 1 };
    ok(zkcl_runtime_new_with_config(&config, &runtime));
    ok(zkcl_runtime_new_with_config(NULL, &other));
    expect_error(zkcl_get_heap_stats(NULL, &stats), ZKCL_INVALID_ARGUMENT);
    expect_error(zkcl_get_heap_stats(runtime, NULL), ZKCL_INVALID_ARGUMENT);
    ok(zkcl_get_heap_stats(runtime, &stats));
    assert(stats.allocation_limit_bytes == mib && stats.committed_bytes == 0);
    assert(stats.collections == 0 && stats.last_collection_ns == 0);
    expect_error(zkcl_get_gc_stats(NULL, &gc), ZKCL_INVALID_ARGUMENT);
    expect_error(zkcl_get_gc_stats(runtime, NULL), ZKCL_INVALID_ARGUMENT);
    ok(zkcl_get_gc_stats(runtime, &gc));
    assert(gc.live_bytes == 0 && gc.total_collection_ns == 0);
    assert(gc.evacuation_reserve_bytes == 32 * 1024);

    char *data = malloc(mib);
    assert(data);
    memset(data, 'a', mib);
    zkcl_value *large = NULL;
    expect_error(zkcl_make_string(runtime, (zkcl_str){data, mib}, &large), ZKCL_OUT_OF_MEMORY);
    assert(!large);
    ok(zkcl_make_string(runtime, (zkcl_str){data, mib / 2}, &large));
    free(data);
    ok(zkcl_get_heap_stats(runtime, &stats));
    assert(stats.large_object_bytes >= mib / 2 && stats.committed_bytes >= stats.large_object_bytes);
    const uint64_t collections = stats.collections;
    zkcl_value_free(large);
    ok(zkcl_collect(runtime));
    ok(zkcl_get_heap_stats(runtime, &stats));
    assert(stats.collections == collections + 1 && stats.large_object_bytes == 0);
    ok(zkcl_get_gc_stats(runtime, &gc));
    assert(gc.live_bytes == 0 && gc.allocated_bytes_since_collection == 0);
    assert(gc.last_reclaimed_bytes >= mib / 2);
    assert(gc.total_collection_ns >= stats.last_collection_ns);
    ok(zkcl_get_heap_stats(other, &stats));
    assert(stats.allocation_limit_bytes > mib && stats.collections == 0);
    zkcl_runtime_free(other);
    zkcl_runtime_free(runtime);
    runtime = NULL;

    struct input input = { "\xce\xbb\nnext\n", false, false };
    struct output output = {0}, error = {0};
    uint64_t budget = 0;
    config = (zkcl_runtime_config){
        .heap_size_bytes = 8 * mib,
        .step_budget = &budget,
        .input = { read_byte, &input },
        .output = { write_bytes, &output },
        .error_output = { write_bytes, &error },
    };
    ok(zkcl_runtime_new_with_config(&config, &runtime));
    ok(zkcl_get_heap_stats(runtime, &stats));
    assert(stats.allocation_limit_bytes == 8 * mib);
    budget = UINT64_MAX;
    zkcl_values *values = NULL;
    expect_error(zkcl_eval(runtime, zkcl_utf8("42"), &values), ZKCL_CANCELLED);
    ok(zkcl_set_step_budget(runtime, NULL));
    ok(zkcl_load(runtime, zkcl_utf8(
        "(assert (string= (read-line *standard-input*) \"\xce\xbb\"))"
        "(assert (string= (read-line *terminal-io*) \"next\"))"
        "(assert (eq :eof (read-line *standard-input* nil :eof)))"
        "(write-string \"\xce\xbb\")"
        "(write-string \" terminal\" *terminal-io*)"
        "(write-string \" trace\" *trace-output*)"
        "(write-string \"error\" *error-output*)"
        "(finish-output) (finish-output *error-output*)")));
    const char expected[] = "\xce\xbb terminal trace";
    assert(output.used == sizeof expected - 1 && !memcmp(output.bytes, expected, output.used));
    assert(error.used == 5 && !memcmp(error.bytes, "error", 5));
    assert(output.flushes > 0 && error.flushes > 0);
    error.fail = true;
    ok(zkcl_load(runtime, zkcl_utf8(
        "(assert (eq :failed (handler-case (write-string \"!\" *error-output*)"
        "  (stream-error () :failed))))"
        "(assert (eq :failed (handler-case (finish-output *error-output*)"
        "  (stream-error () :failed))))")));
    zkcl_runtime_free(runtime);
    runtime = NULL;

    input.fail = true;
    config.step_budget = NULL;
    ok(zkcl_runtime_new_with_config(&config, &runtime));
    ok(zkcl_load(runtime, zkcl_utf8(
        "(assert (eq :failed (handler-case (read-line) (stream-error () :failed))))")));
    input.fail = false;
    input.oversized = true;
    ok(zkcl_load(runtime, zkcl_utf8(
        "(assert (eq :failed (handler-case (read-line) (stream-error () :failed))))")));
    input.oversized = false;
    input.next = "retry\n";
    ok(zkcl_load(runtime, zkcl_utf8("(assert (string= (read-line) \"retry\"))")));
    zkcl_runtime_free(runtime);
}

struct state { zkcl_value *target; unsigned calls; };

static zkcl_error *nested(const zkcl_runtime *rt, const zkcl_values *args,
                          zkcl_returns *results, void *userdata) {
    struct state *state = userdata;
    state->calls++;
    if (zkcl_values_len(args) != 1)
        return zkcl_error_new(zkcl_utf8("one argument required"));
    const zkcl_value *argv[] = { zkcl_values_get(args, 0) };
    zkcl_values *inner = NULL;
    zkcl_error *e = zkcl_collect(rt);
    if (!e) e = zkcl_call(rt, state->target, argv, 1, &inner);
    for (size_t i = 0; !e && i < zkcl_values_len(inner); ++i)
        e = zkcl_return_value(results, zkcl_values_get(inner, i));
    zkcl_values_free(inner);
    return e;
}

static int host_counter = 40;

static int host_add(int x, int y) {
    return x + y;
}

static double host_sum(int n, ...) {
    va_list args;
    va_start(args, n);
    double sum = 0;
    for (int i = 0; i < n; i++) sum += va_arg(args, double);
    va_end(args);
    return sum;
}

struct scope_state {
    int (*increment)(int);
    const char *(*message)(void);
    int input;
    int output;
    int later;
    bool fail;
};

static void *call_off_thread(void *userdata) {
    const struct scope_state *scope = userdata;
    assert(scope->increment(41) == 0);
    return NULL;
}

static zkcl_error *call_in_scope(void *userdata) {
    struct scope_state *scope = userdata;
    scope->output = scope->increment(scope->input);
    if (scope->fail) {
        scope->later = scope->increment(41);
        return zkcl_error_new(zkcl_utf8("secondary host error"));
    }
    const char *first = scope->message();
    const char *second = scope->message();
    assert(first && second && !strcmp(first, "callback text") && !strcmp(second, first));
    pthread_t thread;
    assert(pthread_create(&thread, NULL, call_off_thread, scope) == 0);
    assert(pthread_join(thread, NULL) == 0);
    return NULL;
}

static void test_foreign_host(zkcl_runtime *rt) {
    const zkcl_str add_name = zkcl_utf8("zkcl_test_host_add");
    const zkcl_str counter_name = zkcl_utf8("zkcl_test_host_counter");
    const zkcl_foreign_function add = (zkcl_foreign_function)host_add;
    expect_error(zkcl_register_foreign_function(rt, add_name, NULL), ZKCL_INVALID_ARGUMENT);
    ok(zkcl_register_foreign_function(rt, add_name, add));
    ok(zkcl_register_foreign_function(rt, add_name, add));
    zkcl_error *duplicate = zkcl_register_foreign_function(
        rt, add_name, (zkcl_foreign_function)host_sum);
    assert(duplicate && zkcl_error_message(duplicate).len > 0);
    zkcl_error_free(duplicate);
    ok(zkcl_register_foreign_function(
        rt, zkcl_utf8("zkcl_test_host_sum"), (zkcl_foreign_function)host_sum));
    ok(zkcl_register_foreign_data(rt, counter_name, &host_counter));
    int32_t data = 19;
    zkcl_value *pointer = NULL, *setter = NULL;
    zkcl_values *result = NULL;
    ok(zkcl_make_foreign_pointer(rt, &data, &pointer));
    uintptr_t pointer_address = 0;
    ok(zkcl_get_foreign_pointer(rt, pointer, &pointer_address));
    assert(pointer_address == (uintptr_t)&data);
    ok(zkcl_load(rt, zkcl_utf8(
        "(defun update-host-data (p) (incf (zk-ffi:mem-ref p :int32) 23))")));
    ok(zkcl_lookup_symbol(rt, zkcl_utf8("CL-USER"), zkcl_utf8("UPDATE-HOST-DATA"), &setter));
    const zkcl_value *args[] = {pointer};
    ok(zkcl_collect(rt));
    ok(zkcl_call(rt, setter, args, 1, &result));
    assert(data == 42);
    zkcl_values_free(result);
    zkcl_value_free(setter);
    zkcl_value_free(pointer);
    ok(zkcl_make_foreign_pointer(rt, NULL, &pointer));
    ok(zkcl_get_foreign_pointer(rt, pointer, &pointer_address));
    assert(pointer_address == 0);
    zkcl_value_free(pointer);
    ok(zkcl_load(rt, zkcl_utf8(
        "(assert (null (zk-ffi:find-symbol-pointer :host \"ZKCL_TEST_HOST_ADD\")))"
        "(assert (null (zk-ffi:find-symbol-pointer :default \"zkcl_test_host_add\")))"
        "(assert (= 42 (funcall (zk-ffi:bind-function :host \"zkcl_test_host_add\""
        "                                            :int :int :int) 19 23)))"
        "(assert (= 3.5d0 (funcall (zk-ffi:bind-variadic-function"
        "                            :host \"zkcl_test_host_sum\" 1 :double"
        "                            :int :float :double) 2 1.5 2d0)))"
        "(incf (zk-ffi:mem-ref"
        "        (zk-ffi:find-symbol-pointer :host \"zkcl_test_host_counter\") :int) 2)")));
    assert(host_counter == 42);

    zkcl_runtime *other = NULL;
    ok(zkcl_runtime_new(true, &other));
    ok(zkcl_load(other, zkcl_utf8(
        "(assert (null (zk-ffi:find-symbol-pointer :host \"zkcl_test_host_add\")))")));
    zkcl_runtime_free(other);

    zkcl_values *callbacks = NULL;
    ok(zkcl_eval(rt, zkcl_utf8(
        "(values"
        "  (zk-ffi:make-callback (lambda (n) (if (minusp n) (error \"negative\") (1+ n)))"
        "                        :int :int)"
        "  (zk-ffi:make-callback (lambda () (copy-seq \"callback text\")) :cstring)"
        "  nil)"), &callbacks));
    struct scope_state scope = { .input = 41 };
    uintptr_t address;
    ok(zkcl_get_foreign_pointer(rt, zkcl_values_get(callbacks, 0), &address));
    scope.increment = (int (*)(int))address;
    ok(zkcl_get_foreign_pointer(rt, zkcl_values_get(callbacks, 1), &address));
    scope.message = (const char *(*)(void))address;
    ok(zkcl_get_foreign_pointer(rt, zkcl_values_get(callbacks, 2), &address));
    assert(address == 0);
    ok(zkcl_collect(rt));
    assert(scope.increment(41) == 0);
    ok(zkcl_with_foreign_callbacks(rt, call_in_scope, &scope));
    assert(scope.output == 42 && scope.increment(41) == 0);
    scope.input = -1;
    scope.fail = true;
    expect_error(zkcl_with_foreign_callbacks(rt, call_in_scope, &scope), ZKCL_LISP_CONDITION);
    assert(scope.output == 0 && scope.later == 0);
    scope.input = 41;
    scope.fail = false;
    ok(zkcl_with_foreign_callbacks(rt, call_in_scope, &scope));
    assert(scope.output == 42);
    uint64_t exhausted = 0;
    ok(zkcl_set_step_budget(rt, &exhausted));
    scope.fail = true;
    expect_error(zkcl_with_foreign_callbacks(rt, call_in_scope, &scope), ZKCL_CANCELLED);
    assert(scope.output == 0 && scope.later == 0);
    ok(zkcl_set_step_budget(rt, NULL));
    zkcl_values_free(callbacks);
}

int main(int argc, char **argv) {
    test_runtime_config();
    zkcl_runtime *a = NULL, *b = NULL;
    zkcl_values *tuple = NULL;
    struct state state = {0};
    ok(zkcl_runtime_new(false, &a));
    ok(zkcl_runtime_new(false, &b));
    const char *compiled_asset = getenv("ZKCL_TEST_COMPILED_ASSET");
    if (compiled_asset) test_compiled_asset(a, compiled_asset);
    ok(zkcl_load(a, zkcl_utf8("(assert (null (find-package :zk-ffi)))"
                              "(defun again (x) (values (1+ (car x)) x))")));
    ok(zkcl_lookup_symbol(a, zkcl_utf8("CL-USER"), zkcl_utf8("AGAIN"), &state.target));
    zkcl_value *original = NULL;
    ok(zkcl_resolve_function(a, state.target, &original));
    ok(zkcl_register_function(a, zkcl_utf8("nested"), nested, &state));
    ok(zkcl_eval(a, zkcl_utf8("(nested (list 41))"), &tuple));
    assert(state.calls == 1 && zkcl_values_len(tuple) == 2);
    int64_t n;
    ok(zkcl_get_i64(a, zkcl_values_get(tuple, 0), &n));
    assert(n == 42 && !zkcl_values_get(tuple, 2));
    zkcl_value *saved = NULL;
    ok(zkcl_value_retain(a, zkcl_values_get(tuple, 1), &saved));
    zkcl_values_free(tuple);
    tuple = NULL;
    ok(zkcl_collect(a));
    zkcl_text *text = NULL;
    ok(zkcl_repr(a, saved, &text));
    zkcl_str printed = zkcl_text_view(text);
    assert(printed.len == 4 && !memcmp(printed.data, "(41)", 4));
    zkcl_text_free(text);
    text = NULL;
    expect_error(zkcl_repr(b, saved, &text), ZKCL_INVALID_ARGUMENT);
    assert(!text);
    expect_error(zkcl_eval(a, zkcl_utf8("(nested)"), &tuple), ZKCL_HOST_ERROR);
    assert(!tuple && state.calls == 2);
    ok(zkcl_eval(a, zkcl_utf8("(values)"), &tuple));
    assert(tuple && zkcl_values_len(tuple) == 0);
    zkcl_values_free(tuple);
    tuple = NULL;
    zkcl_error *read_error = zkcl_eval(a, zkcl_utf8("  )"), &tuple);
    assert(zkcl_error_range(read_error).start == 2);
    expect_error(read_error, ZKCL_READ_ERROR);
    expect_error(zkcl_eval(a, (zkcl_str){"\xff", 1}, &tuple), ZKCL_INVALID_ARGUMENT);
    zkcl_value *s = NULL;
    ok(zkcl_make_string(a, (zkcl_str){"a\0b", 3}, &s));
    ok(zkcl_get_string(a, s, &text));
    zkcl_value_free(s);
    assert(zkcl_text_view(text).len == 3 && !memcmp(zkcl_text_view(text).data, "a\0b", 3));
    zkcl_text_free(text);
    zkcl_value *scalar = NULL;
    ok(zkcl_make_i64(a, INT64_MIN, &scalar));
    ok(zkcl_get_i64(a, scalar, &n));
    assert(n == INT64_MIN);
    zkcl_value_free(scalar);
    scalar = NULL;
    ok(zkcl_make_double(a, -3.25, &scalar));
    double d;
    ok(zkcl_get_double(a, scalar, &d));
    assert(d == -3.25);
    zkcl_value_free(scalar);

    zkcl_cancellation *cancel = NULL;
    ok(zkcl_cancellation_new(a, &cancel));
    zkcl_cancel(cancel);
    expect_error(zkcl_eval(a, zkcl_utf8("42"), &tuple), ZKCL_CANCELLED);
    zkcl_cancel_reset(cancel);
    zkcl_cancel_with_cleanup(cancel);
    expect_error(zkcl_eval(a, zkcl_utf8("42"), &tuple), ZKCL_CANCELLED);
    zkcl_cancel_reset(cancel);
    uint64_t zero = 0;
    ok(zkcl_set_step_budget(a, &zero));
    expect_error(zkcl_eval(a, zkcl_utf8("42"), &tuple), ZKCL_CANCELLED);
    ok(zkcl_set_step_budget(a, NULL));
    ok(zkcl_load(a, zkcl_utf8("(defun again (x) (values (car x)))")));
    const zkcl_value *args[] = { saved };
    ok(zkcl_call(a, state.target, args, 1, &tuple));
    assert(zkcl_values_len(tuple) == 1);
    ok(zkcl_get_i64(a, zkcl_values_get(tuple, 0), &n));
    assert(n == 41);
    zkcl_values_free(tuple);
    tuple = NULL;
    ok(zkcl_call(a, original, args, 1, &tuple));
    assert(zkcl_values_len(tuple) == 2);
    ok(zkcl_get_i64(a, zkcl_values_get(tuple, 0), &n));
    assert(n == 42);
    zkcl_values_free(tuple);
    zkcl_value_free(original);
    zkcl_runtime_free(a);
    zkcl_cancellation_free(cancel);
    text = NULL;
    expect_error(zkcl_repr(b, saved, &text), ZKCL_INVALID_ARGUMENT);
    zkcl_value_free(saved);
    zkcl_value_free(state.target);
    zkcl_runtime_free(b);

    zkcl_runtime *foreign = NULL;
    const zkcl_runtime_config foreign_config = { .allow_foreign_calls = true };
    if (argc == 2) {
        ok(zkcl_runtime_new_with_config(&foreign_config, &foreign));
        test_foreign_host(foreign);
        cancel = NULL;
        tuple = NULL;
        ok(zkcl_cancellation_new(foreign, &cancel));
        zkcl_cancel(cancel);
        expect_error(zkcl_eval(foreign, zkcl_utf8("42"), &tuple), ZKCL_CANCELLED);
        zkcl_cancel_reset(cancel);
        zkcl_cancellation_free(cancel);
        ok(zkcl_load_file(foreign, zkcl_utf8(argv[1])));
        ok(zkcl_collect(foreign));
        ok(zkcl_load(foreign, zkcl_utf8("(assert (test-foreign-calls))")));
        zkcl_runtime_free(foreign);
    } else {
        expect_error(zkcl_runtime_new_with_config(&foreign_config, &foreign), ZKCL_INVALID_ARGUMENT);
        assert(!foreign);
        expect_error(zkcl_runtime_new(true, &foreign), ZKCL_INVALID_ARGUMENT);
        ok(zkcl_runtime_new(false, &foreign));
        expect_error(zkcl_register_foreign_function(
            foreign, zkcl_utf8("add"), (zkcl_foreign_function)host_add), ZKCL_INVALID_ARGUMENT);
        expect_error(zkcl_register_foreign_data(
            foreign, zkcl_utf8("counter"), &host_counter), ZKCL_INVALID_ARGUMENT);
        zkcl_value *pointer = NULL;
        expect_error(zkcl_make_foreign_pointer(foreign, &host_counter, &pointer), ZKCL_INVALID_ARGUMENT);
        assert(!pointer);
        struct scope_state unused = {0};
        expect_error(zkcl_with_foreign_callbacks(
            foreign, call_in_scope, &unused), ZKCL_INVALID_ARGUMENT);
        zkcl_values *nil = NULL;
        ok(zkcl_eval(foreign, zkcl_utf8("nil"), &nil));
        uintptr_t address = 123;
        expect_error(zkcl_get_foreign_pointer(
            foreign, zkcl_values_get(nil, 0), &address), ZKCL_INVALID_ARGUMENT);
        assert(address == 123);
        zkcl_values_free(nil);
        zkcl_runtime_free(foreign);
    }
    return 0;
}
