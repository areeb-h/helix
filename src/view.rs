//! `view_html(tree)` and `view_diff(old, new)` — the app runtime's hot path (ADR 0055, Phase 1).
//!
//! An app's view is DATA: element records `{tag, props, children}` and text records `{tag:
//! "#text", text}`, built in Helix by `std/ui.helix` (`import std.ui`). Rendering that tree to HTML
//! and diffing two of them were Helix code in the prototype, and measured too slow for the
//! ADR's budget: a 2,001-node view rendered in 5.4 ms on the JIT (9.0 on the tree-walker),
//! and the client's full-page morph of the result took 11.6 ms p50 / 16.9 ms p95 in
//! Chromium — the whole of a 60 fps frame. So both move here:
//!
//! - `view_html` renders the tree to the SAME bytes `ui.helix`'s Helix renderer produced
//!   (the replay test pins that), for the first paint and for tests.
//! - `view_diff` compares two trees and returns the patch operations that turn the first
//!   into the second, keyed where the children carry keys. The client builds its DOM from
//!   tree data rather than from HTML, so a path of child indices names exactly one node:
//!   an HTML parser's fix-ups (a `<tbody>` it inserts into a `<table>`) would otherwise
//!   make the server's paths and the browser's nodes disagree.
//!
//! Both are pure functions of their arguments, called through the shared builtin table, so
//! the three engines produce the same bytes by construction.

use std::rc::Rc;

use crate::error::HelixError;
use crate::symbol::Symbol;
use crate::value::{Value, write_value};

/// One node of a view, read out of its record.
enum Node {
    Text(String),
    El { tag: String, key: Option<String>, attrs: Vec<(String, String)>, kids: Vec<Node> },
}

const VOID_TAGS: [&str; 5] = ["input", "br", "img", "hr", "meta"];

/// A record's fields, or a dict's entries, as (name, value) in the order Helix's own `keys()`
/// gives them: a record's written order, a dict's sorted keys. A props, style or attrs value
/// is a Dict whenever it has a quoted key (`{"clip-path": …}`), so both have to be read.
fn pairs(v: &Value) -> Option<Vec<(String, Value)>> {
    match v {
        Value::Record(r) => Some(r.iter().map(|(k, v)| (k.as_str().to_string(), v.clone())).collect()),
        Value::Dict(d) => Some(
            d.map()
                .iter()
                .map(|(k, v)| {
                    let name = match k {
                        crate::value::DictKey::Str(s) | crate::value::DictKey::Dna(s) => s.to_string(),
                        crate::value::DictKey::Int(i) => i.to_string(),
                        crate::value::DictKey::Bool(b) => b.to_string(),
                    };
                    (name, v.clone())
                })
                .collect(),
        ),
        _ => None,
    }
}

/// One field of a record or dict — the hot path of reading a node. A record is scanned in
/// place with no allocation; the clone is a reference-count bump.
fn get(v: &Value, name: &str) -> Option<Value> {
    let found = match v {
        Value::Record(r) => r.iter().find(|(k, _)| k.as_str() == name).map(|(_, v)| v.clone()),
        Value::Dict(d) => d.map().get(&crate::value::DictKey::Str(Rc::new(name.to_string()))).cloned(),
        _ => None,
    };
    found.filter(|v| !matches!(v, Value::Missing))
}

/// A value as text: a string as it is, anything else as interpolation writes it.
fn text_of(v: &Value, line: usize, col: usize) -> Result<String, HelixError> {
    match v {
        Value::Str(s) => Ok(s.as_str().to_string()),
        other => shown(other, line, col),
    }
}

/// A value as interpolation writes it (`"{v}"`), so an attribute holding `3.5` or `900.0`
/// reads exactly as it did when the Helix renderer wrote it.
fn shown(v: &Value, line: usize, col: usize) -> Result<String, HelixError> {
    let mut s = String::new();
    write_value(&mut s, v, line, col)?;
    Ok(s)
}

// --- What a view may not contain. -------------------------------------------------------
//
// A view is often built from data a person typed, so the renderer — not each app — is where
// markup injection is stopped. A tag or attribute NAME is written into the page as it is, so a
// name is refused unless it is a plain name: `img src=x onerror=…` as a tag would otherwise be a
// live element. Elements and attributes that run script are refused outright: an app has
// `click`/`input`/… for events and no reason to emit `<script>` or `onclick`. And a URL that
// runs script when followed — `javascript:` (however it is spelled) — is made inert rather than
// refused, because a URL is usually DATA, and one person's bad link must not take the whole view
// down.

