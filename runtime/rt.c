// The Overt runtime.
//
// Compiled code calls these functions with scalars and pointers only, never
// with structs by value, so the C calling convention for structs never matters.
//
// Strings and arrays are {buf, off, len} views of a reference-counted buffer:
// a 24-byte header (count, capacity, used) followed by the elements. `used`
// is how many elements the buffer holds; a view may show fewer.
//
// Counts (of buffers, boxes and closure environments) come in three kinds:
// - above 0: owned by one task, changed with plain loads and stores
// - below 0: shared between tasks, changed atomically; the number of
//   references is the negated count
// - 0: static data (string and array literals), never freed
// A value becomes shared when it's handed to other tasks (see `ovt_mark_*`).

#include <dirent.h>
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

// ---- counts ----

static inline int64_t rc_get(int64_t *p) { return __atomic_load_n(p, __ATOMIC_RELAXED); }

// Adds a reference.
static inline void rc_inc(int64_t *p) {
  int64_t rc = rc_get(p);
  if (rc > 0) *p = rc + 1;
  else if (rc < 0) __atomic_fetch_sub(p, 1, __ATOMIC_RELAXED);
}

// Removes a reference; returns 1 if it was the last one.
static inline int rc_dec(int64_t *p) {
  int64_t rc = rc_get(p);
  if (rc > 1) {
    *p = rc - 1;
    return 0;
  }
  if (rc == 1) return 1;
  if (rc == 0) return 0;
  return __atomic_fetch_add(p, 1, __ATOMIC_ACQ_REL) == -1;
}

// Whether this is the only reference. A shared value with one reference left
// becomes owned again, since no other task can reach it.
static inline int rc_unique(int64_t *p) {
  int64_t rc = __atomic_load_n(p, __ATOMIC_ACQUIRE);
  if (rc == 1) return 1;
  if (rc == -1) {
    *p = 1;
    return 1;
  }
  return 0;
}

// The slow path of an inline `dup`, for counts that aren't above 0.
void ovt_rc_inc(int64_t *p) {
  if (p) rc_inc(p);
}

// Marks a value shared before other tasks can reach it: negates its count,
// then marks what it holds. Stops at values that are already shared or static.
static int mark_count(int64_t *p) {
  int64_t rc = rc_get(p);
  if (rc <= 0) return 0;
  *p = -rc;
  return 1;
}

void ovt_mark_buf(ovt_buf *b, int64_t esize, ovt_fn1 mark) {
  if (!b || !mark_count(&b->rc) || !mark) return;
  char *d = DATA(b);
  for (int64_t i = 0; i < b->used; i++) mark(d + i * esize);
}

void ovt_mark_box(int64_t *box, ovt_fn1 mark) {
  if (box && mark_count(box) && mark) mark(box + 1);
}

// Closure environments: {count, drop function, mark function, captures...}.
void ovt_mark_env(int64_t *env) {
  if (env && mark_count(env)) ((ovt_fn1)env[2])(env);
}

// The mark function of an environment that holds nothing to mark (an `Atomic`'s).
void ovt_mark_none(void *env) { (void)env; }

void ovt_env_release(int64_t *env) {
  if (env && rc_dec(env)) ((ovt_fn1)env[1])(env);
}

void ovt_buf_release(ovt_buf *b, int64_t esize, ovt_fn1 drop) {
  if (!b || !rc_dec(&b->rc)) return;
  if (drop) {
    char *d = DATA(b);
    for (int64_t i = 0; i < b->used; i++) drop(d + i * esize);
  }
  free(b);
}

// Boxes hold a value behind a pointer: an 8-byte count, then the value.
void ovt_box_release(int64_t *box, ovt_fn1 drop) {
  if (!box || !rc_dec(box)) return;
  if (drop) drop(box + 1);
  free(box);
}

// Makes the box `*slot` points to unique, copying the value if it's shared.
void ovt_box_unique(int64_t **slot, int64_t size, ovt_fn1 dup, ovt_fn1 drop) {
  int64_t *box = *slot;
  if (rc_unique(box)) return;
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
  if (b && a->off == 0 && a->len == b->used && rc_unique(&b->rc)) return;
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
  if (out->buf) rc_inc(&out->buf->rc);
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
    rc_inc(&s->buf->rc);
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
    if (out->buf) rc_inc(&out->buf->rc);
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
  if (out->buf) rc_inc(&out->buf->rc);
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
  // Tasks printing at the same time get whole lines.
  flockfile(f);
  fwrite(sdata(s), 1, (size_t)s->len, f);
  fputc('\n', f);
  funlockfile(f);
}

void ovt_dbg(const ovt_str *shown, const char *file, int32_t line, int32_t col, const char *text) {
  fflush(stdout);
  fprintf(stderr, "%s:%d:%d: dbg: %s = %.*s\n", file, line, col, text, (int)shown->len, sdata(shown));
}

// ---- tasks ----
//
// All Overt code runs in tasks: coroutines with their own stacks, run by one
// worker thread per core. Stacks never move, so a pointer into a task's stack
// stays valid while the task waits.
//
// - `main` runs as the first task, on worker 0, which is the process's main
//   thread. The other workers start the first time a task is spawned, so a
//   program that never runs anything in parallel has one thread.
// - Each worker has a run queue; idle workers take tasks from the global
//   queue or steal from other workers, and sleep when there's nothing to do.
// - A task that waits (for its children, or for the blocking pool) parks on
//   a waiter in its own stack frame: it switches back to its worker's
//   scheduler, and whoever finishes the work wakes the waiter, which puts
//   the task back on a run queue.
// - File IO runs on the blocking pool, so a worker isn't stuck in the kernel
//   while other tasks could run.

#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/sysctl.h>

#if !defined(__aarch64__)
#error "Overt's task runtime supports arm64 only so far"
#endif

#if defined(__has_feature)
#if __has_feature(thread_sanitizer)
#define OVT_TSAN 1
void *__tsan_get_current_fiber(void);
void *__tsan_create_fiber(unsigned flags);
void __tsan_destroy_fiber(void *fiber);
void __tsan_switch_to_fiber(void *fiber, unsigned flags);
#endif
#if __has_feature(address_sanitizer)
#define OVT_ASAN 1
void __sanitizer_start_switch_fiber(void **fake_stack_save, const void *bottom, size_t size);
void __sanitizer_finish_switch_fiber(void *fake_stack_save, const void **bottom_old, size_t *size_old);
void __asan_unpoison_memory_region(void const volatile *addr, size_t size);
#endif
#endif

#ifdef __APPLE__
#define SYM(name) "_" #name
#else
#define SYM(name) #name
#endif

// Saved registers: x19–x30, sp and d8–d15, the ones a call must preserve.
typedef struct {
  uint64_t r[21];
} ovt_ctx;

// Saves the current registers in `from` and continues from `to`.
void ovt_ctx_switch(ovt_ctx *from, ovt_ctx *to);
// Where a new task starts: calls ovt_task_main with the task in x19.
void ovt_ctx_start(void);

__asm__(".text\n"
        ".globl " SYM(ovt_ctx_switch) "\n"
        ".p2align 2\n" SYM(ovt_ctx_switch) ":\n"
        "  stp x19, x20, [x0, #0]\n"
        "  stp x21, x22, [x0, #16]\n"
        "  stp x23, x24, [x0, #32]\n"
        "  stp x25, x26, [x0, #48]\n"
        "  stp x27, x28, [x0, #64]\n"
        "  stp x29, x30, [x0, #80]\n"
        "  mov x9, sp\n"
        "  str x9, [x0, #96]\n"
        "  stp d8, d9, [x0, #104]\n"
        "  stp d10, d11, [x0, #120]\n"
        "  stp d12, d13, [x0, #136]\n"
        "  stp d14, d15, [x0, #152]\n"
        "  ldp x19, x20, [x1, #0]\n"
        "  ldp x21, x22, [x1, #16]\n"
        "  ldp x23, x24, [x1, #32]\n"
        "  ldp x25, x26, [x1, #48]\n"
        "  ldp x27, x28, [x1, #64]\n"
        "  ldp x29, x30, [x1, #80]\n"
        "  ldr x9, [x1, #96]\n"
        "  mov sp, x9\n"
        "  ldp d8, d9, [x1, #104]\n"
        "  ldp d10, d11, [x1, #120]\n"
        "  ldp d12, d13, [x1, #136]\n"
        "  ldp d14, d15, [x1, #152]\n"
        "  ret\n"
        ".globl " SYM(ovt_ctx_start) "\n"
        ".p2align 2\n" SYM(ovt_ctx_start) ":\n"
        "  mov x0, x19\n"
        "  bl " SYM(ovt_task_main) "\n"
        "  brk #0\n");

enum { STACK_SIZE = 256 * 1024, MAIN_STACK_SIZE = 8 * 1024 * 1024, POOLED_STACKS = 64 };
enum { ACT_NONE, ACT_PARK, ACT_DONE };

typedef struct ovt_task ovt_task;
typedef struct ovt_group ovt_group;

// Something a task waits for, woken exactly once. It lives in the waiting
// task's stack frame, which stays valid until the task has seen W_DONE; a
// waker's last access is the exchange that sets W_DONE.
typedef struct {
  int state; // W_*, changed atomically
  ovt_task *task;
  int result; // set by the waker, for waits that can end in several ways (R_*)
} ovt_waiter;

enum { W_WAITING, W_PARKED, W_DONE };
enum { R_READY, R_TIMEOUT, R_CANCELLED, R_CLOSED };

// A cancel scope. Every task belongs to one; cancelling a scope cancels the
// scopes below it. A cancelled task's next failing `io` call fails with
// `.Cancelled`, and a task waiting on a socket or a timer wakes up to fail.
typedef struct ovt_cancel {
  int64_t rc;    // changed atomically
  int cancelled; // changed atomically
  int timed_out; // cancelled by a task.timeout deadline
  struct ovt_cancel *parent;
} ovt_cancel;

