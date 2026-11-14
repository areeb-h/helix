//! Utility classes, generated (ADR 0060).
//!
//! An app writes Tailwind's class names: `p-4 rounded-xl bg-accent hover:bg-accent/90
//! md:grid-cols-3`. There is no build step. When a view is rendered, the classes it uses are
//! collected, and this module writes the CSS for exactly those, once each. Unknown names are
//! left alone, as Tailwind leaves them.
//!
//! The vocabulary is Tailwind v4's, with the same values: its spacing scale, type scale,
//! radii, shadows and palette (`style_palette.rs`, from the published package). Helix's
//! computed design tokens join it under their own names:
//!
//! - colours: `bg-accent`, `text-muted`, `border-line`, `bg-surface-2`, `text-danger`,
//!   `bg-card`;
//! - radii and paddings that stay concentric: `rounded-card`, `rounded-control`, `p-card`;
//! - elevations and springs: `shadow-1`, `ease-spring`.
//!
//! So a class follows the app's design, and its light and dark modes, as the components do.
//!
//! **Safety.** A class name can come from data (a tag, a user's choice). The CSS written for it
//! must never escape its rule. So:
//!
//! - only names that parse as a known utility produce CSS;
//! - an arbitrary value (`w-[320px]`) must pass a strict character whitelist, and refuses
//!   `url(`, quotes, semicolons and braces;
//! - every selector is fully escaped.
//!
//! A class that fails any of these produces nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::style_palette::PALETTE;

// --- Values. --------------------------------------------------------------------------------

/// An arbitrary value `[...]`, checked. `_` is a space, as in Tailwind. `None` for anything
/// that could leave the declaration: a `;`, a brace, a quote, a backslash, `url(` and other
/// functions that fetch or run.
fn arbitrary(v: &str) -> Option<String> {
    let inner = v.strip_prefix('[')?.strip_suffix(']')?;
    if inner.is_empty() || inner.len() > 160 {
        return None;
    }
    let ok = inner.chars().all(|c| c.is_ascii_alphanumeric() || "#%.,_()+-*/ ".contains(c));
    let lower = inner.to_ascii_lowercase();
    let banned = ["url(", "expression(", "image-set(", "image(", "element(", "src(", "attr(", "paint("];
    if !ok || banned.iter().any(|b| lower.contains(b)) {
        return None;
    }
    // Parentheses must balance, so a value cannot close a function it did not open.
    let mut depth = 0i32;
    for c in inner.chars() {
        depth += match c {
            '(' => 1,
            ')' => -1,
            _ => 0,
        };
        if depth < 0 {
            return None;
        }
    }
    (depth == 0).then(|| inner.replace('_', " "))
}