/// Elements that execute, embed or retarget a document: refused in any view.
const REFUSED_TAGS: [&str; 10] = ["script", "iframe", "frame", "frameset", "object", "embed", "applet", "base", "meta", "link"];

/// Attributes whose value is a URL the browser may follow or load.
const URL_ATTRS: [&str; 9] = ["href", "src", "action", "formaction", "xlink:href", "poster", "cite", "background", "ping"];

/// What an inert URL is replaced by.
const INERT_URL: &str = "about:blank#blocked";

/// A tag name: a letter, then letters, digits and `-` (SVG's `clipPath` and custom elements'
/// `x-chart` included).
fn valid_tag(t: &str) -> bool {
    let mut c = t.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic()) && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
}

/// An attribute name: a letter or `_`, then letters, digits, `-`, `_`, `.` and `:` (`xlink:href`).
fn valid_attr(a: &str) -> bool {
    let mut c = a.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
}

/// Whether a URL runs script when followed. Browsers ignore ASCII whitespace and control
/// characters inside a scheme and compare it without case, so ` JaVa\tScRiPt:` counts.
fn scripting_url(attr: &str, v: &str) -> bool {
    let scheme: String = v
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && !c.is_ascii_control())
        .take(16)
        .collect::<String>()
        .to_ascii_lowercase();
    // `data:` is a document only where the browser navigates to it; as an image it is data.
    let navigates = matches!(attr, "href" | "xlink:href" | "action" | "formaction");
    scheme.starts_with("javascript:") || scheme.starts_with("vbscript:") || (navigates && scheme.starts_with("data:"))
}

/// Check one element's tag and attributes, making a scripting URL inert.
fn guard(tag: &str, attrs: &mut [(String, String)], line: usize, col: usize) -> Result<(), HelixError> {
    if !valid_tag(tag) {
        return Err(HelixError::new(format!("`{tag}` is not an element name: a view's tag is a letter, then letters, digits and `-`"), line, col)
            .hint("a tag is written into the page as it is, so only a plain name can be one."));
    }
    if REFUSED_TAGS.iter().any(|r| r.eq_ignore_ascii_case(tag)) {
        return Err(HelixError::new(format!("`<{tag}>` cannot be part of a view: it runs or loads a document"), line, col)
            .hint("an app's behaviour is its `update`; its events are the `click`, `input`, `change` and `submit` props."));
    }
    for (k, v) in attrs.iter_mut() {
        if !valid_attr(k) {
            return Err(HelixError::new(format!("`{k}` is not an attribute name"), line, col)
                .hint("an attribute name is a letter or `_`, then letters, digits, `-`, `_`, `.` and `:`."));
        }
        let lower = k.to_ascii_lowercase();
        if lower.starts_with("on") {
            return Err(HelixError::new(format!("`{k}` is an inline script handler, which a view cannot carry"), line, col)
                .hint("send a message instead: `click: \"save\"` posts {msg: \"save\"} to `update`."));
        }
        if lower == "srcdoc" || (lower == "attributename" && v.trim().to_ascii_lowercase().ends_with("href")) {
            return Err(HelixError::new(format!("`{k}` can load script into the page, so a view cannot carry it"), line, col));
        }
        if URL_ATTRS.contains(&lower.as_str()) && scripting_url(&lower, v) {
            *v = INERT_URL.to_string();
        }
    }
    Ok(())
}

fn shape_err(what: &str, line: usize, col: usize) -> HelixError {
    HelixError::new(format!("a view node must be an element record {{tag, props, children}} or text; {what}"), line, col)
        .hint("build views with `ui.el(tag, props, children)` and `ui.text(s)`.")
}

