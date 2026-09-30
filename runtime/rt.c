// The Overt runtime.
//
// Compiled code calls these functions with scalars and pointers only, never
// with structs by value, so the C calling convention for structs never matters.
//
// Strings and arrays are {buf, off, len} views of a reference-counted buffer:
// a 24-byte header (count, capacity, used) followed by the elements. A count
// of 0 marks a static buffer (a string literal) that is never freed. `used`
// is how many elements the buffer holds; a view may show fewer.

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

typedef struct {
  int64_t rc;
  int64_t cap;
  int64_t used;
} ovt_buf;

typedef struct {
  ovt_buf *buf;
  int64_t off;
  int64_t len;
} ovt_arr;

typedef ovt_arr ovt_str;
typedef void (*ovt_fn1)(void *);
typedef int32_t (*ovt_cmp)(const void *, const void *);

// ErrKind values, in declaration order in std/prelude.ovt.
enum { K_INVALID, K_NOT_FOUND, K_DENIED, K_CONFLICT, K_TIMEOUT, K_CANCELLED, K_UNAVAILABLE, K_IO, K_INTERNAL };

// A trap stops the program with this exit status.
enum { OVT_TRAP_STATUS = 101 };

static int ovt_argc;
static char **ovt_argv;

#define DATA(b) ((char *)(b) + sizeof(ovt_buf))

// ---- traps ----

void ovt_flush(void) { fflush(stdout); }

_Noreturn void ovt_trap(const char *msg, int64_t len, const char *file, int32_t line, int32_t col) {
  fflush(stdout);
  fprintf(stderr, "%s:%d:%d: trap: %.*s\n", file, line, col, (int)len, msg);
  fflush(stderr);
  _exit(OVT_TRAP_STATUS);
}

_Noreturn void ovt_trap_str(const ovt_str *msg, const char *file, int32_t line, int32_t col) {
  const char *p = msg->buf ? DATA(msg->buf) + msg->off : "";
  ovt_trap(p, msg->len, file, line, col);
}

_Noreturn void ovt_trap_index(int64_t i, int64_t len, const char *file, int32_t line, int32_t col) {
  char msg[128];
  int n = snprintf(msg, sizeof msg, "index %lld is out of range for length %lld", (long long)i, (long long)len);
  ovt_trap(msg, n, file, line, col);
}

_Noreturn static void oom(void) {
  fputs("trap: out of memory\n", stderr);
  _exit(OVT_TRAP_STATUS);
}

// ---- memory ----

void *ovt_alloc(int64_t size) {
  void *p = malloc(size > 0 ? (size_t)size : 1);
  if (!p) oom();
  return p;
}

void ovt_free(void *p) { free(p); }

static ovt_buf *buf_new(int64_t cap, int64_t esize) {
  if (cap < 1) cap = 1;
  ovt_buf *b = ovt_alloc((int64_t)sizeof(ovt_buf) + cap * esize);
  b->rc = 1;
  b->cap = cap;
  b->used = 0;
  return b;
}

void ovt_buf_release(ovt_buf *b, int64_t esize, ovt_fn1 drop) {
  if (!b || b->rc == 0) return;
  if (--b->rc > 0) return;
  if (drop) {
    char *d = DATA(b);
    for (int64_t i = 0; i < b->used; i++) drop(d + i * esize);
  }
  free(b);
}

// Boxes hold a value behind a pointer: an 8-byte count, then the value.
void ovt_box_release(int64_t *box, ovt_fn1 drop) {
  if (!box) return;
  if (--*box > 0) return;
  if (drop) drop(box + 1);
  free(box);
}

// Makes the box `*slot` points to unique, copying the value if it's shared.
void ovt_box_unique(int64_t **slot, int64_t size, ovt_fn1 dup, ovt_fn1 drop) {
  int64_t *box = *slot;
  if (*box == 1) return;
  int64_t *copy = ovt_alloc(8 + size);
  *copy = 1;
  memcpy(copy + 1, box + 1, (size_t)size);
  if (dup) dup(copy + 1);
  ovt_box_release(box, drop);
  *slot = copy;
}

