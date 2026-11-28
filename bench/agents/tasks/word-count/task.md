A command-line word counter

Write `words.helix`, a command-line tool:

    helix run words.helix --path notes.txt --top 3

prints the `--top` most frequent words in the file named by `--path`, one per line, as the
word, a space, and its count:

    the 12
    and 7
    helix 7

- A word is a run of the letters a–z, after the text is lowercased; everything else
  (digits, punctuation, apostrophes, whitespace) separates words. So `Don't` is the two
  words `don` and `t`.
- Most frequent first; words with the same count in alphabetical order.
- `--top` is optional and defaults to 10. If the file has fewer distinct words, print them
  all.
- `helix run words.helix --help` should describe the two options.

`notes.txt` is a sample to try it on.