fn parse(v: &Value, line: usize, col: usize) -> Result<Node, HelixError> {
    if !matches!(v, Value::Record(_) | Value::Dict(_)) {
        return Err(shape_err(&format!("found {}", crate::value::with_article(v.type_name())), line, col));
    }
    let tag = match get(v, "tag") {
        Some(Value::Str(s)) => s,
        _ => return Err(shape_err("this one has no string `tag`", line, col)),
    };
    if tag.as_str() == "#text" {
        let text = match get(v, "text") {
            Some(t) => text_of(&t, line, col)?,
            None => String::new(),
        };
        return Ok(Node::Text(text));
    }
    let props = get(v, "props").unwrap_or(Value::Missing);
    let key = get(&props, "key").map(|k| text_of(&k, line, col)).transpose()?;
    let mut attrs: Vec<(String, String)> = Vec::new();
    // The props the runtime knows, in a fixed order so a tree renders to one string. The
    // event props (`click`, `input`, `change`, `submit`, `dismiss`, `scroll`) name the message
    // the event sends, and `value` is what a click sends with it; `enter`/`exit` name a motion
    // preset the client plays when the node arrives or leaves, and `layout` asks it to animate
    // the node to its new position (FLIP).
    for (prop, out) in [
        ("class", "class"),
        ("id", "id"),
        ("role", "role"),
        ("key", "data-key"),
        ("click", "data-click"),
        ("value", "data-value"),
        ("input", "data-input"),
        ("change", "data-change"),
        ("submit", "data-submit"),
        ("dismiss", "data-dismiss"),
        ("scroll", "data-scroll"),
        ("enter", "data-enter"),
        ("exit", "data-exit"),
        ("layout", "data-layout"),
        // A shared element: two nodes with the same `share` name, in one frame and the next,
        // are one thing moving — the client morphs the new one from where and how big the old
        // one was (a row growing into its detail, a selection gliding between options).
        ("share", "data-share"),
        // A desktop app's gestures. `dblclick` and `contextmenu` name the message a double
        // click or a right click (or the menu key) sends, with the node's `value`; `keydown`
        // names the message a key in `shortcuts` (space-separated chords: "Enter Ctrl+K /")
        // sends from inside the node, carrying the chord. `reveal: true` scrolls the node into
        // view when it becomes the one revealed, and `copy` is text a click puts on the
        // clipboard. All are data the page's own script reads: none is a handler.
        ("dblclick", "data-dblclick"),
        ("contextmenu", "data-contextmenu"),
        ("keydown", "data-keydown"),
        ("shortcuts", "data-shortcuts"),
        ("reveal", "data-reveal"),
        ("copy", "data-copy"),
        // Gestures (the page's script does the dragging; the app hears the outcome). `drag`
        // names the message a node sends when it is let go, with `drag: {dx, dy, vx, vy}`;
        // `drag_axis` is "x", "y", "both" or a direction ("down", "left"…), and `drag_commit`
        // the distance (px) a throw must reach to be sent at all. `reorder` on a container
        // names the message its keyed children send when dragged into a new order (`{key,
        // from, to}`); `resize` on a divider names the message that carries the size of the
        // pane before it. `handle: true` marks the part of a draggable node that grabs it.
        ("drag", "data-drag"),
        ("drag_axis", "data-drag-axis"),
        ("drag_commit", "data-drag-commit"),
        ("reorder", "data-reorder"),
        ("resize", "data-resize"),
        ("handle", "data-drag-handle"),
        // Between containers: containers with the same `reorder_group` trade rows (a board's
        // columns), and a node with `drop` names the message a row let go over it sends.
        ("reorder_group", "data-reorder-group"),
        ("drop", "data-drop"),
    ] {
        if let Some(v) = get(&props, prop) {
            let t = if prop == "class" { class_of(&v, line, col)? } else { text_of(&v, line, col)? };
            if prop == "class" && t.is_empty() {
                continue;
            }
            attrs.push((out.to_string(), t));
            // A number or a Bool comes back from the page as itself, not as its text: `value={7}`
            // reaches `update` as 7 in a browser, as it does in `ui.replay`.
            if prop == "value" {
                match v {
                    Value::Int(_) | Value::Float(_) => attrs.push(("data-value-type".into(), "number".into())),
                    Value::Bool(_) => attrs.push(("data-value-type".into(), "bool".into())),
                    _ => {}
                }
            }
        }
    }
    // `aria` is a record of ARIA states and properties: `{label: "Close", expanded: false}` is
    // `aria-label="Close" aria-expanded="false"` — `_` becomes `-`, and a Bool is written as
    // the words ARIA expects rather than dropped, since `aria-expanded="false"` means something.
    if let Some(aria) = get(&props, "aria").as_ref().and_then(pairs) {
        for (k, v) in aria {
            if !matches!(v, Value::Missing) {
                attrs.push((format!("aria-{}", k.replace('_', "-")), text_of(&v, line, col)?));
            }
        }
    }
    // `style` is a record of CSS properties (`border_radius` is `border-radius`), and
    // `transition` is appended to it — the same text the Helix renderer built.
    let style = get(&props, "style");
    let base = match style.as_ref().and_then(pairs) {
        Some(st) => st
            .iter()
            .map(|(k, v)| Ok(format!("{}: {}", k.replace('_', "-"), text_of(v, line, col)?)))
            .collect::<Result<Vec<_>, HelixError>>()?
            .join("; "),
        // Markup's `style="--fill: 40%"`: a string is the declarations as written.
        None => match style {
            Some(Value::Str(s)) => s.as_str().to_string(),
            _ => String::new(),
        },
    };
    let styled = match get(&props, "transition") {
        Some(t) => format!("{base}; transition: {}", text_of(&t, line, col)?),
        None => base,
    };
    if !styled.is_empty() {
        attrs.push(("style".to_string(), styled));
    }
    // Raw attributes. A Bool is an HTML boolean attribute: `disabled: true` is present (as
    // `disabled=""`), `disabled: false` is absent — `disabled="false"` would still disable.
    if let Some(raw) = get(&props, "attrs").as_ref().and_then(pairs) {
        for (k, v) in raw {
            match v {
                Value::Bool(false) | Value::Missing => {}
                Value::Bool(true) => attrs.push((k, String::new())),
                v => attrs.push((k, text_of(&v, line, col)?)),
            }
        }
    }
    guard(tag.as_str(), &mut attrs, line, col)?;
    let mut kids = Vec::new();
    if let Some(Value::Array(a)) = get(v, "children") {
        children(&a.to_values(), &mut kids, line, col)?;
    }
    Ok(Node::El { tag: tag.as_str().to_string(), key, attrs, kids })
}

