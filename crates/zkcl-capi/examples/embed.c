#include "zkcl.h"
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>

static void check(zkcl_error *error) {
    if (error) {
        zkcl_str message = zkcl_error_message(error);
        fwrite(message.data, 1, message.len, stderr);
        fputc('\n', stderr);
        zkcl_error_free(error);
        exit(EXIT_FAILURE);
    }
}

static zkcl_error *offset(const zkcl_runtime *rt, const zkcl_values *args,
                          zkcl_returns *results, void *userdata) {
    if (zkcl_values_len(args) != 0)
        return zkcl_error_new(zkcl_utf8("host-offset expects no arguments"));
    zkcl_value *value = NULL;
    zkcl_error *error = zkcl_make_i64(rt, *(int64_t *)userdata, &value);
    if (!error)
        error = zkcl_return_value(results, value);
    zkcl_value_free(value);
    return error;
}

int main(void) {
    int64_t host_offset = 2;
    zkcl_runtime *rt = NULL;
    zkcl_value *function = NULL, *argument = NULL;
    zkcl_values *results = NULL;

    check(zkcl_runtime_new(false, &rt));
    check(zkcl_register_function(rt, zkcl_utf8("host-offset"), offset, &host_offset));
    check(zkcl_load(rt, zkcl_utf8("(defun answer (n) (values (+ n (host-offset)) n))")));
    check(zkcl_lookup_symbol(rt, zkcl_utf8("CL-USER"), zkcl_utf8("ANSWER"), &function));
    check(zkcl_make_i64(rt, 40, &argument));
    const zkcl_value *args[] = { argument };
    check(zkcl_call(rt, function, args, 1, &results));

    int64_t sum, original;
    check(zkcl_get_i64(rt, zkcl_values_get(results, 0), &sum));
    check(zkcl_get_i64(rt, zkcl_values_get(results, 1), &original));
    printf("%" PRId64 " %" PRId64 "\n", sum, original);

    zkcl_values_free(results);
    zkcl_value_free(argument);
    zkcl_value_free(function);
    zkcl_runtime_free(rt);
    return EXIT_SUCCESS;
}