/// A plain number: `4`, `0.5`, `1.5`, `12`. Not `-1`, not `1e3`.
fn number(v: &str) -> Option<f64> {
    if v.is_empty() || v.len() > 8 || !v.chars().all(|c| c.is_ascii_digit() || c == '.') || v.starts_with('.') || v.ends_with('.') {
        return None;
    }
    v.parse::<f64>().ok().filter(|n| n.is_finite())
}
fn int(v: &str) -> Option<i64> {
    if v.is_empty() || v.len() > 4 || !v.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    v.parse().ok()
}
fn fmt_num(n: f64) -> String {
    let s = format!("{n:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() { "0".into() } else { s.to_string() }
}

/// The spacing scale: `4` is `calc(var(--spacing) * 4)`, `px` one pixel, and Helix's own
/// paddings by name.
fn spacing(v: &str) -> Option<String> {
    if let Some(a) = arbitrary(v) {
        return Some(a);
    }
    match v {
        "px" => return Some("1px".into()),
        "0" => return Some("0px".into()),
        "card" => return Some("var(--pad-card)".into()),
        "list" => return Some("var(--pad-list)".into()),
        "overlay" => return Some("var(--pad-overlay)".into()),
        "sheet" => return Some("var(--pad-sheet)".into()),
        "inset" => return Some("var(--inset)".into()),
        _ => {}
    }
    let n = number(v)?;
    // Tailwind takes any multiple of a quarter step.
    if (n * 4.0).fract() != 0.0 {
        return None;
    }
    Some(format!("calc(var(--spacing) * {})", fmt_num(n)))
}

/// A fraction: `1/2` is 50%.
fn fraction(v: &str) -> Option<String> {
    let (a, b) = v.split_once('/')?;
    let (a, b) = (int(a)?, int(b)?);
    if b == 0 || a > b {
        return None;
    }
    Some(format!("calc({a} / {b} * 100%)"))
}

const CONTAINERS: &[(&str, &str)] = &[
    ("3xs", "16rem"), ("2xs", "18rem"), ("xs", "20rem"), ("sm", "24rem"), ("md", "28rem"), ("lg", "32rem"), ("xl", "36rem"),
    ("2xl", "42rem"), ("3xl", "48rem"), ("4xl", "56rem"), ("5xl", "64rem"), ("6xl", "72rem"), ("7xl", "80rem"),
];
fn lookup(table: &[(&str, &str)], k: &str) -> Option<String> {
    table.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
}

/// A size for `w-`, `h-`, `min-`, `max-` and `basis-`. `axis` is 'w' or 'h' (for `screen`).
fn size(v: &str, axis: char) -> Option<String> {
    let screen = if axis == 'w' { "100vw" } else { "100vh" };
    let named = match v {
        "auto" => Some("auto".to_string()),
        "full" => Some("100%".into()),
        "screen" => Some(screen.into()),
        "svw" | "lvw" | "dvw" | "svh" | "lvh" | "dvh" => Some(format!("100{v}")),
        "min" => Some("min-content".into()),
        "max" => Some("max-content".into()),
        "fit" => Some("fit-content".into()),
        "prose" => Some("65ch".into()),
        _ => None,
    };
    named.or_else(|| fraction(v)).or_else(|| lookup(CONTAINERS, v).map(|_| format!("var(--container-{v})"))).or_else(|| spacing(v))
}

/// Helix's design tokens as colours: the app's computed roles, in its light or dark mode.
const TOKEN_COLOURS: &[&str] = &[
    "bg", "surface", "surface-2", "surface-3", "fg", "muted", "line", "line-strong", "accent", "accent-fg", "accent-soft",
    "accent-text", "danger", "danger-soft", "success", "success-soft", "warning", "warning-soft", "focus", "chart-1", "chart-2",
    "chart-3", "chart-4", "chart-5", "chart-6", "hover", "press", "scrim",
];

/// A colour, with an optional `/opacity`: Tailwind's palette, Helix's tokens, the keywords, or
/// an arbitrary value.
fn colour(v: &str) -> Option<String> {
    let (name, alpha) = match v.rsplit_once('/') {
        Some((n, a)) if !n.ends_with(']') || a.starts_with('[') || int(a).is_some() => (n, Some(a)),
        _ => (v, None),
    };
    let base = match name {
        "black" => "#000".to_string(),
        "white" => "#fff".into(),
        "transparent" => "transparent".into(),
        "current" => "currentColor".into(),
        "inherit" => "inherit".into(),
        "card" => "var(--card-bg)".into(),
        "overlay" => "var(--overlay-bg)".into(),
        "frame" => "var(--frame-bg)".into(),
        "page" => "var(--page)".into(),
        n if TOKEN_COLOURS.contains(&n) => format!("var(--{n})"),
        n if n.starts_with('[') => arbitrary(n)?,
        n => PALETTE.iter().find(|(k, _)| *k == n).map(|(_, c)| c.to_string())?,
    };
    match alpha {
        None => Some(base),
        Some(a) => {
            let pct = if let Some(x) = arbitrary(a) { x } else { format!("{}%", int(a).filter(|n| *n <= 100)?) };
            Some(format!("color-mix(in oklab, {base} {pct}, transparent)"))
        }
    }
}

const RADII: &[(&str, &str)] = &[
    ("none", "0"), ("xs", "0.125rem"), ("sm", "0.25rem"), ("md", "0.375rem"), ("lg", "0.5rem"), ("xl", "0.75rem"), ("2xl", "1rem"),
    ("3xl", "1.5rem"), ("4xl", "2rem"), ("full", "9999px"),
    // Helix's geometry: concentric by construction (ADR 0056).
    ("card", "var(--r-card)"), ("control", "var(--r-ctl)"), ("field", "var(--r-field)"), ("chip", "var(--r-chip)"),
    ("list", "var(--r-list)"), ("row", "var(--r-row)"), ("overlay", "var(--r-overlay)"), ("item", "var(--r-item)"),
    ("sheet", "var(--r-sheet)"), ("window", "var(--r-window)"), ("in-card", "var(--r-in-card)"), ("in-sheet", "var(--r-in-sheet)"),
];
fn radius(v: &str) -> Option<String> {
    if v.is_empty() {
        return Some("0.25rem".into());
    }
    lookup(RADII, v).or_else(|| arbitrary(v))
}

const TEXT: &[(&str, &str, &str)] = &[
    ("xs", "0.75rem", "calc(1 / 0.75)"), ("sm", "0.875rem", "calc(1.25 / 0.875)"), ("base", "1rem", "calc(1.5 / 1)"),
    ("lg", "1.125rem", "calc(1.75 / 1.125)"), ("xl", "1.25rem", "calc(1.75 / 1.25)"), ("2xl", "1.5rem", "calc(2 / 1.5)"),
    ("3xl", "1.875rem", "calc(2.25 / 1.875)"), ("4xl", "2.25rem", "calc(2.5 / 2.25)"), ("5xl", "3rem", "1"), ("6xl", "3.75rem", "1"),
    ("7xl", "4.5rem", "1"), ("8xl", "6rem", "1"), ("9xl", "8rem", "1"),
];
/// Helix's type styles (ADR 0056), after Apple's: each a whole `font` with its tracking.
const TYPE_STYLES: &[&str] = &["large-title", "title-1", "title-2", "title-3", "headline", "body", "callout", "footnote", "caption"];
const WEIGHTS: &[(&str, &str)] = &[
    ("thin", "100"), ("extralight", "200"), ("light", "300"), ("normal", "400"), ("medium", "500"), ("semibold", "600"), ("bold", "700"),
    ("extrabold", "800"), ("black", "900"),
];
const TRACKING: &[(&str, &str)] =
    &[("tighter", "-0.05em"), ("tight", "-0.025em"), ("normal", "0em"), ("wide", "0.025em"), ("wider", "0.05em"), ("widest", "0.1em")];
const LEADING: &[(&str, &str)] = &[("none", "1"), ("tight", "1.25"), ("snug", "1.375"), ("normal", "1.5"), ("relaxed", "1.625"), ("loose", "2")];
const SHADOWS: &[(&str, &str)] = &[
    ("2xs", "0 1px rgb(0 0 0 / 0.05)"), ("xs", "0 1px 2px 0 rgb(0 0 0 / 0.05)"),
    ("sm", "0 1px 3px 0 rgb(0 0 0 / 0.1), 0 1px 2px -1px rgb(0 0 0 / 0.1)"),
    ("md", "0 4px 6px -1px rgb(0 0 0 / 0.1), 0 2px 4px -2px rgb(0 0 0 / 0.1)"),
    ("lg", "0 10px 15px -3px rgb(0 0 0 / 0.1), 0 4px 6px -4px rgb(0 0 0 / 0.1)"),
    ("xl", "0 20px 25px -5px rgb(0 0 0 / 0.1), 0 8px 10px -6px rgb(0 0 0 / 0.1)"), ("2xl", "0 25px 50px -12px rgb(0 0 0 / 0.25)"),
    ("none", "0 0 #0000"),
    // Helix's elevations, which know light from dark.
    ("1", "var(--e-1)"), ("2", "var(--e-2)"), ("3", "var(--e-3)"),
];
const BLURS: &[(&str, &str)] =
    &[("none", "0"), ("xs", "4px"), ("sm", "8px"), ("md", "12px"), ("lg", "16px"), ("xl", "24px"), ("2xl", "40px"), ("3xl", "64px")];
const BREAKPOINTS: &[(&str, &str)] = &[("sm", "40rem"), ("md", "48rem"), ("lg", "64rem"), ("xl", "80rem"), ("2xl", "96rem")];

// --- Utilities. -----------------------------------------------------------------------------

/// What one utility writes: declarations, a selector suffix (for `space-y-4`, the children it
/// spaces), the keyframes it needs, and its rank among utilities of the same property family
/// (a shorthand before its sides, so `p-4 pt-2` keeps the 2).
struct Rule {
    decls: Vec<(String, String)>,
    suffix: &'static str,
    keyframes: Option<&'static str>,
    rank: u8,
}
fn rule(decls: &[(&str, String)]) -> Option<Rule> {
    Some(Rule { decls: decls.iter().map(|(p, v)| (p.to_string(), v.clone())).collect(), suffix: "", keyframes: None, rank: 0 })
}
fn ranked(r: Option<Rule>, rank: u8) -> Option<Rule> {
    r.map(|mut r| {
        r.rank = rank;
        r
    })
}
fn one(p: &str, v: impl Into<String>) -> Option<Rule> {
    rule(&[(p, v.into())])
}

const SHADOW_STACK: &str = "var(--tw-inset-shadow, 0 0 #0000), var(--tw-inset-ring-shadow, 0 0 #0000), var(--tw-ring-offset-shadow, 0 0 #0000), var(--tw-ring-shadow, 0 0 #0000), var(--tw-shadow, 0 0 #0000)";
const FILTER_STACK: &str = "var(--tw-blur,) var(--tw-brightness,) var(--tw-contrast,) var(--tw-grayscale,) var(--tw-saturate,) var(--tw-drop-shadow,)";
const BACKDROP_STACK: &str = "var(--tw-backdrop-blur,) var(--tw-backdrop-brightness,) var(--tw-backdrop-saturate,)";
const TRANSITION_DEFAULT: &str = "color, background-color, border-color, outline-color, text-decoration-color, fill, stroke, --tw-gradient-from, --tw-gradient-via, --tw-gradient-to, opacity, box-shadow, transform, translate, scale, rotate, filter, -webkit-backdrop-filter, backdrop-filter";

const KF_SPIN: &str = "@keyframes spin { to { transform: rotate(360deg); } }";
const KF_PING: &str = "@keyframes ping { 75%, 100% { transform: scale(2); opacity: 0; } }";
const KF_PULSE: &str = "@keyframes pulse { 50% { opacity: 0.5; } }";
const KF_BOUNCE: &str = "@keyframes bounce { 0%, 100% { transform: translateY(-25%); animation-timing-function: cubic-bezier(0.8, 0, 1, 1); } 50% { transform: none; animation-timing-function: cubic-bezier(0, 0, 0.2, 1); } }";

/// Sides and corners: the CSS properties a side suffix names.
fn sides(prefix: &str, s: &str) -> Option<(Vec<&'static str>, u8)> {
    let v: Vec<&'static str> = match (prefix, s) {
        ("p", "") => vec!["padding"],
        ("p", "x") => vec!["padding-inline"],
        ("p", "y") => vec!["padding-block"],
        ("p", "t") => vec!["padding-top"],
        ("p", "r") => vec!["padding-right"],
        ("p", "b") => vec!["padding-bottom"],
        ("p", "l") => vec!["padding-left"],
        ("p", "s") => vec!["padding-inline-start"],
        ("p", "e") => vec!["padding-inline-end"],
        ("m", "") => vec!["margin"],
        ("m", "x") => vec!["margin-inline"],
        ("m", "y") => vec!["margin-block"],
        ("m", "t") => vec!["margin-top"],
        ("m", "r") => vec!["margin-right"],
        ("m", "b") => vec!["margin-bottom"],
        ("m", "l") => vec!["margin-left"],
        ("m", "s") => vec!["margin-inline-start"],
        ("m", "e") => vec!["margin-inline-end"],
        _ => return None,
    };
    let rank = match s {
        "" => 0,
        "x" | "y" => 1,
        _ => 2,
    };
    Some((v, rank))
}