// ---- arrays (and strings, with esize 1) ----

// Makes `a` the only view of its buffer, covering all of it, so it can change in place.
void ovt_arr_unique(ovt_arr *a, int64_t esize, ovt_fn1 dup, ovt_fn1 drop) {
  ovt_buf *b = a->buf;
  if (b && b->rc == 1 && a->off == 0 && a->len == b->used) return;
  int64_t first = esize < 8 ? 32 / esize : 4;
  ovt_buf *nb = buf_new(a->len < first ? first : a->len, esize);
  if (a->len) {
    memcpy(DATA(nb), DATA(b) + a->off * esize, (size_t)(a->len * esize));
    if (dup)
      for (int64_t i = 0; i < a->len; i++) dup(DATA(nb) + i * esize);
  }
  nb->used = a->len;
  ovt_buf_release(b, esize, drop);
  a->buf = nb;
  a->off = 0;
}

static void reserve(ovt_arr *a, int64_t esize, int64_t extra) {
  ovt_buf *b = a->buf;
  int64_t need = b->used + extra;
  if (need <= b->cap) return;
  int64_t cap = b->cap * 2;
  if (cap < need) cap = need;
  b = realloc(b, sizeof(ovt_buf) + (size_t)(cap * esize));
  if (!b) oom();
  b->cap = cap;
  a->buf = b;
}

// Adds a slot at the end and returns it; the caller stores the new element there.
void *ovt_arr_push(ovt_arr *a, int64_t esize, ovt_fn1 dup, ovt_fn1 drop) {
  ovt_arr_unique(a, esize, dup, drop);
  reserve(a, esize, 1);
  void *slot = DATA(a->buf) + a->buf->used * esize;
  a->buf->used++;
  a->len++;
  return slot;
}

// Removes the last element into `out`, or returns 0 if the array is empty.
int32_t ovt_arr_pop(ovt_arr *a, int64_t esize, ovt_fn1 dup, ovt_fn1 drop, void *out) {
  if (a->len == 0) return 0;
  ovt_arr_unique(a, esize, dup, drop);
  a->len--;
  a->buf->used--;
  memcpy(out, DATA(a->buf) + a->len * esize, (size_t)esize);
  return 1;
}

void *ovt_arr_insert(ovt_arr *a, int64_t i, int64_t esize, ovt_fn1 dup, ovt_fn1 drop, const char *file, int32_t line, int32_t col) {
  if (i < 0 || i > a->len) ovt_trap_index(i, a->len, file, line, col);
  ovt_arr_unique(a, esize, dup, drop);
  reserve(a, esize, 1);
  char *d = DATA(a->buf);
  memmove(d + (i + 1) * esize, d + i * esize, (size_t)((a->len - i) * esize));
  a->buf->used++;
  a->len++;
  return d + i * esize;
}

void ovt_arr_remove(ovt_arr *a, int64_t i, int64_t esize, ovt_fn1 dup, ovt_fn1 drop, void *out, const char *file, int32_t line, int32_t col) {
  if (i < 0 || i >= a->len) ovt_trap_index(i, a->len, file, line, col);
  ovt_arr_unique(a, esize, dup, drop);
  char *d = DATA(a->buf);
  memcpy(out, d + i * esize, (size_t)esize);
  memmove(d + i * esize, d + (i + 1) * esize, (size_t)((a->len - i - 1) * esize));
  a->buf->used--;
  a->len--;
}

void ovt_arr_clear(ovt_arr *a, int64_t esize, ovt_fn1 drop) {
  ovt_buf_release(a->buf, esize, drop);
  a->buf = NULL;
  a->off = 0;
  a->len = 0;
}

