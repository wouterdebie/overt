# 03: chat

`chat <port> <idle-seconds>` is a line-based chat server on `127.0.0.1:<port>`. Clients pick a nickname, join rooms, and talk to everyone in the same room.

## Protocol

Every line in both directions ends in `\n`; a `\r` before it is ignored. A line from a client is either a command, starting with `/`, or a message.

- A client starts in the room `lobby`, without a nickname.
- `/nick <name>` sets the nickname. A name is 1 to 16 ASCII letters, digits or `_`, and no other connected client may have it; comparisons are exact. The server replies `ok`, or `error: <reason>`. If the client already had a nickname and is in a room, everyone else in that room gets `* <old> is now <new>`.
- `/join <room>` moves the client to another room, with the same rules for the name as nicknames. It needs a nickname. The server replies `ok`. Everyone else in the old room gets `* <nick> left`, and everyone already in the new room gets `* <nick> joined`. Joining the room the client is in just replies `ok`.
- `/quit` closes the connection, as below.
- Any other line starting with `/` gets `error: unknown command`.
- A message goes to everyone else in the client's room as `<nick>: <text>`, where `<text>` is the whole line. The sender gets nothing back. Without a nickname, the server replies `error: set a nickname first` instead. Empty lines are ignored.
- When a client disconnects or quits, everyone else in its room gets `* <nick> left` (if it had a nickname), and its nickname becomes free.
- A client that sends nothing for `<idle-seconds>` seconds is disconnected, the same way.
- Setting your nickname for the first time doesn't announce anything.

Everything a client is sent arrives in the order the server handled it, and messages from one client arrive at every other client in the order they were sent. A client that stops reading must not stop the server from serving the others. It may be disconnected.

The error reasons are up to you; the tests only check the `error: ` prefix.

## Errors

If there aren't exactly two arguments, or the port isn't a number from 1 to 65535, or `<idle-seconds>` isn't a positive integer, or the server can't listen on the port, print a message to stderr and exit with status 1.

## Testing

`python3 tests/run.py <command>` starts the server with `<command> <port> <idle-seconds>` and runs the test cases against it.