/// The CSS for one utility (the class without variants, `!` or a leading `-`).
fn utility(u: &str, negative: bool) -> Option<Rule> {
    let neg = |v: String| if negative { format!("calc({v} * -1)") } else { v };
    // Names with no value.
    let fixed: Option<Option<Rule>> = Some(match u {
        "block" | "inline" | "inline-block" | "flex" | "inline-flex" | "grid" | "inline-grid" | "contents" | "table" | "flow-root" | "list-item" => one("display", u),
        "hidden" => one("display", "none"),
        "static" | "fixed" | "absolute" | "relative" | "sticky" => one("position", u),
        "visible" => one("visibility", "visible"),
        "invisible" => one("visibility", "hidden"),
        "isolate" => one("isolation", "isolate"),
        "box-border" => one("box-sizing", "border-box"),
        "box-content" => one("box-sizing", "content-box"),
        "flex-row" => one("flex-direction", "row"),
        "flex-row-reverse" => one("flex-direction", "row-reverse"),
        "flex-col" => one("flex-direction", "column"),
        "flex-col-reverse" => one("flex-direction", "column-reverse"),
        "flex-wrap" => one("flex-wrap", "wrap"),
        "flex-nowrap" => one("flex-wrap", "nowrap"),
        "flex-wrap-reverse" => one("flex-wrap", "wrap-reverse"),
        "flex-1" => one("flex", "1"),
        "flex-auto" => one("flex", "auto"),
        "flex-initial" => one("flex", "0 auto"),
        "flex-none" => one("flex", "none"),
        "grow" => one("flex-grow", "1"),
        "grow-0" => one("flex-grow", "0"),
        "shrink" => one("flex-shrink", "1"),
        "shrink-0" => one("flex-shrink", "0"),
        "grid-flow-row" => one("grid-auto-flow", "row"),
        "grid-flow-col" => one("grid-auto-flow", "column"),
        "grid-flow-dense" => one("grid-auto-flow", "dense"),
        "col-span-full" => one("grid-column", "1 / -1"),
        "row-span-full" => one("grid-row", "1 / -1"),
        "italic" => one("font-style", "italic"),
        "not-italic" => one("font-style", "normal"),
        "underline" => one("text-decoration-line", "underline"),
        "line-through" => one("text-decoration-line", "line-through"),
        "no-underline" => one("text-decoration-line", "none"),
        "uppercase" | "lowercase" | "capitalize" => one("text-transform", u),
        "normal-case" => one("text-transform", "none"),
        "truncate" => rule(&[("overflow", "hidden".into()), ("text-overflow", "ellipsis".into()), ("white-space", "nowrap".into())]),
        "text-ellipsis" => one("text-overflow", "ellipsis"),
        "text-clip" => one("text-overflow", "clip"),
        "text-wrap" | "text-nowrap" | "text-balance" | "text-pretty" => one("text-wrap", &u[5..]),
        "break-words" => one("overflow-wrap", "break-word"),
        "break-all" => one("word-break", "break-all"),
        "break-keep" => one("word-break", "keep-all"),
        "tabular-nums" => one("font-variant-numeric", "tabular-nums"),
        "proportional-nums" => one("font-variant-numeric", "proportional-nums"),
        "slashed-zero" => one("font-variant-numeric", "slashed-zero"),
        "antialiased" => rule(&[("-webkit-font-smoothing", "antialiased".into()), ("-moz-osx-font-smoothing", "grayscale".into())]),
        "subpixel-antialiased" => rule(&[("-webkit-font-smoothing", "auto".into()), ("-moz-osx-font-smoothing", "auto".into())]),
        "font-sans" => one("font-family", "var(--font)"),
        "font-mono" => one("font-family", "var(--mono)"),
        "font-serif" => one("font-family", "ui-serif, Georgia, Cambria, \"Times New Roman\", Times, serif"),
        "text-left" | "text-center" | "text-right" | "text-justify" | "text-start" | "text-end" => one("text-align", &u[5..]),
        "align-top" | "align-middle" | "align-bottom" | "align-baseline" | "align-text-top" | "align-text-bottom" => one("vertical-align", &u[6..]),
        "list-none" | "list-disc" | "list-decimal" => one("list-style-type", &u[5..]),
        "list-inside" | "list-outside" => one("list-style-position", &u[5..]),
        "whitespace-normal" | "whitespace-nowrap" | "whitespace-pre" | "whitespace-pre-line" | "whitespace-pre-wrap" | "whitespace-break-spaces" => {
            one("white-space", &u[11..])
        }
        "select-none" | "select-text" | "select-all" | "select-auto" => {
            rule(&[("-webkit-user-select", u[7..].into()), ("user-select", u[7..].into())])
        }
        "pointer-events-none" | "pointer-events-auto" => one("pointer-events", &u[15..]),
        "resize" => one("resize", "both"),
        "resize-none" | "resize-x" | "resize-y" => one("resize", match &u[7..] { "x" => "horizontal", "y" => "vertical", _ => "none" }),
        "appearance-none" => one("appearance", "none"),
        "outline-none" => rule(&[("outline", "2px solid transparent".into()), ("outline-offset", "2px".into())]),
        "outline-hidden" => rule(&[("outline", "2px solid transparent".into()), ("outline-offset", "2px".into())]),
        "outline" => rule(&[("outline-style", "solid".into()), ("outline-width", "1px".into())]),
        "border" => ranked(rule(&[("border-style", "solid".into()), ("border-width", "1px".into()), ("border-color", "var(--tw-border, var(--line))".into())]), 0),
        "border-x" | "border-y" | "border-t" | "border-r" | "border-b" | "border-l" => border_width(&u[7..], "1px"),
        "border-solid" | "border-dashed" | "border-dotted" | "border-double" | "border-none" => one("border-style", &u[7..]),
        "rounded" => one("border-radius", "0.25rem"),
        "shadow" => shadow_of("0 1px 3px 0 rgb(0 0 0 / 0.1), 0 1px 2px -1px rgb(0 0 0 / 0.1)"),
        "ring" => ring_of("1px"),
        "ring-inset" => one("--tw-ring-inset", "inset"),
        "transition" => transition_of(TRANSITION_DEFAULT),
        "transition-all" => transition_of("all"),
        "transition-colors" => transition_of("color, background-color, border-color, outline-color, text-decoration-color, fill, stroke"),
        "transition-opacity" => transition_of("opacity"),
        "transition-shadow" => transition_of("box-shadow"),
        "transition-transform" => transition_of("transform, translate, scale, rotate"),
        "transition-none" => one("transition-property", "none"),
        "ease-linear" => one("transition-timing-function", "linear"),
        "ease-in" => one("transition-timing-function", "cubic-bezier(0.4, 0, 1, 1)"),
        "ease-out" => one("transition-timing-function", "cubic-bezier(0, 0, 0.2, 1)"),
        "ease-in-out" => one("transition-timing-function", "cubic-bezier(0.4, 0, 0.2, 1)"),
        // Helix's springs, solved into curves (ADR 0056): a transition that moves like the rest.
        "ease-spring" => rule(&[("transition-timing-function", "var(--spring-spatial)".into()), ("transition-duration", "calc(var(--spring-spatial-ms) * 1ms)".into())]),
        "ease-spring-fast" => rule(&[("transition-timing-function", "var(--spring-spatial-fast)".into()), ("transition-duration", "calc(var(--spring-spatial-fast-ms) * 1ms)".into())]),
        "ease-spring-slow" => rule(&[("transition-timing-function", "var(--spring-spatial-slow)".into()), ("transition-duration", "calc(var(--spring-spatial-slow-ms) * 1ms)".into())]),
        "animate-none" => one("animation", "none"),
        "animate-spin" => Some(Rule { keyframes: Some(KF_SPIN), ..rule(&[("animation", "spin 1s linear infinite".into())])? }),
        "animate-ping" => Some(Rule { keyframes: Some(KF_PING), ..rule(&[("animation", "ping 1s cubic-bezier(0, 0, 0.2, 1) infinite".into())])? }),
        "animate-pulse" => Some(Rule { keyframes: Some(KF_PULSE), ..rule(&[("animation", "pulse 2s cubic-bezier(0.4, 0, 0.6, 1) infinite".into())])? }),
        "animate-bounce" => Some(Rule { keyframes: Some(KF_BOUNCE), ..rule(&[("animation", "bounce 1s infinite".into())])? }),
        "object-contain" | "object-cover" | "object-fill" | "object-none" | "object-scale-down" => one("object-fit", &u[7..]),
        "object-center" | "object-top" | "object-bottom" | "object-left" | "object-right" => one("object-position", &u[7..]),
        "aspect-square" => one("aspect-ratio", "1 / 1"),
        "aspect-video" => one("aspect-ratio", "16 / 9"),
        "aspect-auto" => one("aspect-ratio", "auto"),
        "@container" => one("container-type", "inline-size"),
        "sr-only" => rule(&[
            ("position", "absolute".into()), ("width", "1px".into()), ("height", "1px".into()), ("padding", "0".into()), ("margin", "-1px".into()),
            ("overflow", "hidden".into()), ("clip-path", "inset(50%)".into()), ("white-space", "nowrap".into()), ("border-width", "0".into()),
        ]),
        "not-sr-only" => rule(&[("position", "static".into()), ("width", "auto".into()), ("height", "auto".into()), ("margin", "0".into()), ("overflow", "visible".into()), ("clip-path", "none".into()), ("white-space", "normal".into())]),
        "bg-cover" | "bg-contain" | "bg-auto" => one("background-size", &u[3..]),
        "bg-center" | "bg-top" | "bg-bottom" | "bg-left" | "bg-right" => one("background-position", &u[3..]),
        "bg-no-repeat" | "bg-repeat" | "bg-repeat-x" | "bg-repeat-y" => one("background-repeat", &u[3..]),
        "bg-fixed" | "bg-local" | "bg-scroll" => one("background-attachment", &u[3..]),
        "bg-none" => one("background-image", "none"),
        "scroll-smooth" => one("scroll-behavior", "smooth"),
        "snap-x" => one("scroll-snap-type", "x var(--tw-snap, proximity)"),
        "snap-y" => one("scroll-snap-type", "y var(--tw-snap, proximity)"),
        "snap-mandatory" => one("--tw-snap", "mandatory"),
        "snap-start" | "snap-center" | "snap-end" => one("scroll-snap-align", &u[5..]),
        "touch-none" | "touch-auto" | "touch-manipulation" | "touch-pan-x" | "touch-pan-y" => one("touch-action", &u[6..]),
        "overscroll-contain" | "overscroll-none" | "overscroll-auto" => one("overscroll-behavior", &u[11..]),
        "divide-y" => Some(Rule { suffix: " > :not(:last-child)", ..rule(&[("border-bottom-width", "1px".into()), ("border-style", "solid".into()), ("border-color", "var(--tw-divide, var(--line))".into())])? }),
        "divide-x" => Some(Rule { suffix: " > :not(:last-child)", ..rule(&[("border-right-width", "1px".into()), ("border-style", "solid".into()), ("border-color", "var(--tw-divide, var(--line))".into())])? }),
        "grayscale" => filter_of("--tw-grayscale", "grayscale(100%)"),
        "grayscale-0" => filter_of("--tw-grayscale", "grayscale(0%)"),
        _ => return valued(u, negative, &neg),
    });
    fixed.flatten()
}

