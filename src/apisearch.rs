//! `helix search <term>` — find a capability by what it DOES, not by a name you already
//! know.
//!
//! **The gap this closes is this project's most-repeated lesson.** `helix doc <name>` and
//! `helix describe <name>` both need the name; `helix describe` alone dumps 120 KB. So the
//! only way to answer "is there anything in here for repeated HTTP headers?" was to dump
//! the catalog and grep it — which is exactly what two separate field reports did, and
//! what this project's costliest recorded mistake was months of *not* doing (building
//! around a "missing" `scan` that `helix doc Array` printed all along).
//!
//! A search over names alone would not have helped either of them: the words they had
//! were "repeated header" and "group by", while the names they needed were `get_all` and
//! `frequencies`. So this searches the **doc text and the notes** as well as names and
//! signatures — the prose is where the intent lives — and the **language forms** in
//! `syntaxdocs`, because syntax has no name to look up at all.
//!
//! ## Two rules the first version got wrong, both found by using it
//!
//! **A query is matched TERM BY TERM, not as one substring.** `repeated` found 2 rows and
//! `header` found 15, but `repeated header` found *nothing*, because the phrase appears
//! nowhere verbatim. For a command whose whole premise is "describe what you want", that
//! is the wrong failure: the more a reader says about their problem, the fewer answers
//! they got. Now every term must match somewhere in the entry, and the score is the sum,
//! so extra words *narrow* the result rather than eliminating it.
//!
//! **A term matches at a WORD BOUNDARY, not anywhere inside a word.** `helix search raw`
//! returned four rows — `Array.choice`, `Array.sample`, `randn`, `random_int` — every one
//! of them a hit inside "d**raw**n at random", while the raw-string form the reader wanted
//! was not there at all. Requiring the match to start a word (or follow a `_`, `.`, space
//! or punctuation) kills that entire class while keeping the prefix search that makes
//! `head` find `headers` and `re_` list the regex family. It is a prefix-at-boundary test,
//! not whole-word equality, because a reader types the stem they remember.
//!
//! Ranking stays deliberately crude and explainable: an exact name beats a name prefix,
//! which beats the signature, which beats the prose. Nothing is fuzzy — a row can always
//! say why it is there, and `--json` names the field.

use crate::{docs, registry, syntaxdocs};

/// One catalog entry that matched, with why.
pub struct Hit {
    /// `builtin`, `syntax`, or the receiver type for a method.
    pub owner: &'static str,
    pub name: &'static str,
    pub sig: String,
    pub doc: String,
    pub effect: &'static str,
    /// Which field(s) matched — shown so a surprising row explains itself.
    pub matched: String,
    score: u16,
}

