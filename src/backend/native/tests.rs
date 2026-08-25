//! The differential campaign (ADR 0033 Stage 1's acceptance gate): the same
//! data through BOTH engines, verb by verb, compared as `column_values` and as
//! frozen `framefmt` bytes. Runs when the dev build carries both features; the
//! native-only tests below it run wherever the native engine exists.
//!
//! ADR 0034's decided DELTAS are asserted AS deltas — the test proves the
//! divergence is exactly the decided one, so an accidental one cannot hide.

use std::rc::Rc;

use crate::backend::{ColData, ColExpr, Df};
use crate::value::Value;

use super::NativeFrame;


/// `Value` derives no `PartialEq` (function/handle variants), so cells compare
/// as `type:display` — distinct across types (`int:1` vs `str:1`) and exact for
/// every dtype a column can hold.
fn repr(v: &Value) -> String {
    format!("{}:{}", v.type_name(), v)
}

fn reprs(vs: &[Value]) -> Vec<String> {
    vs.iter().map(repr).collect()
}

fn data() -> Vec<(String, ColData)> {
    vec![
        (
            "region".into(),
            ColData::Str(vec!["east".into(), "west".into(), "east".into(), "north".into()]),
        ),
        ("samples".into(), ColData::IntOpt(vec![Some(12), Some(8), None, Some(31)])),
        ("af".into(), ColData::Float(vec![Some(0.5), Some(0.25), Some(0.125), None])),
        ("qc".into(), ColData::Bool(vec![true, false, true, true])),
    ]
}

fn native() -> Df {
    super::build_frame(data(), 0, 0).expect("native build_frame")
}

fn col(name: &str) -> ColExpr {
    ColExpr::Col(name.into())
}

fn lit(v: Value) -> ColExpr {
    ColExpr::Lit(v)
}

fn bin(op: crate::ast::BinOp, l: ColExpr, r: ColExpr) -> ColExpr {
    ColExpr::Binary(op, Box::new(l), Box::new(r))
}

/// Pull every column of both frames and require identical `Value` sequences.
#[cfg(feature = "dataframes")]
fn assert_frames_equal(a: &Df, b: &Df, what: &str) {
    let an = a.column_names(0, 0).unwrap();
    let bn = b.column_names(0, 0).unwrap();
    assert_eq!(an, bn, "{what}: column names diverge");
    for n in &an {
        let av = a.column_values(n, 0, 0).unwrap();
        let bv = b.column_values(n, 0, 0).unwrap();
        assert_eq!(reprs(&av), reprs(&bv), "{what}: column `{n}` diverges");
    }
    // The frozen format is the user-visible contract — compare its bytes too.
    let at = crate::framefmt::frame_text(&**a, 0, 0).unwrap();
    let bt = crate::framefmt::frame_text(&**b, 0, 0).unwrap();
    assert_eq!(at, bt, "{what}: frozen text diverges");
}

#[cfg(feature = "dataframes")]
mod against_the_oracle {
    use super::*;
    use crate::ast::BinOp;

    fn polars() -> Df {
        crate::backend::polars::build_frame(data(), 0, 0).expect("polars build_frame")
    }

    #[test]
    fn construction_and_reads_agree() {
        assert_frames_equal(&native(), &polars(), "build_frame");
    }

    #[test]
    fn filter_select_sort_head_agree() {
        let (n, p) = (native(), polars());
        let pred = bin(BinOp::Gt, col("samples"), lit(Value::Int(10)));
        assert_frames_equal(
            &n.filter(&pred, 0, 0).unwrap(),
            &p.filter(&pred, 0, 0).unwrap(),
            "filter samples > 10",
        );
        let names = vec!["af".to_string(), "region".to_string()];
        assert_frames_equal(
            &n.select(&names, 0, 0).unwrap(),
            &p.select(&names, 0, 0).unwrap(),
            "select af, region",
        );
        let keys = vec!["region".to_string(), "samples".to_string()];
        assert_frames_equal(
            &n.sort(&keys, 0, 0).unwrap(),
            &p.sort(&keys, 0, 0).unwrap(),
            "sort region, samples (stable, missing placement)",
        );
        assert_frames_equal(&n.head(2), &p.head(2), "head 2");
    }

