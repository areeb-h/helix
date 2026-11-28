Write the tests for a slug function

`slug.helix` exports `slugify(title)`, which turns a title into the part of a URL that names
a page. Its rules are in the comment above it:

- lowercase;
- every run of characters that are not a letter a–z or a digit 0–9 becomes a single `-`;
- no `-` at the start or the end;
- at most 40 characters: a longer slug is cut to 40, and then any `-` left at the end is
  removed.

Write `slug_test.helix`, the tests for it. They will be run against this `slug.helix`, which
is correct, and against several copies of it that each break one of the rules: a good test
file passes on the first and fails on every one of the others. Do not change `slug.helix`.
