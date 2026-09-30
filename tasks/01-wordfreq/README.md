# 01: wordfreq

`wordfreq <file> <n>` prints the `n` most frequent words in a text file.

## Behavior

- A word is a run of ASCII letters (`a`–`z` and `A`–`Z`). Every other byte separates words, including digits, punctuation and bytes of non-ASCII characters.
- Words are compared without regard to case, and printed in lowercase.
- Output is one line per word, `<count> <word>`, most frequent first. Words with the same count are in alphabetical order.
- If the file has fewer than `n` distinct words, print all of them. `n` may be 0.

## Errors

Print a message to stderr, print nothing to stdout, and exit with status 1 when:
- there aren't exactly two arguments
- `n` isn't a non-negative integer
- the file can't be read

## Testing

`python3 tests/run.py <command>` runs the program with `<command>` on each test case.
