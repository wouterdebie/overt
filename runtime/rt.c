// Overt runtime, milestone 0: process start and exit, printing, traps.
//
// Compiled code calls these with scalars and pointers only, never structs,
// so the C calling convention for structs never matters.

#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

// Heap buffers (the bytes of a `str`, the elements of an array) start with a
// 16-byte header. A count of 0 marks a static buffer that is never freed.
typedef struct {
  int64_t rc;
  int64_t cap;
} ovt_buf;

#define OVT_DATA(b) ((const char *)(b) + sizeof(ovt_buf))

// A trap stops the program with this exit status.
enum { OVT_TRAP_STATUS = 101 };

static int ovt_argc;
static char **ovt_argv;

void ovt_rt_init(int32_t argc, char **argv) {
  ovt_argc = argc;
  ovt_argv = argv;
}

void ovt_rt_exit(void) { fflush(stdout); }

void ovt_print_str(const ovt_buf *buf, int64_t off, int64_t len) {
  fwrite(OVT_DATA(buf) + off, 1, (size_t)len, stdout);
  fputc('\n', stdout);
}

void ovt_print_int(int64_t v) { printf("%lld\n", (long long)v); }

void ovt_print_bool(int32_t v) { fputs(v ? "true\n" : "false\n", stdout); }

_Noreturn void ovt_trap(const char *msg, int64_t len, const char *file, int32_t line, int32_t col) {
  fflush(stdout);
  fprintf(stderr, "%s:%d:%d: trap: %.*s\n", file, line, col, (int)len, msg);
  _exit(OVT_TRAP_STATUS);
}