    #[test]
    fn with_columns_arithmetic_agrees_where_no_delta_applies() {
        let (n, p) = (native(), polars());
        // Addition and comparison carry no decided delta — must match exactly.
        let cols = vec![
            ("bumped".to_string(), bin(BinOp::Add, col("samples"), lit(Value::Int(1)))),
            ("hot".to_string(), bin(BinOp::Ge, col("af"), lit(Value::Float(0.25)))),
        ];
        assert_frames_equal(
            &n.with_columns(&cols, 0, 0).unwrap(),
            &p.with_columns(&cols, 0, 0).unwrap(),
            "with_columns add/compare",
        );
    }

    #[test]
    fn group_agg_agrees_on_every_aggregation() {
        for agg in ["count", "sum", "mean", "min", "max", "std"] {
            let (n, p) = (native(), polars());
            let keys = vec!["region".to_string()];
            assert_frames_equal(
                &n.group_agg(&keys, agg, "af", 0, 0).unwrap(),
                &p.group_agg(&keys, agg, "af", 0, 0).unwrap(),
                &format!("group region .{agg}(af)"),
            );
        }
    }

    #[test]
    fn join_agrees_on_all_four_kinds() {
        let extra = |mk: fn(Vec<(String, ColData)>, usize, usize) -> _| -> Df {
            let cols = vec![
                (
                    "region".to_string(),
                    ColData::Str(vec!["east".into(), "south".into()]),
                ),
                ("lab".to_string(), ColData::Str(vec!["L1".into(), "L2".into()])),
            ];
            mk(cols, 0, 0)
        };
        let _ = extra; // keep the shape obvious even though each side builds its own
        for how in ["inner", "left", "right", "outer"] {
            let n_right = super::super::build_frame(
                vec![
                    ("region".to_string(), ColData::Str(vec!["east".into(), "south".into()])),
                    ("lab".to_string(), ColData::Str(vec!["L1".into(), "L2".into()])),
                ],
                0,
                0,
            )
            .unwrap();
            let p_right = crate::backend::polars::build_frame(
                vec![
                    ("region".to_string(), ColData::Str(vec!["east".into(), "south".into()])),
                    ("lab".to_string(), ColData::Str(vec!["L1".into(), "L2".into()])),
                ],
                0,
                0,
            )
            .unwrap();
            let keys = vec!["region".to_string()];
            assert_frames_equal(
                &native().join(&n_right, &keys, how, 0, 0).unwrap(),
                &polars().join(&p_right, &keys, how, 0, 0).unwrap(),
                &format!("join {how} on region"),
            );
        }
    }

    #[test]
    fn unique_and_vstack_agree() {
        let (n, p) = (native(), polars());
        assert_frames_equal(
            &n.unique_by(&[], 0, 0).unwrap(),
            &p.unique_by(&[], 0, 0).unwrap(),
            "unique whole-row",
        );
        let keys = vec!["region".to_string()];
        assert_frames_equal(
            &n.unique_by(&keys, 0, 0).unwrap(),
            &p.unique_by(&keys, 0, 0).unwrap(),
            "unique by region (keep last)",
        );
        assert_frames_equal(
            &n.vstack(&native(), 0, 0).unwrap(),
            &p.vstack(&polars(), 0, 0).unwrap(),
            "vstack self",
        );
    }

    /// ADR 0034 §1's deltas WERE asserted as deltas here. As of ADR 0036 there are
    /// none left to assert: both engines answer the language.
    ///
    /// The rename is deliberate. The old name — and the old assertion, which pinned
    /// `polars % stays truncated (delta recorded)` — encoded the belief that a
    /// standing delta list was a stable thing to test against. It was not: a policy
    /// saying "some divergences are expected" cannot detect an unexpected one, and
    /// twelve more were live in v0.5.1 while this test sat green.
    #[test]
    fn arithmetic_is_one_semantics_on_both_engines() {
        let mk = |native: bool| -> Df {
            let cols = vec![
                ("x".to_string(), ColData::Int(vec![7, 2])),
                ("y".to_string(), ColData::Int(vec![-3, 2])),
            ];
            if native {
                super::super::build_frame(cols, 0, 0).unwrap()
            } else {
                crate::backend::polars::build_frame(cols, 0, 0).unwrap()
            }
        };
        // `%` is EUCLIDEAN on both engines now: `7 % -3` is `1`, not polars' floored
        // `-2`. The expectation is the scalar kernel's own answer, so this pin cannot
        // drift away from the language even if someone edits both engines together.
        let modexpr = vec![("m".to_string(), bin(BinOp::Mod, col("x"), col("y")))];
        let want_mod = crate::interp::ops::eval_binary(
            &BinOp::Mod,
            Value::Int(7),
            Value::Int(-3),
            0,
            0,
        )
        .expect("scalar kernel");
        for (label, native) in [("native", true), ("polars", false)] {
            let got =
                mk(native).with_columns(&modexpr, 0, 0).unwrap().column_values("m", 0, 0).unwrap();
            assert_eq!(repr(&got[0]), repr(&want_mod), "[{label}] `%` must be euclidean");
        }

        // `/` is TRUE DIVISION on both engines: `2 / 2` is `1.0`, never Int `1`.
        let divexpr = vec![("d".to_string(), bin(BinOp::Div, col("x"), col("y")))];
        for (label, native) in [("native", true), ("polars", false)] {
            let got =
                mk(native).with_columns(&divexpr, 0, 0).unwrap().column_values("d", 0, 0).unwrap();
            assert_eq!(repr(&got[1]), "Float:1.0", "[{label}] `/` must be true division");
        }
    }

