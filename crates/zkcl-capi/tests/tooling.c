#include "zkcl.h"
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>

static void ok(zkcl_error *e) {
    if (e) {
        zkcl_str s = zkcl_error_message(e);
        fprintf(stderr, "%.*s\n", (int)s.len, s.data);
        zkcl_error_free(e);
        abort();
    }
}
static bool same(zkcl_str a, const char *b) {
    return a.len == strlen(b) && memcmp(a.data, b, a.len) == 0;
}
static zkcl_value *saved_local;
static unsigned stops;
static zkcl_error *debugger(const zkcl_runtime *rt, const zkcl_values *args,
                            zkcl_returns *results, void *userdata) {
    (void)results;
    (void)userdata;
    assert(zkcl_values_len(args) == 1);
    ++stops;
    zkcl_debug_frame_list *frames = NULL;
    ok(zkcl_debug_frames(rt, &frames));
    assert(zkcl_debug_frames_len(frames) > 0);
    zkcl_debug_frame frame;
    ok(zkcl_debug_frames_get(frames, 0, &frame));
    assert(frame.locals_available);
    zkcl_debug_frames_free(frames);
    zkcl_debug_variable_list *variables = NULL;
    ok(zkcl_debug_variables(rt, 0, &variables));
    assert(zkcl_debug_variables_len(variables) > 0);
    for (size_t i = 0; i < zkcl_debug_variables_len(variables); ++i) {
        zkcl_debug_variable variable;
        ok(zkcl_debug_variables_get(variables, i, &variable));
        zkcl_text *name = NULL;
        ok(zkcl_repr(rt, variable.name, &name));
        if (same(zkcl_text_view(name), "X")) {
            assert(variable.value && variable.writable);
            ok(zkcl_value_retain(rt, variable.value, &saved_local));
            zkcl_value *replacement = NULL;
            ok(zkcl_make_i64(rt, 43, &replacement));
            ok(zkcl_debug_set_local(rt, 0, variable.name, replacement));
            zkcl_value_free(replacement);
        }
        zkcl_text_free(name);
    }
    assert(saved_local);
    zkcl_debug_variables_free(variables);
    zkcl_values *values = NULL;
    ok(zkcl_debug_eval(rt, 0, zkcl_utf8("(+ x 2)"), &values));
    int64_t n = 0;
    ok(zkcl_get_i64(rt, zkcl_values_get(values, 0), &n));
    assert(n == 45);
    zkcl_values_free(values);
    bool resumable = false;
    zkcl_error *e = zkcl_debug_motion(rt, ZKCL_DEBUG_CONTINUE, &resumable);
    if (e) return e;
    assert(resumable);
    return NULL;
}
int main(void) {
    zkcl_runtime *rt = NULL;
    ok(zkcl_runtime_new(false, &rt));
    int64_t version = 7;
    zkcl_source *source = NULL;
    ok(zkcl_source_new(zkcl_utf8("(defun capi-debug (x) (declare (optimize (debug 3))) (+ x 1))"),
                       zkcl_utf8("/virtual/tooling.lisp"), zkcl_utf8("file:///virtual/tooling.lisp"),
                       &version, 123, &source));
    uint64_t source_id = zkcl_source_id(source);
    zkcl_values *values = NULL;
    ok(zkcl_eval_document(rt, source, &values));
    zkcl_values_free(values);
    zkcl_source_free(source);
    zkcl_definitions *definitions = NULL;
    ok(zkcl_find_definitions(rt, zkcl_utf8("CAPI-DEBUG"), &definitions));
    assert(zkcl_definitions_len(definitions) > 0);
    zkcl_definition definition;
    ok(zkcl_definitions_get(definitions, 0, &definition));
    assert(definition.source && zkcl_source_id(definition.source) == source_id);
    assert(zkcl_source_base_offset(definition.source) == 123);
    int64_t got_version = 0;
    assert(zkcl_source_version(definition.source, &got_version) && got_version == version);
    assert(same(zkcl_source_uri(definition.source), "file:///virtual/tooling.lisp"));
    zkcl_definitions_free(definitions);

    ok(zkcl_set_debugger(rt, debugger, NULL));
    zkcl_value *function = NULL;
    ok(zkcl_lookup_symbol(rt, zkcl_utf8("CL-USER"), zkcl_utf8("CAPI-DEBUG"), &function));
    uint64_t breakpoint = 0;
    zkcl_location_list *locations = NULL;
    ok(zkcl_executable_locations(rt, function, &locations));
    assert(zkcl_executable_locations_len(locations) > 0);
    zkcl_executable_locations_free(locations);
    zkcl_breakpoint_options options = {0};
    options.enabled = true;
    options.condition = zkcl_utf8("(= x 42)");
    options.hit_count = 2;
    options.follow = ZKCL_BREAKPOINT_NAME;
    ok(zkcl_add_breakpoint_with_options(rt, function, 0, false, &options, &breakpoint));
    ok(zkcl_eval(rt, zkcl_utf8("(capi-debug 42)"), &values));
    assert(stops == 0);
    zkcl_values_free(values);
    ok(zkcl_eval(rt, zkcl_utf8("(capi-debug 42)"), &values));
    assert(stops == 1);
    zkcl_breakpoint_list *breakpoints = NULL;
    ok(zkcl_breakpoints(rt, &breakpoints));
    assert(zkcl_breakpoints_len(breakpoints) == 1);
    zkcl_breakpoint breakpoint_info;
    ok(zkcl_breakpoints_get(breakpoints, 0, &breakpoint_info));
    assert(breakpoint_info.verified && breakpoint_info.hits == 2);
    zkcl_breakpoints_free(breakpoints);
    int64_t n = 0;
    ok(zkcl_get_i64(rt, zkcl_values_get(values, 0), &n));
    assert(n == 44);
    zkcl_values_free(values);
    bool removed = false;
    ok(zkcl_remove_breakpoint(rt, breakpoint, &removed));
    assert(removed);
    zkcl_value_free(function);
    ok(zkcl_set_debugger(rt, NULL, NULL));
    ok(zkcl_collect(rt));
    ok(zkcl_get_i64(rt, saved_local, &n));
    assert(n == 42);
    zkcl_value_free(saved_local);

    ok(zkcl_eval(rt, zkcl_utf8("(list 1 2)"), &values));
    zkcl_value *object = NULL;
    ok(zkcl_value_retain(rt, zkcl_values_get(values, 0), &object));
    zkcl_values_free(values);
    zkcl_value *replacement = NULL;
    ok(zkcl_make_string(rt, zkcl_utf8("kept by inspection"), &replacement));
    ok(zkcl_inspection_replace(rt, object, 0, replacement));
    zkcl_value_free(replacement);
    zkcl_inspection *inspection = NULL;
    ok(zkcl_inspect(rt, object, 0, 2, &inspection));
    zkcl_value_free(object);
    ok(zkcl_collect(rt));
    zkcl_inspection_part part;
    ok(zkcl_inspection_get(inspection, 0, &part));
    assert(part.value && part.writable);
    zkcl_text *string = NULL;
    ok(zkcl_get_string(rt, part.value, &string));
    assert(same(zkcl_text_view(string), "kept by inspection"));
    zkcl_text_free(string);
    zkcl_inspection_free(inspection);

    zkcl_error *error = zkcl_eval(rt, zkcl_utf8("(car 42)"), &values);
    assert(error && !values);
    assert(zkcl_error_kind(error) == ZKCL_LISP_CONDITION);
    assert(same(zkcl_error_condition_type(error), "TYPE-ERROR"));
    assert(zkcl_error_stack_len(error) > 0);
    zkcl_error_frame_info trace;
    ok(zkcl_error_frame_view(zkcl_error_stack_get(error, 0), &trace));
    assert(trace.source && trace.has_range);
    zkcl_source *retained_source = NULL;
    ok(zkcl_source_retain(trace.source, &retained_source));
    zkcl_error_free(error);
    assert(same(zkcl_source_text(retained_source), "(car 42)"));
    zkcl_source_free(retained_source);

    ok(zkcl_profile_start(rt, 1000));
    ok(zkcl_eval(rt, zkcl_utf8("(capi-debug 1)"), &values));
    zkcl_values_free(values);
    zkcl_profile *profile = NULL;
    ok(zkcl_profile_report(rt, true, &profile));
    zkcl_profile_info report;
    ok(zkcl_profile_view(profile, &report));
    assert(!report.running && report.function_count > 0);
    zkcl_profile_free(profile);
    zkcl_runtime_free(rt);
    return 0;
}
