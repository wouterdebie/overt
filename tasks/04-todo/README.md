# 04: todo

`todo <port>` is an HTTP/1.1 JSON API on `127.0.0.1:<port>` that keeps a list of todos in memory.

## Todos

A todo is `{"id": 1, "title": "buy milk", "done": false}`. Ids start at 1 and go up by one for each todo created; they're never reused, even after a delete.

## Endpoints

- `POST /todos` creates a todo from `{"title": ..., "done": ...}`, where `done` is optional and defaults to `false`. It replies `201` with the new todo.
- `GET /todos` replies `200` with an array of all todos, in id order. With `?done=true` or `?done=false`, only those.
- `GET /todos/<id>` replies `200` with the todo.
- `PATCH /todos/<id>` changes the fields given in `{"title": ..., "done": ...}`, both optional, and replies `200` with the updated todo.
- `DELETE /todos/<id>` deletes the todo and replies `204` with no body.

Titles are stored without leading and trailing spaces. In a request body, a field that's `null` counts as missing, and other fields are ignored.

## Errors

Errors reply with `{"error": "<message>"}`; the message is up to you. A request that fails changes nothing.

- `400` when:
  - the body isn't a JSON object
  - `title` is missing from a `POST`
  - `title` isn't a string of 1 to 200 characters (Unicode code points) once leading and trailing spaces are removed
  - `done` isn't `true` or `false`
  - the query is something other than `done=true` or `done=false`
- `404` when:
  - there's no todo with that id; an id that isn't a positive integer counts as missing
  - the path is something else
- `405` for another method on `/todos` or `/todos/<id>`.

## HTTP

- Requests have a `Content-Length` body, or none. Header names are case-insensitive.
- Responses have `Content-Type: application/json`, except `204`s, which have no body.
- Connections stay open between requests: HTTP/1.1 does this by default, and HTTP/1.0 does it with `Connection: keep-alive`, which the response then repeats. A request with `Connection: close` has its connection closed after the response, and so does an HTTP/1.0 request without `keep-alive`.
- A client may send its next request before the response to the last one arrives. Responses come back in order.
- A request that isn't valid HTTP gets a `400`, or has its connection closed.
- Many clients may use the server at once.

If there isn't exactly one argument, or the port isn't a number from 1 to 65535, or the server can't listen on it, print a message to stderr and exit with status 1.

## Testing

`python3 tests/run.py <command>` starts the server with `<command> <port>` and runs the test cases against it.
