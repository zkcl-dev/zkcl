#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

static int counter;

uint64_t identity_uint64(uint64_t x) { return x; }
int8_t identity_int8(int8_t x) { return x; }
float identity_float(float x) { return x; }
double sum_mixed_arguments(double x, float y, int16_t z) { return x + y + z; }
size_t utf8_byte_length(const char *s) { return s ? strlen(s) : 0; }
const char *echo_text(const char *s) { return s; }
const char *invalid_utf8(void) { return "\xff"; }
void *counter_address(void) { return &counter; }
void increment_counter(int *p) { ++*p; }
int read_counter(const int *p) { return p ? *p : -1; }
void *null_pointer(void) { return NULL; }

int zkcl_global_number = 37;

double sum_promoted(int base, ...) {
    va_list args;
    va_start(args, base);
    int small = va_arg(args, int);
    double fraction = va_arg(args, double);
    va_end(args);
    return base + small + fraction;
}

static int (*saved_int_callback)(int, int);
void retain_int_callback(int (*callback)(int, int)) { saved_int_callback = callback; }
int call_saved_int_callback(int a, int b) { return saved_int_callback(a, b); }
int get_negative_int(void) { return -73; }
int sum_three_ints(int a, int b, int c) { return a + b + c; }
double multiply_doubles(double a, double b) { return a * b; }
void replace_counter(int value) { counter = value; }
