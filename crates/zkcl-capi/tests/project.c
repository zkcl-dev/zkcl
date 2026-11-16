#include "zkcl.h"
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>

static void check(zkcl_error *error) {
    if (error) {
        zkcl_str message = zkcl_error_message(error);
        fwrite(message.data, 1, message.len, stderr);
        zkcl_str details = zkcl_error_details(error);
        fwrite(details.data, 1, details.len, stderr);
        fputc('\n', stderr);
        zkcl_error_free(error);
        exit(1);
    }
}

static zkcl_error *offset(const zkcl_runtime *rt, const zkcl_values *arguments,
                           zkcl_returns *results, void *userdata) {
    assert(zkcl_values_len(arguments) == 0);
    ++*(int *)userdata;
    zkcl_value *zero = NULL;
    zkcl_error *error = zkcl_make_i64(rt, 0, &zero);
    if (!error) error = zkcl_return_value(results, zero);
    zkcl_value_free(zero);
    return error;
}

static zkcl_runtime *runtime(int *calls) {
    zkcl_runtime *rt = NULL;
    check(zkcl_runtime_new(false, &rt));
    check(zkcl_load(rt, zkcl_utf8("(defpackage #:embed.host (:use #:cl) (:export #:offset))")));
    check(zkcl_register_function(rt, zkcl_utf8("EMBED.HOST:OFFSET"), offset, calls));
    return rt;
}

static void verify(const zkcl_runtime *rt, const zkcl_values *values) {
    assert(zkcl_values_len(values) == 3);
    int64_t answer = 0, loads = 0;
    check(zkcl_get_i64(rt, zkcl_values_get(values, 0), &answer));
    check(zkcl_get_i64(rt, zkcl_values_get(values, 2), &loads));
    assert(answer == 42 && loads == 1);
    zkcl_text *resource = NULL;
    check(zkcl_get_string(rt, zkcl_values_get(values, 1), &resource));
    zkcl_str text = zkcl_text_view(resource);
    assert(text.len == strlen("embedded resources"));
    assert(memcmp(text.data, "embedded resources", text.len) == 0);
    zkcl_text_free(resource);
}

static void call_host(const zkcl_runtime *rt) {
    zkcl_value *function = NULL;
    zkcl_values *values = NULL;
    check(zkcl_lookup_symbol(rt, zkcl_utf8("EMBED.APP"), zkcl_utf8("CALL-HOST"), &function));
    check(zkcl_call(rt, function, NULL, 0, &values));
    int64_t answer = 0;
    check(zkcl_get_i64(rt, zkcl_values_get(values, 0), &answer));
    assert(answer == 42);
    zkcl_values_free(values);
    zkcl_value_free(function);
}

int main(void) {
    zkcl_project *project = NULL;
    check(zkcl_project_open(zkcl_utf8(getenv("ZKCL_EMBED_PROJECT")), &project));
    zkcl_project_options options = {0};
    options.target_kind = 1;
    options.binary = zkcl_utf8("app");
    zkcl_text *metadata = NULL;
    check(zkcl_project_info(project, &metadata));
    assert(zkcl_text_view(metadata).len > 0);
    zkcl_text_free(metadata);
    metadata = NULL;
    check(zkcl_project_resolve(project, &options, &metadata));
    assert(zkcl_text_view(metadata).len > 0);
    zkcl_text_free(metadata);

    int calls = 0;
    zkcl_runtime *rt = runtime(&calls);
    zkcl_project_session *session = NULL;
    check(zkcl_project_load(project, rt, &options, &session));
    zkcl_values *values = NULL;
    check(zkcl_project_call(session, &values));
    verify(rt, values);
    zkcl_values_free(values);
    call_host(rt);
    assert(calls == 1);
    zkcl_value *symbol = NULL;
    uint32_t status = 0;
    check(zkcl_find_symbol(rt, zkcl_utf8("EMBED.ASDF"), zkcl_utf8("ANSWER"), &status, &symbol));
    assert(status == 2 && symbol);
    zkcl_value_free(symbol);
    check(zkcl_project_write_lock(session));
    check(zkcl_project_write_bundle(session, zkcl_utf8(getenv("ZKCL_EMBED_BUNDLE"))));
    zkcl_project_session_free(session);
    zkcl_project_free(project);
    zkcl_runtime_free(rt);

    zkcl_bundle *bundle = NULL;
    check(zkcl_bundle_open(zkcl_utf8(getenv("ZKCL_EMBED_BUNDLE")), &bundle));
    calls = 0;
    rt = runtime(&calls);
    check(zkcl_bundle_load(bundle, rt, zkcl_utf8(getenv("ZKCL_EMBED_CACHE"))));
    values = NULL;
    check(zkcl_bundle_call(bundle, rt, &values));
    verify(rt, values);
    call_host(rt);
    assert(calls == 1);
    zkcl_values_free(values);
    zkcl_bundle_free(bundle);
    zkcl_runtime_free(rt);
    return 0;
}
