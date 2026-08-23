//! Builtins: bioinformatics readers and the dna constructor — moved verbatim from the one-file dispatch
//! (2026-08-24). The `call` guard names exactly the arms this file holds;
//! `dispatch` is the original match text, arm for arm.

use std::rc::Rc;

use crate::error::HelixError;
use crate::value::Value;

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;

pub(super) fn call(name: &str, args: Vec<Value>, line: usize, col: usize) -> Called {
    if !matches!(name, "dna" | "read_vcf" | "read_bcf" | "read_sam" | "read_bam" | "read_gff" | "read_bed" | "read_fasta" | "read_fastq" | "align") {
        return Called::Not(args);
    }
    Called::Done(dispatch(name, args, line, col))
}

fn dispatch(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    match name {
                "dna" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Str(s) => make_dna(s, line, col),
                        // Idempotent: `dna(x)` where x is already a Dna answers x.
                        // `T(x)` with x : T has one sensible meaning, and the field's
                        // defensive `dna(primer)` calls were raising on the module's
                        // own documented input type.
                        d @ Value::Dna(_) => Ok(d.clone()),
                        other => Err(type_err("dna", "a string", other, line, col)),
                    }
                }
                "read_vcf" => {
                    // `read_vcf(path)` scans; `read_vcf(path, "chr:start-end")` does an
                    // indexed region query against the file's `.tbi`.
                    if args.is_empty() || args.len() > 2 {
                        return Err(HelixError::new(
                            format!("`read_vcf` takes 1 or 2 arguments, got {}", args.len()),
                            line,
                            col,
                        ));
                    }
                    let path = match &args[0] {
                        Value::Str(s) => s,
                        other => {
                            return Err(type_err("read_vcf", "a string path", other, line, col));
                        }
                    };
                    let df = match args.get(1) {
                        Some(Value::Str(region)) => {
                            crate::vcf::read_vcf_region(path, region, line, col)?
                        }
                        Some(other) => {
                            return Err(type_err("read_vcf", "a string region", other, line, col));
                        }
                        None => crate::vcf::read_vcf(path, line, col)?,
                    };
                    Ok(Value::dataframe(df))
                }
                "read_bcf" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Str(s) => Ok(Value::dataframe(crate::vcf::read_bcf(s, line, col)?)),
                        other => Err(type_err("read_bcf", "a string path", other, line, col)),
                    }
                }
                "read_sam" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Str(s) => Ok(Value::dataframe(crate::sam::read_sam(s, line, col)?)),
                        other => Err(type_err("read_sam", "a string path", other, line, col)),
                    }
                }
                "read_bam" => {
                    // `read_bam(path)` scans; `read_bam(path, "chr:start-end")` does an
                    // indexed region query against the file's `.bai`.
                    if args.is_empty() || args.len() > 2 {
                        return Err(HelixError::new(
                            format!("`read_bam` takes 1 or 2 arguments, got {}", args.len()),
                            line,
                            col,
                        ));
                    }
                    let path = match &args[0] {
                        Value::Str(s) => s,
                        other => {
                            return Err(type_err("read_bam", "a string path", other, line, col));
                        }
                    };
                    let df = match args.get(1) {
                        Some(Value::Str(region)) => {
                            crate::sam::read_bam_region(path, region, line, col)?
                        }
                        Some(other) => {
                            return Err(type_err("read_bam", "a string region", other, line, col));
                        }
                        None => crate::sam::read_bam(path, line, col)?,
                    };
                    Ok(Value::dataframe(df))
                }
                "read_gff" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Str(s) => Ok(Value::dataframe(crate::gff::read_gff(s, line, col)?)),
                        other => Err(type_err("read_gff", "a string path", other, line, col)),
                    }
                }
                "read_bed" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Str(s) => Ok(Value::dataframe(crate::bed::read_bed(s, line, col)?)),
                        other => Err(type_err("read_bed", "a string path", other, line, col)),
                    }
                }
                "read_fasta" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Str(s) => crate::bio::read_fasta(s, line, col),
                        other => Err(type_err("read_fasta", "a string path", other, line, col)),
                    }
                }
                "read_fastq" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Str(s) => crate::bio::read_fastq(s, line, col),
                        other => Err(type_err("read_fastq", "a string path", other, line, col)),
                    }
                }
                // ---- tensor constructors ----
                "align" => {
                    if args.len() < 2 || args.len() > 4 {
                        return Err(HelixError::new(
                            format!(
                                "`align` takes (a, b), (a, b, mode), or (a, b, mode, scoring), got {} arguments",
                                args.len()
                            ),
                            line,
                            col,
                        )
                        .hint("e.g. `align(a, b)`, `align(a, b, \"local\")`, or `align(a, b, \"global\", {match: 2, mismatch: -3, gap_open: -5, gap_extend: -1})`."));
                    }
                    let seq = |v: &Value| -> Result<Vec<Value>, HelixError> {
                        match v {
                            Value::Array(arr) => Ok(arr.to_values().into_owned()),
                            other => Err(type_err("align", "an array", other, line, col)),
                        }
                    };
                    let a = seq(&args[0])?;
                    let b = seq(&args[1])?;
                    // The Gotoh DP fills six O(n·m) tables (~27 bytes/cell at i64 scores);
                    // cap the cell count so a huge pair fails cleanly instead of trying to
                    // allocate gigabytes.
                    const MAX_ALIGN_CELLS: u128 = 50_000_000;
                    if (a.len() as u128 + 1) * (b.len() as u128 + 1) > MAX_ALIGN_CELLS {
                        return Err(HelixError::new(
                            "`align` sequences are too large (the alignment matrix would exceed 50M cells)",
                            line,
                            col,
                        ));
                    }
                    // Optional trailing args, in either order: a mode string and/or a
                    // scoring record. Scoring defaults to match +1 / mismatch −1 / no
                    // gap-open / gap-extend −1; any field may be overridden.
                    let mut mode = crate::align::Mode::Global;
                    let mut sc =
                        crate::align::Scoring { match_: 1, mismatch: -1, gap_open: 0, gap_extend: -1 };
                    let (mut mode_set, mut scoring_set) = (false, false);
                    for extra in &args[2..] {
                        match extra {
                            Value::Str(s) => {
                                if mode_set {
                                    return Err(HelixError::new("`align` was given two modes", line, col));
                                }
                                mode = match s.as_str() {
                                    "global" => crate::align::Mode::Global,
                                    "local" => crate::align::Mode::Local,
                                    "semiglobal" => crate::align::Mode::Semiglobal,
                                    other => {
                                        return Err(HelixError::new(
                                            format!("unknown align mode `{other}`"),
                                            line,
                                            col,
                                        )
                                        .hint("use \"global\", \"local\", or \"semiglobal\"."))
                                    }
                                };
                                mode_set = true;
                            }
                            Value::Record(fields) => {
                                if scoring_set {
                                    return Err(HelixError::new(
                                        "`align` was given two scoring records",
                                        line,
                                        col,
                                    ));
                                }
                                for (k, v) in fields.iter() {
                                    let iv = as_int(v, "align scoring", line, col)?;
                                    if !(-1_000_000..=1_000_000).contains(&iv) {
                                        return Err(HelixError::new(
                                            "`align` scoring values must be within ±1000000",
                                            line,
                                            col,
                                        ));
                                    }
                                    match k.as_str() {
                                        "match" => sc.match_ = iv,
                                        "mismatch" => sc.mismatch = iv,
                                        "gap_open" => sc.gap_open = iv,
                                        "gap_extend" => sc.gap_extend = iv,
                                        other => {
                                            return Err(HelixError::new(
                                                format!("unknown align scoring field `{other}`"),
                                                line,
                                                col,
                                            )
                                            .hint("scoring fields: match, mismatch, gap_open, gap_extend."))
                                        }
                                    }
                                }
                                scoring_set = true;
                            }
                            other => {
                                return Err(type_err(
                                    "align",
                                    "a mode string or a scoring record",
                                    other,
                                    line,
                                    col,
                                ))
                            }
                        }
                    }
                    let (score, steps) =
                        crate::align::align_path(a.len(), b.len(), mode, sc, |i, j| values_equal(&a[i], &b[j]));
                    let mut a_al = Vec::with_capacity(steps.len());
                    let mut b_al = Vec::with_capacity(steps.len());
                    let mut matches = 0i64;
                    for st in &steps {
                        match st {
                            crate::align::Step::Both(ia, ib) => {
                                if values_equal(&a[*ia], &b[*ib]) {
                                    matches += 1;
                                }
                                a_al.push(a[*ia].clone());
                                b_al.push(b[*ib].clone());
                            }
                            crate::align::Step::OnlyA(ia) => {
                                a_al.push(a[*ia].clone());
                                b_al.push(Value::Missing);
                            }
                            crate::align::Step::OnlyB(ib) => {
                                a_al.push(Value::Missing);
                                b_al.push(b[*ib].clone());
                            }
                        }
                    }
                    Ok(Value::Record(Rc::new(vec![
                        (Symbol::intern("score"), Value::Int(score)),
                        (Symbol::intern("matches"), Value::Int(matches)),
                        (Symbol::intern("length"), Value::Int(steps.len() as i64)),
                        (Symbol::intern("a_aligned"), Value::array(a_al)),
                        (Symbol::intern("b_aligned"), Value::array(b_al)),
                    ])))
                }
        _ => Err(HelixError::new(
            format!("internal: `{name}` routed to the wrong builtin module"),
            line,
            col,
        )),
    }
}
