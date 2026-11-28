The logic of a todo list

Write `todo.helix`, the state of a todo-list app, as a module exporting four functions. It
has no user interface; the app calls these.

- `init()` — the state with no todos.
- `update(state, msg)` — the state after a message. A message is a record with a `kind`:
  - `{kind: "add", text: "buy milk"}` adds a todo, not done, at the end. Its text is
    trimmed of surrounding whitespace; if nothing is left, the message is ignored. Its `id`
    is a number one higher than the last id given out (the first todo is 1); ids are never
    reused, even after a todo is removed.
  - `{kind: "toggle", id: 3}` flips whether todo 3 is done.
  - `{kind: "remove", id: 3}` removes todo 3.
  - `{kind: "clear_done"}` removes every todo that is done.
  - A `toggle` or `remove` for an id that does not exist changes nothing.
- `visible(state, filter)` — the todos to show, in the order they were added, as records
  `{id, text, done}`. `filter` is `"all"`, `"active"` (not done) or `"done"`.
- `summary(state)` — the footer text: `"3 items left"`, counting todos not done, and
  `"1 item left"` when there is exactly one.