static ovt_cancel *cancel_new(ovt_cancel *parent) {
  ovt_cancel *c = ovt_alloc(sizeof(ovt_cancel));
  c->rc = 1;
  c->cancelled = 0;
  c->timed_out = 0;
  c->parent = parent;
  if (parent) __atomic_add_fetch(&parent->rc, 1, __ATOMIC_RELAXED);
  return c;
}

static void cancel_ref(ovt_cancel *c) {
  if (c) __atomic_add_fetch(&c->rc, 1, __ATOMIC_RELAXED);
}

static void cancel_unref(ovt_cancel *c) {
  while (c && __atomic_sub_fetch(&c->rc, 1, __ATOMIC_ACQ_REL) == 0) {
    ovt_cancel *p = c->parent;
    free(c);
    c = p;
  }
}

static void cancel_scope(ovt_cancel *c);

static int is_cancelled(ovt_cancel *c) {
  for (; c; c = c->parent)
    if (__atomic_load_n(&c->cancelled, __ATOMIC_ACQUIRE)) return 1;
  return 0;
}

struct ovt_task {
  ovt_ctx ctx;
  char *stack; // the mapping: a guard page, then the stack
  size_t stack_size;
  int32_t (*fn)(void *);
  void *arg;
  int32_t result;
  ovt_group *group;   // told when this task finishes
  int action;         // what the scheduler does once the task has switched out
  ovt_waiter *waiter; // what it parked on
  ovt_cancel *cancel; // its cancel scope (a reference), or NULL
  ovt_task *next;     // run queue link
#ifdef OVT_TSAN
  void *fiber;
#endif
};

typedef struct {
  ovt_ctx sched; // the scheduler loop, on the worker thread's own stack
  ovt_task *current;
  pthread_mutex_t mu;
  ovt_task *head, *tail;
  int64_t queued; // changed under `mu`, read atomically
  int id;
#ifdef OVT_TSAN
  void *fiber;
#endif
#ifdef OVT_ASAN
  const void *stack_bottom;
  size_t stack_size;
#endif
} ovt_worker;

static ovt_worker *workers;
static int nworkers = 1;
static int64_t live_tasks; // changed atomically
static __thread ovt_worker *tl_worker;

static pthread_mutex_t global_mu = PTHREAD_MUTEX_INITIALIZER;
static ovt_task *global_head, *global_tail;
static int64_t global_queued;

static pthread_mutex_t idle_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t idle_cv = PTHREAD_COND_INITIALIZER;
static int64_t idle_workers;

static pthread_mutex_t stacks_mu = PTHREAD_MUTEX_INITIALIZER;
static char *free_stacks[POOLED_STACKS];
static int nfree_stacks;
static size_t page_size;

// Not inlined, so a task that moved to another thread reads the new value.
__attribute__((noinline)) static ovt_worker *cur_worker(void) { return tl_worker; }

static ovt_task *cur_task(void) {
  ovt_worker *w = cur_worker();
  return w ? w->current : NULL;
}

// ---- stacks ----

static char *stack_new(size_t size) {
  if (size == STACK_SIZE) {
    pthread_mutex_lock(&stacks_mu);
    char *s = nfree_stacks ? free_stacks[--nfree_stacks] : NULL;
    pthread_mutex_unlock(&stacks_mu);
    if (s) return s;
  }
  char *s = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
  if (s == MAP_FAILED) oom();
  // The lowest page is the guard: running into it means the stack overflowed.
  mprotect(s, page_size, PROT_NONE);
  return s;
}

static void stack_free(char *s, size_t size) {
#ifdef OVT_ASAN
  // A finished task never returned from its last frames, so clear their poison.
  __asan_unpoison_memory_region(s + page_size, size - page_size);
#endif
  if (size == STACK_SIZE) {
    // Let the OS take the touched pages back when it needs them.
    madvise(s + page_size, size - page_size, MADV_FREE);
    pthread_mutex_lock(&stacks_mu);
    if (nfree_stacks < POOLED_STACKS) {
      free_stacks[nfree_stacks++] = s;
      s = NULL;
    }
    pthread_mutex_unlock(&stacks_mu);
    if (!s) return;
  }
  munmap(s, size);
}

// ---- switching ----

// Every switch goes through here, so the sanitizers can follow it.
static void switch_to_task(ovt_worker *w, ovt_task *t) {
#ifdef OVT_TSAN
  __tsan_switch_to_fiber(t->fiber, 0);
#endif
#ifdef OVT_ASAN
  void *fake = NULL;
  __sanitizer_start_switch_fiber(&fake, t->stack + page_size, t->stack_size - page_size);
#endif
  ovt_ctx_switch(&w->sched, &t->ctx);
#ifdef OVT_ASAN
  __sanitizer_finish_switch_fiber(fake, NULL, NULL);
#endif
}

static void switch_to_sched(ovt_worker *w, ovt_task *t, int finished) {
#ifdef OVT_TSAN
  __tsan_switch_to_fiber(w->fiber, 0);
#endif
#ifdef OVT_ASAN
  void *fake = NULL;
  // A finished task never comes back, so its fake stack can go.
  __sanitizer_start_switch_fiber(finished ? NULL : &fake, w->stack_bottom, w->stack_size);
#endif
  (void)finished;
  ovt_ctx_switch(&t->ctx, &w->sched);
#ifdef OVT_ASAN
  __sanitizer_finish_switch_fiber(fake, NULL, NULL);
#endif
}

// ---- run queues ----

static void wake_idle(void) {
  if (__atomic_load_n(&idle_workers, __ATOMIC_SEQ_CST) > 0) {
    pthread_mutex_lock(&idle_mu);
    pthread_cond_signal(&idle_cv);
    pthread_mutex_unlock(&idle_mu);
  }
}

static void push_global(ovt_task *t) {
  t->next = NULL;
  pthread_mutex_lock(&global_mu);
  if (global_tail) global_tail->next = t;
  else global_head = t;
  global_tail = t;
  __atomic_add_fetch(&global_queued, 1, __ATOMIC_SEQ_CST);
  pthread_mutex_unlock(&global_mu);
  wake_idle();
}

static void push_local(ovt_worker *w, ovt_task *t) {
  t->next = NULL;
  pthread_mutex_lock(&w->mu);
  if (w->tail) w->tail->next = t;
  else w->head = t;
  w->tail = t;
  __atomic_add_fetch(&w->queued, 1, __ATOMIC_SEQ_CST);
  pthread_mutex_unlock(&w->mu);
  wake_idle();
}

// Makes a task runnable: on this worker's queue, or the global one from a
// thread that isn't a worker.
static void make_runnable(ovt_task *t) {
  ovt_worker *w = cur_worker();
  if (w) push_local(w, t);
  else push_global(t);
}

static ovt_task *pop_queue(pthread_mutex_t *mu, ovt_task **head, ovt_task **tail, int64_t *queued) {
  if (__atomic_load_n(queued, __ATOMIC_SEQ_CST) == 0) return NULL;
  pthread_mutex_lock(mu);
  ovt_task *t = *head;
  if (t) {
    *head = t->next;
    if (!*head) *tail = NULL;
    __atomic_sub_fetch(queued, 1, __ATOMIC_SEQ_CST);
  }
  pthread_mutex_unlock(mu);
  return t;
}

static ovt_task *find_work(ovt_worker *w) {
  ovt_task *t = pop_queue(&w->mu, &w->head, &w->tail, &w->queued);
  if (t) return t;
  t = pop_queue(&global_mu, &global_head, &global_tail, &global_queued);
  if (t) return t;
  for (int k = 1; k < nworkers; k++) {
    ovt_worker *v = &workers[(w->id + k) % nworkers];
    t = pop_queue(&v->mu, &v->head, &v->tail, &v->queued);
    if (t) return t;
  }
  return NULL;
}

static int any_work(void) {
  if (__atomic_load_n(&global_queued, __ATOMIC_SEQ_CST)) return 1;
  for (int k = 0; k < nworkers; k++)
    if (__atomic_load_n(&workers[k].queued, __ATOMIC_SEQ_CST)) return 1;
  return 0;
}

// ---- waiting ----

static void waiter_init(ovt_waiter *w) {
  w->state = W_WAITING;
  w->task = cur_task();
}

// Parks the current task until wake(w) has been called.
static void wait_for(ovt_waiter *w) {
  while (__atomic_load_n(&w->state, __ATOMIC_ACQUIRE) != W_DONE) {
    ovt_worker *wk = cur_worker();
    ovt_task *t = wk->current;
    t->waiter = w;
    t->action = ACT_PARK;
    switch_to_sched(wk, t, 0);
  }
}

static void wake(ovt_waiter *w) {
  ovt_task *t = w->task;
  if (__atomic_exchange_n(&w->state, W_DONE, __ATOMIC_ACQ_REL) == W_PARKED) make_runnable(t);
}

// ---- tasks and groups ----

struct ovt_group {
  int64_t next; // the next index to run, changed atomically
  int64_t n;
  int32_t (*body)(void *ctx, int64_t i);
  void *ctx;
  int failed;        // set by the first failure, so no new indices start
  int64_t first_bad; // the lowest index that failed
  int64_t pending;   // helper tasks still running, plus one for the caller
  ovt_waiter done;   // woken by the last helper to finish
  ovt_cancel *scope; // cancelled by the first failure
};

static ovt_task *task_new(int32_t (*fn)(void *), void *arg, size_t stack_size, ovt_group *g, ovt_cancel *cancel) {
  ovt_task *t = ovt_alloc(sizeof(ovt_task));
  memset(t, 0, sizeof *t);
  t->fn = fn;
  t->arg = arg;
  t->group = g;
  t->cancel = cancel;
  cancel_ref(cancel);
  t->stack_size = stack_size;
  t->stack = stack_new(stack_size);
  uintptr_t top = ((uintptr_t)t->stack + stack_size) & ~(uintptr_t)15;
  t->ctx.r[0] = (uint64_t)t;                   // x19
  t->ctx.r[11] = (uint64_t)&ovt_ctx_start;     // x30, where the first switch returns to
  t->ctx.r[12] = (uint64_t)top;                // sp
#ifdef OVT_TSAN
  t->fiber = __tsan_create_fiber(0);
#endif
  __atomic_add_fetch(&live_tasks, 1, __ATOMIC_RELAXED);
  return t;
}