fn border_width(side: &str, w: &str) -> Option<Rule> {
    let (props, rank): (Vec<&str>, u8) = match side {
        "" => (vec!["border-width"], 0),
        "x" => (vec!["border-inline-width"], 1),
        "y" => (vec!["border-block-width"], 1),
        "t" => (vec!["border-top-width"], 2),
        "r" => (vec!["border-right-width"], 2),
        "b" => (vec!["border-bottom-width"], 2),
        "l" => (vec!["border-left-width"], 2),
        _ => return None,
    };
    let mut d: Vec<(String, String)> = vec![("border-style".into(), "solid".into())];
    d.extend(props.iter().map(|p| (p.to_string(), w.to_string())));
    d.push(("border-color".into(), "var(--tw-border, var(--line))".into()));
    Some(Rule { decls: d, suffix: "", keyframes: None, rank })
}
fn shadow_of(v: &str) -> Option<Rule> {
    rule(&[("--tw-shadow", v.into()), ("box-shadow", SHADOW_STACK.into())])
}
fn ring_of(w: &str) -> Option<Rule> {
    rule(&[("--tw-ring-shadow", format!("var(--tw-ring-inset,) 0 0 0 calc({w} + var(--tw-ring-offset-width, 0px)) var(--tw-ring-color, var(--focus))")), ("box-shadow", SHADOW_STACK.into())])
}
fn transition_of(props: &str) -> Option<Rule> {
    rule(&[
        ("transition-property", props.into()),
        ("transition-timing-function", "var(--tw-ease, cubic-bezier(0.4, 0, 0.2, 1))".into()),
        ("transition-duration", "var(--tw-duration, 150ms)".into()),
    ])
}
fn filter_of(var: &str, v: &str) -> Option<Rule> {
    rule(&[(var, v.into()), ("filter", FILTER_STACK.into())])
}
fn backdrop_of(var: &str, v: &str) -> Option<Rule> {
    rule(&[(var, v.into()), ("-webkit-backdrop-filter", BACKDROP_STACK.into()), ("backdrop-filter", BACKDROP_STACK.into())])
}