/// A node's children, as markup writes them (ADR 0061): a string or a number is text, a list
/// is its items in place (`{items.map(row)}`), and `missing` or a Bool is nothing, so
/// `{if busy then <spinner/> else missing}` and `{done and <Badge/>}` leave no gap.
fn children(vs: &[Value], out: &mut Vec<Node>, line: usize, col: usize) -> Result<(), HelixError> {
    for c in vs {
        match c {
            Value::Missing | Value::Bool(_) => {}
            Value::Str(s) => out.push(Node::Text(s.as_str().to_string())),
            Value::Int(_) | Value::Float(_) => out.push(Node::Text(shown(c, line, col)?)),
            Value::Array(a) => children(&a.to_values(), out, line, col)?,
            other => out.push(parse(other, line, col)?),
        }
    }
    Ok(())
}

/// A `class` value: a string as it is, or a list whose strings are joined (`class={["row",
/// if on then "on" else missing]}`), skipping `missing`, Bools and empty strings.
fn class_of(v: &Value, line: usize, col: usize) -> Result<String, HelixError> {
    match v {
        Value::Array(a) => {
            let mut parts = Vec::new();
            for x in a.to_values().iter() {
                match x {
                    Value::Missing | Value::Bool(_) => {}
                    Value::Array(_) => parts.push(class_of(x, line, col)?),
                    other => parts.push(text_of(other, line, col)?),
                }
            }
            Ok(parts.into_iter().filter(|p| !p.trim().is_empty()).collect::<Vec<_>>().join(" "))
        }
        other => text_of(other, line, col),
    }
}

fn escape(s: &str) -> std::borrow::Cow<'_, str> {
    crate::interp::builtins::encoding::html_escape(s)
}