void ovt_task_main(ovt_task *t) {
#ifdef OVT_ASAN
  __sanitizer_finish_switch_fiber(NULL, NULL, NULL);
#endif
  t->result = t->fn(t->arg);
  ovt_worker *w = cur_worker();
  t->action = ACT_DONE;
  switch_to_sched(w, t, 1);
  __builtin_unreachable();
}

static ovt_task *main_task;
static int main_done; // changed atomically

// What happens once a task has switched out, run on the scheduler's stack.
static void after_switch(ovt_worker *w, ovt_task *t) {
  switch (t->action) {
  case ACT_PARK: {
    int waiting = W_WAITING;
    // Fails if the waiter was woken while the task was switching out.
    if (!__atomic_compare_exchange_n(&t->waiter->state, &waiting, W_PARKED, 0, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) push_local(w, t);
    break;
  }
  case ACT_DONE: {
    __atomic_sub_fetch(&live_tasks, 1, __ATOMIC_RELAXED);
    if (t == main_task) {
      // Worker 0 returns to ovt_rt_run, wherever `main` finished.
      pthread_mutex_lock(&idle_mu);
      __atomic_store_n(&main_done, 1, __ATOMIC_RELEASE);
      pthread_cond_broadcast(&idle_cv);
      pthread_mutex_unlock(&idle_mu);
      break;
    }
    ovt_group *g = t->group;
    cancel_unref(t->cancel);
    stack_free(t->stack, t->stack_size);
#ifdef OVT_TSAN
    __tsan_destroy_fiber(t->fiber);
#endif
    free(t);
    if (g) {
      ovt_waiter *done = &g->done;
      if (__atomic_sub_fetch(&g->pending, 1, __ATOMIC_ACQ_REL) == 0) wake(done);
    }
    break;
  }
  default:
    push_local(w, t);
  }
}

static int main_finished(ovt_worker *w) { return w->id == 0 && __atomic_load_n(&main_done, __ATOMIC_ACQUIRE); }

// Runs tasks until there are none left for a while, then sleeps; returns
// when `main` has finished (only on worker 0).
static void worker_loop(ovt_worker *w) {
  for (;;) {
    if (main_finished(w)) return;
    ovt_task *t = find_work(w);
    if (!t) {
      for (int spin = 0; spin < 100 && !t; spin++) {
        sched_yield();
        t = find_work(w);
      }
    }
    if (!t) {
      pthread_mutex_lock(&idle_mu);
      __atomic_add_fetch(&idle_workers, 1, __ATOMIC_SEQ_CST);
      if (!any_work() && !main_finished(w)) pthread_cond_wait(&idle_cv, &idle_mu);
      __atomic_sub_fetch(&idle_workers, 1, __ATOMIC_SEQ_CST);
      pthread_mutex_unlock(&idle_mu);
      continue;
    }
    w->current = t;
    switch_to_task(w, t);
    w->current = NULL;
    after_switch(w, t);
  }
}

static void signal_stack(void);

static void *worker_thread(void *arg) {
  ovt_worker *w = arg;
  tl_worker = w;
  signal_stack();
#ifdef OVT_TSAN
  w->fiber = __tsan_get_current_fiber();
#endif
#ifdef OVT_ASAN
  pthread_t self = pthread_self();
  size_t size = pthread_get_stacksize_np(self);
  w->stack_bottom = (char *)pthread_get_stackaddr_np(self) - size;
  w->stack_size = size;
#endif
  worker_loop(w);
  return NULL;
}

static pthread_once_t workers_once = PTHREAD_ONCE_INIT;

static void start_workers(void) {
  for (int i = 1; i < nworkers; i++) {
    pthread_t th;
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
    if (pthread_create(&th, &attr, worker_thread, &workers[i]) != 0) {
      fputs("trap: can't start a worker thread\n", stderr);
      _exit(OVT_TRAP_STATUS);
    }
    pthread_attr_destroy(&attr);
  }
}

static void run_indices(ovt_group *g) {
  for (;;) {
    if (__atomic_load_n(&g->failed, __ATOMIC_ACQUIRE)) return;
    int64_t i = __atomic_fetch_add(&g->next, 1, __ATOMIC_RELAXED);
    if (i >= g->n) return;
    if (g->body(g->ctx, i)) {
      if (!__atomic_exchange_n(&g->failed, 1, __ATOMIC_ACQ_REL)) cancel_scope(g->scope);
      int64_t cur = __atomic_load_n(&g->first_bad, __ATOMIC_RELAXED);
      while (i < cur && !__atomic_compare_exchange_n(&g->first_bad, &cur, i, 1, __ATOMIC_RELAXED, __ATOMIC_RELAXED)) {
      }
    }
  }
}

static int32_t group_helper(void *arg) {
  run_indices(arg);
  return 0;
}

// For task.map: a buffer for the results, zeroed status bytes, and cleanup
// after a failure, with errors laid out like the prelude's `Err`.
ovt_buf *ovt_buf_new(int64_t cap, int64_t esize) { return buf_new(cap, esize); }

void *ovt_calloc(int64_t n) {
  void *p = calloc(n > 0 ? (size_t)n : 1, 1);
  if (!p) oom();
  return p;
}

typedef struct {
  int32_t kind;
  ovt_str msg;
} ovt_err;

// Drops the results that were made (status 1) and the errors (status 2)
// other than errs[keep].
void ovt_parallel_cleanup(const char *status, int64_t n, char *results, int64_t usize, ovt_fn1 drop, ovt_err *errs, int64_t keep) {
  for (int64_t i = 0; i < n; i++) {
    if (status[i] == 1 && drop) drop(results + i * usize);
    if (status[i] == 2 && i != keep) ovt_buf_release(errs[i].msg.buf, 1, NULL);
  }
}

// Runs body(ctx, i) for every i in [0, n), spread over up to one task per
// worker, and returns once they have all finished. A body returns nonzero
// when it failed; after a failure no new indices start. Returns -1 when every
// body succeeded, otherwise the lowest index that failed.
int64_t ovt_parallel(int64_t n, int32_t (*body)(void *, int64_t), void *ctx) {
  ovt_group g = {0};
  g.n = n;
  g.body = body;
  g.ctx = ctx;
  g.first_bad = INT64_MAX;
  waiter_init(&g.done);
  int64_t helpers = (n < nworkers ? n : nworkers) - 1;
  ovt_task *self = g.done.task;
  if (!self) helpers = 0;
  g.pending = helpers + 1;
  // A scope of its own, cancelled by the first failure, so the other bodies
  // stop at their next `io` call.
  ovt_cancel *outer = self ? self->cancel : NULL;
  ovt_cancel *scope = cancel_new(outer);
  g.scope = scope;
  if (helpers > 0) {
    pthread_once(&workers_once, start_workers);
    for (int64_t k = 0; k < helpers; k++) make_runnable(task_new(group_helper, &g, STACK_SIZE, &g, scope));
  }
  // The calling task takes part too, in the same scope; if a helper finishes
  // last, it wakes us.
  if (self) self->cancel = scope;
  run_indices(&g);
  if (self) self->cancel = outer;
  if (__atomic_sub_fetch(&g.pending, 1, __ATOMIC_ACQ_REL) > 0) wait_for(&g.done);
  cancel_unref(scope);
  return g.first_bad == INT64_MAX ? -1 : g.first_bad;
}

// ---- the blocking pool ----
//
// Threads that make blocking calls (file IO) for tasks. The task parks until
// its call is done, and its worker runs other tasks meanwhile.

typedef struct ovt_job {
  void (*fn)(void *);
  void *arg;
  ovt_waiter done;
  struct ovt_job *next;
} ovt_job;

enum { POOL_MAX = 64 };
static pthread_mutex_t pool_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t pool_cv = PTHREAD_COND_INITIALIZER;
static ovt_job *pool_head, *pool_tail;
static int pool_threads, pool_idle;

static void *pool_thread(void *arg) {
  (void)arg;
  pthread_mutex_lock(&pool_mu);
  for (;;) {
    while (!pool_head) {
      pool_idle++;
      pthread_cond_wait(&pool_cv, &pool_mu);
      pool_idle--;
    }
    ovt_job *j = pool_head;
    pool_head = j->next;
    if (!pool_head) pool_tail = NULL;
    pthread_mutex_unlock(&pool_mu);
    j->fn(j->arg);
    wake(&j->done);
    pthread_mutex_lock(&pool_mu);
  }
  return NULL;
}

// Runs fn(arg) on the blocking pool while the current task waits. With no
// other task alive, there's nothing else to run, so it just calls fn.
static void blocking(void (*fn)(void *), void *arg) {
  ovt_task *t = cur_task();
  if (!t || __atomic_load_n(&live_tasks, __ATOMIC_RELAXED) <= 1) {
    fn(arg);
    return;
  }
  ovt_job j = {fn, arg, {0, NULL}, NULL};
  waiter_init(&j.done);
  pthread_mutex_lock(&pool_mu);
  if (pool_tail) pool_tail->next = &j;
  else pool_head = &j;
  pool_tail = &j;
  if (pool_idle > 0) {
    pthread_cond_signal(&pool_cv);
  } else if (pool_threads < POOL_MAX) {
    pthread_t th;
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
    if (pthread_create(&th, &attr, pool_thread, NULL) == 0) pool_threads++;
    pthread_attr_destroy(&attr);
  }
  pthread_mutex_unlock(&pool_mu);
  wait_for(&j.done);
}

// ---- the poller ----
//
// One thread runs kqueue for every task that waits on a socket or a timer.
// A waiting task hands the poller a request and parks on a waiter in its own
// frame; the poller wakes it once, with R_READY, R_TIMEOUT or R_CANCELLED.
// Requests belong to the poller from then on, and only its thread touches
// them, so an fd event, a deadline and a cancellation can't both wake a task.

#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
#include <netdb.h>
#include <sys/event.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <time.h>

static void str_from(ovt_str *out, const char *p, int64_t n);

typedef struct preq {
  int fd; // -1 for a plain timer
  int16_t filter;
  int registered; // an fd event is registered with kqueue
  int64_t deadline; // monotonic ns, 0 for none
  ovt_waiter *w; // NULL for a deadline that cancels `expire`
  ovt_cancel *cancel; // the waiting task's scope (a reference)
  ovt_cancel *expire; // for task.timeout: the scope to cancel at the deadline (a reference)
  int done;
  int64_t heap_at; // index in the timer heap, or -1
  struct preq *prev, *next; // the live list
  struct preq *free_next;
} preq;

enum { M_WAIT, M_CANCEL, M_EXPIRE, M_UNEXPIRE };

typedef struct msg {
  int kind;
  preq *req;
  ovt_cancel *scope; // for M_CANCEL and M_UNEXPIRE (a reference)
  struct msg *next;
} msg;

static int kq = -1;
static pthread_once_t poller_once = PTHREAD_ONCE_INIT;
static int poller_started; // changed atomically
static pthread_mutex_t sub_mu = PTHREAD_MUTEX_INITIALIZER;
static msg *sub_head, *sub_tail;
// Only the poller thread touches these.
static preq *live_head;
static preq **heap;
static int64_t heap_len, heap_cap;

static int64_t now_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static void heap_swap(int64_t a, int64_t b) {
  preq *t = heap[a];
  heap[a] = heap[b];
  heap[b] = t;
  heap[a]->heap_at = a;
  heap[b]->heap_at = b;
}

static void heap_up(int64_t i) {
  while (i > 0 && heap[(i - 1) / 2]->deadline > heap[i]->deadline) {
    heap_swap(i, (i - 1) / 2);
    i = (i - 1) / 2;
  }
}

static void heap_down(int64_t i) {
  for (;;) {
    int64_t l = 2 * i + 1, r = l + 1, m = i;
    if (l < heap_len && heap[l]->deadline < heap[m]->deadline) m = l;
    if (r < heap_len && heap[r]->deadline < heap[m]->deadline) m = r;
    if (m == i) return;
    heap_swap(i, m);
    i = m;
  }
}

static void heap_push(preq *r) {
  if (heap_len == heap_cap) {
    heap_cap = heap_cap ? heap_cap * 2 : 64;
    heap = realloc(heap, sizeof(preq *) * (size_t)heap_cap);
    if (!heap) oom();
  }
  r->heap_at = heap_len;
  heap[heap_len++] = r;
  heap_up(r->heap_at);
}

static void heap_remove(preq *r) {
  int64_t i = r->heap_at;
  if (i < 0) return;
  heap_len--;
  if (i != heap_len) {
    heap[i] = heap[heap_len];
    heap[i]->heap_at = i;
    heap_down(i);
    heap_up(i);
  }
  r->heap_at = -1;
}

static void live_push(preq *r) {
  r->prev = NULL;
  r->next = live_head;
  if (live_head) live_head->prev = r;
  live_head = r;
}

static void live_remove(preq *r) {
  if (r->prev) r->prev->next = r->next;
  else if (live_head == r) live_head = r->next;
  if (r->next) r->next->prev = r->prev;
  r->prev = r->next = NULL;
}

// Ends a request and wakes its task. The request is freed after the current
// batch of events, which may still mention it.
static void complete(preq *r, int result, preq **freed) {
  r->done = 1;
  if (r->registered && result != R_READY) {
    struct kevent ev;
    EV_SET(&ev, r->fd, r->filter, EV_DELETE, 0, 0, NULL);
    kevent(kq, &ev, 1, NULL, 0, NULL);
  }
  heap_remove(r);
  live_remove(r);
  if (r->w) {
    r->w->result = result;
    wake(r->w);
  }
  r->free_next = *freed;
  *freed = r;
}

static void cancel_live(preq **freed) {
  for (preq *r = live_head, *next; r; r = next) {
    next = r->next;
    if (is_cancelled(r->cancel)) complete(r, R_CANCELLED, freed);
  }
}

static void drain_submissions(preq **freed) {
  pthread_mutex_lock(&sub_mu);
  msg *m = sub_head;
  sub_head = sub_tail = NULL;
  pthread_mutex_unlock(&sub_mu);
  while (m) {
    msg *next = m->next;
    preq *r = m->req;
    switch (m->kind) {
    case M_WAIT:
      live_push(r);
      if (is_cancelled(r->cancel)) {
        complete(r, R_CANCELLED, freed);
        break;
      }
      if (r->fd >= 0) {
        struct kevent ev;
        EV_SET(&ev, r->fd, r->filter, EV_ADD | EV_ONESHOT, 0, 0, r);
        if (kevent(kq, &ev, 1, NULL, 0, NULL) < 0) {
          // Let the task retry its call and see the error.
          complete(r, R_READY, freed);
          break;
        }
        r->registered = 1;
      }
      if (r->deadline) heap_push(r);
      break;
    case M_CANCEL:
      cancel_live(freed);
      cancel_unref(m->scope);
      break;
    case M_EXPIRE:
      heap_push(r);
      break;
    case M_UNEXPIRE:
      for (int64_t i = 0; i < heap_len; i++) {
        if (heap[i]->expire == m->scope) {
          preq *x = heap[i];
          heap_remove(x);
          x->done = 1;
          x->free_next = *freed;
          *freed = x;
          break;
        }
      }
      cancel_unref(m->scope);
      break;
    }
    free(m);
    m = next;
  }
}

static void *poller_thread(void *arg) {
  (void)arg;
  struct kevent evs[256];
  for (;;) {
    struct timespec ts, *tsp = NULL;
    if (heap_len) {
      int64_t d = heap[0]->deadline - now_ns();
      if (d < 0) d = 0;
      ts.tv_sec = d / 1000000000;
      ts.tv_nsec = d % 1000000000;
      tsp = &ts;
    }
    int n = kevent(kq, NULL, 0, evs, 256, tsp);
    preq *freed = NULL;
    for (int i = 0; i < n; i++) {
      if (evs[i].filter == EVFILT_USER) {
        drain_submissions(&freed);
        continue;
      }
      preq *r = evs[i].udata;
      if (!r->done) {
        r->registered = 0; // one-shot: kqueue has removed it
        complete(r, R_READY, &freed);
      }
    }
    int64_t now = now_ns();
    while (heap_len && heap[0]->deadline <= now) {
      preq *r = heap[0];
      if (r->expire) {
        heap_remove(r);
        r->done = 1;
        __atomic_store_n(&r->expire->timed_out, 1, __ATOMIC_RELEASE);
        __atomic_store_n(&r->expire->cancelled, 1, __ATOMIC_RELEASE);
        cancel_live(&freed);
        r->free_next = freed;
        freed = r;
      } else {
        complete(r, R_TIMEOUT, &freed);
      }
    }
    while (freed) {
      preq *r = freed;
      freed = r->free_next;
      cancel_unref(r->cancel);
      cancel_unref(r->expire);
      free(r);
    }
  }
  return NULL;
}

static void poller_start(void) {
  kq = kqueue();
  if (kq < 0) {
    fputs("trap: can't create a kqueue\n", stderr);
    _exit(OVT_TRAP_STATUS);
  }
  struct kevent ev;
  EV_SET(&ev, 1, EVFILT_USER, EV_ADD | EV_CLEAR, 0, 0, NULL);
  kevent(kq, &ev, 1, NULL, 0, NULL);
  pthread_t th;
  pthread_attr_t attr;
  pthread_attr_init(&attr);
  pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
  if (pthread_create(&th, &attr, poller_thread, NULL) != 0) {
    fputs("trap: can't start the poller thread\n", stderr);
    _exit(OVT_TRAP_STATUS);
  }
  pthread_attr_destroy(&attr);
  __atomic_store_n(&poller_started, 1, __ATOMIC_RELEASE);
}

static void submit(int kind, preq *r, ovt_cancel *scope) {
  pthread_once(&poller_once, poller_start);
  msg *m = ovt_alloc(sizeof(msg));
  m->kind = kind;
  m->req = r;
  m->scope = scope;
  m->next = NULL;
  pthread_mutex_lock(&sub_mu);
  if (sub_tail) sub_tail->next = m;
  else sub_head = m;
  sub_tail = m;
  pthread_mutex_unlock(&sub_mu);
  struct kevent ev;
  EV_SET(&ev, 1, EVFILT_USER, 0, NOTE_TRIGGER, 0, NULL);
  kevent(kq, &ev, 1, NULL, 0, NULL);
}

// Cancels a scope: its tasks' next io calls fail, and those waiting on the
// poller wake up.
static void cancel_scope(ovt_cancel *c) {
  if (!c) return;
  __atomic_store_n(&c->cancelled, 1, __ATOMIC_RELEASE);
  if (__atomic_load_n(&poller_started, __ATOMIC_ACQUIRE)) {
    cancel_ref(c);
    submit(M_CANCEL, NULL, c);
  }
}

static int task_cancelled(void) {
  ovt_task *t = cur_task();
  return t && is_cancelled(t->cancel);
}

// Parks the current task until `fd` is ready for `filter` (or, with fd -1,
// until the deadline), the deadline passes, or its scope is cancelled.
static int wait_io(int fd, int16_t filter, int64_t deadline) {
  ovt_task *t = cur_task();
  if (is_cancelled(t->cancel)) return R_CANCELLED;
  preq *r = ovt_alloc(sizeof(preq));
  memset(r, 0, sizeof *r);
  r->fd = fd;
  r->filter = filter;
  r->deadline = deadline;
  r->heap_at = -1;
  r->cancel = t->cancel;
  cancel_ref(r->cancel);
  ovt_waiter w;
  waiter_init(&w);
  r->w = &w;
  submit(M_WAIT, r, NULL);
  wait_for(&w);
  return w.result;
}

static void raise_file_limit(void) {
  struct rlimit rl;
  if (getrlimit(RLIMIT_NOFILE, &rl) != 0) return;
  rlim_t want = rl.rlim_max;
  int max = 0;
  size_t len = sizeof max;
  if (sysctlbyname("kern.maxfilesperproc", &max, &len, NULL, 0) == 0 && max > 0 && (rlim_t)max < want) want = (rlim_t)max;
  if (want > rl.rlim_cur) {
    rl.rlim_cur = want;
    setrlimit(RLIMIT_NOFILE, &rl);
  }
}

// ---- task locks ----
//
// A lock for tasks: a task that has to wait parks instead of blocking its
// worker thread. Unlocking hands the lock to the first waiter.

typedef struct twait {
  ovt_waiter w;
  struct twait *next;
} twait;

typedef struct {
  pthread_mutex_t mu;
  int locked;
  twait *head, *tail;
} tmutex;

static void tlock(tmutex *m) {
  pthread_mutex_lock(&m->mu);
  if (!m->locked) {
    m->locked = 1;
    pthread_mutex_unlock(&m->mu);
    return;
  }
  twait tw;
  waiter_init(&tw.w);
  tw.next = NULL;
  if (m->tail) m->tail->next = &tw;
  else m->head = &tw;
  m->tail = &tw;
  pthread_mutex_unlock(&m->mu);
  wait_for(&tw.w);
}

static void tunlock(tmutex *m) {
  pthread_mutex_lock(&m->mu);
  twait *n = m->head;
  if (n) {
    m->head = n->next;
    if (!m->head) m->tail = NULL;
    pthread_mutex_unlock(&m->mu);
    wake(&n->w); // still locked: it's n's now
    return;
  }
  m->locked = 0;
  pthread_mutex_unlock(&m->mu);
}

// ---- sockets ----
//
// `net.Conn` and `net.Listener` are shared handles: a struct holding one
// closure-like value whose environment is an ovt_sock, so copies share the
// socket and it closes when the last copy goes. `close` shuts it down at once,
// waking any task waiting on it; the fd itself is closed on the last drop.

typedef struct {
  int64_t rc;
  ovt_fn1 drop;
  ovt_fn1 mark;
  int fd;
  int closed; // changed atomically
  int64_t timeout; // ns a read or write may wait, or 0
  tmutex rmu, wmu;
  char *rbuf; // bytes read but not yet returned
  int64_t rstart, rlen, rcap;
} ovt_sock;

void ovt_mark_none(void *env);

static void sock_free(void *p) {
  ovt_sock *s = p;
  close(s->fd);
  pthread_mutex_destroy(&s->rmu.mu);
  pthread_mutex_destroy(&s->wmu.mu);
  free(s->rbuf);
  free(s);
}

static ovt_sock *sock_new(int fd) {
  fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK);
  fcntl(fd, F_SETFD, FD_CLOEXEC);
  int one = 1;
  setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
  ovt_sock *s = ovt_alloc(sizeof(ovt_sock));
  memset(s, 0, sizeof *s);
  s->rc = 1;
  s->drop = sock_free;
  s->mark = ovt_mark_none;
  s->fd = fd;
  pthread_mutex_init(&s->rmu.mu, NULL);
  pthread_mutex_init(&s->wmu.mu, NULL);
  return s;
}

