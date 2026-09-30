# 05: todo on sqlite

The todo API of task 04, with the todos kept in a sqlite database, so they survive a restart.

`todo <port>` serves the same HTTP/1.1 JSON API on `127.0.0.1:<port>`, with the same endpoints, errors and HTTP behavior; task 04's README describes it. The differences:

- The todos are stored in the sqlite database `todos.db` in the current directory, created if it doesn't exist.
- A change is saved before its response is sent, so a todo the server has confirmed survives the server being killed.
- Ids keep going up across restarts: a new todo never gets the id of a todo that existed before, even one that was deleted before the restart.

## Testing

`python3 tests/run.py <command>` runs task 04's tests against `<command>` in a fresh directory, then restarts the server to check what's saved.