/// Does `term` occur in `hay` at the start of a word?
///
/// A word starts at the beginning of the text or after any non-alphanumeric character, so
/// `all` is found in `get_all` and `match` in `re_match`, while `raw` is NOT found in
/// `drawn`. Both strings are already lowercased by the caller.
fn starts_a_word(hay: &str, term: &str) -> bool {
    if term.is_empty() {
        return false;
    }
    let mut from = 0usize;
    while let Some(i) = hay[from..].find(term) {
        let at = from + i;
        // `at == 0` is a boundary; otherwise the preceding CHARACTER decides. Splitting on
        // alphanumeric rather than on whitespace is what lets `_`, `.`, `(` and `-` all
        // count as separators, which matters because half this corpus is identifiers.
        if !hay[..at].chars().next_back().is_some_and(char::is_alphanumeric) {
            return true;
        }
        // Advance one CHARACTER, not one byte: the doc prose contains `µ`, `—` and `×`.
        from = at + hay[at..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Does `term` match `hay`, ignoring a plural `s` on the term?
///
/// The catalog writes one noun and a reader types the other — "count occurrences" against
/// a note that says "occurrence", "repeated headers" against "header". Trying the
/// singular is the smallest rule that closes that, and it stays explainable: a trailing
/// `s` (or `es`) is dropped only when the full term found nothing, and only when enough
/// stem is left that the shortening is not a wildcard.
fn matches_term(hay: &str, term: &str) -> bool {
    if starts_a_word(hay, term) {
        return true;
    }
    if term.strip_suffix("es").filter(|s| s.len() >= 4).is_some_and(|stem| starts_a_word(hay, stem)) {
        return true;
    }
    term.strip_suffix('s').filter(|s| s.len() >= 4).is_some_and(|stem| starts_a_word(hay, stem))
}

/// Score one catalog entry against every term, or `None` if any term is missing.
///
/// The AND across terms is the point: a reader who says more about their problem should
/// get a shorter list, not an empty one.
fn score(terms: &[String], q: &str, name: &str, sig: &str, doc: &str, notes: &str) -> Option<(u16, String)> {
    let lname = name.to_lowercase();
    let (lsig, ldoc, lnotes) = (sig.to_lowercase(), doc.to_lowercase(), notes.to_lowercase());
    let mut total = 0u16;
    let mut fields: Vec<&'static str> = Vec::new();
    for t in terms {
        let (points, field) = if lname == *t {
            (6, "name")
        } else if matches_term(&lname, t) {
            (4, "name")
        } else if matches_term(&lsig, t) {
            (2, "signature")
        } else if matches_term(&ldoc, t) {
            (1, "doc")
        // Notes carry the caveats — "last-wins", "wire order and repeats are kept" — and
        // the vocabulary a form does not contain ("switch" on `match`, "null" on
        // `missing`), which is often the exact word a reader arrives with.
        } else if matches_term(&lnotes, t) {
            (1, "notes")
        } else {
            return None;
        };
        total += points;
        if !fields.contains(&field) {
            fields.push(field);
        }
    }
    // The whole query naming an entry outright is the strongest signal there is.
    if lname == q {
        total += 5;
    }
    Some((total, fields.join(" and the ")))
}

/// Every builtin, method and language form matching `query`, best first.
pub fn search(query: &str) -> Vec<Hit> {
    let q = query.to_lowercase();
    let terms: Vec<String> = q.split_whitespace().map(str::to_string).collect();
    let mut hits: Vec<Hit> = Vec::new();
    if terms.is_empty() {
        return hits;
    }

    for b in registry::BUILTINS {
        let d = docs::builtin_doc(b.path);
        let (sig, doc, notes) = d.map_or(("", "", ""), |d| (d.sig, d.doc, d.notes));
        if let Some((score, matched)) = score(&terms, &q, b.path, sig, doc, notes) {
            hits.push(Hit {
                owner: "builtin",
                name: b.path,
                sig: if sig.is_empty() { format!("{}(…)", b.path) } else { sig.to_string() },
                doc: doc.to_string(),
                effect: crate::capability::effect_of(b.path).label(),
                matched,
                score,
            });
        }
    }

    for (ty, methods) in registry::type_method_tables() {
        for m in methods {
            let d = docs::method_doc(ty, m);
            let (sig, doc, notes) = d.map_or(("", "", ""), |d| (d.sig, d.doc, d.notes));
            if let Some((score, matched)) = score(&terms, &q, m, sig, doc, notes) {
                hits.push(Hit {
                    owner: ty,
                    name: m,
                    sig: if sig.is_empty() { format!("{m}(…)") } else { sig.to_string() },
                    doc: doc.to_string(),
                    effect: crate::capability::method_effect_of(m).label(),
                    matched,
                    score,
                });
            }
        }
    }

    // Syntax LAST to build but not last to rank: a language form scores like anything
    // else, so `helix search match` puts the form above `re_match` on the exact-name
    // bonus, which is the right answer to that query.
    for s in syntaxdocs::SYNTAX {
        if let Some((score, matched)) = score(&terms, &q, s.name, s.form, s.doc, s.notes) {
            hits.push(Hit {
                owner: "syntax",
                name: s.name,
                sig: s.form.to_string(),
                doc: s.doc.to_string(),
                effect: "pure",
                matched,
                score,
            });
        }
    }

    // Best first; ties by owner then name so the order is stable between runs — an
    // unstable listing makes a diff of two searches unreadable.
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.owner.cmp(b.owner)).then(a.name.cmp(b.name)));
    hits
}

/// The human listing.
pub fn render(query: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!(
            "no match for `{query}`\n\n\
             `helix search` looks at names, signatures, docs, notes and the language forms.\n\
             Every word has to match — try fewer words, or plainer ones (\"header\", \"group\", \"random\").\n\
             `helix doc <Type>` lists one type's methods.\n"
        );
    }
    let mut s = format!("{} match{} for `{query}`\n\n", hits.len(), if hits.len() == 1 { "" } else { "es" });
    for h in hits {
        // The receiver is part of how you CALL it, so it leads: `Array.frequencies()` is
        // usable as printed, where a bare `frequencies()` is not. A language form has no
        // receiver and is shown as written.
        let call = match h.owner {
            "builtin" | "syntax" => h.sig.clone(),
            ty => format!("{ty}.{}", h.sig),
        };
        let tag = match h.owner {
            "syntax" => "  [syntax]".to_string(),
            _ if h.effect == "pure" => String::new(),
            _ => format!("  [{}]", h.effect),
        };
        s.push_str(&format!("  {call}{tag}\n"));
        if !h.doc.is_empty() {
            s.push_str(&format!("      {}\n", h.doc));
        }
        // Say why a row is here when the reason is not visible in the name — otherwise a
        // prose hit looks like noise.
        if h.matched != "name" {
            s.push_str(&format!("      (matched in the {})\n", h.matched));
        }
    }
    s.push_str("\nFull entry: `helix describe <name>`. One type's methods: `helix doc <Type>`.\n");
    s
}

/// The same listing as data.
pub fn to_json(query: &str, hits: &[Hit]) -> serde_json::Value {
    serde_json::json!({
        "query": query,
        "count": hits.len(),
        "matches": hits.iter().map(|h| serde_json::json!({
            "owner": h.owner,
            "name": h.name,
            "sig": h.sig,
            "doc": h.doc,
            "effect": h.effect,
            "matched_in": h.matched,
        })).collect::<Vec<_>>(),
    })
}
