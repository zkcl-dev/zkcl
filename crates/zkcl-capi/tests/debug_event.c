#include "zkcl.h"
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>

static void ok(zkcl_error *error) {
    if (error) {
        zkcl_str message = zkcl_error_message(error);
        fprintf(stderr, "%.*s\n", (int)message.len, message.data);
        zkcl_error_free(error);
        abort();
    }
}
static bool same(zkcl_str text, const char *expected) {
    return text.len == strlen(expected) && memcmp(text.data, expected, text.len) == 0;
}
struct state { zkcl_evaluation *evaluation; zkcl_value *original; unsigned operations; };
static zkcl_error *inspect_and_edit(const zkcl_runtime *rt, void *userdata) {
    struct state *state = userdata;
    ++state->operations;
    /* Reentry is rejected while the handle's mutable operation is borrowed. */
    zkcl_evaluation_result reentry;
    zkcl_error *error = zkcl_evaluation_resume(state->evaluation, NULL, &reentry);
    assert(error && zkcl_error_kind(error) == ZKCL_INVALID_ARGUMENT);
    assert(!reentry.values && !reentry.debug_event);
    zkcl_error_free(error);
    zkcl_debug_variable_list *variables = NULL;
    ok(zkcl_debug_variables(rt, 0, &variables));
    for (size_t i = 0; i < zkcl_debug_variables_len(variables); ++i) {
        zkcl_debug_variable variable;
        ok(zkcl_debug_variables_get(variables, i, &variable));
        zkcl_text *name = NULL;
        ok(zkcl_repr(rt, variable.name, &name));
        if (same(zkcl_text_view(name), "X")) {
            assert(variable.value && variable.writable);
            ok(zkcl_value_retain(rt, variable.value, &state->original));
            zkcl_value *replacement = NULL;
            ok(zkcl_make_string(rt, zkcl_utf8("changed"), &replacement));
            ok(zkcl_debug_set_local(rt, 0, variable.name, replacement));
            zkcl_value_free(replacement);
        }
        zkcl_text_free(name);
    }
    assert(state->original);
    zkcl_debug_variables_free(variables);
    ok(zkcl_collect(rt));
    zkcl_values *values = NULL;
    ok(zkcl_debug_eval(rt, 0, zkcl_utf8("(length x)"), &values));
    int64_t length = 0;
    ok(zkcl_get_i64(rt, zkcl_values_get(values, 0), &length));
    assert(length == 7);
    zkcl_values_free(values);
    return NULL;
}
static zkcl_error *continue_execution(const zkcl_runtime *rt, void *userdata) {
    (void)userdata;
    bool resumable = false;
    zkcl_error *error = zkcl_debug_motion(rt, ZKCL_DEBUG_CONTINUE, &resumable);
    if (error) return error;
    assert(resumable);
    return NULL;
}
int main(void) {
    zkcl_runtime *rt = NULL;
    ok(zkcl_runtime_new(false, &rt));
    ok(zkcl_load(rt, zkcl_utf8("(defun capi-event (x) (declare (optimize (debug 3))) (length x))")));
    zkcl_value *function = NULL;
    ok(zkcl_lookup_symbol(rt, zkcl_utf8("CL-USER"), zkcl_utf8("CAPI-EVENT"), &function));
    uint64_t breakpoint = 0;
    ok(zkcl_add_breakpoint(rt, function, NULL, &breakpoint));
    zkcl_value_free(function);
    struct state state = {0};
    ok(zkcl_start_eval(rt, zkcl_utf8("(capi-event (copy-seq \"original\"))"), &state.evaluation));
    ok(zkcl_evaluation_enable_debugging(state.evaluation));
    zkcl_debug_event *event = NULL;
    bool ready = false;
    for (unsigned slice = 0; slice < 1000 && !ready; ++slice) {
        zkcl_evaluation_result result;
        ok(zkcl_evaluation_resume(state.evaluation, NULL, &result));
        switch (result.state) {
        case ZKCL_EVALUATION_RUNNABLE:
            assert(!result.values && !result.debug_event);
            break;
        case ZKCL_EVALUATION_PAUSED: {
            assert(!event && !result.values && result.debug_event);
            event = result.debug_event;
            assert(zkcl_debug_event_condition(event));
            const zkcl_debug_frame_list *frames = zkcl_debug_event_frames(event);
            assert(zkcl_debug_frames_len(frames) >= 1);
            zkcl_debug_frame frame;
            ok(zkcl_debug_frames_get(frames, 0, &frame));
            assert(frame.locals_available);
            /* resume has returned, and the host is free to retain/inspect. */
            ok(zkcl_collect(rt));
            ok(zkcl_evaluation_with_debugger(state.evaluation, inspect_and_edit, &state));
            ok(zkcl_collect(rt));
            zkcl_text *text = NULL;
            ok(zkcl_get_string(rt, state.original, &text));
            assert(same(zkcl_text_view(text), "original"));
            zkcl_text_free(text);
            ok(zkcl_evaluation_with_debugger(state.evaluation, continue_execution, NULL));
            break;
        }
        case ZKCL_EVALUATION_READY: {
            assert(!result.debug_event && result.values && event);
            int64_t value = 0;
            ok(zkcl_get_i64(rt, zkcl_values_get(result.values, 0), &value));
            assert(value == 7);
            zkcl_values_free(result.values);
            ready = true;
            break;
        }
        default:
            assert(0);
        }
    }
    assert(ready && state.operations == 1);
    zkcl_evaluation_free(state.evaluation);
    /* Both snapshots continue owning their roots after execution finishes. */
    ok(zkcl_collect(rt));
    assert(zkcl_debug_event_condition(event));
    zkcl_debug_event_free(event);
    zkcl_value_free(state.original);
    bool removed = false;
    ok(zkcl_remove_breakpoint(rt, breakpoint, &removed));
    assert(removed);
    zkcl_runtime_free(rt);
    return 0;
}