void ovt_arr_reverse(ovt_arr *a, int64_t esize, ovt_fn1 dup, ovt_fn1 drop) {
  if (a->len < 2) return;
  ovt_arr_unique(a, esize, dup, drop);
  char *d = DATA(a->buf);
  char tmp[256];
  char *t = esize <= (int64_t)sizeof tmp ? tmp : ovt_alloc(esize);
  for (int64_t i = 0, j = a->len - 1; i < j; i++, j--) {
    memcpy(t, d + i * esize, (size_t)esize);
    memcpy(d + i * esize, d + j * esize, (size_t)esize);
    memcpy(d + j * esize, t, (size_t)esize);
  }
  if (t != tmp) free(t);
}

// Stable merge sort; elements move as bits, so reference counts don't change.
static void merge_sort(char *d, char *tmp, int64_t n, int64_t esize, ovt_cmp cmp) {
  if (n < 2) return;
  if (n <= 12) {
    for (int64_t i = 1; i < n; i++) {
      memcpy(tmp, d + i * esize, (size_t)esize);
      int64_t j = i;
      while (j > 0 && cmp(d + (j - 1) * esize, tmp) > 0) {
        memcpy(d + j * esize, d + (j - 1) * esize, (size_t)esize);
        j--;
      }
      memcpy(d + j * esize, tmp, (size_t)esize);
    }
    return;
  }
  int64_t h = n / 2;
  merge_sort(d, tmp, h, esize, cmp);
  merge_sort(d + h * esize, tmp, n - h, esize, cmp);
  if (cmp(d + (h - 1) * esize, d + h * esize) <= 0) return;
  memcpy(tmp, d, (size_t)(h * esize));
  int64_t i = 0, j = h, k = 0;
  while (i < h && j < n) {
    if (cmp(d + j * esize, tmp + i * esize) < 0) {
      memcpy(d + k++ * esize, d + j++ * esize, (size_t)esize);
    } else {
      memcpy(d + k++ * esize, tmp + i++ * esize, (size_t)esize);
    }
  }
  while (i < h) memcpy(d + k++ * esize, tmp + i++ * esize, (size_t)esize);
}

void ovt_arr_sort(ovt_arr *a, int64_t esize, ovt_fn1 dup, ovt_fn1 drop, ovt_cmp cmp) {
  if (a->len < 2) return;
  ovt_arr_unique(a, esize, dup, drop);
  char *tmp = ovt_alloc(a->len * esize);
  merge_sort(DATA(a->buf), tmp, a->len, esize, cmp);
  free(tmp);
}

void ovt_arr_slice(ovt_arr *out, const ovt_arr *a, int64_t lo, int64_t hi, const char *file, int32_t line, int32_t col) {
  if (lo < 0 || hi > a->len || lo > hi) {
    char msg[160];
    int n = snprintf(msg, sizeof msg, "slice %lld..%lld is out of range for length %lld", (long long)lo, (long long)hi, (long long)a->len);
    ovt_trap(msg, n, file, line, col);
  }
  out->buf = a->buf;
  out->off = a->off + lo;
  out->len = hi - lo;
  if (out->buf && out->buf->rc > 0) out->buf->rc++;
}

// ---- strings ----

static inline const char *sdata(const ovt_str *s) { return s->buf ? DATA(s->buf) + s->off : ""; }

void ovt_str_slice(ovt_str *out, const ovt_str *s, int64_t lo, int64_t hi, const char *file, int32_t line, int32_t col) {
  ovt_arr_slice(out, s, lo, hi, file, line, col);
  const unsigned char *d = (const unsigned char *)sdata(s);
  if ((lo < s->len && (d[lo] & 0xC0) == 0x80) || (hi < s->len && (d[hi] & 0xC0) == 0x80)) {
    char msg[160];
    int n = snprintf(msg, sizeof msg, "slice %lld..%lld cuts a UTF-8 character in half", (long long)lo, (long long)hi);
    ovt_trap(msg, n, file, line, col);
  }
}

void ovt_str_append_bytes(ovt_str *s, const char *p, int64_t n) {
  if (n == 0) return;
  ovt_arr_unique(s, 1, NULL, NULL);
  reserve(s, 1, n);
  memcpy(DATA(s->buf) + s->buf->used, p, (size_t)n);
  s->buf->used += n;
  s->len += n;
}

