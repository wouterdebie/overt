# 01: jsonfmt

`jsonfmt` reads one JSON document from stdin, checks that it's valid, and prints it pretty-printed to stdout.

## Output

- Indent with two spaces per level.
- A non-empty array or object has one element per line. An object member is `"key": value`. Elements are separated by `,` at the end of the line.
- Empty arrays and objects print as `[]` and `{}`.
- Object members keep their input order. Duplicate keys are kept.
- Strings and numbers print exactly as they appear in the input, including escapes and exponents.
- The output ends with a newline.

For example, `{"a":[1, 2.50,{}], "b" : "x\ty", "c": []}` prints as:

```
{
  "a": [
    1,
    2.50,
    {}
  ],
  "b": "x\ty",
  "c": []
}
```

## Validity

The input is valid when it's exactly one JSON value (RFC 8259), optionally surrounded by whitespace.
- **Whitespace:** space, tab, newline and carriage return.
- **Strings:** no raw bytes below 0x20. The only escapes are `\"` `\\` `\/` `\b` `\f` `\n` `\r` `\t` and `\u` followed by exactly four hex digits. Other bytes, including non-ASCII ones, are taken as they are.
- **Numbers:** an optional `-`, then `0` or a digit 1–9 followed by digits, then optionally `.` and one or more digits, then optionally `e` or `E`, an optional `+` or `-`, and one or more digits.
- **Literals:** `true`, `false` and `null`.
- **Nesting:** documents nested 1,000 levels deep must work.

## Errors

On invalid input:
- print `error: line L, column C: <message>` to stderr, where the message is up to you
- print nothing to stdout
- exit with status 1

`L` and `C` are 1-based and give the first byte that can't continue a valid JSON document, or the end of the input if the input stops early. Lines are separated by `\n`, and columns count bytes.

Examples:
- `[1,]` fails at column 4, the `]`.
- `{"a" 1}` fails at column 6.
- `01` fails at column 2, because `0` is a complete number.
- `"abc` fails at column 5, the end of the input.

## Testing

`python3 tests/run.py <command>` runs the program with `<command>` on each test case.
