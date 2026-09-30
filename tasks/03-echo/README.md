# 03: echo

`echo <port>` is a TCP server on `127.0.0.1:<port>` that sends back every line it receives.

## Behavior

- It serves any number of clients at the same time, and a slow or silent client doesn't hold up the others.
- Each line a client sends, ending in `\n`, comes back to that client unchanged, `\n` included, in the order sent. Lines can be any bytes except `\n`, up to 64 KiB.
- A line may arrive in several pieces, or several lines in one piece; what counts is the `\n`.
- When a client closes its side, the server drops any unfinished line and closes the connection.
- The server keeps running until it's killed.

## Errors

If there isn't exactly one argument, or the port isn't a number from 1 to 65535, or the server can't listen on it, print a message to stderr and exit with status 1.

## Testing

`python3 tests/run.py <command>` starts the server with `<command> <port>` and runs the test cases against it.