void ovt_str_append(ovt_str *s, const ovt_str *t) {
  if (s->len == 0 && t->buf) {
    // Appending to an empty string: share the other string instead of copying.
    ovt_buf_release(s->buf, 1, NULL);
    *s = *t;
    if (s->buf->rc > 0) s->buf->rc++;
    return;
  }
  ovt_str_append_bytes(s, sdata(t), t->len);
}

void ovt_str_concat(ovt_str *out, const ovt_str *a, const ovt_str *b) {
  ovt_buf *nb = buf_new(a->len + b->len, 1);
  memcpy(DATA(nb), sdata(a), (size_t)a->len);
  memcpy(DATA(nb) + a->len, sdata(b), (size_t)b->len);
  nb->used = a->len + b->len;
  out->buf = nb;
  out->off = 0;
  out->len = a->len + b->len;
}

int32_t ovt_str_eq(const ovt_str *a, const ovt_str *b) {
  return a->len == b->len && (a->len == 0 || memcmp(sdata(a), sdata(b), (size_t)a->len) == 0);
}

int32_t ovt_str_cmp(const ovt_str *a, const ovt_str *b) {
  int64_t n = a->len < b->len ? a->len : b->len;
  int c = n ? memcmp(sdata(a), sdata(b), (size_t)n) : 0;
  if (c) return c < 0 ? -1 : 1;
  return a->len < b->len ? -1 : a->len > b->len ? 1 : 0;
}

int64_t ovt_str_find(const ovt_str *s, const ovt_str *pat) {
  if (pat->len == 0) return 0;
  if (pat->len > s->len) return -1;
  const char *d = sdata(s), *p = sdata(pat);
  const char *end = d + s->len - pat->len;
  for (const char *c = d; c <= end;) {
    c = memchr(c, p[0], (size_t)(end - c + 1));
    if (!c) return -1;
    if (memcmp(c, p, (size_t)pat->len) == 0) return c - d;
    c++;
  }
  return -1;
}

void ovt_str_case(ovt_str *out, const ovt_str *s, int32_t upper) {
  const char *d = sdata(s);
  int64_t first = 0;
  while (first < s->len && !(upper ? (d[first] >= 'a' && d[first] <= 'z') : (d[first] >= 'A' && d[first] <= 'Z'))) first++;
  if (first == s->len) {
    // Nothing to change: share the string instead of copying it.
    *out = *s;
    if (out->buf && out->buf->rc > 0) out->buf->rc++;
    return;
  }
  ovt_buf *nb = buf_new(s->len, 1);
  char *o = DATA(nb);
  for (int64_t i = 0; i < s->len; i++) {
    char c = d[i];
    if (upper && c >= 'a' && c <= 'z') c -= 32;
    if (!upper && c >= 'A' && c <= 'Z') c += 32;
    o[i] = c;
  }
  nb->used = s->len;
  out->buf = nb;
  out->off = 0;
  out->len = s->len;
}

static int64_t utf8_valid(const unsigned char *d, int64_t n) {
  int64_t i = 0;
  while (i < n) {
    unsigned char c = d[i];
    int len = c < 0x80 ? 1 : (c & 0xE0) == 0xC0 ? 2 : (c & 0xF0) == 0xE0 ? 3 : (c & 0xF8) == 0xF0 ? 4 : 0;
    if (!len || i + len > n) return 0;
    for (int k = 1; k < len; k++)
      if ((d[i + k] & 0xC0) != 0x80) return 0;
    if (len == 2 && c < 0xC2) return 0;
    i += len;
  }
  return 1;
}

// Shares the bytes' buffer if they're valid UTF-8.
int32_t ovt_str_from_bytes(ovt_str *out, const ovt_arr *b) {
  if (!utf8_valid((const unsigned char *)sdata(b), b->len)) return 0;
  *out = *b;
  if (out->buf && out->buf->rc > 0) out->buf->rc++;
  return 1;
}

