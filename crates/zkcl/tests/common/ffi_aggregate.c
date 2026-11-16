#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

typedef struct { double value; int32_t tag; } mixed;
typedef struct { uint8_t prefix; mixed item; uint16_t suffix; } outer;
typedef struct { float a; float b; } pair;
typedef struct { pair low; pair high; } hfa;
typedef struct { uint8_t value; } tiny;
typedef struct { uint8_t a, b, c; } triple;
typedef struct { const char *text; uint16_t count; } message;

message static_message(void) {
    return (message){"native", 6};
}

typedef struct { int status; const char *detail; } partial_status;

/* The detail member is specified only on success. */
partial_status no_detail(void) {
    partial_status result;
    result.status = -1;
    return result;
}

size_t aggregate_layout(int index) {
    const size_t values[] = {
        sizeof(mixed), _Alignof(mixed), offsetof(mixed, value), offsetof(mixed, tag),
        sizeof(outer), _Alignof(outer), offsetof(outer, prefix),
        offsetof(outer, item), offsetof(outer, suffix)
    };
    return values[index];
}

mixed transform_mixed(int32_t delta, mixed input, double factor) {
    mixed result;
    memset(&result, 0xab, sizeof result);
    result.value = input.value * factor;
    result.tag = input.tag + delta;
    return result;
}

outer transform_outer(outer input) {
    input.prefix++;
    input.item.value *= -2;
    input.item.tag += 123;
    input.suffix--;
    return input;
}

outer visit_outer(outer input, int (*visitor)(void)) {
    input.suffix += visitor();
    return input;
}

hfa transform_hfa(hfa input, float delta) {
    return (hfa){{input.high.b, input.high.a}, {input.low.b + delta, input.low.a}};
}

tiny transform_tiny(tiny input) {
    return (tiny){input.value ^ 0xff};
}

triple rotate_three(triple input) {
    return (triple){input.c, input.a, input.b};
}

mixed spilled_mixed(double a, double b, double c, double d, double e, double f,
                    double g, double h, int i, int j, int k, int l,
                    int m, int n, int o, int p, mixed input, int32_t tag) {
    input.value += a + b + c + d + e + f + g + h;
    input.tag += i + j + k + l + m + n + o + p + tag;
    return input;
}

mixed variadic_mixed(int delta, ...) {
    va_list args;
    va_start(args, delta);
    mixed input = va_arg(args, mixed);
    double factor = va_arg(args, double);
    va_end(args);
    return transform_mixed(delta, input, factor);
}

mixed visit_mixed_fields(mixed input, mixed (*visitor)(mixed, double)) {
    mixed result = visitor(input, 0.5);
    result.tag += 10;
    return result;
}

static int saw_zero;
int visitor_returned_zero(void) { return saw_zero; }

outer visit_outer_fields(outer input, outer (*visitor)(outer)) {
    outer result = visitor(input);
    saw_zero = result.prefix == 0 && result.item.value == 0 &&
               result.item.tag == 0 && result.suffix == 0;
    return result;
}

hfa visit_float_fields(hfa input, hfa (*visitor)(hfa)) {
    hfa result = visitor(input);
    result.high.b *= 2;
    return result;
}

tiny visit_tiny_field(tiny input, tiny (*visitor)(tiny)) {
    tiny result = visitor(input);
    result.value++;
    return result;
}

typedef struct { uint8_t kind; message first; const char *second; const char *input; } messages;

messages gather_strings(const char *input, const char *(*produce)(int)) {
    const char *first = produce(0);
    const char *second = produce(1);
    return (messages){42, {first, 1}, second, input};
}

void transfer_message(const char *(*produce)(int), void (*consume)(message)) {
    const char *text = produce(0);
    consume((message){text + 1, 3});
}

void forward_input_message(const char *input, void (*consume)(message)) {
    consume((message){input + 1, 3});
}

messages echo_messages(messages input) {
    return input;
}

messages callback_messages(messages (*produce)(void), void (*collect)(void)) {
    messages input = produce();
    collect();
    return input;
}