/// Utilities with a value: `p-4`, `bg-red-500/50`, `grid-cols-3`, `w-[320px]`.
fn valued(u: &str, negative: bool, neg: &dyn Fn(String) -> String) -> Option<Rule> {
    // Padding and margin, with sides.
    for (pre, can_neg) in [("p", false), ("m", true)] {
        if let Some(rest) = u.strip_prefix(pre) {
            let (side, v) = match rest.split_once('-') {
                Some((s, v)) if s.len() <= 1 => (s, v),
                _ => continue,
            };
            let (props, rank) = sides(pre, side)?;
            if negative && !can_neg {
                return None;
            }
            let val = if pre == "m" && v == "auto" { "auto".to_string() } else { neg(spacing(v)?) };
            let decls: Vec<(String, String)> = props.iter().map(|p| (p.to_string(), val.clone())).collect();
            return Some(Rule { decls, suffix: "", keyframes: None, rank });
        }
    }
    let (head, v) = split_value(u)?;
    match head {
        "w" => one("width", size(v, 'w')?),
        "h" => one("height", size(v, 'h')?),
        "size" => {
            let s = size(v, 'w')?;
            rule(&[("width", s.clone()), ("height", s)])
        }
        "min-w" => one("min-width", size(v, 'w')?),
        "min-h" => one("min-height", size(v, 'h')?),
        "max-w" => one("max-width", if v == "none" { "none".into() } else { size(v, 'w')? }),
        "max-h" => one("max-height", if v == "none" { "none".into() } else { size(v, 'h')? }),
        "basis" => one("flex-basis", size(v, 'w')?),
        "gap" => one("gap", spacing(v)?),
        "gap-x" => ranked(one("column-gap", spacing(v)?), 1),
        "gap-y" => ranked(one("row-gap", spacing(v)?), 1),
        "space-x" => Some(Rule { suffix: " > :not(:last-child)", ..one("margin-inline-end", neg(spacing(v)?))? }),
        "space-y" => Some(Rule { suffix: " > :not(:last-child)", ..one("margin-block-end", neg(spacing(v)?))? }),
        "inset" => ranked(one("inset", inset_value(v, neg)?), 0),
        "inset-x" => ranked(one("inset-inline", inset_value(v, neg)?), 1),
        "inset-y" => ranked(one("inset-block", inset_value(v, neg)?), 1),
        "top" | "right" | "bottom" | "left" => ranked(one(head, inset_value(v, neg)?), 2),
        "start" => ranked(one("inset-inline-start", inset_value(v, neg)?), 2),
        "end" => ranked(one("inset-inline-end", inset_value(v, neg)?), 2),
        "z" => one("z-index", if v == "auto" { "auto".into() } else { int(v).map(|n| neg(n.to_string())).or_else(|| arbitrary(v))? }),
        "order" => one("order", match v { "first" => "-9999".into(), "last" => "9999".into(), "none" => "0".into(), _ => neg(int(v)?.to_string()) }),
        "grid-cols" | "grid-rows" => {
            let prop = if head == "grid-cols" { "grid-template-columns" } else { "grid-template-rows" };
            one(prop, match v {
                "none" => "none".to_string(),
                "subgrid" => "subgrid".into(),
                _ => match int(v) {
                    Some(n) if (1..=24).contains(&n) => format!("repeat({n}, minmax(0, 1fr))"),
                    _ => arbitrary(v)?,
                },
            })
        }
        "col-span" | "row-span" => {
            let n = int(v).filter(|n| (1..=24).contains(n))?;
            one(if head == "col-span" { "grid-column" } else { "grid-row" }, format!("span {n} / span {n}"))
        }
        "col-start" | "col-end" | "row-start" | "row-end" => {
            let prop = match head { "col-start" => "grid-column-start", "col-end" => "grid-column-end", "row-start" => "grid-row-start", _ => "grid-row-end" };
            one(prop, if v == "auto" { "auto".into() } else { neg(int(v)?.to_string()) })
        }
        "auto-cols" | "auto-rows" => {
            let prop = if head == "auto-cols" { "grid-auto-columns" } else { "grid-auto-rows" };
            one(prop, match v { "auto" => "auto".into(), "min" => "min-content".into(), "max" => "max-content".into(), "fr" => "minmax(0, 1fr)".into(), _ => arbitrary(v)? })
        }
        "justify" => one("justify-content", align_value(v)?),
        "justify-items" => one("justify-items", v.to_string()).filter(|_| ["start", "end", "center", "stretch", "normal"].contains(&v)),
        "justify-self" => one("justify-self", v.to_string()).filter(|_| ["auto", "start", "end", "center", "stretch"].contains(&v)),
        "items" => one("align-items", match v { "start" => "flex-start", "end" => "flex-end", "center" => "center", "baseline" => "baseline", "stretch" => "stretch", _ => return None }),
        "content" => one("align-content", align_value(v)?),
        "self" => one("align-self", match v { "auto" => "auto", "start" => "flex-start", "end" => "flex-end", "center" => "center", "stretch" => "stretch", "baseline" => "baseline", _ => return None }),
        "place-content" | "place-items" | "place-self" => one(head, v.to_string()).filter(|_| ["start", "end", "center", "stretch", "between", "around", "evenly", "baseline", "auto"].contains(&v)).map(|mut r| {
            if v == "between" || v == "around" || v == "evenly" {
                r.decls[0].1 = format!("space-{v}");
            }
            r
        }),
        "overflow" | "overflow-x" | "overflow-y" => one(head, v.to_string()).filter(|_| ["auto", "hidden", "clip", "visible", "scroll"].contains(&v)),
        "text" => {
            if let Some((sz, lh)) = TEXT.iter().find(|(n, _, _)| *n == v).map(|(_, s, l)| (s.to_string(), l.to_string())) {
                return rule(&[("font-size", sz), ("line-height", lh)]);
            }
            if let Some((name, lead)) = v.split_once('/')
                && let Some((_, sz, _)) = TEXT.iter().find(|(n, _, _)| *n == name)
            {
                return rule(&[("font-size", sz.to_string()), ("line-height", spacing(lead).or_else(|| lookup(LEADING, lead))?)]);
            }
            if TYPE_STYLES.contains(&v) {
                return rule(&[("font", format!("var(--type-{v})")), ("letter-spacing", format!("var(--track-{v})"))]);
            }
            if let Some(a) = arbitrary(v) {
                // An arbitrary text value is a size when it looks like a length, else a colour.
                let is_len = a.ends_with("px") || a.ends_with("rem") || a.ends_with("em") || a.starts_with("clamp(");
                return one(if is_len { "font-size" } else { "color" }, a);
            }
            one("color", colour(v)?)
        }
        "font" => one("font-weight", lookup(WEIGHTS, v).or_else(|| int(v).filter(|n| (100..=900).contains(n)).map(|n| n.to_string()))?),
        "tracking" => one("letter-spacing", lookup(TRACKING, v).or_else(|| arbitrary(v))?),
        "leading" => one("line-height", lookup(LEADING, v).or_else(|| spacing(v))?),
        "line-clamp" => {
            if v == "none" {
                return rule(&[("overflow", "visible".into()), ("display", "block".into()), ("-webkit-box-orient", "horizontal".into()), ("-webkit-line-clamp", "unset".into())]);
            }
            let n = int(v).filter(|n| (1..=12).contains(n))?;
            rule(&[("overflow", "hidden".into()), ("display", "-webkit-box".into()), ("-webkit-box-orient", "vertical".into()), ("-webkit-line-clamp", n.to_string())])
        }
        "indent" => one("text-indent", neg(spacing(v)?)),
        "bg" => {
            if let Some(dir) = v.strip_prefix("linear-to-") {
                let to = match dir { "t" => "top", "tr" => "top right", "r" => "right", "br" => "bottom right", "b" => "bottom", "bl" => "bottom left", "l" => "left", "tl" => "top left", _ => return None };
                return one("background-image", format!("linear-gradient(to {to}, var(--tw-gradient-stops))"));
            }
            if let Some(angle) = v.strip_prefix("linear-") {
                let deg = int(angle).filter(|n| *n <= 360)?;
                return one("background-image", format!("linear-gradient({}deg, var(--tw-gradient-stops))", if negative { -deg } else { deg }));
            }
            if v == "radial" {
                return one("background-image", "radial-gradient(var(--tw-gradient-stops))");
            }
            one("background-color", colour(v)?)
        }
        "from" => ranked(rule(&[("--tw-gradient-from", colour(v)?), ("--tw-gradient-stops", "var(--tw-gradient-from), var(--tw-gradient-to, transparent)".into())]), 0),
        "via" => ranked(rule(&[("--tw-gradient-via", colour(v)?), ("--tw-gradient-stops", "var(--tw-gradient-from, transparent), var(--tw-gradient-via), var(--tw-gradient-to, transparent)".into())]), 1),
        "to" => ranked(one("--tw-gradient-to", colour(v)?), 2),
        "border" | "border-x" | "border-y" | "border-t" | "border-r" | "border-b" | "border-l" => {
            let side = head.strip_prefix("border").unwrap_or("").trim_start_matches('-');
            if let Some(n) = int(v) {
                return border_width(side, &format!("{n}px"));
            }
            if side.is_empty() {
                return ranked(rule(&[("--tw-border", colour(v)?), ("border-color", colour(v)?)]), 3);
            }
            None
        }
        "rounded" | "rounded-t" | "rounded-r" | "rounded-b" | "rounded-l" | "rounded-tl" | "rounded-tr" | "rounded-br" | "rounded-bl" | "rounded-s" | "rounded-e" => {
            let r = radius(v)?;
            let (props, rank): (Vec<&str>, u8) = match head {
                "rounded" => (vec!["border-radius"], 0),
                "rounded-t" => (vec!["border-top-left-radius", "border-top-right-radius"], 1),
                "rounded-r" => (vec!["border-top-right-radius", "border-bottom-right-radius"], 1),
                "rounded-b" => (vec!["border-bottom-right-radius", "border-bottom-left-radius"], 1),
                "rounded-l" => (vec!["border-top-left-radius", "border-bottom-left-radius"], 1),
                "rounded-s" => (vec!["border-start-start-radius", "border-end-start-radius"], 1),
                "rounded-e" => (vec!["border-start-end-radius", "border-end-end-radius"], 1),
                "rounded-tl" => (vec!["border-top-left-radius"], 2),
                "rounded-tr" => (vec!["border-top-right-radius"], 2),
                "rounded-br" => (vec!["border-bottom-right-radius"], 2),
                _ => (vec!["border-bottom-left-radius"], 2),
            };
            Some(Rule { decls: props.iter().map(|p| (p.to_string(), r.clone())).collect(), suffix: "", keyframes: None, rank })
        }
        "outline" => {
            if let Some(n) = int(v) {
                return rule(&[("outline-style", "solid".into()), ("outline-width", format!("{n}px"))]);
            }
            one("outline-color", colour(v)?)
        }
        "outline-offset" => one("outline-offset", neg(format!("{}px", int(v)?))),
        "shadow" => {
            if let Some(s) = lookup(SHADOWS, v) {
                return shadow_of(&s);
            }
            one("--tw-shadow-color", colour(v)?)
        }
        "inset-shadow" => rule(&[("--tw-inset-shadow", match v { "2xs" => "inset 0 1px rgb(0 0 0 / 0.05)", "xs" => "inset 0 1px 1px rgb(0 0 0 / 0.05)", "sm" => "inset 0 2px 4px rgb(0 0 0 / 0.05)", "none" => "0 0 #0000", _ => return None }.into()), ("box-shadow", SHADOW_STACK.into())]),
        "ring" => {
            if let Some(n) = int(v) {
                return ring_of(&format!("{n}px"));
            }
            one("--tw-ring-color", colour(v)?)
        }
        "ring-offset" => {
            if let Some(n) = int(v) {
                return rule(&[("--tw-ring-offset-width", format!("{n}px")), ("--tw-ring-offset-shadow", format!("0 0 0 {n}px var(--tw-ring-offset-color, var(--surface))"))]);
            }
            one("--tw-ring-offset-color", colour(v)?)
        }
        "divide" => one("--tw-divide", colour(v)?).map(|r| Rule { suffix: " > :not(:last-child)", ..r }),
        "opacity" => one("opacity", if let Some(n) = int(v).filter(|n| *n <= 100) { format!("{n}%") } else { arbitrary(v)? }),
        "blur" => filter_of("--tw-blur", &format!("blur({})", lookup(BLURS, v).or_else(|| arbitrary(v))?)),
        "brightness" => filter_of("--tw-brightness", &format!("brightness({}%)", int(v)?)),
        "contrast" => filter_of("--tw-contrast", &format!("contrast({}%)", int(v)?)),
        "saturate" => filter_of("--tw-saturate", &format!("saturate({}%)", int(v)?)),
        "backdrop-blur" => backdrop_of("--tw-backdrop-blur", &format!("blur({})", lookup(BLURS, v).or_else(|| arbitrary(v))?)),
        "backdrop-brightness" => backdrop_of("--tw-backdrop-brightness", &format!("brightness({}%)", int(v)?)),
        "backdrop-saturate" => backdrop_of("--tw-backdrop-saturate", &format!("saturate({}%)", int(v)?)),
        "duration" => rule(&[("--tw-duration", format!("{}ms", int(v)?)), ("transition-duration", format!("{}ms", int(v)?))]),
        "delay" => one("transition-delay", format!("{}ms", int(v)?)),
        "scale" | "scale-x" | "scale-y" => {
            let n = int(v).map(|n| format!("{n}%")).or_else(|| arbitrary(v))?;
            let n = neg(n);
            match head {
                "scale" => one("scale", format!("{n} {n}")),
                "scale-x" => rule(&[("--tw-scale-x", n), ("scale", "var(--tw-scale-x, 1) var(--tw-scale-y, 1)".into())]),
                _ => rule(&[("--tw-scale-y", n), ("scale", "var(--tw-scale-x, 1) var(--tw-scale-y, 1)".into())]),
            }
        }
        "rotate" => one("rotate", neg(int(v).map(|n| format!("{n}deg")).or_else(|| arbitrary(v))?)),
        "translate-x" | "translate-y" => {
            let t = neg(fraction(v).or_else(|| if v == "full" { Some("100%".into()) } else { None }).or_else(|| spacing(v))?);
            let var = if head == "translate-x" { "--tw-translate-x" } else { "--tw-translate-y" };
            rule(&[(var, t), ("translate", "var(--tw-translate-x, 0) var(--tw-translate-y, 0)".into())])
        }
        "origin" => one("transform-origin", match v { "center" => "center", "top" => "top", "top-right" => "top right", "right" => "right", "bottom-right" => "bottom right", "bottom" => "bottom", "bottom-left" => "bottom left", "left" => "left", "top-left" => "top left", _ => return None }),
        "cursor" => one("cursor", v.to_string()).filter(|_| ["auto", "default", "pointer", "wait", "text", "move", "help", "not-allowed", "none", "progress", "crosshair", "grab", "grabbing", "col-resize", "row-resize", "zoom-in", "zoom-out"].contains(&v)),
        "fill" => one("fill", if v == "none" { "none".into() } else { colour(v)? }),
        "stroke" => {
            if let Some(n) = int(v) {
                return one("stroke-width", n.to_string());
            }
            one("stroke", if v == "none" { "none".into() } else { colour(v)? })
        }
        "accent" => one("accent-color", colour(v)?),
        "caret" => one("caret-color", colour(v)?),
        "decoration" => one("text-decoration-color", colour(v)?),
        "placeholder" => one("color", colour(v)?).map(|r| Rule { suffix: "::placeholder", ..r }),
        "aspect" => one("aspect-ratio", fraction_ratio(v).or_else(|| arbitrary(v))?),
        "columns" => one("columns", int(v).filter(|n| (1..=12).contains(n)).map(|n| n.to_string()).or_else(|| lookup(CONTAINERS, v))?),
        _ => None,
    }
}
fn fraction_ratio(v: &str) -> Option<String> {
    let (a, b) = v.split_once('/')?;
    Some(format!("{} / {}", int(a)?, int(b)?))
}
fn inset_value(v: &str, neg: &dyn Fn(String) -> String) -> Option<String> {
    match v {
        "auto" => Some("auto".into()),
        "full" => Some(neg("100%".into())),
        _ => Some(neg(fraction(v).or_else(|| spacing(v))?)),
    }
}
fn align_value(v: &str) -> Option<&'static str> {
    Some(match v {
        "start" => "flex-start",
        "end" => "flex-end",
        "center" => "center",
        "between" => "space-between",
        "around" => "space-around",
        "evenly" => "space-evenly",
        "stretch" => "stretch",
        "normal" => "normal",
        "baseline" => "baseline",
        _ => return None,
    })
}

