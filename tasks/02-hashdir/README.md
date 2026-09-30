# 02: hashdir

`hashdir <dir>` prints the SHA-256 hash of every file under a directory, in the format of `shasum -a 256`.

## Behavior

- Every regular file under `<dir>` is listed, at any depth, including hidden ones (names starting with `.`). Directories aren't listed. Symbolic links aren't followed and aren't listed, whether they point to a file, a directory or nothing.
- There's one line per file: the hash as 64 lowercase hex digits, two spaces, then the file's path.
- The path is `<dir>` exactly as given, then `/`, then the file's path inside `<dir>`. If `<dir>` already ends in `/`, no second `/` is added.
- Lines are sorted by path, comparing bytes, the way `LC_ALL=C sort` does.
- Files are hashed in parallel, so a large tree takes less time on more cores.
- Implement SHA-256 yourself, following FIPS 180-4. Don't use a library or another program for it.
- File names in the tests are valid UTF-8 and contain no newlines or backslashes.

For example, for a directory `photos` holding `b.txt` and `a/c.txt`, `hashdir photos` prints:

```
<hash of c.txt>  photos/a/c.txt
<hash of b.txt>  photos/b.txt
```

## Errors

Print a message to stderr, print nothing to stdout, and exit with status 1 when:
- there isn't exactly one argument
- `<dir>` doesn't exist or isn't a directory
- a file or directory under `<dir>` can't be read

## Testing

`python3 tests/run.py <command>` runs the program with `<command>` on each test case.