static void msg_str(ovt_str *err, const char *a, const ovt_str *b, const char *c) {
  str_from(err, a, (int64_t)strlen(a));
  if (b) ovt_str_append_bytes(err, sdata(b), b->len);
  if (c) ovt_str_append_bytes(err, c, (int64_t)strlen(c));
}

// "<what> <addr>: <reason>".
static int32_t net_fail(ovt_str *err, const char *what, const ovt_str *addr, int e) {
  char reason[256];
  strerror_r(e, reason, sizeof reason);
  str_from(err, what, (int64_t)strlen(what));
  if (addr) {
    ovt_str_append_bytes(err, " ", 1);
    ovt_str_append_bytes(err, sdata(addr), addr->len);
  }
  ovt_str_append_bytes(err, ": ", 2);
  ovt_str_append_bytes(err, reason, (int64_t)strlen(reason));
  switch (e) {
  case EADDRINUSE: return K_CONFLICT + 1;
  case EACCES: case EPERM: return K_DENIED + 1;
  case ECONNREFUSED: case EHOSTUNREACH: case ENETUNREACH: case EMFILE: case ENFILE: return K_UNAVAILABLE + 1;
  case ETIMEDOUT: return K_TIMEOUT + 1;
  case EINVAL: return K_INVALID + 1;
  default: return K_IO + 1;
  }
}