/// A utility's family and its value: `grid-cols-3` is (`grid-cols`, `3`), `bg-red-500/50` is
/// (`bg`, `red-500/50`). The family is the longest known prefix that is followed by `-`.
fn split_value(u: &str) -> Option<(&'static str, &str)> {
    const HEADS: &[&str] = &[
        "backdrop-brightness", "backdrop-saturate", "backdrop-blur", "pointer-events", "outline-offset", "justify-items", "justify-self",
        "place-content", "place-items", "place-self", "inset-shadow", "ring-offset", "line-clamp", "overflow-x", "overflow-y", "grid-cols",
        "grid-rows", "col-span", "row-span", "col-start", "col-end", "row-start", "row-end", "auto-cols", "auto-rows", "translate-x",
        "translate-y", "decoration", "placeholder", "brightness", "rounded-tl", "rounded-tr", "rounded-br", "rounded-bl", "rounded-t",
        "rounded-r", "rounded-b", "rounded-l", "rounded-s", "rounded-e", "border-x", "border-y", "border-t", "border-r", "border-b",
        "border-l", "contrast", "saturate", "duration", "tracking", "leading", "scale-x", "scale-y", "inset-x", "inset-y", "space-x",
        "space-y", "overflow", "rounded", "content", "columns", "justify", "opacity", "outline", "divide", "border", "indent", "rotate",
        "origin", "cursor", "stroke", "accent", "aspect", "max-w", "max-h", "min-w", "min-h", "gap-x", "gap-y", "shadow", "inset",
        "items", "order", "scale", "basis", "caret", "start", "right", "bottom", "delay", "left", "self", "text", "font", "from", "size",
        "blur", "fill", "ring", "gap", "top", "end", "via", "bg", "to", "w", "h", "z",
    ];
    HEADS.iter().find_map(|h| u.strip_prefix(h).and_then(|r| r.strip_prefix('-')).filter(|v| !v.is_empty()).map(|v| (*h, v)))
}

