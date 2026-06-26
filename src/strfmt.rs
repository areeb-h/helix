//! Format specifiers for string interpolation — the optional `:spec` after an
//! interpolation hole, e.g. `"{x:.2f}"`, `"{n:08b}"`, `"{p:.1%}"`. A pragmatic
//! subset of Python's format mini-language:
//!
//! ```text
//! spec := [align][zero][width][.precision][type]
//! align := '<' (left) | '>' (right) | '^' (center)
//! zero  := '0'                      zero-pad numbers (after the sign)
//! width := digits                   minimum field width
//! .precision := '.' digits          decimals (float types) or significant cap
//! type := 'f' | 'e' | 'g' | '%' | 'd' | 'x' | 'X' | 'b' | 'o' | 's'
//! ```
//!
//! The spec is parsed once at parse time (so a malformed spec is a *parse* error,
//! not a runtime surprise) into a [`FormatSpec`], then applied per render by both
//! engines via [`FormatSpec::apply`].

use crate::value::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Align {
    Left,
    Right,
    Center,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FormatSpec {
    align: Option<Align>,
    zero: bool,
    width: Option<usize>,
    precision: Option<usize>,
    ty: Option<char>,
}

/// Parse a format spec (the text after the `:` in `{expr:spec}`). Returns a
/// human-readable message on a malformed spec.
pub fn parse_spec(s: &str) -> Result<FormatSpec, String> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut i = 0;

    let align = if i < n && matches!(chars[i], '<' | '>' | '^') {
        let a = match chars[i] {
            '<' => Align::Left,
            '>' => Align::Right,
            _ => Align::Center,
        };
        i += 1;
        Some(a)
    } else {
        None
    };

    let zero = if i < n && chars[i] == '0' {
        i += 1;
        true
    } else {
        false
    };

    let mut width = None;
    let ws = i;
    while i < n && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i > ws {
        width = Some(
            chars[ws..i]
                .iter()
                .collect::<String>()
                .parse()
                .map_err(|_| "format width is too large".to_string())?,
        );
    }

    let mut precision = None;
    if i < n && chars[i] == '.' {
        i += 1;
        let ps = i;
        while i < n && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i == ps {
            return Err("expected digits after `.` in a format spec".to_string());
        }
        precision = Some(
            chars[ps..i]
                .iter()
                .collect::<String>()
                .parse()
                .map_err(|_| "format precision is too large".to_string())?,
        );
    }

    let mut ty = None;
    if i < n {
        let c = chars[i];
        if matches!(c, 'f' | 'e' | 'g' | '%' | 'd' | 'x' | 'X' | 'b' | 'o' | 's') {
            ty = Some(c);
            i += 1;
        } else {
            return Err(format!("unknown format type `{c}` (use one of f e g % d x X b o s)"));
        }
    }

    if i != n {
        return Err(format!("invalid format spec `{s}`"));
    }
    Ok(FormatSpec { align, zero, width, precision, ty })
}

impl FormatSpec {
    /// Render `v` according to this spec. Returns a message if the value's type is
    /// incompatible with the requested format type (e.g. `:.2f` on a string).
    pub fn apply(&self, v: &Value) -> Result<String, String> {
        let body = self.render_core(v)?;
        Ok(self.pad(v, body))
    }

    fn render_core(&self, v: &Value) -> Result<String, String> {
        let as_f = |v: &Value| -> Result<f64, String> {
            match v {
                Value::Int(i) => Ok(*i as f64),
                Value::Float(f) => Ok(*f),
                _ => Err(format!("cannot format a {} with a numeric format spec", v.type_name())),
            }
        };
        let as_i = |v: &Value| -> Result<i64, String> {
            match v {
                Value::Int(i) => Ok(*i),
                _ => Err(format!("cannot format a {} with an integer format spec", v.type_name())),
            }
        };
        match self.ty {
            Some('f') => Ok(format!("{:.*}", self.precision.unwrap_or(6), as_f(v)?)),
            Some('e') => Ok(format!("{:.*e}", self.precision.unwrap_or(6), as_f(v)?)),
            Some('g') => match self.precision {
                Some(p) => Ok(format!("{:.*}", p, as_f(v)?)),
                None => Ok(format!("{}", as_f(v)?)),
            },
            Some('%') => Ok(format!("{:.*}%", self.precision.unwrap_or(2), as_f(v)? * 100.0)),
            Some('d') => Ok(format!("{}", as_i(v)?)),
            Some('x') => Ok(format!("{:x}", as_i(v)?)),
            Some('X') => Ok(format!("{:X}", as_i(v)?)),
            Some('b') => Ok(format!("{:b}", as_i(v)?)),
            Some('o') => Ok(format!("{:o}", as_i(v)?)),
            Some('s') | None => match (self.precision, v) {
                // A bare precision on a number means fixed decimals, like `:.2f`.
                (Some(p), Value::Int(_)) | (Some(p), Value::Float(_)) if self.ty.is_none() => {
                    Ok(format!("{:.*}", p, as_f(v)?))
                }
                _ => crate::value::display_value(v, 0, 0).map_err(|e| e.message),
            },
            Some(other) => Err(format!("unknown format type `{other}`")),
        }
    }

    fn pad(&self, v: &Value, body: String) -> String {
        let width = self.width.unwrap_or(0);
        let len = body.chars().count();
        if len >= width {
            return body;
        }
        let fill = width - len;
        let is_num = matches!(v, Value::Int(_) | Value::Float(_));
        // Zero-pad applies to numbers and goes *after* any leading sign, so
        // `{-3.1:07.2f}` is `-003.10`, not `00-3.10`.
        if self.zero && is_num && self.align.is_none() {
            let (sign, rest) = if let Some(r) = body.strip_prefix('-') {
                ("-", r)
            } else if let Some(r) = body.strip_prefix('+') {
                ("+", r)
            } else {
                ("", body.as_str())
            };
            return format!("{sign}{}{rest}", "0".repeat(fill));
        }
        // Default alignment: numbers right, everything else left.
        let align = self
            .align
            .clone()
            .unwrap_or(if is_num { Align::Right } else { Align::Left });
        match align {
            Align::Left => format!("{body}{}", " ".repeat(fill)),
            Align::Right => format!("{}{body}", " ".repeat(fill)),
            Align::Center => {
                let l = fill / 2;
                format!("{}{body}{}", " ".repeat(l), " ".repeat(fill - l))
            }
        }
    }
}
