#include "zkcl.h"

#include <assert.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>

static void ok(zkcl_error *error) {
    if (error) {
        zkcl_str message = zkcl_error_message(error);
        fprintf(stderr, "%.*s\n", (int)message.len, message.data);
        zkcl_error_free(error);
        assert(0);
    }
}

static zkcl_evaluation_result drive(const zkcl_evaluation *evaluation, const zkcl_waker *waker) {
    for (unsigned slice = 0; slice < 1000; slice++) {
        zkcl_evaluation_result result;
        ok(zkcl_evaluation_resume(evaluation, waker, &result));
        if (result.state != ZKCL_EVALUATION_RUNNABLE) return result;
        assert(result.values == NULL);
    }
    assert(0);
    return (zkcl_evaluation_result){0};
}

static void drive_cancelled(const zkcl_evaluation *evaluation, const zkcl_waker *waker) {
    for (unsigned slice = 0; slice < 1000; slice++) {
        zkcl_evaluation_result result;
        zkcl_error *error = zkcl_evaluation_resume(evaluation, waker, &result);
        assert(result.values == NULL);
        if (error) {
            assert(zkcl_error_kind(error) == ZKCL_CANCELLED);
            zkcl_error_free(error);
            return;
        }
        assert(result.state == ZKCL_EVALUATION_RUNNABLE);
    }
    assert(0);
}

struct state {
    pthread_t owner;
    zkcl_completion *completion;
    atomic_uint notifications;
    atomic_uint stop_notifications;
    atomic_uint wake_releases;
    atomic_uint result_releases;
    int64_t argument;
};

static void notify(void *userdata) {
    struct state *state = userdata;
    atomic_fetch_add(&state->notifications, 1);
}

static void notify_stop(void *userdata) {
    struct state *state = userdata;
    atomic_fetch_add(&state->stop_notifications, 1);
}

static void release_waker(void *userdata) {
    struct state *state = userdata;
    atomic_fetch_add(&state->wake_releases, 1);
}

static void release_result(void *userdata) {
    struct state *state = userdata;
    atomic_fetch_add(&state->result_releases, 1);
}

static zkcl_error *start(const zkcl_runtime *rt, const zkcl_values *args,
                         zkcl_completion *completion, void *userdata) {
    struct state *state = userdata;
    assert(pthread_equal(pthread_self(), state->owner));
    assert(state->completion == NULL && zkcl_values_len(args) == 1);
    ok(zkcl_get_i64(rt, zkcl_values_get(args, 0), &state->argument));
    state->completion = completion;
    return NULL;
}

static zkcl_error *materialize(const zkcl_runtime *rt, zkcl_returns *results, void *userdata) {
    struct state *state = userdata;
    assert(pthread_equal(pthread_self(), state->owner));
    zkcl_value *value = NULL;
    ok(zkcl_make_i64(rt, state->argument + 1, &value));
    ok(zkcl_return_value(results, value));
    zkcl_value_free(value);
    return NULL;
}

static void *finish_from_worker(void *userdata) {
    struct state *state = userdata;
    assert(!zkcl_completion_is_cancelled(state->completion));
    assert(zkcl_completion_finish(state->completion, materialize, state, release_result));
    state->completion = NULL;
    return NULL;
}

static void *cancel_from_worker(void *userdata) {
    zkcl_cancel_with_cleanup(userdata);
    return NULL;
}