fn write_html(n: &Node, out: &mut String) {
    match n {
        Node::Text(t) => out.push_str(&escape(t)),
        Node::El { tag, attrs, kids, .. } => {
            out.push('<');
            out.push_str(tag);
            for (k, v) in attrs {
                out.push(' ');
                out.push_str(k);
                out.push_str("=\"");
                out.push_str(&escape(v));
                out.push('"');
            }
            out.push('>');
            if VOID_TAGS.contains(&tag.as_str()) {
                return;
            }
            for k in kids {
                write_html(k, out);
            }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
}

/// `view_classes(tree)` — every class name the tree uses, once each, sorted (ADR 0060).
pub fn view_classes(tree: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    fn walk(n: &Node, out: &mut std::collections::BTreeSet<String>) {
        if let Node::El { attrs, kids, .. } = n {
            for (k, v) in attrs {
                if k == "class" {
                    out.extend(v.split_whitespace().map(str::to_string));
                }
            }
            for k in kids {
                walk(k, out);
            }
        }
    }
    let node = parse(tree, line, col)?;
    let mut set = std::collections::BTreeSet::new();
    walk(&node, &mut set);
    Ok(Value::array(set.into_iter().map(|c| s(&c)).collect()))
}

/// `utility_css(classes)` — the stylesheet for the utilities among `classes` (ADR 0060).
pub fn utility_css(classes: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let Value::Array(a) = classes else {
        return Err(HelixError::new(format!("`utility_css` takes an array of class names, not {}", crate::value::with_article(classes.type_name())), line, col));
    };
    let mut set = std::collections::BTreeSet::new();
    for v in a.to_values().iter() {
        if let Value::Str(c) = v {
            set.extend(c.split_whitespace().map(str::to_string));
        }
    }
    Ok(Value::Str(Rc::new(crate::style::css_for(&set))))
}

/// `view_html(tree)` — the tree as HTML.
pub fn view_html(tree: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let node = parse(tree, line, col)?;
    let mut out = String::new();
    write_html(&node, &mut out);
    Ok(Value::Str(Rc::new(out)))
}

// --- The diff. ------------------------------------------------------------------------------

fn s(text: &str) -> Value {
    Value::Str(Rc::new(text.to_string()))
}
fn int(i: usize) -> Value {
    Value::Int(i as i64)
}
fn path_value(path: &[usize]) -> Value {
    Value::int_array(path.iter().map(|&i| i as i64).collect())
}
fn op(fields: Vec<(&str, Value)>) -> Value {
    Value::Record(Rc::new(fields.into_iter().map(|(k, v)| (Symbol::intern(k), v)).collect()))
}

/// The compact form the client builds a DOM from: a string for text, `[tag, [[name,
/// value], ...], [children]]` for an element.
fn wire(n: &Node) -> Value {
    match n {
        Node::Text(t) => s(t),
        Node::El { tag, attrs, kids, .. } => Value::array(vec![
            s(tag),
            Value::array(attrs.iter().map(|(k, v)| Value::array(vec![s(k), s(v)])).collect()),
            Value::array(kids.iter().map(wire).collect()),
        ]),
    }
}

fn diff_node(old: &Node, new: &Node, path: &mut Vec<usize>, ops: &mut Vec<Value>) {
    match (old, new) {
        (Node::Text(a), Node::Text(b)) => {
            if a != b {
                ops.push(op(vec![("op", s("text")), ("path", path_value(path)), ("text", s(b))]));
            }
        }
        (
            Node::El { tag: ta, key: ka, attrs: aa, kids: ca },
            Node::El { tag: tb, key: kb, attrs: ab, kids: cb },
        ) if ta == tb && ka == kb => {
            let set: Vec<Value> = ab
                .iter()
                .filter(|(k, v)| aa.iter().find(|(ok, _)| ok == k).map(|(_, ov)| ov) != Some(v))
                .map(|(k, v)| Value::array(vec![s(k), s(v)]))
                .collect();
            let remove: Vec<Value> =
                aa.iter().filter(|(k, _)| !ab.iter().any(|(nk, _)| nk == k)).map(|(k, _)| s(k)).collect();
            if !set.is_empty() || !remove.is_empty() {
                ops.push(op(vec![
                    ("op", s("attrs")),
                    ("path", path_value(path)),
                    ("set", Value::array(set)),
                    ("remove", Value::array(remove)),
                ]));
            }
            diff_kids(ca, cb, path, ops);
        }
        _ => ops.push(op(vec![("op", s("replace")), ("path", path_value(path)), ("node", wire(new))])),
    }
}

fn key_of(n: &Node) -> Option<&str> {
    match n {
        Node::El { key: Some(k), .. } => Some(k.as_str()),
        _ => None,
    }
}

/// Children: by key when every child on both sides has one and the keys are unique — so a
/// reordered list moves its nodes, and their running transitions, rather than rewriting
/// each one in place — and by position otherwise.
fn diff_kids(old: &[Node], new: &[Node], path: &mut Vec<usize>, ops: &mut Vec<Value>) {
    let keys = |kids: &[Node]| -> Option<Vec<String>> {
        let ks: Vec<String> = kids.iter().map(|k| key_of(k).map(str::to_string)).collect::<Option<_>>()?;
        let mut sorted = ks.clone();
        sorted.sort();
        sorted.dedup();
        (sorted.len() == ks.len()).then_some(ks)
    };
    let here = |ops: &mut Vec<Value>, path: &[usize], fields: Vec<(&str, Value)>| {
        let mut f = vec![("path", path_value(path))];
        f.extend(fields);
        ops.push(op(f));
    };
    if let (Some(ok), Some(nk)) = (keys(old), keys(new))
        && !old.is_empty()
        && !new.is_empty()
    {
        // Remove what is gone, back to front so the indices stay true.
        let mut cur: Vec<String> = ok.clone();
        for i in (0..ok.len()).rev() {
            if !nk.contains(&ok[i]) {
                here(ops, path, vec![("op", s("remove")), ("at", int(i))]);
                cur.remove(i);
            }
        }
        // Then build the new order left to right: keep, move into place, or insert.
        for (t, k) in nk.iter().enumerate() {
            if cur.get(t) == Some(k) {
                continue;
            }
            if let Some(j) = cur.iter().position(|c| c == k) {
                here(ops, path, vec![("op", s("move")), ("from", int(j)), ("to", int(t))]);
                let moved = cur.remove(j);
                cur.insert(t, moved);
            } else {
                here(ops, path, vec![("op", s("insert")), ("at", int(t)), ("node", wire(&new[t]))]);
                cur.insert(t, k.clone());
            }
        }
        // Now the order matches: diff each survivor where it now stands.
        for (t, k) in nk.iter().enumerate() {
            if let Some(o) = ok.iter().position(|c| c == k) {
                path.push(t);
                diff_node(&old[o], &new[t], path, ops);
                path.pop();
            }
        }
        return;
    }
    let common = old.len().min(new.len());
    for i in 0..common {
        path.push(i);
        diff_node(&old[i], &new[i], path, ops);
        path.pop();
    }
    for (i, n) in new.iter().enumerate().skip(common) {
        here(ops, path, vec![("op", s("insert")), ("at", int(i)), ("node", wire(n))]);
    }
    for i in (common..old.len()).rev() {
        here(ops, path, vec![("op", s("remove")), ("at", int(i))]);
    }
}

/// `view_diff(old, new)` — the patch operations that turn the rendered `old` into `new`. With
/// `missing` for `old` it is one `replace` of the root: the first frame a client receives.
pub fn view_diff(old: &Value, new: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let n = parse(new, line, col)?;
    let mut ops = Vec::new();
    match old {
        Value::Missing => ops.push(op(vec![("op", s("replace")), ("path", Value::int_array(vec![])), ("node", wire(&n))])),
        _ => {
            let o = parse(old, line, col)?;
            diff_node(&o, &n, &mut Vec::new(), &mut ops);
        }
    }
    Ok(Value::array(ops))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(fields: Vec<(&str, Value)>) -> Value {
        op(fields)
    }
    fn text(t: &str) -> Value {
        rec(vec![("tag", s("#text")), ("props", rec(vec![])), ("children", Value::array(vec![])), ("text", s(t))])
    }
    fn el(tag: &str, props: Vec<(&str, Value)>, kids: Vec<Value>) -> Value {
        rec(vec![("tag", s(tag)), ("props", rec(props)), ("children", Value::array(kids))])
    }
    fn ops_of(old: &Value, new: &Value) -> Vec<String> {
        match view_diff(old, new, 0, 0).expect("diff") {
            Value::Array(a) => a.to_values().iter().map(|o| format!("{o}")).collect(),
            other => panic!("not an array: {other}"),
        }
    }

    #[test]
    fn markup_that_runs_script_is_refused_or_made_inert() {
        let render = |t: Value| view_html(&t, 0, 0);
        let err = |t: Value| render(t).err().map(|e| e.message).unwrap_or_default();
        assert!(err(el("img src=x onerror=alert(1)", vec![], vec![])).contains("not an element name"));
        assert!(err(el("script", vec![], vec![text("alert(1)")])).contains("cannot be part of a view"));
        assert!(err(el("IFRAME", vec![], vec![])).contains("cannot be part of a view"));
        assert!(err(el("a", vec![("attrs", rec(vec![("onclick", s("x"))]))], vec![])).contains("inline script handler"));
        assert!(err(el("animate", vec![("attrs", rec(vec![("attributeName", s("href"))]))], vec![])).contains("can load script"));
        // An attribute name from data is checked like a tag: a dict key can be anything.
        let mut d = crate::value::DictMap::default();
        d.insert(crate::value::DictKey::Str(Rc::new("x onmouseover".into())), s("y"));
        let bad = el("a", vec![("attrs", Value::Dict(Rc::new(crate::value::Dict::from_map(d))))], vec![]);
        assert!(err(bad).contains("not an attribute name"));
        // A scripting URL, however spelled, is made inert; ordinary and image-data URLs are kept.
        for url in ["javascript:alert(1)", " JaVa\tScRiPt:alert(1)", "vbscript:x", "data:text/html,<b>"] {
            let Value::Str(h) = render(el("a", vec![("attrs", rec(vec![("href", s(url))]))], vec![])).unwrap() else { panic!() };
            assert_eq!(h.as_str(), "<a href=\"about:blank#blocked\"></a>", "{url}");
        }
        let Value::Str(h) = render(el("img", vec![("attrs", rec(vec![("src", s("data:image/png;base64,AA=="))]))], vec![])).unwrap() else { panic!() };
        assert_eq!(h.as_str(), "<img src=\"data:image/png;base64,AA==\">");
        let Value::Str(h) = render(el("a", vec![("attrs", rec(vec![("href", s("https://example.com/?q=javascript:"))]))], vec![])).unwrap() else { panic!() };
        assert_eq!(h.as_str(), "<a href=\"https://example.com/?q=javascript:\"></a>");
        // SVG names keep working.
        assert!(render(el("clipPath", vec![("attrs", rec(vec![("xlink:href", s("#a")), ("stroke-width", s("2"))]))], vec![])).is_ok());
    }

    #[test]
    fn component_props_render_in_a_fixed_order_with_aria_and_boolean_attributes() {
        let t = el(
            "button",
            vec![
                ("attrs", rec(vec![("disabled", Value::Bool(true)), ("hidden", Value::Bool(false)), ("type", s("button"))])),
                ("aria", rec(vec![("expanded", Value::Bool(false)), ("has_popup", s("menu"))])),
                ("layout", Value::Bool(true)),
                ("exit", s("fade")),
                ("enter", s("pop")),
                ("dismiss", s("close")),
                ("value", s("7")),
                ("change", s("c")),
                ("role", s("switch")),
                ("id", s("b1")),
                ("class", s("btn")),
            ],
            vec![],
        );
        let Value::Str(h) = view_html(&t, 0, 0).expect("render") else { panic!() };
        assert_eq!(
            h.as_str(),
            "<button class=\"btn\" id=\"b1\" role=\"switch\" data-value=\"7\" data-change=\"c\" data-dismiss=\"close\" \
             data-enter=\"pop\" data-exit=\"fade\" data-layout=\"true\" aria-expanded=\"false\" \
             aria-has-popup=\"menu\" disabled=\"\" type=\"button\"></button>"
        );
    }

    #[test]
    fn gestures_are_data_the_page_reads() {
        let v = el(
            "div",
            vec![
                ("reorder", s("moved")),
                ("drag", s("thrown")),
                ("drag_axis", s("down")),
                ("drag_commit", Value::Int(96)),
                ("resize", s("size")),
                ("handle", Value::Bool(true)),
                ("reorder_group", s("board")),
                ("drop", s("dropped")),
            ],
            vec![],
        );
        let Value::Str(h) = view_html(&v, 0, 0).expect("render") else { panic!() };
        assert_eq!(
            h.as_str(),
            "<div data-drag=\"thrown\" data-drag-axis=\"down\" data-drag-commit=\"96\" data-reorder=\"moved\" data-resize=\"size\" data-drag-handle=\"true\" data-reorder-group=\"board\" data-drop=\"dropped\"></div>"
        );
    }

    #[test]
    fn a_shared_element_names_itself_after_the_motion_props() {
        let t = el("span", vec![("share", s("seg-thumb")), ("layout", Value::Bool(true)), ("class", s("seg-thumb"))], vec![]);
        let Value::Str(h) = view_html(&t, 0, 0).expect("render") else { panic!() };
        assert_eq!(h.as_str(), "<span class=\"seg-thumb\" data-layout=\"true\" data-share=\"seg-thumb\"></span>");
    }

    #[test]
    fn html_escapes_text_and_attributes_and_closes_only_non_void_tags() {
        let t = el(
            "div",
            vec![("class", s("a\"b")), ("style", rec(vec![("border_radius", s("8px"))])), ("transition", s("x 1s"))],
            vec![text("<hi> & 'you'"), el("input", vec![("attrs", rec(vec![("value", Value::Float(3.5))]))], vec![])],
        );
        let Value::Str(h) = view_html(&t, 0, 0).expect("render") else { panic!() };
        assert_eq!(
            h.as_str(),
            "<div class=\"a&quot;b\" style=\"border-radius: 8px; transition: x 1s\">&lt;hi&gt; &amp; &#39;you&#39;<input value=\"3.5\"></div>"
        );
    }

    /// A props, style or attrs value with a quoted key (`{"clip-path": …}`) is a Dict, not a
    /// Record. The first Rust renderer read records only and dropped the explorer's clip-path;
    /// the replay test caught the changed bytes.
    #[test]
    fn attributes_with_quoted_names_come_from_a_dict() {
        let mut m = std::collections::BTreeMap::new();
        m.insert(crate::value::DictKey::Str(Rc::new("clip-path".to_string())), s("url(#p)"));
        let dict = Value::Dict(Rc::new(crate::value::Dict::from_map(m)));
        let t = el("g", vec![("attrs", dict)], vec![]);
        let Value::Str(h) = view_html(&t, 0, 0).expect("render") else { panic!() };
        assert_eq!(h.as_str(), "<g clip-path=\"url(#p)\"></g>");
    }

    #[test]
    fn an_unchanged_tree_needs_no_operations_and_a_change_touches_only_what_changed() {
        let a = el("ul", vec![], vec![el("li", vec![], vec![text("one")]), el("li", vec![("class", s("x"))], vec![text("two")])]);
        assert!(ops_of(&a, &a).is_empty());
        let b = el("ul", vec![], vec![el("li", vec![], vec![text("one")]), el("li", vec![("class", s("y"))], vec![text("2")])]);
        assert_eq!(
            ops_of(&a, &b),
            vec![
                "{op: \"attrs\", path: [1], remove: [], set: [[\"class\", \"y\"]]}".to_string(),
                "{op: \"text\", path: [1, 0], text: \"2\"}".to_string(),
            ]
        );
    }

    #[test]
    fn keyed_children_move_instead_of_being_rewritten() {
        let item = |k: &str| el("li", vec![("key", s(k))], vec![text(k)]);
        let a = el("ul", vec![], vec![item("a"), item("b"), item("c")]);
        let b = el("ul", vec![], vec![item("c"), item("a"), item("d")]);
        assert_eq!(
            ops_of(&a, &b),
            vec![
                "{at: 1, op: \"remove\", path: []}".to_string(),
                "{from: 1, op: \"move\", path: [], to: 0}".to_string(),
                "{at: 2, node: [\"li\", [[\"data-key\", \"d\"]], [\"d\"]], op: \"insert\", path: []}".to_string(),
            ]
        );
    }

    #[test]
    fn a_first_frame_replaces_the_root_and_a_changed_tag_replaces_the_node() {
        let a = el("p", vec![], vec![text("x")]);
        // (A record prints its fields sorted; it stores them in written order.)
        assert_eq!(ops_of(&Value::Missing, &a), vec!["{node: [\"p\", [], [\"x\"]], op: \"replace\", path: []}".to_string()]);
        let b = el("div", vec![], vec![text("x")]);
        assert_eq!(ops_of(&a, &b), vec!["{node: [\"div\", [], [\"x\"]], op: \"replace\", path: []}".to_string()]);
    }
}