    /// Division by zero errors and names the row — on BOTH engines now (ADR 0036).
    /// The polars backend used to answer `missing`, which this test recorded as a
    /// delta and which was, in practice, a wrong number with exit 0.
    #[test]
    fn division_by_zero_errors_with_the_row() {
        let mk = |native: bool| -> Df {
            let cols = vec![
                ("x".to_string(), ColData::Int(vec![1, 2])),
                ("y".to_string(), ColData::Int(vec![0, 2])),
            ];
            if native {
                super::super::build_frame(cols, 0, 0).unwrap()
            } else {
                crate::backend::polars::build_frame(cols, 0, 0).unwrap()
            }
        };
        let divexpr = vec![("d".to_string(), bin(BinOp::Div, col("x"), col("y")))];
        for (label, native) in [("native", true), ("polars", false)] {
            // The polars backend is LAZY, so its error may surface at the verb or at
            // materialization — ADR 0036's declared caret delta. The MESSAGE and the
            // ROW must be identical either way, and that is what is asserted.
            let err = match mk(native).with_columns(&divexpr, 0, 0) {
                Err(e) => e,
                Ok(built) => match built.column_values("d", 0, 0) {
                    Err(e) => e,
                    Ok(v) => panic!("[{label}] dividing by zero must error, got {:?}", reprs(&v)),
                },
            };
            assert!(err.message.contains("division by zero"), "[{label}] {}", err.message);
            assert!(
                err.hint.as_deref().unwrap_or("").contains("row 0"),
                "[{label}] the row is named: {:?}",
                err.hint
            );
        }
    }
}

// ---- native-only behavior (no oracle needed) ----

#[test]
fn cache_is_identity_and_count_is_free() {
    let f = native();
    let c = f.cache(0, 0).unwrap();
    assert_eq!(f.row_count(0, 0).unwrap(), c.row_count(0, 0).unwrap());
    assert_eq!(
        reprs(&f.column_values("region", 0, 0).unwrap()),
        reprs(&c.column_values("region", 0, 0).unwrap())
    );
}