// --- Variants. ------------------------------------------------------------------------------

/// What the variants of one class do: wrap the rule in at-rules, add to its selector, or put
/// something in front of it. `order` sorts rules so later variants win, as Tailwind's do.
#[derive(Default)]
struct Shape {
    at: Vec<String>,
    prefix: Vec<String>,
    suffix: String,
    order: u32,
}

fn apply_variant(v: &str, s: &mut Shape) -> Option<()> {
    let pseudo = |s: &mut Shape, p: &str, w: u32| {
        s.suffix.push_str(p);
        s.order += w;
    };
    match v {
        "hover" => {
            s.suffix.push_str(":hover");
            s.at.push("@media (hover: hover)".into());
            s.order += 10;
        }
        "focus" => pseudo(s, ":focus", 12),
        "focus-visible" => pseudo(s, ":focus-visible", 13),
        "focus-within" => pseudo(s, ":focus-within", 11),
        "active" => pseudo(s, ":active", 14),
        "visited" => pseudo(s, ":visited", 9),
        "disabled" => pseudo(s, ":disabled", 15),
        "enabled" => pseudo(s, ":enabled", 8),
        "checked" => pseudo(s, ":checked", 8),
        "required" => pseudo(s, ":required", 8),
        "invalid" => pseudo(s, ":invalid", 8),
        "placeholder-shown" => pseudo(s, ":placeholder-shown", 8),
        "first" => pseudo(s, ":first-child", 5),
        "last" => pseudo(s, ":last-child", 5),
        "only" => pseudo(s, ":only-child", 5),
        "odd" => pseudo(s, ":nth-child(odd)", 5),
        "even" => pseudo(s, ":nth-child(even)", 5),
        "empty" => pseudo(s, ":empty", 5),
        "open" => pseudo(s, ":is([open], :popover-open)", 6),
        "placeholder" => pseudo(s, "::placeholder", 7),
        "selection" => pseudo(s, " ::selection", 7),
        "*" => {
            s.suffix.push_str(" > *");
            s.order += 4;
        }
        "group-hover" | "group-focus" | "group-focus-visible" | "group-focus-within" | "group-active" | "group-open" => {
            let p = &v[6..];
            let sel = if p == "open" { "[open]".to_string() } else { format!(":{p}") };
            s.prefix.push(format!(".group{sel} "));
            if p == "hover" {
                s.at.push("@media (hover: hover)".into());
            }
            s.order += 10;
        }
        "peer-checked" | "peer-focus" | "peer-hover" | "peer-disabled" | "peer-invalid" => {
            s.prefix.push(format!(".peer:{} ~ ", &v[5..]));
            s.order += 10;
        }
        "dark" => {
            s.prefix.push(":root[data-theme=\"dark\"] ".into());
            s.order += 100;
        }
        "motion-safe" => s.at.push("@media (prefers-reduced-motion: no-preference)".into()),
        "motion-reduce" => s.at.push("@media (prefers-reduced-motion: reduce)".into()),
        "contrast-more" => s.at.push("@media (prefers-contrast: more)".into()),
        "print" => s.at.push("@media print".into()),
        "aria-selected" | "aria-checked" | "aria-expanded" | "aria-disabled" | "aria-pressed" | "aria-current" | "aria-invalid" => {
            s.suffix.push_str(&format!("[{v}=\"true\"]"));
            s.order += 8;
        }
        _ => {
            if let Some(bp) = v.strip_prefix("max-").and_then(|b| lookup(BREAKPOINTS, b)) {
                s.at.push(format!("@media (width < {bp})"));
                s.order += 1000;
                return Some(());
            }
            if let Some(i) = BREAKPOINTS.iter().position(|(n, _)| *n == v) {
                s.at.push(format!("@media (width >= {})", BREAKPOINTS[i].1));
                s.order += 2000 + 100 * i as u32;
                return Some(());
            }
            if let Some(c) = v.strip_prefix('@') {
                let w = lookup(CONTAINERS, c)?;
                s.at.push(format!("@container (width >= {w})"));
                s.order += 3000;
                return Some(());
            }
            // `data-[state=open]` and `data-active`: an attribute, checked like any arbitrary value.
            if let Some(rest) = v.strip_prefix("data-") {
                let attr = if let Some(inner) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                    let (k, val) = inner.split_once('=')?;
                    if !ident(k) || !ident(val) {
                        return None;
                    }
                    format!("[data-{k}=\"{val}\"]")
                } else {
                    if !ident(rest) {
                        return None;
                    }
                    format!("[data-{rest}]")
                };
                s.suffix.push_str(&attr);
                s.order += 8;
                return Some(());
            }
            return None;
        }
    }
    Some(())
}
fn ident(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Splits `md:hover:bg-red-500` at the colons that are not inside brackets.
fn split_variants(class: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in class.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ':' if depth == 0 => {
                out.push(&class[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&class[start..]);
    out
}

/// A class name as a CSS selector: every character that is not a letter, digit, `-` or `_` is
/// escaped, and a leading digit is written as its code point.
fn escape(class: &str) -> String {
    let mut out = String::with_capacity(class.len() + 8);
    for (i, c) in class.chars().enumerate() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            if i == 0 && c.is_ascii_digit() {
                let _ = write!(out, "\\{:x} ", c as u32);
            } else {
                out.push(c);
            }
        } else {
            out.push('\\');
            out.push(c);
        }
    }
    out
}

// --- The stylesheet. ------------------------------------------------------------------------

/// The CSS for one class: its sort key and its rule, or `None` when it is not a utility.
/// Where a rule sorts: variant order, rank in its family, family, then the class itself.
type SortKey = (u32, u8, u32, String);

fn compile(class: &str) -> Option<(SortKey, String, Option<&'static str>)> {
    if class.is_empty() || class.len() > 200 || !class.chars().all(|c| c.is_ascii_graphic()) {
        return None;
    }
    let parts = split_variants(class);
    let (last, variants) = parts.split_last()?;
    let mut shape = Shape::default();
    for v in variants {
        apply_variant(v, &mut shape)?;
    }
    let mut u: &str = last;
    let mut important = false;
    if let Some(x) = u.strip_suffix('!') {
        u = x;
        important = true;
    } else if let Some(x) = u.strip_prefix('!') {
        u = x;
        important = true;
    }
    let negative = u.starts_with('-') && u.len() > 1;
    if negative {
        u = &u[1..];
    }
    let r = utility(u, negative)?;
    let bang = if important { " !important" } else { "" };
    let decls = r.decls.iter().map(|(p, v)| format!("{p}: {v}{bang};")).collect::<Vec<_>>().join(" ");
    let selector = format!("{}.{}{}{}", shape.prefix.join(""), escape(class), shape.suffix, r.suffix);
    let mut css = format!("{selector} {{ {decls} }}");
    for at in shape.at.iter().rev() {
        css = format!("{at} {{ {css} }}");
    }
    let family = family_rank(u);
    Some(((shape.order, r.rank, family, class.to_string()), css, r.keyframes))
}

/// Families sort in a fixed order so that, at one variant level, a longhand always follows the
/// shorthand it refines wherever the classes appear.
fn family_rank(u: &str) -> u32 {
    const ORDER: &[&str] = &["p", "m", "inset", "rounded", "border", "gap", "grid", "flex", "from", "via", "to", "text", "font", "bg"];
    ORDER.iter().position(|f| u.starts_with(f)).map(|i| i as u32).unwrap_or(99)
}

/// The stylesheet for a set of class names: every one that is a utility, once, in an order in
/// which later variants and longhands win. Wrapped in the `hx-utilities` cascade layer, which
/// comes after the components', so a utility on a component always applies. The output is the
/// same for the same set, in any order.
pub fn css_for(classes: &BTreeSet<String>) -> String {
    let mut rules: BTreeMap<SortKey, String> = BTreeMap::new();
    let mut frames: BTreeSet<&'static str> = BTreeSet::new();
    for c in classes {
        if let Some((key, css, kf)) = compile(c) {
            rules.insert(key, css);
            if let Some(k) = kf {
                frames.insert(k);
            }
        }
    }
    if rules.is_empty() {
        return String::new();
    }
    let mut out = String::from("@layer hx-utilities { :root { --spacing: 0.25rem; ");
    for (n, v) in CONTAINERS {
        let _ = write!(out, "--container-{n}: {v}; ");
    }
    out.push_str("} ");
    for k in frames {
        out.push_str(k);
        out.push(' ');
    }
    for css in rules.values() {
        out.push_str(css);
        out.push(' ');
    }
    out.push('}');
    out
}

/// Whether a class name is a utility this engine writes CSS for.
#[cfg(test)]
pub fn is_utility(class: &str) -> bool {
    compile(class).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn css(classes: &[&str]) -> String {
        css_for(&classes.iter().map(|s| s.to_string()).collect())
    }
    fn has(classes: &[&str], want: &str) {
        let c = css(classes);
        assert!(c.contains(want), "{classes:?} lacks {want:?} in:\n{c}");
    }

    #[test]
    fn tailwinds_vocabulary_with_tailwinds_values() {
        has(&["p-4"], ".p-4 { padding: calc(var(--spacing) * 4); }");
        has(&["px-2.5"], ".px-2\\.5 { padding-inline: calc(var(--spacing) * 2.5); }");
        has(&["-mt-2"], ".-mt-2 { margin-top: calc(calc(var(--spacing) * 2) * -1); }");
        has(&["mx-auto"], ".mx-auto { margin-inline: auto; }");
        has(&["w-1/2"], ".w-1\\/2 { width: calc(1 / 2 * 100%); }");
        has(&["max-w-md"], ".max-w-md { max-width: var(--container-md); }");
        has(&["grid-cols-3"], ".grid-cols-3 { grid-template-columns: repeat(3, minmax(0, 1fr)); }");
        has(&["text-sm"], ".text-sm { font-size: 0.875rem; line-height: calc(1.25 / 0.875); }");
        has(&["font-semibold"], ".font-semibold { font-weight: 600; }");
        has(&["bg-blue-500"], ".bg-blue-500 { background-color: oklch(62.3% 0.214 259.815); }");
        has(&["bg-blue-500/50"], "color-mix(in oklab, oklch(62.3% 0.214 259.815) 50%, transparent)");
        has(&["rounded-xl"], ".rounded-xl { border-radius: 0.75rem; }");
        has(&["shadow-lg"], "--tw-shadow: 0 10px 15px -3px rgb(0 0 0 / 0.1)");
        has(&["truncate"], "text-overflow: ellipsis;");
        has(&["animate-spin"], "@keyframes spin");
        has(&["space-y-2"], ".space-y-2 > :not(:last-child) { margin-block-end: calc(var(--spacing) * 2); }");
    }

    #[test]
    fn helix_tokens_follow_the_design() {
        has(&["bg-accent"], ".bg-accent { background-color: var(--accent); }");
        has(&["text-muted"], ".text-muted { color: var(--muted); }");
        has(&["bg-accent/10"], "color-mix(in oklab, var(--accent) 10%, transparent)");
        has(&["rounded-card"], ".rounded-card { border-radius: var(--r-card); }");
        has(&["p-card"], ".p-card { padding: var(--pad-card); }");
        has(&["shadow-2"], "--tw-shadow: var(--e-2)");
        has(&["text-title-2"], ".text-title-2 { font: var(--type-title-2); letter-spacing: var(--track-title-2); }");
        has(&["ease-spring"], "var(--spring-spatial)");
        has(&["border"], "border-color: var(--tw-border, var(--line));");
    }

    #[test]
    fn variants_wrap_and_order_like_tailwind() {
        has(&["hover:bg-accent"], "@media (hover: hover) { .hover\\:bg-accent:hover { background-color: var(--accent); } }");
        has(&["md:grid-cols-2"], "@media (width >= 48rem) { .md\\:grid-cols-2 {");
        has(&["dark:text-white"], ":root[data-theme=\"dark\"] .dark\\:text-white { color: #fff; }");
        has(&["group-hover:opacity-100"], ".group:hover .group-hover\\:opacity-100 { opacity: 100%; }");
        has(&["data-[state=open]:block"], ".data-\\[state\\=open\\]\\:block[data-state=\"open\"] { display: block; }");
        has(&["focus-visible:ring-2"], ".focus-visible\\:ring-2:focus-visible {");
        has(&["2xl:p-8"], "@media (width >= 96rem) { .\\32 xl\\:p-8 {");
        has(&["p-4!"], "padding: calc(var(--spacing) * 4) !important;");
        // A longhand after its shorthand, and a breakpoint after the base, whatever the input order.
        let c = css(&["pt-2", "md:p-8", "p-4"]);
        let (a, b, d) = (c.find(".p-4 ").unwrap(), c.find(".pt-2 ").unwrap(), c.find("md\\:p-8").unwrap());
        assert!(a < b && b < d, "{c}");
        assert_eq!(css(&["p-4", "bg-accent"]), css(&["bg-accent", "p-4"]));
    }

    #[test]
    fn arbitrary_values_cannot_escape_their_rule() {
        has(&["w-[320px]"], ".w-\\[320px\\] { width: 320px; }");
        has(&["grid-cols-[1fr_2fr]"], "grid-template-columns: 1fr 2fr;");
        has(&["bg-[#0a84ff]"], "background-color: #0a84ff;");
        for bad in [
            "bg-[url(https://evil.example/x.png)]",
            "w-[1px;color:red]",
            "w-[1px}body{color:red]",
            "bg-[red\\;]",
            "bg-['x']",
            "w-[calc(1px))]",
            "bg-[image-set(a)]",
            "w-[expression(alert(1))]",
            "data-[a=b\"]:block",
            "data-[a=b]c]:block",
        ] {
            assert!(!is_utility(bad), "{bad} must not be a utility");
            assert_eq!(css(&[bad]), "", "{bad}");
        }
        // Unknown names and non-ASCII produce nothing, as in Tailwind.
        for nope in ["btn", "card", "p-", "p-4.3", "text-[", "héllo", "bg-notacolour-500", "grid-cols-99"] {
            assert!(!is_utility(nope), "{nope}");
        }
    }
}