void ovt_str_runes(ovt_arr *out, const ovt_str *s) {
  const unsigned char *d = (const unsigned char *)sdata(s);
  ovt_buf *nb = buf_new(s->len, 4);
  uint32_t *o = (uint32_t *)DATA(nb);
  int64_t n = 0;
  for (int64_t i = 0; i < s->len;) {
    unsigned char c = d[i];
    uint32_t cp;
    int len;
    if (c < 0x80) { cp = c; len = 1; }
    else if ((c & 0xE0) == 0xC0) { cp = c & 0x1F; len = 2; }
    else if ((c & 0xF0) == 0xE0) { cp = c & 0x0F; len = 3; }
    else { cp = c & 0x07; len = 4; }
    for (int k = 1; k < len && i + k < s->len; k++) cp = (cp << 6) | (d[i + k] & 0x3F);
    o[n++] = cp;
    i += len;
  }
  nb->used = n;
  out->buf = nb;
  out->off = 0;
  out->len = n;
}

uint64_t ovt_hash_bytes(const ovt_str *s) {
  // FNV-1a over 8-byte words, finished with a strong mix.
  const unsigned char *d = (const unsigned char *)sdata(s);
  uint64_t h = 0xcbf29ce484222325ull ^ (uint64_t)s->len;
  int64_t i = 0;
  for (; i + 8 <= s->len; i += 8) {
    uint64_t w;
    memcpy(&w, d + i, 8);
    h = (h ^ w) * 0x100000001b3ull;
    h ^= h >> 29;
  }
  for (; i < s->len; i++) h = (h ^ d[i]) * 0x100000001b3ull;
  h ^= h >> 33;
  h *= 0xff51afd7ed558ccdull;
  h ^= h >> 33;
  return h;
}

uint64_t ovt_hash_mix(uint64_t h, uint64_t x) {
  uint64_t z = h ^ (x + 0x9e3779b97f4a7c15ull + (h << 6) + (h >> 2));
  z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ull;
  z = (z ^ (z >> 27)) * 0x94d049bb133111ebull;
  return z ^ (z >> 31);
}

// ---- showing values (the string builder is an ovt_str being appended to) ----

void ovt_sb_int(ovt_str *sb, int64_t v) {
  char buf[32];
  int n = snprintf(buf, sizeof buf, "%lld", (long long)v);
  ovt_str_append_bytes(sb, buf, n);
}

void ovt_sb_uint(ovt_str *sb, uint64_t v) {
  char buf[32];
  int n = snprintf(buf, sizeof buf, "%llu", (unsigned long long)v);
  ovt_str_append_bytes(sb, buf, n);
}

// The shortest decimal form that reads back as the same number, always with a `.` or exponent.
void ovt_sb_f64(ovt_str *sb, double v) {
  char buf[64];
  int n = 0;
  if (v != v) n = snprintf(buf, sizeof buf, "nan");
  else if (v == 1.0 / 0.0) n = snprintf(buf, sizeof buf, "inf");
  else if (v == -1.0 / 0.0) n = snprintf(buf, sizeof buf, "-inf");
  else {
    for (int p = 1; p <= 17; p++) {
      n = snprintf(buf, sizeof buf, "%.*g", p, v);
      if (strtod(buf, NULL) == v) break;
    }
    if (!strpbrk(buf, ".e")) n += snprintf(buf + n, sizeof buf - (size_t)n, ".0");
  }
  ovt_str_append_bytes(sb, buf, n);
}

void ovt_sb_dur(ovt_str *sb, int64_t ns) {
  static const struct { int64_t n; const char *u; } units[] = {
    {3600000000000LL, "h"}, {60000000000LL, "m"}, {1000000000LL, "s"}, {1000000LL, "ms"}, {1000LL, "us"}, {1, "ns"}};
  char buf[48];
  int n;
  int64_t a = ns < 0 ? -ns : ns;
  size_t k = 0;
  while (k < 5 && a < units[k].n) k++;
  if (a % units[k].n == 0) n = snprintf(buf, sizeof buf, "%lld%s", (long long)(ns / units[k].n), units[k].u);
  else n = snprintf(buf, sizeof buf, "%.3g%s", (double)ns / (double)units[k].n, units[k].u);
  ovt_str_append_bytes(sb, buf, n);
}