static int32_t cancelled_fail(ovt_str *err) {
  str_from(err, "cancelled", 9);
  return K_CANCELLED + 1;
}

// Parses "host:port" or ":port" into an IPv4 address. Host names other than
// "localhost" are looked up on the blocking pool.
typedef struct {
  char host[256];
  const char *port;
  struct sockaddr_in sa;
  int ok;
} resolve_job;

static void run_resolve(void *a) {
  resolve_job *j = a;
  struct addrinfo hints, *res = NULL;
  memset(&hints, 0, sizeof hints);
  hints.ai_family = AF_INET;
  hints.ai_socktype = SOCK_STREAM;
  if (getaddrinfo(j->host, j->port, &hints, &res) == 0 && res) {
    memcpy(&j->sa, res->ai_addr, sizeof j->sa);
    j->ok = 1;
  }
  if (res) freeaddrinfo(res);
}

static int32_t parse_addr(const ovt_str *addr, int listening, struct sockaddr_in *sa, ovt_str *err) {
  char buf[300];
  if (addr->len >= (int64_t)sizeof buf) {
    msg_str(err, "address too long: ", addr, NULL);
    return K_INVALID + 1;
  }
  memcpy(buf, sdata(addr), (size_t)addr->len);
  buf[addr->len] = 0;
  char *colon = strrchr(buf, ':');
  char *end = NULL;
  long port = colon ? strtol(colon + 1, &end, 10) : -1;
  if (!colon || !*(colon + 1) || *end || port < 0 || port > 65535) {
    msg_str(err, "expected an address like \"127.0.0.1:8080\" or \":8080\", got \"", addr, "\"");
    return K_INVALID + 1;
  }
  *colon = 0;
  memset(sa, 0, sizeof *sa);
  sa->sin_family = AF_INET;
  sa->sin_port = htons((uint16_t)port);
  if (!buf[0]) {
    sa->sin_addr.s_addr = htonl(listening ? INADDR_ANY : INADDR_LOOPBACK);
    return 0;
  }
  if (!strcmp(buf, "localhost")) {
    sa->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return 0;
  }
  if (inet_pton(AF_INET, buf, &sa->sin_addr) == 1) return 0;
  resolve_job j;
  memset(&j, 0, sizeof j);
  snprintf(j.host, sizeof j.host, "%s", buf);
  j.port = colon + 1;
  blocking(run_resolve, &j);
  if (!j.ok) {
    msg_str(err, "can't find the address of ", addr, NULL);
    return K_NOT_FOUND + 1;
  }
  *sa = j.sa;
  sa->sin_port = htons((uint16_t)port);
  return 0;
}

int32_t ovt_net_listen(const ovt_str *addr, void **out, ovt_str *err) {
  if (task_cancelled()) return cancelled_fail(err);
  struct sockaddr_in sa;
  int32_t code = parse_addr(addr, 1, &sa, err);
  if (code) return code;
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0) return net_fail(err, "can't listen on", addr, errno);
  int one = 1;
  setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
  if (bind(fd, (struct sockaddr *)&sa, sizeof sa) != 0 || listen(fd, 4096) != 0) {
    int e = errno;
    close(fd);
    return net_fail(err, "can't listen on", addr, e);
  }
  *out = sock_new(fd);
  return 0;
}

int32_t ovt_net_accept(ovt_sock *l, void **out, ovt_str *err) {
  for (;;) {
    if (task_cancelled()) return cancelled_fail(err);
    int fd = accept(l->fd, NULL, NULL);
    if (fd >= 0) {
      int one = 1;
      setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
      *out = sock_new(fd);
      return 0;
    }
    int e = errno;
    if (e == EINTR || e == ECONNABORTED) continue;
    if (e == EAGAIN || e == EWOULDBLOCK) {
      if (__atomic_load_n(&l->closed, __ATOMIC_ACQUIRE)) return net_fail(err, "can't accept: the listener is closed", NULL, EBADF);
      if (wait_io(l->fd, EVFILT_READ, 0) == R_CANCELLED) return cancelled_fail(err);
      continue;
    }
    return net_fail(err, "can't accept a connection", NULL, e);
  }
}

int32_t ovt_net_connect(const ovt_str *addr, void **out, ovt_str *err) {
  if (task_cancelled()) return cancelled_fail(err);
  struct sockaddr_in sa;
  int32_t code = parse_addr(addr, 0, &sa, err);
  if (code) return code;
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0) return net_fail(err, "can't connect to", addr, errno);
  ovt_sock *s = sock_new(fd);
  if (connect(fd, (struct sockaddr *)&sa, sizeof sa) != 0) {
    int e = errno;
    if (e == EINPROGRESS) {
      if (wait_io(fd, EVFILT_WRITE, 0) == R_CANCELLED) {
        sock_free(s);
        return cancelled_fail(err);
      }
      socklen_t len = sizeof e;
      getsockopt(fd, SOL_SOCKET, SO_ERROR, &e, &len);
    }
    if (e) {
      sock_free(s);
      return net_fail(err, "can't connect to", addr, e);
    }
  }
  int one = 1;
  setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
  *out = s;
  return 0;
}

static int64_t deadline_of(ovt_sock *s) { return s->timeout ? now_ns() + s->timeout : 0; }

static int32_t timed_out(ovt_sock *s, ovt_str *err, const char *what) {
  char m[128];
  int n = snprintf(m, sizeof m, "%s: nothing happened for %lld ms", what, (long long)(s->timeout / 1000000));
  str_from(err, m, n);
  return K_TIMEOUT + 1;
}

