//! Regular expressions (`re_match`, `re_find_all`, `re_replace`, `re_captures`,
//! `re_split`), behind the `regex` cargo feature — on by default.
//!
//! **A regex cannot hang your program, and that is not an accident.** Rust's `regex`
//! crate uses finite automata, not backtracking, so matching is **linear in the length of
//! the input** and there is no pattern/input pair that blows up exponentially. Python,
//! JavaScript, Java and PCRE can all be hung by a crafted pattern — the ReDoS class —
//! which for a language that serves HTTP means a user-supplied string can take the
//! process down. ADR 0024 says user input never aborts the host; a backtracking engine
//! would contradict that in the one place it matters most, so this engine is a
//! requirement rather than a convenience. The cost is that backreferences and lookaround
//! are unsupported: they are what make backtracking necessary, and they are the price of
//! the guarantee.
//!
//! **Why the names say `re_`.** `contains`, `replace`, `split` and `index_of` already
//! exist and take LITERAL text. Whether `.` means "any character" or "a dot" is not a
//! detail a reader should have to infer from which overload they picked, so the regex
//! family is named for what it is. Every call site says which language it is written in,
//! and `helix search re_` lists the family.
//!
//! **Patterns are compiled once and cached.** A regex in a loop that recompiles per
//! iteration is the classic silent 100× — the exact "pit of failure" shape a field report
//! spent a section on. The cache is keyed by the pattern text and is pure memoization:
//! the same pattern always yields the same automaton, so nothing observable depends on
//! whether a compile was reused.

use crate::error::HelixError;
use crate::value::Value;
#[cfg(feature = "regex")]
use std::rc::Rc;

/// Compile `pattern`, reusing an earlier compilation of the same text.
///
/// An invalid pattern is a Helix error naming what the engine objected to — the crate's
/// own message is precise (it points into the pattern) and reproducing it badly would be
/// worse than passing it through.
#[cfg(feature = "regex")]
fn compiled(pattern: &str, line: usize, col: usize) -> Result<Rc<regex::Regex>, HelixError> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static CACHE: RefCell<HashMap<String, Rc<regex::Regex>>> = RefCell::new(HashMap::new());
    }
    CACHE.with(|c| {
        if let Some(r) = c.borrow().get(pattern) {
            return Ok(Rc::clone(r));
        }
        let re = regex::Regex::new(pattern).map_err(|e| {
            HelixError::new(format!("invalid regular expression: {e}"), line, col).hint(
                "backreferences and lookaround are not supported — they are what make a regex \
                 able to hang, and this engine is linear-time by construction."
                    .to_string(),
            )
        })?;
        let re = Rc::new(re);
        c.borrow_mut().insert(pattern.to_string(), Rc::clone(&re));
        Ok(re)
    })
}