void ovt_sb_cstr(ovt_str *sb, const char *s, int64_t n) { ovt_str_append_bytes(sb, s, n); }

// A string as an Overt literal: quoted, with escapes.
void ovt_sb_quoted(ovt_str *sb, const ovt_str *s) {
  const unsigned char *d = (const unsigned char *)sdata(s);
  ovt_str_append_bytes(sb, "\"", 1);
  int64_t start = 0;
  for (int64_t i = 0; i < s->len; i++) {
    const char *esc = NULL;
    char hex[8];
    switch (d[i]) {
      case '"': esc = "\\\""; break;
      case '\\': esc = "\\\\"; break;
      case '\n': esc = "\\n"; break;
      case '\t': esc = "\\t"; break;
      case '\r': esc = "\\r"; break;
      case '$': esc = (i + 1 < s->len && d[i + 1] == '{') ? "\\$" : NULL; break;
      default:
        if (d[i] < 0x20) {
          snprintf(hex, sizeof hex, "\\x%02x", d[i]);
          esc = hex;
        }
    }
    if (esc) {
      ovt_str_append_bytes(sb, (const char *)d + start, i - start);
      ovt_str_append_bytes(sb, esc, (int64_t)strlen(esc));
      start = i + 1;
    }
  }
  ovt_str_append_bytes(sb, (const char *)d + start, s->len - start);
  ovt_str_append_bytes(sb, "\"", 1);
}

// ---- parsing numbers ----

int32_t ovt_parse_int(const ovt_str *s, int64_t *out) {
  const char *d = sdata(s);
  int64_t n = s->len, i = 0;
  int neg = 0;
  if (n > 0 && d[0] == '-') {
    neg = 1;
    i = 1;
  }
  if (i == n) return 0;
  uint64_t v = 0;
  for (; i < n; i++) {
    if (d[i] < '0' || d[i] > '9') return 0;
    uint64_t digit = (uint64_t)(d[i] - '0');
    if (v > (UINT64_MAX - digit) / 10) return 0;
    v = v * 10 + digit;
  }
  if (neg ? v > (uint64_t)INT64_MAX + 1 : v > (uint64_t)INT64_MAX) return 0;
  *out = neg ? (int64_t)(0 - v) : (int64_t)v;
  return 1;
}

int32_t ovt_parse_f64(const ovt_str *s, double *out) {
  if (s->len == 0 || s->len > 400) return 0;
  char buf[401];
  memcpy(buf, sdata(s), (size_t)s->len);
  buf[s->len] = 0;
  for (int64_t i = 0; i < s->len; i++) {
    char c = buf[i];
    if (!((c >= '0' && c <= '9') || c == '.' || c == 'e' || c == 'E' || c == '-' || c == '+')) return 0;
  }
  char *end;
  errno = 0;
  double v = strtod(buf, &end);
  if (end != buf + s->len) return 0;
  *out = v;
  return 1;
}

// ---- printing ----

void ovt_print(const ovt_str *s, int32_t to_stderr) {
  FILE *f = to_stderr ? stderr : stdout;
  if (to_stderr) fflush(stdout);
  fwrite(sdata(s), 1, (size_t)s->len, f);
  fputc('\n', f);
}

void ovt_dbg(const ovt_str *shown, const char *file, int32_t line, int32_t col, const char *text) {
  fflush(stdout);
  fprintf(stderr, "%s:%d:%d: dbg: %s = %.*s\n", file, line, col, text, (int)shown->len, sdata(shown));
}

// ---- process ----

void ovt_rt_init(int32_t argc, char **argv) {
  ovt_argc = argc;
  ovt_argv = argv;
}

void ovt_rt_exit(void) { fflush(stdout); }

static void str_from(ovt_str *out, const char *p, int64_t n) {
  out->buf = NULL;
  out->off = 0;
  out->len = 0;
  ovt_str_append_bytes(out, p, n);
}