// Reads more bytes into the buffer. Returns 1 when some arrived, 0 at the end
// of the stream (or a reset), or an error code + 2.
static int32_t fill(ovt_sock *s, ovt_str *err) {
  int64_t deadline = deadline_of(s);
  for (;;) {
    if (s->rstart > 0 && s->rstart == s->rlen) s->rstart = s->rlen = 0;
    if (s->rstart > 0 && s->rlen == s->rcap) {
      memmove(s->rbuf, s->rbuf + s->rstart, (size_t)(s->rlen - s->rstart));
      s->rlen -= s->rstart;
      s->rstart = 0;
    }
    if (s->rlen == s->rcap) {
      s->rcap = s->rcap ? s->rcap * 2 : 4096;
      s->rbuf = realloc(s->rbuf, (size_t)s->rcap);
      if (!s->rbuf) oom();
    }
    if (task_cancelled()) return cancelled_fail(err) + 2;
    ssize_t n = read(s->fd, s->rbuf + s->rlen, (size_t)(s->rcap - s->rlen));
    if (n > 0) {
      s->rlen += n;
      return 1;
    }
    if (n == 0) return 0;
    int e = errno;
    if (e == EINTR) continue;
    if (e == ECONNRESET || e == ENOTCONN || e == EBADF) return 0;
    if (e == EAGAIN || e == EWOULDBLOCK) {
      // An idle connection holds no buffer while it waits.
      if (s->rstart == s->rlen) {
        free(s->rbuf);
        s->rbuf = NULL;
        s->rcap = s->rstart = s->rlen = 0;
      }
      int r = wait_io(s->fd, EVFILT_READ, deadline);
      if (r == R_CANCELLED) return cancelled_fail(err) + 2;
      if (r == R_TIMEOUT) return timed_out(s, err, "read") + 2;
      continue;
    }
    return net_fail(err, "can't read from the connection", NULL, e) + 2;
  }
}

enum { MAX_LINE = 1 << 20 };

// The next line, without "\n" (and a "\r" before it), in `out`; `*got` is 0
// at the end of the stream, where an unfinished line is dropped.
int32_t ovt_net_read_line(ovt_sock *s, ovt_arr *out, int32_t text, int32_t *got, ovt_str *err) {
  tlock(&s->rmu);
  int32_t code = 0;
  *got = 0;
  int64_t scanned = 0;
  for (;;) {
    char *start = s->rbuf ? s->rbuf + s->rstart : NULL;
    int64_t avail = s->rlen - s->rstart;
    char *nl = avail > scanned ? memchr(start + scanned, '\n', (size_t)(avail - scanned)) : NULL;
    if (nl) {
      int64_t n = nl - start;
      int64_t len = n > 0 && start[n - 1] == '\r' ? n - 1 : n;
      out->buf = NULL;
      out->off = 0;
      out->len = 0;
      if (text && !utf8_valid((const unsigned char *)start, len)) {
        str_from(err, "the line isn't valid UTF-8; read it with read_line_bytes", 56);
        code = K_INVALID + 1;
      } else {
        ovt_str_append_bytes(out, start, len);
        *got = 1;
      }
      s->rstart += n + 1;
      break;
    }
    scanned = avail;
    if (avail > MAX_LINE) {
      str_from(err, "the line is longer than 1 MiB", 29);
      code = K_INVALID + 1;
      break;
    }
    int32_t r = fill(s, err);
    if (r == 0) break;
    if (r >= 2) {
      code = r - 2;
      break;
    }
  }
  tunlock(&s->rmu);
  return code;
}

// Some bytes as they arrive; empty at the end of the stream.
int32_t ovt_net_read(ovt_sock *s, ovt_arr *out, ovt_str *err) {
  tlock(&s->rmu);
  int32_t code = 0;
  out->buf = NULL;
  out->off = 0;
  out->len = 0;
  if (s->rlen == s->rstart) {
    int32_t r = fill(s, err);
    if (r >= 2) code = r - 2;
  }
  if (!code && s->rlen > s->rstart) {
    ovt_str_append_bytes(out, s->rbuf + s->rstart, s->rlen - s->rstart);
    s->rstart = s->rlen = 0;
  }
  tunlock(&s->rmu);
  return code;
}

int32_t ovt_net_write(ovt_sock *s, const ovt_arr *data, ovt_str *err) {
  tlock(&s->wmu);
  int32_t code = 0;
  const char *p = sdata(data);
  int64_t left = data->len;
  int64_t deadline = deadline_of(s);
  while (left > 0) {
    if (task_cancelled()) {
      code = cancelled_fail(err);
      break;
    }
    ssize_t n = write(s->fd, p, (size_t)left);
    if (n > 0) {
      p += n;
      left -= n;
      continue;
    }
    int e = errno;
    if (e == EINTR) continue;
    if (e == EAGAIN || e == EWOULDBLOCK) {
      int r = wait_io(s->fd, EVFILT_WRITE, deadline);
      if (r == R_CANCELLED) {
        code = cancelled_fail(err);
        break;
      }
      if (r == R_TIMEOUT) {
        code = timed_out(s, err, "write");
        break;
      }
      continue;
    }
    if (e == EPIPE || e == ECONNRESET || e == ENOTCONN || e == EBADF) {
      str_from(err, "the connection is closed", 24);
      code = K_IO + 1;
    } else {
      code = net_fail(err, "can't write to the connection", NULL, e);
    }
    break;
  }
  tunlock(&s->wmu);
  return code;
}

void ovt_net_set_timeout(ovt_sock *s, int64_t ns) { s->timeout = ns > 0 ? ns : 0; }

void ovt_net_close(ovt_sock *s) {
  if (!__atomic_exchange_n(&s->closed, 1, __ATOMIC_ACQ_REL)) shutdown(s->fd, SHUT_RDWR);
}

void ovt_net_peer(ovt_sock *s, ovt_str *out) {
  struct sockaddr_in sa;
  socklen_t len = sizeof sa;
  char ip[INET_ADDRSTRLEN] = "?";
  char m[64];
  int port = 0;
  if (getpeername(s->fd, (struct sockaddr *)&sa, &len) == 0) {
    inet_ntop(AF_INET, &sa.sin_addr, ip, sizeof ip);
    port = ntohs(sa.sin_port);
  }
  int n = snprintf(m, sizeof m, "%s:%d", ip, port);
  str_from(out, m, n);
}

int64_t ovt_net_port(ovt_sock *s) {
  struct sockaddr_in sa;
  socklen_t len = sizeof sa;
  if (getsockname(s->fd, (struct sockaddr *)&sa, &len) != 0) return 0;
  return ntohs(sa.sin_port);
}

// ---- time ----

// Waits `ns`; returns early if the task is cancelled.
void ovt_time_sleep(int64_t ns) {
  if (ns <= 0) return;
  wait_io(-1, 0, now_ns() + ns);
}

static int64_t start_ns;

int64_t ovt_time_monotonic(void) { return now_ns() - start_ns; }

// ---- channels ----
//
// `Chan[T]` is a shared handle whose environment is an ovt_chan: a bounded
// ring buffer of values, with queues of parked senders and receivers. A value
// is moved in by `send` and out by `recv`; codegen marks it shared first.

typedef struct cwait {
  ovt_waiter w;
  void *slot; // a sender's value, or where a receiver wants it
  int ok;
  struct cwait *next;
} cwait;

typedef struct {
  int64_t rc;
  ovt_fn1 drop;
  ovt_fn1 mark;
  pthread_mutex_t mu;
  int64_t esize;
  ovt_fn1 edrop;
  char *buf; // a ring of `alloc` slots, which grows up to `cap`
  int64_t cap, alloc, head, len;
  int closed;
  cwait *snd_head, *snd_tail, *rcv_head, *rcv_tail;
} ovt_chan;

// Makes room for one more value; the caller has checked len < cap.
static void chan_grow(ovt_chan *c) {
  if (c->len < c->alloc) return;
  int64_t n = c->alloc ? c->alloc * 2 : 8;
  if (n > c->cap) n = c->cap;
  char *nb = ovt_alloc(n * (c->esize > 0 ? c->esize : 1));
  for (int64_t i = 0; i < c->len; i++) memcpy(nb + i * c->esize, c->buf + ((c->head + i) % c->alloc) * c->esize, (size_t)c->esize);
  free(c->buf);
  c->buf = nb;
  c->alloc = n;
  c->head = 0;
}

static void chan_free(void *p) {
  ovt_chan *c = p;
  if (c->edrop)
    for (int64_t i = 0; i < c->len; i++) c->edrop(c->buf + ((c->head + i) % c->alloc) * c->esize);
  pthread_mutex_destroy(&c->mu);
  free(c->buf);
  free(c);
}

void *ovt_chan_new(int64_t cap, int64_t esize, ovt_fn1 edrop) {
  if (cap < 1) cap = 1;
  ovt_chan *c = ovt_alloc(sizeof(ovt_chan));
  memset(c, 0, sizeof *c);
  c->rc = 1;
  c->drop = chan_free;
  c->mark = ovt_mark_none;
  pthread_mutex_init(&c->mu, NULL);
  c->esize = esize;
  c->edrop = edrop;
  c->cap = cap;
  return c;
}

static void cq_push(cwait **head, cwait **tail, cwait *w) {
  w->next = NULL;
  if (*tail) (*tail)->next = w;
  else *head = w;
  *tail = w;
}

static cwait *cq_pop(cwait **head, cwait **tail) {
  cwait *w = *head;
  if (w) {
    *head = w->next;
    if (!*head) *tail = NULL;
  }
  return w;
}

