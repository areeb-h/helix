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
//! signatures — the prose is where the intent lives.
//!
//! Ranking is deliberately crude and explainable: an exact name beats a name substring,
//! which beats a hit in the prose. Nothing here is fuzzy — a reader can always see why a
//! row matched, and `--json` says which field it was.

use crate::{docs, registry};

/// One catalog entry that matched, with why.
pub struct Hit {
    /// `builtin`, or the receiver type for a method.
    pub owner: &'static str,
    pub name: &'static str,
    pub sig: String,
    pub doc: String,
    pub effect: &'static str,
    /// Which field matched — shown so a surprising row explains itself.
    pub matched: &'static str,
    score: u8,
}

/// Score a candidate against the query, or `None` when nothing matched.
///
/// The query is lowercased once by the caller; every field is compared lowercased, because
/// a reader searching for "header" should not have to know the catalog spells it `Header`.
fn score(q: &str, name: &str, sig: &str, doc: &str, notes: &str) -> Option<(u8, &'static str)> {
    let lname = name.to_lowercase();
    if lname == q {
        return Some((4, "name"));
    }
    if lname.contains(q) {
        return Some((3, "name"));
    }
    if sig.to_lowercase().contains(q) {
        return Some((2, "signature"));
    }
    if doc.to_lowercase().contains(q) {
        return Some((1, "doc"));
    }
    // Notes carry the caveats — "last-wins", "wire order and repeats are kept", "does not
    // remove NaN" — which is often the exact sentence a reader is hunting for.
    if notes.to_lowercase().contains(q) {
        return Some((1, "notes"));
    }
    None
}

/// Every builtin and method matching `query`, best first.
pub fn search(query: &str) -> Vec<Hit> {
    let q = query.to_lowercase();
    let mut hits: Vec<Hit> = Vec::new();

    for b in registry::BUILTINS {
        let d = docs::builtin_doc(b.path);
        let (sig, doc, notes) = d.map_or(("", "", ""), |d| (d.sig, d.doc, d.notes));
        if let Some((score, matched)) = score(&q, b.path, sig, doc, notes) {
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
            if let Some((score, matched)) = score(&q, m, sig, doc, notes) {
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
             `helix search` looks at names, signatures, docs and notes. Try a plainer word \
             (\"header\", \"group\", \"random\"),\nor `helix doc <Type>` to list one type's methods.\n"
        );
    }
    let mut s = format!("{} match{} for `{query}`\n\n", hits.len(), if hits.len() == 1 { "" } else { "es" });
    for h in hits {
        // The receiver is part of how you CALL it, so it leads: `Array.frequencies()` is
        // usable as printed, where a bare `frequencies()` is not.
        let call =
            if h.owner == "builtin" { h.sig.clone() } else { format!("{}.{}", h.owner, h.sig) };
        let eff = if h.effect == "pure" { String::new() } else { format!("  [{}]", h.effect) };
        s.push_str(&format!("  {call}{eff}\n"));
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
