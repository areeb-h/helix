//! The app library's components, for `helix doc ui`, `helix doc ui.button`, `helix describe`
//! and `helix search` — read from the library itself, so they can never drift from it.
//!
//! An agent writing an app needs a component's parameters before it writes the tag: what
//! `<ui.BarChart …/>` takes, and which arguments have defaults. The library is Helix source
//! (`std/ui.helix`, compiled into the binary), and every `export fn` there has its signature on
//! one line and, usually, a comment above it saying what it is. Reading those here is the same
//! source the program runs, so a new parameter is documented the moment it exists.

/// One exported function of `std.ui`.
#[derive(Debug, Clone)]
pub struct UiFn {
    pub name: String,
    /// The parameter list as written, defaults included: `caption, msg, variant = "default"`.
    pub params: String,
    /// The comment above it, joined into prose; empty when there is none.
    pub doc: String,
}

impl UiFn {
    /// `ui.button(caption, msg, variant = "default")`.
    pub fn signature(&self) -> String {
        format!("ui.{}({})", self.name, self.params)
    }
    /// The tag a markup view writes: `ui.button` is `<ui.Button …/>`.
    pub fn tag(&self) -> String {
        let mut out = String::from("ui.");
        let mut up = true;
        for c in self.name.chars() {
            if c == '_' {
                up = true;
            } else if up {
                out.push(c.to_ascii_uppercase());
                up = false;
            } else {
                out.push(c);
            }
        }
        out
    }
    /// The required parameters (those without a default), by name.
    pub fn required(&self) -> Vec<String> {
        split_params(&self.params).into_iter().filter(|p| !p.contains('=')).map(|p| p.trim().to_string()).collect()
    }
    /// A markup example: the tag with its required parameters as attributes.
    pub fn example(&self) -> String {
        let attrs: Vec<String> = self.required().iter().filter(|p| *p != "children").map(|p| format!("{p}={{…}}")).collect();
        let has_children = split_params(&self.params).iter().any(|p| p.trim().starts_with("children"));
        let open = if attrs.is_empty() { format!("<{}", self.tag()) } else { format!("<{} {}", self.tag(), attrs.join(" ")) };
        if has_children { format!("{open}>…</{}>", self.tag()) } else { format!("{open} />") }
    }
}

/// The parameters of a list, split at the top-level commas (a default may hold a record or a
/// call with commas of its own).
fn split_params(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut depth, mut cur, mut quote) = (0i32, String::new(), None::<char>);
    for c in s.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
                continue;
            }
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                cur.push(c);
                continue;
            }
            None => {}
        }
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out.into_iter().map(|p| p.trim().to_string()).collect()
}

/// Every `export fn` of the embedded `std/ui.helix`, in the order the library defines them.
pub fn functions() -> Vec<UiFn> {
    let src = crate::module::EMBEDDED.iter().find(|(k, _)| *k == "std/ui.helix").map(|(_, s)| *s).unwrap_or("");
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix("export fn ") else { continue };
        let Some(open) = rest.find('(') else { continue };
        let name = rest[..open].trim().to_string();
        // The parameter list: up to the parenthesis that closes it, strings skipped.
        let (mut depth, mut end, mut quote) = (0i32, None, None::<char>);
        for (j, c) in rest[open..].char_indices() {
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                }
                continue;
            }
            match c {
                '"' | '\'' => quote = Some(c),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + j);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { continue };
        let params = rest[open + 1..end].to_string();
        // The comment block directly above (a blank line ends it).
        let mut doc_lines = Vec::new();
        let mut k = i;
        while k > 0 {
            let l = lines[k - 1].trim();
            match l.strip_prefix('#') {
                Some(c) if !l.starts_with("#[") => doc_lines.push(c.trim().to_string()),
                _ => break,
            }
            k -= 1;
        }
        doc_lines.reverse();
        let doc = doc_lines.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
        out.push(UiFn { name, params, doc });
    }
    out
}

/// The component called `query`: `ui.bar_chart`, `bar_chart`, `ui.BarChart` or `BarChart`.
pub fn find(query: &str) -> Option<UiFn> {
    let q = query.strip_prefix("ui.").unwrap_or(query);
    let snake = crate::parser::snake_case(q);
    functions().into_iter().find(|f| f.name == q || f.name == snake)
}

/// The first sentence of a doc, for a one-line listing.
pub fn first_sentence(doc: &str) -> &str {
    match doc.find(". ") {
        Some(i) => &doc[..=i],
        None => doc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_library_documents_itself() {
        let all = functions();
        assert!(all.len() > 80, "found {}", all.len());
        let b = find("ui.button").expect("button");
        assert!(b.params.starts_with("caption, msg, variant = \"default\""), "{}", b.params);
        assert_eq!(b.tag(), "ui.Button");
        assert_eq!(find("BarChart").map(|f| f.name), Some("bar_chart".into()));
        let card = find("card").unwrap();
        assert_eq!(card.example(), "<ui.Card title={…}>…</ui.Card>");
        assert!(!find("sheet").unwrap().doc.is_empty());
    }
}