// Moves the value at `v` into the channel, waiting while it's full. On
// failure (closed, or cancelled) the value is dropped.
int32_t ovt_chan_send(ovt_chan *c, void *v, ovt_str *err) {
  if (task_cancelled()) {
    if (c->edrop) c->edrop(v);
    return cancelled_fail(err);
  }
  pthread_mutex_lock(&c->mu);
  if (!c->closed) {
    cwait *r = cq_pop(&c->rcv_head, &c->rcv_tail);
    if (r) {
      memcpy(r->slot, v, (size_t)c->esize);
      r->ok = 1;
      pthread_mutex_unlock(&c->mu);
      wake(&r->w);
      return 0;
    }
    if (c->len < c->cap) {
      chan_grow(c);
      memcpy(c->buf + ((c->head + c->len) % c->alloc) * c->esize, v, (size_t)c->esize);
      c->len++;
      pthread_mutex_unlock(&c->mu);
      return 0;
    }
    cwait me;
    waiter_init(&me.w);
    me.slot = v;
    me.ok = 0;
    cq_push(&c->snd_head, &c->snd_tail, &me);
    pthread_mutex_unlock(&c->mu);
    wait_for(&me.w);
    if (me.ok) return 0;
  } else {
    pthread_mutex_unlock(&c->mu);
  }
  if (c->edrop) c->edrop(v);
  str_from(err, "the channel is closed", 21);
  return K_UNAVAILABLE + 1;
}

// Moves the next value into `out` and returns 1, waiting while the channel is
// empty; returns 0 once it's closed and empty.
int32_t ovt_chan_recv(ovt_chan *c, void *out) {
  pthread_mutex_lock(&c->mu);
  if (c->len > 0) {
    memcpy(out, c->buf + c->head * c->esize, (size_t)c->esize);
    c->head = (c->head + 1) % c->alloc;
    c->len--;
    cwait *s = cq_pop(&c->snd_head, &c->snd_tail);
    if (s) {
      memcpy(c->buf + ((c->head + c->len) % c->alloc) * c->esize, s->slot, (size_t)c->esize);
      c->len++;
      s->ok = 1;
    }
    pthread_mutex_unlock(&c->mu);
    if (s) wake(&s->w);
    return 1;
  }
  if (c->closed) {
    pthread_mutex_unlock(&c->mu);
    return 0;
  }
  cwait me;
  waiter_init(&me.w);
  me.slot = out;
  me.ok = 0;
  cq_push(&c->rcv_head, &c->rcv_tail, &me);
  pthread_mutex_unlock(&c->mu);
  wait_for(&me.w);
  return me.ok;
}

// Closes the channel: waiting receivers get `none`, and senders fail.
void ovt_chan_close(ovt_chan *c) {
  pthread_mutex_lock(&c->mu);
  c->closed = 1;
  cwait *rs = c->rcv_head, *ss = c->snd_head;
  c->rcv_head = c->rcv_tail = c->snd_head = c->snd_tail = NULL;
  pthread_mutex_unlock(&c->mu);
  while (rs) {
    cwait *n = rs->next;
    wake(&rs->w);
    rs = n;
  }
  while (ss) {
    cwait *n = ss->next;
    wake(&ss->w);
    ss = n;
  }
}

int64_t ovt_chan_len(ovt_chan *c) {
  pthread_mutex_lock(&c->mu);
  int64_t n = c->len;
  pthread_mutex_unlock(&c->mu);
  return n;
}

// ---- Shared ----
//
// `Shared[T]` is a shared handle whose environment is an ovt_shared holding
// the value. `lock s as v { ... }` takes a task lock (a waiting task parks,
// and the holder may move between threads), and unlocking marks everything
// the value reaches shared again, since the block may have put new values
// into it or copied values out of it.

typedef struct {
  int64_t rc;
  ovt_fn1 drop;
  ovt_fn1 mark;
  tmutex mu;
  ovt_task *owner; // changed atomically
  ovt_fn1 vdrop, vmark;
  _Alignas(16) char value[];
} ovt_shared;

static void shared_free(void *p) {
  ovt_shared *s = p;
  if (s->vdrop) s->vdrop(s->value);
  pthread_mutex_destroy(&s->mu.mu);
  free(s);
}

void *ovt_shared_new(int64_t size, ovt_fn1 vdrop, ovt_fn1 vmark) {
  ovt_shared *s = ovt_alloc((int64_t)sizeof(ovt_shared) + (size > 0 ? size : 1));
  memset(s, 0, sizeof *s);
  s->rc = 1;
  s->drop = shared_free;
  s->mark = ovt_mark_none;
  pthread_mutex_init(&s->mu.mu, NULL);
  s->vdrop = vdrop;
  s->vmark = vmark;
  return s;
}

void *ovt_shared_value(ovt_shared *s) { return s->value; }

void ovt_shared_lock(ovt_shared *s, const char *file, int32_t line, int32_t col) {
  ovt_task *t = cur_task();
  if (t && __atomic_load_n(&s->owner, __ATOMIC_ACQUIRE) == t) {
    static const char m[] = "this task already holds this lock";
    ovt_trap(m, sizeof m - 1, file, line, col);
  }
  tlock(&s->mu);
  __atomic_store_n(&s->owner, t, __ATOMIC_RELEASE);
}

void ovt_shared_unlock(ovt_shared *s) {
  if (s->vmark) s->vmark(s->value);
  __atomic_store_n(&s->owner, NULL, __ATOMIC_RELEASE);
  tunlock(&s->mu);
}

// ---- task groups ----
//
// `task.group(|g| ...)` runs the body in the calling task; `g.spawn(f)`
// starts a task running the closure `f`. The group waits for all of them.
// If the body fails, the group's scope is cancelled first, so the spawned
// tasks stop at their next io call.

typedef struct {
  int64_t rc;
  ovt_fn1 drop;
  ovt_fn1 mark;
  int64_t pending; // spawned tasks running, plus one for the body; atomic
  ovt_waiter done;
  ovt_cancel *scope;
  int finished;
} ovt_tgroup;

static void tgroup_free(void *p) {
  ovt_tgroup *g = p;
  cancel_unref(g->scope);
  free(g);
}

void *ovt_group_new(void) {
  ovt_tgroup *g = ovt_alloc(sizeof(ovt_tgroup));
  memset(g, 0, sizeof *g);
  g->rc = 1;
  g->drop = tgroup_free;
  g->mark = ovt_mark_none;
  g->pending = 1;
  waiter_init(&g->done);
  ovt_task *t = cur_task();
  g->scope = cancel_new(t ? t->cancel : NULL);
  return g;
}

typedef struct {
  void (*fn)(void *);
  void *env;
  ovt_tgroup *g;
} spawn_arg;

void ovt_env_release(int64_t *env);

static int32_t spawned_main(void *a) {
  spawn_arg sa = *(spawn_arg *)a;
  free(a);
  sa.fn(sa.env);
  ovt_env_release(sa.env);
  // The group outlives this: task.group holds it until everything is done.
  if (__atomic_sub_fetch(&sa.g->pending, 1, __ATOMIC_ACQ_REL) == 0) wake(&sa.g->done);
  return 0;
}

// Starts a task running the closure {fn, env}; takes over a reference to env.
void ovt_group_spawn(ovt_tgroup *g, void (*fn)(void *), void *env, const char *file, int32_t line, int32_t col) {
  if (__atomic_load_n(&g->finished, __ATOMIC_ACQUIRE)) {
    static const char m[] = "this task group has already finished";
    ovt_trap(m, sizeof m - 1, file, line, col);
  }
  __atomic_add_fetch(&g->pending, 1, __ATOMIC_ACQ_REL);
  spawn_arg *a = ovt_alloc(sizeof(spawn_arg));
  a->fn = fn;
  a->env = env;
  a->g = g;
  pthread_once(&workers_once, start_workers);
  make_runnable(task_new(spawned_main, a, STACK_SIZE, NULL, g->scope));
}

void ovt_group_wait(ovt_tgroup *g, int32_t body_failed) {
  if (body_failed) cancel_scope(g->scope);
  if (__atomic_sub_fetch(&g->pending, 1, __ATOMIC_ACQ_REL) > 0) wait_for(&g->done);
  __atomic_store_n(&g->finished, 1, __ATOMIC_RELEASE);
}

// ---- task.timeout ----

typedef struct {
  int32_t (*body)(void *);
  void *ctx;
  int32_t result;
  ovt_waiter done;
} timeout_arg;

static int32_t timeout_main(void *p) {
  timeout_arg *a = p;
  a->result = a->body(a->ctx);
  wake(&a->done);
  return 0;
}

// Runs body(ctx) in a child task, in a scope that's cancelled after `ns`.
// Returns 0 if it succeeded, 1 if it failed, and 2 if the deadline passed
// first, whatever the body did after being cancelled.
int32_t ovt_timeout(int64_t ns, int32_t (*body)(void *), void *ctx) {
  ovt_task *self = cur_task();
  ovt_cancel *scope = cancel_new(self ? self->cancel : NULL);
  timeout_arg a;
  a.body = body;
  a.ctx = ctx;
  a.result = 0;
  waiter_init(&a.done);
  preq *r = ovt_alloc(sizeof(preq));
  memset(r, 0, sizeof *r);
  r->fd = -1;
  r->deadline = now_ns() + (ns > 0 ? ns : 0);
  r->heap_at = -1;
  r->expire = scope;
  cancel_ref(scope);
  submit(M_EXPIRE, r, NULL);
  pthread_once(&workers_once, start_workers);
  make_runnable(task_new(timeout_main, &a, STACK_SIZE, NULL, scope));
  wait_for(&a.done);
  cancel_ref(scope);
  submit(M_UNEXPIRE, NULL, scope);
  int32_t res = __atomic_load_n(&scope->timed_out, __ATOMIC_ACQUIRE) ? 2 : a.result == 0 ? 0 : 1;
  cancel_unref(scope);
  return res;
}

// ---- stack overflow ----

static void on_fault(int sig, siginfo_t *info, void *uc) {
  (void)uc;
  ovt_task *t = cur_task();
  char *a = info->si_addr;
  if (t && a >= t->stack && a < t->stack + page_size) {
    static const char msg[] = "trap: stack overflow (recursion too deep?)\n";
    fflush(stdout);
    write(2, msg, sizeof msg - 1);
    _exit(OVT_TRAP_STATUS);
  }
  signal(sig, SIG_DFL); // not ours: crash as usual
}

