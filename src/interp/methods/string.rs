//! String methods — moved verbatim from the one-file methods module (2026-08-24).

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;
#[allow(unused_imports)]
use std::rc::Rc;

pub(crate) fn string_method(
    s: &Rc<String>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    // Arity check; methods that take arguments call it with their own count.
    let arity = |n: usize| -> Result<(), HelixError> {
        if args.len() != n {
            return Err(HelixError::new(
                format!(
                    "`{}` expects {} argument{}, got {}",
                    name,
                    n,
                    if n == 1 { "" } else { "s" },
                    args.len()
                ),
                line,
                col,
            ));
        }
        Ok(())
    };
    match name {
        "upper" => {
            arity(0)?;
            Ok(Value::Str(Rc::new(s.to_uppercase())))
        }
        "lower" => {
            arity(0)?;
            Ok(Value::Str(Rc::new(s.to_lowercase())))
        }
        "count" | "length" => {
            arity(0)?;
            Ok(Value::Int(s.chars().count() as i64))
        }
        // `chars()` — the string as an array of one-character strings, so the whole array
        // vocabulary (`map`/`filter`/`reduce`/`enumerate`) applies to text. Without it the
        // linear-time spelling was `s.replace("", "\t").split("\t")`, which is both
        // undiscoverable and WRONG — it yields `["", "a", "b", "c", ""]`, with an empty
        // string at each end. The obvious `s[i]` walk is quadratic (each index counts
        // scalars from the start), so the only correct spelling was also the hidden one.
        //
        // Unicode SCALARS, matching `count`/`length`/`reverse`/`take`/`drop`, which all
        // measure the same unit. Not bytes, and not grapheme clusters (which need a table
        // and would disagree with every other method here).
        "chars" => {
            arity(0)?;
            Ok(Value::array(
                s.chars().map(|c| Value::Str(Rc::new(c.to_string()))).collect::<Vec<_>>(),
            ))
        }
        "reverse" => {
            arity(0)?;
            Ok(Value::Str(Rc::new(s.chars().rev().collect())))
        }
        "trim" => {
            arity(0)?;
            Ok(Value::Str(Rc::new(s.trim().to_string())))
        }
        // Text layout: `repeat` builds separators (`"-".repeat(64)`); `ljust`/`rjust`/
        // `center` pad to a width with spaces — for *computed* widths (the `{x:20}`
        // format spec only takes a literal width), e.g. aligning a column to the
        // longest label. Padding measures Unicode scalar count, like the format spec.
        // `take(n)` / `drop(n)` — first n characters / all but the first n, mirroring the
        // Array methods (slicing `s[a:b]` also works; these are the prefix shorthand).
        // Counted by Unicode scalar, so they're correct on non-ASCII text.
        "take" | "drop" => {
            arity(1)?;
            let n = match &args[0] {
                Value::Int(n) if *n >= 0 => *n as usize,
                Value::Int(_) => {
                    return Err(HelixError::new(format!("`{name}` needs a non-negative count"), line, col))
                }
                other => return Err(type_err(name, "an integer count", other, line, col)),
            };
            let out: String = if name == "take" {
                s.chars().take(n).collect()
            } else {
                s.chars().skip(n).collect()
            };
            Ok(Value::Str(Rc::new(out)))
        }
        "repeat" => {
            arity(1)?;
            let n = match &args[0] {
                Value::Int(n) if *n >= 0 => *n as usize,
                Value::Int(_) => {
                    return Err(HelixError::new("`repeat` needs a non-negative count", line, col))
                }
                other => return Err(type_err("repeat", "an integer count", other, line, col)),
            };
            if s.len().saturating_mul(n) > crate::interp::MAX_STRING_LEN {
                return Err(HelixError::new(
                    format!("`repeat` would exceed {} bytes", crate::interp::MAX_STRING_LEN),
                    line,
                    col,
                )
                .hint("use a smaller count."));
            }
            Ok(Value::Str(Rc::new(s.repeat(n))))
        }
        "ljust" | "rjust" | "center" => {
            arity(1)?;
            const MAX_PAD: usize = 1 << 20;
            let width = match &args[0] {
                Value::Int(w) if *w >= 0 && (*w as usize) <= MAX_PAD => *w as usize,
                Value::Int(w) if *w < 0 => {
                    return Err(HelixError::new(format!("`{name}` needs a non-negative width"), line, col))
                }
                Value::Int(_) => {
                    return Err(HelixError::new(format!("`{name}` width is too large (max {MAX_PAD})"), line, col))
                }
                other => return Err(type_err(name, "an integer width", other, line, col)),
            };
            let len = s.chars().count();
            if len >= width {
                return Ok(Value::Str(s.clone()));
            }
            let fill = width - len;
            let padded = match name {
                "ljust" => format!("{s}{}", " ".repeat(fill)),
                "rjust" => format!("{}{s}", " ".repeat(fill)),
                _ => {
                    let l = fill / 2;
                    format!("{}{s}{}", " ".repeat(l), " ".repeat(fill - l))
                }
            };
            Ok(Value::Str(Rc::new(padded)))
        }
        "split" => {
            arity(1)?;
            let sep = str_arg(args, 0, name, line, col)?;
            if sep.is_empty() {
                return Err(HelixError::new("`split` separator cannot be empty", line, col)
                    .hint("split on a non-empty string, e.g. `s.split(\",\")`."));
            }
            window_count_guard("split", s.matches(sep).count() + 1, line, col)?;
            let parts: Vec<Value> =
                s.split(sep).map(|p| Value::Str(Rc::new(p.to_string()))).collect();
            Ok(Value::array(parts))
        }
        // Where a needle starts, as a CHARACTER index — the unit every other String
        // method counts in (`take`, `drop`, `chars`, `s[a:b]`), not the byte offset
        // the underlying search returns. Answering in bytes would be a silent trap on
        // any non-ASCII input, since the index is meant to be fed straight back to
        // `take`/`drop`. `missing` when absent, exactly like an array's `index_of`.
        // An empty needle answers 0, matching its neighbour `contains("")` — the two
        // ask the same question, so they agree.
        // `index_of(needle, from?)` — `from` RESUMES the search at a character index.
        //
        // Without it no search can continue past its last hit, which is the innermost
        // loop of every parser: a field report called it "the most common operation in
        // any parser" and had to re-slice the string on each step to work around it,
        // turning a linear scan into a quadratic one.
        //
        // `from` is a CHARACTER index, like everything else this method touches — it is
        // fed by the previous answer, so a byte offset here would silently mislocate the
        // moment any multi-byte character appeared earlier in the string.
        // The regex family lives in its own module: a different pattern LANGUAGE, and
        // keeping it separate is what stops a literal method and a regex one from ever
        // sharing a name.
        n if crate::regexes::is_regex_method(n) => crate::regexes::method(s, n, args, line, col),
        "index_of" => {
            if args.is_empty() || args.len() > 2 {
                return Err(HelixError::new(
                    format!("`{name}` expects 1 or 2 arguments, got {}", args.len()),
                    line,
                    col,
                )
                .hint("`s.index_of(needle)` searches from the start; `s.index_of(needle, from)` resumes at a character index.".to_string()));
            }
            let needle = str_arg(args, 0, name, line, col)?;
            let from = match args.get(1) {
                None => 0usize,
                Some(Value::Int(i)) if *i >= 0 => *i as usize,
                // A negative offset is a mistake, not a from-the-end convention: `take`
                // and `drop` do not have one either, so inventing it here would make the
                // string methods disagree with each other.
                Some(Value::Int(_)) => {
                    return Err(HelixError::new(
                        format!("`{name}`'s starting index cannot be negative"),
                        line,
                        col,
                    ));
                }
                Some(other) => return Err(type_err(name, "an Int index", other, line, col)),
            };
            // Character offset -> byte offset. Past the end is not an error: a search
            // that has run off the end simply finds nothing, which is what a parser loop
            // wants as its termination condition.
            let Some((start_byte, _)) = s.char_indices().nth(from).or_else(|| {
                (from == s.chars().count()).then_some((s.len(), '\0'))
            }) else {
                return Ok(Value::Missing);
            };
            Ok(match s[start_byte..].find(needle) {
                // The answer stays an ABSOLUTE character index, so it can be fed back in
                // as the next `from` without the caller tracking an origin.
                Some(byte) => Value::Int(s[..start_byte + byte].chars().count() as i64),
                None => Value::Missing,
            })
        }
        // `char_at(i)` — the i-th character, without building the whole char array.
        //
        // `s.chars()[i]` allocates a Vec of one-character strings for the entire string
        // to read one of them, which is what made a character-at-a-time scan quadratic in
        // a field report. This walks to `i` instead: O(i), not O(1) — UTF-8 has no
        // constant-time character index without a side table, and claiming O(1) here
        // would be a lie the docs then repeat. The win is the allocation, not the walk.
        "char_at" => {
            arity(1)?;
            let i = match &args[0] {
                Value::Int(i) if *i >= 0 => *i as usize,
                Value::Int(_) => {
                    return Err(HelixError::new(
                        format!("`{name}`'s index cannot be negative"),
                        line,
                        col,
                    ));
                }
                other => return Err(type_err(name, "an Int index", other, line, col)),
            };
            // Past the end answers `missing`, matching every other absent-lookup in the
            // language (ADR 0001) rather than raising.
            Ok(match s.chars().nth(i) {
                Some(c) => Value::Str(Rc::new(c.to_string())),
                None => Value::Missing,
            })
        }
        // The LAST occurrence — a CHARACTER index exactly like `index_of` (a byte
        // index would silently mislocate anything past a multi-byte character).
        "last_index_of" => {
            arity(1)?;
            let needle = str_arg(args, 0, name, line, col)?;
            Ok(match s.rfind(needle) {
                Some(byte) => Value::Int(s[..byte].chars().count() as i64),
                None => Value::Missing,
            })
        }
        // `split` at the FIRST separator only, keeping the rest of the tail intact:
        // `(before, after)`, or `missing` when the separator does not occur. The
        // spelling this replaces was `let eq = part.split("="), k = eq[0], v = if
        // eq.count() <= 1 then "" else part.drop(k.count() + 1)` — split everything,
        // discard the rest, then recover the tail by arithmetic on the first part's
        // length, which is easy to get wrong by one. An empty separator is refused
        // exactly as `split` refuses it: same argument role, same rule.
        "split_once" => {
            arity(1)?;
            let sep = str_arg(args, 0, name, line, col)?;
            if sep.is_empty() {
                return Err(HelixError::new("`split_once` separator cannot be empty", line, col)
                    .hint("split on a non-empty string, e.g. `s.split_once(\"=\")`."));
            }
            Ok(match s.split_once(sep) {
                Some((a, b)) => Value::Tuple(Rc::new(vec![
                    Value::Str(Rc::new(a.to_string())),
                    Value::Str(Rc::new(b.to_string())),
                ])),
                None => Value::Missing,
            })
        }
        // The sibling of `Array.concat`. Interpolation ("{a}{b}") remains the everyday
        // way to build a string; this exists so that a verb which joins two sequences
        // means the same thing on both sequence types, which is the whole of the
        // surprise the review reported.
        "concat" => {
            arity(1)?;
            let other = str_arg(args, 0, name, line, col)?;
            let mut out = String::with_capacity(s.len() + other.len());
            out.push_str(s);
            out.push_str(other);
            Ok(Value::Str(Rc::new(out)))
        }
        "replace" => {
            arity(2)?;
            let from = str_arg(args, 0, name, line, col)?;
            let to = str_arg(args, 1, name, line, col)?;
            Ok(Value::Str(Rc::new(s.replace(from, to))))
        }
        // `replace` swaps EVERY occurrence; this swaps exactly the first — the
        // pair every string library ends up needing both halves of.
        "replace_first" => {
            arity(2)?;
            let from = str_arg(args, 0, name, line, col)?;
            let to = str_arg(args, 1, name, line, col)?;
            Ok(Value::Str(Rc::new(s.replacen(from, to, 1))))
        }
        "contains" => {
            arity(1)?;
            Ok(Value::Bool(s.contains(str_arg(args, 0, name, line, col)?)))
        }
        "starts_with" => {
            arity(1)?;
            Ok(Value::Bool(s.starts_with(str_arg(args, 0, name, line, col)?)))
        }
        "ends_with" => {
            arity(1)?;
            Ok(Value::Bool(s.ends_with(str_arg(args, 0, name, line, col)?)))
        }
        "phred" => {
            // Decode a FASTQ Phred+33 quality string to per-base integer quality
            // scores (each character's ASCII value minus 33, the Sanger/Illumina-1.8+
            // encoding). Composes with the array verbs — `read.qual.phred().mean()`
            // is a read's mean quality; `read.qual` is `missing` propagates here too.
            arity(0)?;
            let mut scores: Vec<i64> = Vec::with_capacity(s.len());
            for (i, b) in s.bytes().enumerate() {
                if !(33..=126).contains(&b) {
                    return Err(HelixError::new(
                        format!(
                            "`phred` found a non-quality byte {b} at position {i}; a Phred+33 \
                             quality string uses the printable characters '!' (0) through '~' (93)"
                        ),
                        line,
                        col,
                    ));
                }
                scores.push((b - 33) as i64);
            }
            Ok(Value::int_array(scores))
        }
        "parse_json" => {
            arity(0)?;
            crate::json::parse(s).map_err(|e| HelixError::new(e, line, col))
        }
        // `"3.14".to_float()` / `"42".to_int()`: parse a numeric string. Same impl as the
        // free functions `to_float`/`to_int`, so both spellings agree.
        "to_float" => {
            arity(0)?;
            crate::interp::builtins::parse_str_float(s, line, col)
        }
        "to_int" => {
            arity(0)?;
            crate::interp::builtins::parse_str_int(s, line, col)
        }
        // `text.write_to(path)` / `append_to(path)`: the receiver is the text and the
        // argument is the path (the reverse of the underlying `writers` arg order).
        "write_to" | "append_to" => {
            arity(1)?;
            // Capability gate (ADR 0021): writing text to a path is `FsWrite` authority.
            crate::capability::gate_method(name, args, line, col)?;
            let path = args[0].clone();
            let a = vec![path, Value::Str(s.clone())];
            if name == "write_to" {
                crate::writers::write_text(&a, line, col)
            } else {
                crate::writers::append_text(&a, line, col)
            }
        }
        _ => Err(unknown_method(
            "String",
            name,
            &crate::registry::methods_of(crate::registry::STRING_METHODS),
            line,
            col,
        )),
    }
}