#[test]
fn csv_round_trips_with_dtypes_and_missing() {
    let dir = std::env::temp_dir().join("helix_native_csv_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("roundtrip.csv");
    let p = path.to_str().unwrap();
    let f = native();
    f.write_csv(p, b',', 0, 0).unwrap();
    let back = super::csv::read_csv(p, 0, 0).unwrap();
    let nf = back.as_any().downcast_ref::<NativeFrame>().expect("native frame");
    assert_eq!(nf.len(), 4);
    assert_eq!(
        reprs(&back.column_values("samples", 0, 0).unwrap()),
        reprs(&f.column_values("samples", 0, 0).unwrap()),
        "ints and missing survive the round trip"
    );
    assert_eq!(
        reprs(&back.column_values("af", 0, 0).unwrap()),
        reprs(&f.column_values("af", 0, 0).unwrap()),
        "floats keep their point so dtype re-infers"
    );
    assert_eq!(
        reprs(&back.column_values("qc", 0, 0).unwrap()),
        reprs(&f.column_values("qc", 0, 0).unwrap()),
        "bools survive"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn strict_logic_follows_the_scalar_truth_table() {
    // false and missing == false; true or missing == true — Kleene, as scalars.
    let cols = vec![
        ("a".to_string(), ColData::Bool(vec![false, true])),
        ("b".to_string(), ColData::IntOpt(vec![None, None])),
    ];
    let f = super::build_frame(cols, 0, 0).unwrap();
    let is_b_missing = ColExpr::IsMissing(Box::new(col("b")));
    let missing_lit = lit(Value::Missing);
    let and_expr = vec![(
        "r".to_string(),
        bin(crate::ast::BinOp::And, col("a"), missing_lit.clone()),
    )];
    let r = f.with_columns(&and_expr, 0, 0).unwrap().column_values("r", 0, 0).unwrap();
    assert_eq!(reprs(&r), vec!["Bool:false", "Missing:missing"]);
    let or_expr =
        vec![("r".to_string(), bin(crate::ast::BinOp::Or, col("a"), missing_lit))];
    let r = f.with_columns(&or_expr, 0, 0).unwrap().column_values("r", 0, 0).unwrap();
    assert_eq!(reprs(&r), vec!["Missing:missing", "Bool:true"]);
    let m = f
        .with_columns(&[("m".to_string(), is_b_missing)], 0, 0)
        .unwrap()
        .column_values("m", 0, 0)
        .unwrap();
    assert_eq!(reprs(&m), vec!["Bool:true", "Bool:true"]);
}

/// The `Rc<NativeFrame>` must satisfy the seam type.
#[test]
fn the_trait_object_shape_holds() {
    let f: Df = Rc::new(
        NativeFrame::new(
            vec![("x".to_string(), super::columns::Col::I64 { vals: vec![1], valid: vec![true] })],
            0,
            0,
        )
        .unwrap(),
    );
    assert_eq!(f.row_count(0, 0).unwrap(), 1);
}

// ---- parquet (ADR 0033 Stage 2) ----

fn tdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join("helix_native_pq_test");
    std::fs::create_dir_all(&d).expect("test tmpdir");
    d
}

#[test]
fn parquet_round_trips_with_dtypes_and_missing() {
    let p = tdir().join("rt.parquet");
    let p = p.to_str().expect("utf8 path");
    let f = super::build_frame(
        vec![
            ("id".to_string(), ColData::IntOpt(vec![Some(1), None, Some(3)])),
            ("name".to_string(), ColData::StrOpt(vec![Some("a".into()), Some("b".into()), None])),
            ("score".to_string(), ColData::Float(vec![Some(1.5), None, Some(3.5)])),
            ("ok".to_string(), ColData::Bool(vec![true, false, true])),
        ],
        0,
        0,
    )
    .unwrap();
    f.write_parquet(p, 0, 0).unwrap();
    let back = super::parquet_io::read_parquet(p, 0, 0).unwrap();
    for c in ["id", "name", "score", "ok"] {
        assert_eq!(
            reprs(&back.column_values(c, 0, 0).unwrap()),
            reprs(&f.column_values(c, 0, 0).unwrap()),
            "column `{c}` survives the round trip"
        );
    }
    let _ = std::fs::remove_file(p);
}

#[test]
fn an_empty_frame_round_trips_as_schema_only() {
    let p = tdir().join("empty.parquet");
    let p = p.to_str().expect("utf8 path");
    let f = super::build_frame(
        vec![("x".to_string(), ColData::Int(vec![])), ("s".to_string(), ColData::Str(vec![]))],
        0,
        0,
    )
    .unwrap();
    f.write_parquet(p, 0, 0).unwrap();
    let back = super::parquet_io::read_parquet(p, 0, 0).unwrap();
    assert_eq!(back.row_count(0, 0).unwrap(), 0);
    assert_eq!(back.column_names(0, 0).unwrap(), vec!["x", "s"]);
    let _ = std::fs::remove_file(p);
}

/// Foreign flat dtypes, from a file this test writes with the low-level parquet
/// API — dates read as their text; plain INT32 widens to Int.
#[test]
fn foreign_dtypes_read_as_text_or_widen() {
    use parquet::basic::{LogicalType, Repetition, Type as PhysType};
    use parquet::data_type::Int32Type;
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;
    use parquet::schema::types::Type;
    use std::sync::Arc;

    let p = tdir().join("foreign.parquet");
    let path = p.to_str().expect("utf8 path");
    let fields = vec![
        Arc::new(
            Type::primitive_type_builder("d", PhysType::INT32)
                .with_logical_type(Some(LogicalType::Date))
                .with_repetition(Repetition::REQUIRED)
                .build()
                .unwrap(),
        ),
        Arc::new(
            Type::primitive_type_builder("n", PhysType::INT32)
                .with_repetition(Repetition::REQUIRED)
                .build()
                .unwrap(),
        ),
    ];
    let schema =
        Arc::new(Type::group_type_builder("schema").with_fields(fields).build().unwrap());
    let file = std::fs::File::create(path).unwrap();
    let mut w =
        SerializedFileWriter::new(file, schema, Arc::new(WriterProperties::builder().build()))
            .unwrap();
    let mut rg = w.next_row_group().unwrap();
    let mut cw = rg.next_column().unwrap().expect("date column");
    cw.typed::<Int32Type>().write_batch(&[0, 20688], None, None).unwrap();
    cw.close().unwrap();
    let mut cw = rg.next_column().unwrap().expect("int column");
    cw.typed::<Int32Type>().write_batch(&[-5, 41], None, None).unwrap();
    cw.close().unwrap();
    rg.close().unwrap();
    w.close().unwrap();

    let back = super::parquet_io::read_parquet(path, 0, 0).unwrap();
    assert_eq!(
        reprs(&back.column_values("d", 0, 0).unwrap()),
        vec!["String:1970-01-01", "String:2026-08-23"],
        "dates read as ISO text"
    );
    assert_eq!(
        reprs(&back.column_values("n", 0, 0).unwrap()),
        vec!["Int:-5", "Int:41"],
        "INT32 widens to Int"
    );
    let _ = std::fs::remove_file(path);
}

/// A nested file is refused with a clean, named error — not a panic, not a
/// half-read frame.
#[test]
fn a_nested_file_is_refused_cleanly() {
    use parquet::basic::{Repetition, Type as PhysType};
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;
    use parquet::schema::types::Type;
    use std::sync::Arc;

    let p = tdir().join("nested.parquet");
    let path = p.to_str().expect("utf8 path");
    let leaf = Arc::new(
        Type::primitive_type_builder("item", PhysType::INT64)
            .with_repetition(Repetition::REPEATED)
            .build()
            .unwrap(),
    );
    let group = Arc::new(
        Type::group_type_builder("xs")
            .with_repetition(Repetition::OPTIONAL)
            .with_fields(vec![leaf])
            .build()
            .unwrap(),
    );
    let schema =
        Arc::new(Type::group_type_builder("schema").with_fields(vec![group]).build().unwrap());
    let file = std::fs::File::create(path).unwrap();
    let w =
        SerializedFileWriter::new(file, schema, Arc::new(WriterProperties::builder().build()))
            .unwrap();
    w.close().unwrap();

    let err = match super::parquet_io::read_parquet(path, 0, 0) {
        Err(e) => e,
        Ok(_) => panic!("nested schema must be refused"),
    };
    assert!(err.message.contains("nested"), "{}", err.message);
    assert!(err.message.contains("xs"), "the column is named: {}", err.message);
    let _ = std::fs::remove_file(path);
}

#[cfg(feature = "dataframes")]
mod parquet_cross_engine {
    use super::*;

    /// The compatibility contract both ways: files the polars engine writes,
    /// the native engine reads — and vice versa — cell-identical.
    #[test]
    fn each_engine_reads_the_others_files() {
        let dir = super::tdir();
        let cols = || data();

        // polars writes -> native reads
        let p1 = dir.join("polars_wrote.parquet");
        let p1s = p1.to_str().expect("utf8 path");
        let pf = crate::backend::polars::build_frame(cols(), 0, 0).unwrap();
        pf.write_parquet(p1s, 0, 0).unwrap();
        let native_read = crate::backend::native::read_parquet(p1s, 0, 0).unwrap();
        let polars_read = crate::backend::polars::read_parquet(p1s, 0, 0).unwrap();
        for c in ["region", "samples", "af", "qc"] {
            assert_eq!(
                reprs(&native_read.column_values(c, 0, 0).unwrap()),
                reprs(&polars_read.column_values(c, 0, 0).unwrap()),
                "native reads polars' file: column `{c}`"
            );
        }

        // native writes -> polars reads
        let p2 = dir.join("native_wrote.parquet");
        let p2s = p2.to_str().expect("utf8 path");
        let nf = super::super::build_frame(cols(), 0, 0).unwrap();
        nf.write_parquet(p2s, 0, 0).unwrap();
        let polars_read = crate::backend::polars::read_parquet(p2s, 0, 0).unwrap();
        for c in ["region", "samples", "af", "qc"] {
            assert_eq!(
                reprs(&polars_read.column_values(c, 0, 0).unwrap()),
                reprs(&nf.column_values(c, 0, 0).unwrap()),
                "polars reads the native file: column `{c}`"
            );
        }
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
    }
}

#[cfg(feature = "dataframes")]
mod tz_aware_timestamps {
    use super::*;

    /// Write a parquet with isAdjustedToUTC=true timestamps via the low-level
    /// API, then read it through the POLARS engine. Totality (ADR 0024) demands
    /// a value or a clean error — never a panic/abort.
    #[test]
    fn the_polars_engine_reads_a_utc_timestamp_file_without_panicking() {
        use parquet::basic::{LogicalType, Repetition, TimeUnit, TimestampType, Type as PhysType};
        use parquet::data_type::Int64Type;
        use parquet::file::properties::WriterProperties;
        use parquet::file::writer::SerializedFileWriter;
        use parquet::schema::types::Type;
        use std::sync::Arc;

        let p = super::tdir().join("tzaware.parquet");
        let path = p.to_str().expect("utf8 path");
        let fields = vec![Arc::new(
            Type::primitive_type_builder("ts", PhysType::INT64)
                .with_logical_type(Some(LogicalType::Timestamp(TimestampType {
                    is_adjusted_to_u_t_c: true,
                    unit: TimeUnit::MILLIS,
                })))
                .with_repetition(Repetition::REQUIRED)
                .build()
                .unwrap(),
        )];
        let schema =
            Arc::new(Type::group_type_builder("schema").with_fields(fields).build().unwrap());
        let file = std::fs::File::create(path).unwrap();
        let mut w = SerializedFileWriter::new(
            file,
            schema,
            Arc::new(WriterProperties::builder().build()),
        )
        .unwrap();
        let mut rg = w.next_row_group().unwrap();
        let mut cw = rg.next_column().unwrap().expect("ts column");
        cw.typed::<Int64Type>().write_batch(&[1_787_443_200_123], None, None).unwrap();
        cw.close().unwrap();
        rg.close().unwrap();
        w.close().unwrap();

        // The polars engine: a value or a clean error — the assert is that we
        // GET HERE at all (a panic aborts the test process).
        match crate::backend::polars::read_parquet(path, 0, 0) {
            Ok(df) => {
                let vals = df.column_values("ts", 0, 0);
                match vals {
                    Ok(v) => {
                        assert_eq!(v.len(), 1);
                        // Cross-engine agreement: the native reader's rendering.
                        let native = crate::backend::native::read_parquet(path, 0, 0).unwrap();
                        assert_eq!(
                            reprs(&v),
                            reprs(&native.column_values("ts", 0, 0).unwrap()),
                            "both engines render the tz-aware instant identically"
                        );
                    }
                    Err(e) => {
                        println!("polars column_values errored cleanly: {}", e.message);
                    }
                }
            }
            Err(e) => {
                println!("polars read_parquet errored cleanly: {}", e.message);
            }
        }
        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// v0.5.1 sweep pins: cross-backend divergences the release sweep caught. Each
// was written against the broken engine first and confirmed to fail there.
// ---------------------------------------------------------------------------

#[test]
fn csv_distinguishes_empty_string_from_missing() {
    // RFC 4180: `""` is an empty STRING; a bare empty field is MISSING. The
    // reader conflated them (a valid `""` came back missing) and the writer
    // wrote a valid empty string as a bare field — data loss both directions.
    let dir = std::env::temp_dir().join("helix_native_csv_empty");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("empty.csv");
    let p = path.to_str().unwrap();
    std::fs::write(&path, "a,s\n1,x\n2,\"\"\n3,\n").unwrap();
    let back = super::csv::read_csv(p, 0, 0).unwrap();
    assert_eq!(
        reprs(&back.column_values("s", 0, 0).unwrap()),
        reprs(&[
            Value::Str(Rc::new("x".to_string())),
            Value::Str(Rc::new(String::new())),
            Value::Missing,
        ]),
        "quoted empty is a string; bare empty is missing"
    );
    // And back out: the `""` cell writes as `""`, the missing cell as nothing.
    back.write_csv(p, b',', 0, 0).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a,s\n1,x\n2,\"\"\n3,\n");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn csv_integer_overflow_stays_text() {
    // Twenty digits don't fit i64 and must NOT round through f64 (they silently
    // became 1e20 — data loss); the column stays Str, as the polars backend does.
    let dir = std::env::temp_dir().join("helix_native_csv_big");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.csv");
    std::fs::write(&path, "v\n99999999999999999999\n1\n").unwrap();
    let back = super::csv::read_csv(path.to_str().unwrap(), 0, 0).unwrap();
    assert_eq!(
        reprs(&back.column_values("v", 0, 0).unwrap()),
        reprs(&[
            Value::Str(Rc::new("99999999999999999999".to_string())),
            Value::Str(Rc::new("1".to_string())),
        ]),
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn join_key_dtype_mismatch_refuses() {
    // Int keys left, Float keys right: the hash keys can never collide, so the
    // old behavior was a silent 0-row inner join — exit 0 hiding a schema bug.
    let l = super::build_frame(
        vec![
            ("id".into(), ColData::IntOpt(vec![Some(1), Some(2)])),
            ("u".into(), ColData::IntOpt(vec![Some(1), Some(1)])),
        ],
        0,
        0,
    )
    .unwrap();
    let r = super::build_frame(
        vec![
            ("id".into(), ColData::Float(vec![Some(1.0), Some(2.5)])),
            ("w".into(), ColData::IntOpt(vec![Some(2), Some(2)])),
        ],
        0,
        0,
    )
    .unwrap();
    let err = match l.join(&r, &["id".to_string()], "inner", 0, 0) {
        Err(e) => e,
        Ok(_) => panic!("mismatched key dtypes must refuse"),
    };
    assert!(err.message.contains("join key `id` is"), "{}", err.message);
}

#[test]
fn group_string_min_max_is_lexical() {
    // Strings order in scalar Helix ("a" < "b"), so a Str column answers
    // lexical min/max — the polars backend already did; native refused.
    let f = super::build_frame(
        vec![
            ("k".into(), ColData::Str(vec!["a".into(), "a".into()])),
            ("s".into(), ColData::Str(vec!["y".into(), "x".into()])),
        ],
        0,
        0,
    )
    .unwrap();
    let mn = f.group_agg(&["k".to_string()], "min", "s", 0, 0).unwrap();
    let mx = f.group_agg(&["k".to_string()], "max", "s", 0, 0).unwrap();
    assert_eq!(
        reprs(&mn.column_values("s", 0, 0).unwrap()),
        reprs(&[Value::Str(Rc::new("x".to_string()))]),
    );
    assert_eq!(
        reprs(&mx.column_values("s", 0, 0).unwrap()),
        reprs(&[Value::Str(Rc::new("y".to_string()))]),
    );
}

#[test]
fn float_keys_collapse_signed_zero() {
    // -0.0 == 0.0 as a scalar (and in the polars backend), so unique/group/join
    // keys must not tell them apart — raw `to_bits` keys did.
    let f = super::build_frame(
        vec![("x".into(), ColData::Float(vec![Some(-0.0), Some(0.0), Some(1.0)]))],
        0,
        0,
    )
    .unwrap();
    assert_eq!(f.unique_by(&[], 0, 0).unwrap().row_count(0, 0).unwrap(), 2);
}

/// ADR 0036's arithmetic pins. Separate module so it can import `BinOp`, which
/// the file's other dual-engine tests get from `mod against_the_oracle`.
#[cfg(feature = "dataframes")]
mod one_semantics {
    use super::*;
    use crate::ast::BinOp;

    // ---------------------------------------------------------------------------
    // ADR 0036 (v0.6.0) — one semantics. These are the arithmetic pins: the two
    // backends must not merely agree with each other, they must agree with the
    // SCALAR KERNEL, which is why each expected value below is the one
    // `interp::ops::eval_binary` produces.
    // ---------------------------------------------------------------------------

    /// The reciprocal-multiply divergence (ADR 0036 policy 1, D13).
    ///
    /// polars rewrites division-by-a-constant into multiplication by the reciprocal:
    /// `41.0 * 0.1` is `4.1000000000000005`, not `4.1`. It needs TWO OR MORE ROWS to
    /// trigger, which is why a one-row fixture — the natural thing to write — reports
    /// agreement. Every value here is chosen so `x * 0.1 != x / 10.0`, except `55`,
    /// which is exact either way and is included precisely so the test cannot pass by
    /// accident on a lucky value.
    #[test]
    fn division_by_a_literal_is_ieee_exact_on_both_engines() {
        // `ColData` is not `Clone`, so each engine gets freshly built columns.
        let mk = || vec![("b".to_string(), ColData::Int(vec![41, 38, 55, 29]))];
        let n = crate::backend::native::build_frame(mk(), 0, 0).unwrap();
        let p = crate::backend::polars::build_frame(mk(), 0, 0).unwrap();
        let e = vec![("r".to_string(), bin(BinOp::Div, col("b"), lit(Value::Int(10))))];
        let expect: Vec<Value> = [41.0f64, 38.0, 55.0, 29.0]
            .iter()
            .map(|x| Value::Float(x / 10.0))
            .collect();
        for (label, f) in [("native", &n), ("polars", &p)] {
            let got = f.with_columns(&e, 0, 0).unwrap().column_values("r", 0, 0).unwrap();
            assert_eq!(reprs(&got), reprs(&expect), "[{label}] division by a literal is not IEEE-exact");
        }
    }

    /// `%` and `//` are euclidean on both engines, and keep Int when both operands are
    /// Int. polars' own `%` is FLOORED (`7 % -3` is `-2` there, `1` in Helix), and it
    /// refused `//` inside a query outright until v0.6.0.
    #[test]
    fn euclidean_mod_and_floordiv_agree_with_the_scalar_kernel() {
        let mk = || vec![("a".to_string(), ColData::Int(vec![7, -7, 7, -7]))];
        let n = crate::backend::native::build_frame(mk(), 0, 0).unwrap();
        let p = crate::backend::polars::build_frame(mk(), 0, 0).unwrap();
        for (op, rhs) in [(BinOp::Mod, 3i64), (BinOp::Mod, -3), (BinOp::FloorDiv, 2)] {
            let e = vec![("r".to_string(), bin(op, col("a"), lit(Value::Int(rhs))))];
            // The scalar kernel IS the expectation — not a hand-written table.
            let expect: Vec<Value> = [7i64, -7, 7, -7]
                .iter()
                .map(|a| {
                    crate::interp::ops::eval_binary(&op, Value::Int(*a), Value::Int(rhs), 0, 0)
                        .expect("scalar kernel")
                })
                .collect();
            for (label, f) in [("native", &n), ("polars", &p)] {
                let got = f.with_columns(&e, 0, 0).unwrap().column_values("r", 0, 0).unwrap();
                assert_eq!(reprs(&got), reprs(&expect), "[{label}] {op:?} by {rhs}");
            }
        }
    }

    /// A zero divisor is an error naming the 0-based row, on BOTH engines, for `/ % //`.
    /// polars used to answer three different silent things: `missing` for Int `/0`,
    /// `inf` for Float `/0`, `NaN` for `0.0 / 0.0`.
    #[test]
    fn zero_divisor_errors_with_the_row_on_both_engines() {
        let mk = || {
            vec![
                ("a".to_string(), ColData::Int(vec![1, 2, 3])),
                ("z".to_string(), ColData::Int(vec![1, 1, 0])),
            ]
        };
        for op in [BinOp::Div, BinOp::Mod, BinOp::FloorDiv] {
            let n = crate::backend::native::build_frame(mk(), 0, 0).unwrap();
            let p = crate::backend::polars::build_frame(mk(), 0, 0).unwrap();
            let e = vec![("r".to_string(), bin(op, col("a"), col("z")))];
            for (label, f) in [("native", &n), ("polars", &p)] {
                // The polars backend is LAZY, so the error may surface at materialization
                // rather than at the verb — hence both are tried. That timing difference
                // is ADR 0036's declared caret delta; the MESSAGE must not differ.
                let err = match f.with_columns(&e, 0, 0) {
                    Err(e) => e,
                    Ok(built) => match built.column_values("r", 0, 0) {
                        Err(e) => e,
                        Ok(v) => panic!("[{label}] {op:?} by zero did not raise: {:?}", reprs(&v)),
                    },
                };
                assert!(
                    err.message.contains("division by zero")
                        || err.message.contains("modulo by zero"),
                    "[{label}] {op:?}: {}",
                    err.message
                );
                assert!(
                    err.hint.as_deref().unwrap_or("").contains("at row 2 of the frame."),
                    "[{label}] {op:?} did not name the row: {:?}",
                    err.hint
                );
            }
        }
    }

    /// A MISSING divisor is not a zero divisor — it must propagate, not raise (ADR 0001).
    #[test]
    fn a_missing_divisor_propagates_rather_than_raising() {
        let mk = || {
            vec![
                ("a".to_string(), ColData::Int(vec![1, 2])),
                ("z".to_string(), ColData::IntOpt(vec![None, Some(2)])),
            ]
        };
        let n = crate::backend::native::build_frame(mk(), 0, 0).unwrap();
        let p = crate::backend::polars::build_frame(mk(), 0, 0).unwrap();
        let e = vec![("r".to_string(), bin(BinOp::Div, col("a"), col("z")))];
        let expect = vec![Value::Missing, Value::Float(1.0)];
        for (label, f) in [("native", &n), ("polars", &p)] {
            let got = f.with_columns(&e, 0, 0).unwrap().column_values("r", 0, 0).unwrap();
            assert_eq!(reprs(&got), reprs(&expect), "[{label}] missing divisor");
        }
    }
}