// Gives the calling thread a stack for signal handlers, since the fault
// handler can't run on a stack that has just overflowed.
static void signal_stack(void) {
#if !defined(OVT_ASAN) && !defined(OVT_TSAN)
  stack_t ss;
  ss.ss_size = 64 * 1024;
  ss.ss_sp = mmap(NULL, ss.ss_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
  ss.ss_flags = 0;
  if (ss.ss_sp != MAP_FAILED) sigaltstack(&ss, NULL);
#endif
}

// ---- starting ----

// Runs fn in the main task and returns its result, once the main task has
// finished. Called by `main` and by each test's child process.
int32_t ovt_rt_run(int32_t (*fn)(void)) {
  page_size = (size_t)getpagesize();
  int n = 0;
  const char *env = getenv("OVT_WORKERS");
  if (env) n = atoi(env);
  if (n <= 0) n = (int)sysconf(_SC_NPROCESSORS_ONLN);
  if (n <= 0) n = 1;
  nworkers = n;
  workers = ovt_alloc((int64_t)sizeof(ovt_worker) * n);
  memset(workers, 0, sizeof(ovt_worker) * (size_t)n);
  for (int i = 0; i < n; i++) {
    workers[i].id = i;
    pthread_mutex_init(&workers[i].mu, NULL);
  }
#if !defined(OVT_ASAN) && !defined(OVT_TSAN)
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_sigaction = on_fault;
  sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
  sigaction(SIGSEGV, &sa, NULL);
  sigaction(SIGBUS, &sa, NULL);
#endif
  ovt_worker *w = &workers[0];
  tl_worker = w;
  signal_stack();
#ifdef OVT_TSAN
  w->fiber = __tsan_get_current_fiber();
#endif
#ifdef OVT_ASAN
  pthread_t self = pthread_self();
  size_t size = pthread_get_stacksize_np(self);
  w->stack_bottom = (char *)pthread_get_stackaddr_np(self) - size;
  w->stack_size = size;
#endif
  start_ns = now_ns();
  // Writing to a closed socket fails with EPIPE instead of killing the process.
  signal(SIGPIPE, SIG_IGN);
  raise_file_limit();
  main_task = task_new((int32_t(*)(void *))fn, NULL, MAIN_STACK_SIZE, NULL, NULL);
  push_local(w, main_task);
  worker_loop(w);
  return main_task->result;
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
  char reason[256];
  strerror_r(e, reason, sizeof reason);
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

static int32_t fs_read_now(const ovt_str *path, ovt_arr *out, int32_t want_text, ovt_str *err) {
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

static int32_t read_stdin_now(ovt_arr *out, int32_t want_text, ovt_str *err) {
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

static int32_t fs_write_now(const ovt_str *path, const ovt_str *data, ovt_str *err) {
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

static int32_t fs_exists_now(const ovt_str *path) {
  char p[4096];
  if (path->len >= (int64_t)sizeof p) return 0;
  memcpy(p, sdata(path), (size_t)path->len);
  p[path->len] = 0;
  struct stat st;
  return stat(p, &st) == 0;
}

// Copies `path` into `p` as a C string; returns 0 if it's too long.
static int c_path(char *p, size_t size, const ovt_str *path) {
  if (path->len >= (int64_t)size) return 0;
  memcpy(p, sdata(path), (size_t)path->len);
  p[path->len] = 0;
  return 1;
}

// fs.walk and fs.list: entries laid out like `fs.Entry`, {path: str, kind: fs.Kind}.
typedef struct {
  ovt_str path;
  int32_t kind;
} ovt_entry;

enum { KIND_FILE, KIND_DIR, KIND_LINK, KIND_OTHER };

static void entry_drop(void *e) { ovt_buf_release(((ovt_entry *)e)->path.buf, 1, NULL); }

static int entry_cmp(const void *a, const void *b) {
  const ovt_str *x = &((const ovt_entry *)a)->path, *y = &((const ovt_entry *)b)->path;
  int64_t n = x->len < y->len ? x->len : y->len;
  int c = memcmp(sdata(x), sdata(y), (size_t)n);
  if (c) return c;
  return x->len < y->len ? -1 : x->len > y->len;
}

// Lists `dir` + "/" + `rel` into `out`, with paths `rel` + "/" + name (or just
// name when `rel` is empty). When `deep`, also lists every directory found.
static int32_t list_dir(const ovt_str *dir, int deep, ovt_arr *out, ovt_str *err) {
  // Directories still to list, as indexes into `out`; -1 is `dir` itself.
  int64_t *todo = ovt_alloc(8 * 16);
  int64_t ntodo = 0, captodo = 16;
  todo[ntodo++] = -1;
  int32_t code = 0;
  while (ntodo && !code) {
    int64_t at = todo[--ntodo];
    ovt_str rel = {0};
    if (at >= 0) rel = ((ovt_entry *)(DATA(out->buf)))[at].path;
    ovt_str full = {0};
    ovt_str_append(&full, dir);
    if (at >= 0) {
      if (full.len == 0 || sdata(&full)[full.len - 1] != '/') ovt_str_append_bytes(&full, "/", 1);
      ovt_str_append_bytes(&full, sdata(&rel), rel.len);
    }
    char p[4096];
    DIR *d = NULL;
    if (!c_path(p, sizeof p, &full)) errno = ENAMETOOLONG;
    else d = opendir(p);
    if (!d) {
      int e = errno;
      err_msg(err, "can't read", &full, e);
      code = kind_of_errno(e) + 1;
      ovt_buf_release(full.buf, 1, NULL);
      break;
    }
    struct dirent *de;
    while ((de = readdir(d))) {
      const char *name = de->d_name;
      if (!strcmp(name, ".") || !strcmp(name, "..")) continue;
      int kind;
      switch (de->d_type) {
      case DT_REG: kind = KIND_FILE; break;
      case DT_DIR: kind = KIND_DIR; break;
      case DT_LNK: kind = KIND_LINK; break;
      case DT_UNKNOWN: {
        char q[4096];
        struct stat st;
        int n = snprintf(q, sizeof q, "%s/%s", p, name);
        if (n >= (int)sizeof q || lstat(q, &st) != 0) kind = KIND_OTHER;
        else kind = S_ISREG(st.st_mode) ? KIND_FILE : S_ISDIR(st.st_mode) ? KIND_DIR : S_ISLNK(st.st_mode) ? KIND_LINK : KIND_OTHER;
        break;
      }
      default: kind = KIND_OTHER;
      }
      ovt_str path = {0};
      if (at >= 0) {
        // `rel` may have moved when `out` grew; read it again.
        ovt_str r = ((ovt_entry *)(DATA(out->buf)))[at].path;
        ovt_str_append_bytes(&path, sdata(&r), r.len);
        ovt_str_append_bytes(&path, "/", 1);
      }
      ovt_str_append_bytes(&path, name, (int64_t)strlen(name));
      ovt_entry *slot = ovt_arr_push(out, sizeof(ovt_entry), NULL, NULL);
      slot->path = path;
      slot->kind = kind;
      if (deep && kind == KIND_DIR) {
        if (ntodo == captodo) {
          captodo *= 2;
          todo = realloc(todo, (size_t)(8 * captodo));
          if (!todo) oom();
        }
        todo[ntodo++] = out->len - 1;
      }
    }
    closedir(d);
    ovt_buf_release(full.buf, 1, NULL);
  }
  free(todo);
  if (code) {
    ovt_buf_release(out->buf, sizeof(ovt_entry), entry_drop);
    out->buf = NULL;
    out->len = 0;
    return code;
  }
  if (out->len > 1) qsort(DATA(out->buf), (size_t)out->len, sizeof(ovt_entry), entry_cmp);
  return 0;
}

// Wrappers that run each file operation on the blocking pool.

typedef struct {
  const ovt_str *path, *data;
  ovt_arr *out;
  ovt_str *err;
  int32_t flag, result;
} fs_job;

static void run_read(void *a) {
  fs_job *j = a;
  j->result = fs_read_now(j->path, j->out, j->flag, j->err);
}
static void run_stdin(void *a) {
  fs_job *j = a;
  j->result = read_stdin_now(j->out, j->flag, j->err);
}
static void run_write(void *a) {
  fs_job *j = a;
  j->result = fs_write_now(j->path, j->data, j->err);
}
static void run_exists(void *a) {
  fs_job *j = a;
  j->result = fs_exists_now(j->path);
}
static void run_list(void *a) {
  fs_job *j = a;
  j->result = list_dir(j->path, j->flag, j->out, j->err);
}

int32_t ovt_fs_read(const ovt_str *path, ovt_arr *out, int32_t want_text, ovt_str *err) {
  if (task_cancelled()) return cancelled_fail(err);
  fs_job j = {path, NULL, out, err, want_text, 0};
  blocking(run_read, &j);
  return j.result;
}

int32_t ovt_read_stdin(ovt_arr *out, int32_t want_text, ovt_str *err) {
  if (task_cancelled()) return cancelled_fail(err);
  fs_job j = {NULL, NULL, out, err, want_text, 0};
  blocking(run_stdin, &j);
  return j.result;
}

int32_t ovt_fs_write(const ovt_str *path, const ovt_str *data, ovt_str *err) {
  if (task_cancelled()) return cancelled_fail(err);
  fs_job j = {path, data, NULL, err, 0, 0};
  blocking(run_write, &j);
  return j.result;
}

int32_t ovt_fs_exists(const ovt_str *path) {
  fs_job j = {path, NULL, NULL, NULL, 0, 0};
  blocking(run_exists, &j);
  return j.result;
}

// `deep` is 1 for fs.walk and 0 for fs.list.
int32_t ovt_fs_list(const ovt_str *path, int32_t deep, ovt_arr *out, ovt_str *err) {
  out->buf = NULL;
  out->off = 0;
  out->len = 0;
  if (task_cancelled()) return cancelled_fail(err);
  fs_job j = {path, NULL, out, err, deep, 0};
  blocking(run_list, &j);
  return j.result;
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
    int32_t status = ovt_rt_run(fn);
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