int main(void) {
    zkcl_runtime *rt = NULL;
    ok(zkcl_runtime_new(false, &rt));
    struct state state = {.owner = pthread_self()};
    ok(zkcl_register_async_function(rt, zkcl_utf8("c-async"), start, &state));
    zkcl_waker *waker = NULL;
    ok(zkcl_waker_new(notify, &state, release_waker, &waker));

    zkcl_value *function = NULL, *argument = NULL;
    zkcl_evaluation *evaluation = NULL;
    ok(zkcl_lookup_symbol(rt, zkcl_utf8("CL-USER"), zkcl_utf8("C-ASYNC"), &function));
    ok(zkcl_make_i64(rt, 41, &argument));
    const zkcl_value *argv[] = {argument};
    ok(zkcl_start_call(rt, function, argv, 1, &evaluation));
    zkcl_value_free(argument);
    zkcl_value_free(function);
    ok(zkcl_collect(rt));
    zkcl_evaluation_result result = drive(evaluation, waker);
    assert(result.state == ZKCL_EVALUATION_WAITING && result.values == NULL);
    pthread_t worker;
    assert(pthread_create(&worker, NULL, finish_from_worker, &state) == 0);
    assert(pthread_join(worker, NULL) == 0);
    assert(atomic_load(&state.notifications) == 1);
    result = drive(evaluation, waker);
    assert(result.state == ZKCL_EVALUATION_READY && zkcl_values_len(result.values) == 1);
    int64_t returned = 0;
    ok(zkcl_get_i64(rt, zkcl_values_get(result.values, 0), &returned));
    assert(returned == 42 && atomic_load(&state.result_releases) == 1);
    zkcl_values_free(result.values);
    zkcl_evaluation_free(evaluation);

    ok(zkcl_load(rt, zkcl_utf8("(defparameter *c-cleaned* nil)")));
    ok(zkcl_start_eval(rt, zkcl_utf8("(unwind-protect (c-async 5) (setq *c-cleaned* t))"), &evaluation));
    result = drive(evaluation, waker);
    assert(result.state == ZKCL_EVALUATION_WAITING);
    zkcl_waker *stop_waker = NULL;
    ok(zkcl_waker_new(notify_stop, &state, NULL, &stop_waker));
    assert(!zkcl_completion_poll_cancelled(state.completion, stop_waker));
    zkcl_waker_free(stop_waker);
    zkcl_cancellation *cancellation = NULL;
    ok(zkcl_cancellation_new(rt, &cancellation));
    assert(pthread_create(&worker, NULL, cancel_from_worker, cancellation) == 0);
    assert(pthread_join(worker, NULL) == 0);
    assert(atomic_load(&state.notifications) == 2);
    result = drive(evaluation, waker);
    assert(result.state == ZKCL_EVALUATION_WAITING);
    assert(zkcl_completion_is_cancelled(state.completion));
    assert(atomic_load(&state.stop_notifications) == 1);
    zkcl_completion_free(state.completion);
    state.completion = NULL;
    drive_cancelled(evaluation, waker);
    zkcl_evaluation_free(evaluation);
    zkcl_cancel_reset(cancellation);
    ok(zkcl_load(rt, zkcl_utf8("(assert *c-cleaned*)")));

    ok(zkcl_start_eval(rt, zkcl_utf8("(sleep 60)"), &evaluation));
    result = drive(evaluation, waker);
    assert(result.state == ZKCL_EVALUATION_WAITING_TIMER && result.delay_ns > 0);
    zkcl_waker_free(waker);
    assert(atomic_load(&state.wake_releases) == 0);
    zkcl_cancel(cancellation);
    drive_cancelled(evaluation, NULL);
    assert(atomic_load(&state.wake_releases) == 1);
    zkcl_evaluation_free(evaluation);
    zkcl_cancel_reset(cancellation);
    zkcl_cancellation_free(cancellation);

    ok(zkcl_start_eval(rt, zkcl_utf8("(c-async 100)"), &evaluation));
    result = drive(evaluation, NULL);
    assert(result.state == ZKCL_EVALUATION_WAITING);
    zkcl_evaluation_free(evaluation);
    zkcl_runtime_free(rt);
    assert(zkcl_completion_is_cancelled(state.completion));
    assert(!zkcl_completion_finish(state.completion, materialize, &state, release_result));
    assert(atomic_load(&state.result_releases) == 2);
    return 0;
}
