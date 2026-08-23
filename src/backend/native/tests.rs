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

    /// ADR 0034 §1's deltas, asserted AS deltas: the native engine follows the
    /// language where the polars backend follows polars.
    #[test]
    fn the_decided_arithmetic_deltas_are_exactly_the_decided_ones() {
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
        let modexpr = vec![("m".to_string(), bin(BinOp::Mod, col("x"), col("y")))];
        // Native: euclidean, like the language. 7 % -3 == 1.
        let nm = mk(true).with_columns(&modexpr, 0, 0).unwrap().column_values("m", 0, 0).unwrap();
        assert_eq!(repr(&nm[0]), "Int:1", "native % is the language's euclidean %");
        // Oracle: truncated. 7 % -3 == -2 — the delta, pinned so it can't drift silently.
        let pm = mk(false).with_columns(&modexpr, 0, 0).unwrap().column_values("m", 0, 0).unwrap();
        assert_eq!(repr(&pm[0]), "Int:-2", "polars % stays truncated (delta recorded)");

        let divexpr = vec![("d".to_string(), bin(BinOp::Div, col("x"), col("y")))];
        // Native: true division -> Float, like the language. 2 / 2 == 1.0.
        let nd = mk(true).with_columns(&divexpr, 0, 0).unwrap().column_values("d", 0, 0).unwrap();
        assert_eq!(repr(&nd[1]), "Float:1.0", "native / is true division");
    }

    /// Division by zero: the language errors; the native engine errors WITH the
    /// row; the polars backend answers missing (the delta).
    #[test]
    fn division_by_zero_errors_with_the_row() {
        let cols = vec![
            ("x".to_string(), ColData::Int(vec![1, 2])),
            ("y".to_string(), ColData::Int(vec![0, 2])),
        ];
        let n = super::super::build_frame(cols, 0, 0).unwrap();
        let divexpr = vec![("d".to_string(), bin(BinOp::Div, col("x"), col("y")))];
        let err = match n.with_columns(&divexpr, 0, 0) {
            Err(e) => e,
            Ok(_) => panic!("dividing by zero must error"),
        };
        assert!(err.message.contains("division by zero"), "{}", err.message);
        assert!(
            err.hint.as_deref().unwrap_or("").contains("row 0"),
            "the row is named: {:?}",
            err.hint
        );
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