/// Dispatch one `re_*` String method. `s` is the receiver.
#[cfg(feature = "regex")]
pub fn method(
    s: &str,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let want = |n: usize| -> Result<(), HelixError> {
        if args.len() == n {
            Ok(())
        } else {
            Err(HelixError::new(
                format!("`{name}` expects {n} argument{}, got {}", if n == 1 { "" } else { "s" }, args.len()),
                line,
                col,
            ))
        }
    };
    let pattern = |i: usize| -> Result<&str, HelixError> {
        match args.get(i) {
            Some(Value::Str(p)) => Ok(p.as_str()),
            other => Err(HelixError::new(
                format!(
                    "`{name}` needs a pattern string, got {}",
                    other.map_or("nothing".to_string(), |v| crate::value::with_article(v.type_name()).to_string())
                ),
                line,
                col,
            )),
        }
    };
    let str_val = |t: &str| Value::Str(Rc::new(t.to_string()));

    match name {
        "re_match" => {
            want(1)?;
            let re = compiled(pattern(0)?, line, col)?;
            Ok(Value::Bool(re.is_match(s)))
        }
        "re_find" => {
            want(1)?;
            let re = compiled(pattern(0)?, line, col)?;
            // `missing` for "no match", the shape every other absent-lookup uses
            // (ADR 0001) — `index_of` and `split_once` already answer this way.
            Ok(re.find(s).map_or(Value::Missing, |m| str_val(m.as_str())))
        }
        "re_find_all" => {
            want(1)?;
            let re = compiled(pattern(0)?, line, col)?;
            pieces_guard(name, s, || re.find_iter(s).count(), line, col)?;
            Ok(Value::array(re.find_iter(s).map(|m| str_val(m.as_str())).collect()))
        }
        "re_replace" => {
            want(2)?;
            let re = compiled(pattern(0)?, line, col)?;
            let Some(Value::Str(to)) = args.get(1) else {
                return Err(HelixError::new(
                    format!("`{name}`'s replacement must be a string"),
                    line,
                    col,
                )
                .hint("group references are `$1`, `$2`, … — `$$` for a literal dollar.".to_string()));
            };
            // EVERY occurrence, matching the literal `replace` beside it. `replace_first`
            // is the literal family's opt-out and a `re_replace_first` can join it later;
            // silently differing from its neighbour would be the worse default.
            //
            // The result can pass the string cap — `s.re_replace("a", "aa")` in a loop
            // doubles — and its size is not known until each match is expanded. So this is
            // `Regex::replace_all`'s own loop (`find_iter` for a literal template,
            // `captures_iter` + `expand` otherwise), building into a buffer that is held to
            // the cap and grows fallibly: one comparison per piece, where the crate's
            // version would grow without asking until the allocator aborted the process.
            //
            // A LITERAL template's result is decided before anything is built: its worst
            // case (every boundary a match) settles ordinary calls in O(1), and only one
            // whose worst case passes the cap counts its matches (one more regex pass) to
            // size the result exactly — refusing it, or reserving exactly that much.
            use crate::interp::growth::push;
            let literal = !to.contains('$');
            let mut size = s.len();
            if literal && s.len().saturating_add((s.len() + 1).saturating_mul(to.len())) > crate::interp::MAX_STRING_LEN {
                let (hits, matched) =
                    re.find_iter(s).fold((0usize, 0usize), |(n, m), x| (n + 1, m + x.len()));
                size = (s.len() - matched).saturating_add(hits.saturating_mul(to.len()));
                crate::interp::growth::check_text(name, size, line, col)?;
            }
            let mut out = crate::interp::growth::text_buffer(name, size, line, col)?;
            let mut last = 0;
            if literal {
                for m in re.find_iter(s) {
                    push(name, &mut out, &s[last..m.start()], line, col)?;
                    push(name, &mut out, to, line, col)?;
                    last = m.end();
                }
            } else {
                let mut piece = String::new();
                for caps in re.captures_iter(s) {
                    let Some(m) = caps.get(0) else { continue };
                    push(name, &mut out, &s[last..m.start()], line, col)?;
                    piece.clear();
                    caps.expand(to, &mut piece);
                    push(name, &mut out, &piece, line, col)?;
                    last = m.end();
                }
            }
            push(name, &mut out, &s[last..], line, col)?;
            Ok(Value::Str(Rc::new(out)))
        }
        "re_captures" => {
            want(1)?;
            let re = compiled(pattern(0)?, line, col)?;
            Ok(match re.captures(s) {
                None => Value::Missing,
                Some(c) => Value::array(
                    c.iter()
                        // Group 0 is the whole match, so the array reads exactly like the
                        // pattern: `[whole, first, second, …]`. A group that did not
                        // participate is `missing` rather than "" — an empty capture and
                        // an absent one are different answers (ADR 0001).
                        .map(|m| m.map_or(Value::Missing, |m| str_val(m.as_str())))
                        .collect(),
                ),
            })
        }
        "re_split" => {
            want(1)?;
            let re = compiled(pattern(0)?, line, col)?;
            pieces_guard(name, s, || re.split(s).count(), line, col)?;
            Ok(Value::array(re.split(s).map(str_val).collect()))
        }
        _ => Err(HelixError::new(format!("`{name}` is not a regex method"), line, col)),
    }
}

/// The array-size guard for `re_find_all`/`re_split`: one fresh string per piece. The
/// pieces never hold more text than the input, and there are at most `len + 1` of them, so
/// an input under ~22 MB needs no count at all; only a larger one runs `count` (one more
/// pass of the regex) to see whether it really passes the budget.
#[cfg(feature = "regex")]
fn pieces_guard(
    name: &str,
    s: &str,
    count: impl FnOnce() -> usize,
    line: usize,
    col: usize,
) -> Result<(), HelixError> {
    use crate::interp::growth::{STR_ELEMENT, check_array};
    let size = |n: usize| n.saturating_mul(STR_ELEMENT).saturating_add(s.len());
    if size(s.len() + 1) <= crate::value::MATERIALIZE_BUDGET {
        return Ok(());
    }
    check_array(name, size(count()), line, col)
}

/// The error a regex method answers with in a build without the `regex` feature — ADR
/// 0032's gate-the-body shape: the method still exists, type-checks and describes itself.
#[cfg(not(feature = "regex"))]
pub fn method(
    s: &str,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let _ = (s, args);
    Err(HelixError::new(format!("this build has no regex support, so `{name}` cannot run"), line, col)
        .hint("rebuild without `--no-default-features`, or with `--features regex`."))
}

/// Is `name` one of the regex String methods?
pub fn is_regex_method(name: &str) -> bool {
    matches!(
        name,
        "re_match" | "re_find" | "re_find_all" | "re_replace" | "re_captures" | "re_split"
    )
}