void ovt_os_args(ovt_arr *out) {
  out->buf = NULL;
  out->off = 0;
  out->len = 0;
  for (int i = 1; i < ovt_argc; i++) {
    ovt_str *slot = ovt_arr_push(out, sizeof(ovt_str), NULL, NULL);
    str_from(slot, ovt_argv[i], (int64_t)strlen(ovt_argv[i]));
  }
}

int32_t ovt_os_env(const ovt_str *name, ovt_str *out) {
  char key[512];
  if (name->len >= (int64_t)sizeof key) return 0;
  memcpy(key, sdata(name), (size_t)name->len);
  key[name->len] = 0;
  const char *v = getenv(key);
  if (!v) return 0;
  str_from(out, v, (int64_t)strlen(v));
  return 1;
}

_Noreturn void ovt_os_exit(int64_t status) {
  fflush(stdout);
  fflush(stderr);
  exit((int)status);
}

// "<what> <path>: <reason>", with the whole path.
static void err_msg(ovt_str *err, const char *what, const ovt_str *path, int e) {
  const char *reason = strerror(e);
  str_from(err, what, (int64_t)strlen(what));
  ovt_str_append_bytes(err, " ", 1);
  ovt_str_append_bytes(err, sdata(path), path->len);
  ovt_str_append_bytes(err, ": ", 2);
  ovt_str_append_bytes(err, reason, (int64_t)strlen(reason));
}

static int32_t kind_of_errno(int e) {
  switch (e) {
    case ENOENT: case ENOTDIR: return K_NOT_FOUND;
    case EACCES: case EPERM: return K_DENIED;
    case EISDIR: return K_INVALID;
    default: return K_IO;
  }
}

// Reads a whole file descriptor. Returns 0, or an ErrKind with `err` set.
static int32_t read_fd(int fd, ovt_arr *out) {
  out->buf = NULL;
  out->off = 0;
  out->len = 0;
  struct stat st;
  int64_t hint = (fstat(fd, &st) == 0 && S_ISREG(st.st_mode)) ? (int64_t)st.st_size : 65536;
  out->buf = buf_new(hint + 1, 1);
  for (;;) {
    reserve(out, 1, 65536);
    ssize_t n = read(fd, DATA(out->buf) + out->buf->used, (size_t)(out->buf->cap - out->buf->used));
    if (n < 0) {
      if (errno == EINTR) continue;
      return -1;
    }
    if (n == 0) break;
    out->buf->used += n;
    out->len += n;
  }
  return 0;
}

int32_t ovt_fs_read(const ovt_str *path, ovt_arr *out, int32_t want_text, ovt_str *err) {
  char p[4096];
  if (path->len >= (int64_t)sizeof p) {
    err_msg(err, "can't read", path, ENAMETOOLONG);
    return K_INVALID + 1;
  }
  memcpy(p, sdata(path), (size_t)path->len);
  p[path->len] = 0;
  int fd = open(p, O_RDONLY);
  if (fd < 0) {
    int e = errno;
    err_msg(err, "can't read", path, e);
    return kind_of_errno(e) + 1;
  }
  if (read_fd(fd, out) < 0) {
    int e = errno;
    close(fd);
    ovt_buf_release(out->buf, 1, NULL);
    err_msg(err, "can't read", path, e);
    return kind_of_errno(e) + 1;
  }
  close(fd);
  if (want_text && !utf8_valid((const unsigned char *)sdata(out), out->len)) {
    ovt_buf_release(out->buf, 1, NULL);
    const char *m = " isn't valid UTF-8 text; read it with fs.read_bytes";
    str_from(err, sdata(path), path->len);
    ovt_str_append_bytes(err, m, (int64_t)strlen(m));
    return K_INVALID + 1;
  }
  return 0;
}

int32_t ovt_read_stdin(ovt_arr *out, int32_t want_text, ovt_str *err) {
  if (read_fd(0, out) < 0) {
    str_from(err, "can't read stdin", 16);
    return K_IO + 1;
  }
  if (want_text && !utf8_valid((const unsigned char *)sdata(out), out->len)) {
    ovt_buf_release(out->buf, 1, NULL);
    str_from(err, "stdin isn't valid UTF-8 text; read it with os.read_stdin_bytes", 62);
    return K_INVALID + 1;
  }
  return 0;
}

int32_t ovt_fs_write(const ovt_str *path, const ovt_str *data, ovt_str *err) {
  char p[4096];
  if (path->len >= (int64_t)sizeof p) {
    err_msg(err, "can't write", path, ENAMETOOLONG);
    return K_INVALID + 1;
  }
  memcpy(p, sdata(path), (size_t)path->len);
  p[path->len] = 0;
  int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (fd < 0) {
    int e = errno;
    err_msg(err, "can't write", path, e);
    return kind_of_errno(e) + 1;
  }
  const char *d = sdata(data);
  int64_t left = data->len;
  while (left > 0) {
    ssize_t n = write(fd, d, (size_t)left);
    if (n < 0) {
      if (errno == EINTR) continue;
      int e = errno;
      close(fd);
      err_msg(err, "can't write", path, e);
      return kind_of_errno(e) + 1;
    }
    d += n;
    left -= n;
  }
  close(fd);
  return 0;
}

int32_t ovt_fs_exists(const ovt_str *path) {
  char p[4096];
  if (path->len >= (int64_t)sizeof p) return 0;
  memcpy(p, sdata(path), (size_t)path->len);
  p[path->len] = 0;
  struct stat st;
  return stat(p, &st) == 0;
}

// A failing `main`: print the error and exit with status 1.
_Noreturn void ovt_main_failed(const ovt_str *msg) {
  fflush(stdout);
  fprintf(stderr, "error: %.*s\n", (int)msg->len, sdata(msg));
  fflush(stderr);
  exit(1);
}

// ---- tests ----
//
// Each test runs in a forked child, so a trap fails only that test. The
// child's output is captured and shown only if the test fails.

static int tests_run, tests_failed;

void ovt_test_run(const char *name, int32_t (*fn)(void)) {
  fflush(stdout);
  fflush(stderr);
  char path[] = "/tmp/ovt-test-XXXXXX";
  int fd = mkstemp(path);
  pid_t pid = fork();
  if (pid == 0) {
    dup2(fd, 1);
    dup2(fd, 2);
    int32_t status = fn();
    fflush(stdout);
    fflush(stderr);
    _exit(status);
  }
  int status = 0;
  waitpid(pid, &status, 0);
  tests_run++;
  int ok = WIFEXITED(status) && WEXITSTATUS(status) == 0;
  if (!ok) {
    tests_failed++;
    printf("FAIL %s\n", name);
    lseek(fd, 0, SEEK_SET);
    char buf[4096];
    ssize_t n;
    while ((n = read(fd, buf, sizeof buf)) > 0) {
      fwrite("  ", 1, 2, stdout);
      for (ssize_t i = 0; i < n; i++) {
        fputc(buf[i], stdout);
        if (buf[i] == '\n' && i + 1 < n) fwrite("  ", 1, 2, stdout);
      }
    }
    if (WIFSIGNALED(status)) printf("  killed by signal %d\n", WTERMSIG(status));
  }
  close(fd);
  unlink(path);
}

int32_t ovt_test_finish(void) {
  if (tests_failed == 0) printf("%d tests passed\n", tests_run);
  else printf("%d of %d tests failed\n", tests_failed, tests_run);
  fflush(stdout);
  return tests_failed ? 1 : 0;
}

// An `ex` line whose two sides differ.
void ovt_expect_failed(const ovt_str *left, const ovt_str *right) {
  fprintf(stderr, "left:  %.*s\nright: %.*s\n", (int)left->len, sdata(left), (int)right->len, sdata(right));
}

void ovt_note(const char *text) { fprintf(stderr, "%s\n", text); }

void ovt_note_str(const char *prefix, const ovt_str *s) { fprintf(stderr, "%s%.*s\n", prefix, (int)s->len, sdata(s)); }
