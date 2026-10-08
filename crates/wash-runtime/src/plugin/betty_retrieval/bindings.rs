//! Generated bindings for `betty-blocks:retrieval`, plus the tokio-postgres
//! value conversions the `store` interface binds parameters and decodes rows
//! with.

// Plugin-local rather than under the shared `wit/` tree: any interface there
// that `use`s `wasmcloud:postgres/types@0.2.0` makes the stock postgres
// plugin's `bindgen!` panic.
crate::wasmtime::component::bindgen!({
    path: "src/plugin/betty_retrieval/wit",
    world: "plugin-imports",
    imports: { default: store | async | trappable | tracing },
    with: {
        "betty-blocks:retrieval/store@0.1.0.transaction": super::tx::TxHandle,
    },
});

/// The same `pg-value` <-> tokio-postgres conversions as the stock postgres
/// plugin (see `wasmcloud_postgres/conversions.rs`'s header), applied to this
/// plugin's own generated types.
pub(crate) mod conversions {
    use super::wasmcloud::postgres::types::{
        Date, HashableF64, MacAddressEui48, MacAddressEui64, Numeric, Offset, PgValue, Time,
        Timestamp, TimestampTz,
    };

    include!("../wasmcloud_postgres/conversions.rs");

    /// Convert one fetched row into WIT `pg-value`s, in column order. A
    /// `String` error names the column that failed to convert, and why.
    pub(crate) fn row_to_values(r: &Row) -> Result<Vec<PgValue>, String> {
        (0..r.len())
            .map(|idx| {
                r.try_get(idx).map_err(|e| {
                    format!("column {idx}: {}", super::super::errors::with_sources(&e))
                })
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use bytes::{BufMut as _, BytesMut};
    use tokio_postgres::types::{FromSql as _, IsNull, ToSql, Type, to_sql_checked};

    use super::wasmcloud::postgres::types::PgValue;

    type Point = ((u64, i16, i8), (u64, i16, i8));

    /// The `pg-value` point for `(x, y)`.
    fn point(x: f64, y: f64) -> Point {
        use bigdecimal::num_traits::Float as _;
        (x.integer_decode(), y.integer_decode())
    }

    fn put_points(out: &mut BytesMut, points: &[(f64, f64)]) {
        out.put_i32(i32::try_from(points.len()).expect("a test's point count fits an i32"));
        for (x, y) in points {
            out.put_f64(*x);
            out.put_f64(*y);
        }
    }

    /// A `path` as Postgres sends it (`path_send`): whether it is closed, a
    /// point count, then each point's two `float8`s.
    fn path_bytes(closed: bool, points: &[(f64, f64)]) -> BytesMut {
        let mut out = BytesMut::new();
        out.put_u8(u8::from(closed));
        put_points(&mut out, points);
        out
    }

    /// A `polygon` as Postgres sends it (`poly_send`): a point count, then
    /// each point's two `float8`s.
    fn polygon_bytes(points: &[(f64, f64)]) -> BytesMut {
        let mut out = BytesMut::new();
        put_points(&mut out, points);
        out
    }

    /// An array member already in its binary form.
    #[derive(Debug)]
    struct Raw(BytesMut);

    impl ToSql for Raw {
        fn to_sql(
            &self,
            _ty: &Type,
            out: &mut BytesMut,
        ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
            out.put_slice(&self.0);
            Ok(IsNull::No)
        }

        fn accepts(_ty: &Type) -> bool {
            true
        }

        to_sql_checked!();
    }

    /// `members` as Postgres sends a one-dimensional array of `array_ty`.
    fn array_bytes(array_ty: &Type, members: Vec<BytesMut>) -> BytesMut {
        let mut out = BytesMut::new();
        members
            .into_iter()
            .map(Raw)
            .collect::<Vec<_>>()
            .to_sql(array_ty, &mut out)
            .expect("an array of raw members encodes");
        out
    }

    fn decode(ty: &Type, raw: &[u8]) -> Result<PgValue, String> {
        PgValue::from_sql(ty, raw).map_err(|e| e.to_string())
    }

    #[test]
    fn a_path_column_decodes_to_its_points_whether_closed_or_open() {
        // 512 is here on purpose: its leading bytes, read as a point count,
        // are `i32::MIN`.
        let points = [(0.0, 0.0), (512.0, 2.5), (-1.0, 1.0)];
        let expected: Vec<Point> = points.iter().map(|(x, y)| point(*x, *y)).collect();
        for closed in [true, false] {
            match decode(&Type::PATH, &path_bytes(closed, &points)) {
                Ok(PgValue::Path(decoded)) => assert_eq!(decoded, expected, "closed={closed}"),
                other => panic!("closed={closed}: expected a path, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_polygon_column_decodes_to_its_points() {
        let points = [(0.0, 0.0), (4.0, 0.0), (4.0, -3.0)];
        let expected: Vec<Point> = points.iter().map(|(x, y)| point(*x, *y)).collect();
        match decode(&Type::POLYGON, &polygon_bytes(&points)) {
            Ok(PgValue::Polygon(decoded)) => assert_eq!(decoded, expected),
            other => panic!("expected a polygon, got {other:?}"),
        }
    }

    #[test]
    fn path_and_polygon_array_columns_decode_to_one_point_list_per_member() {
        let first = [(0.0, 0.0), (1.0, 1.0)];
        let second = [(5.0, 5.0), (6.0, 6.0), (7.0, 7.0)];
        let expected: Vec<Vec<Point>> = [first.as_slice(), second.as_slice()]
            .iter()
            .map(|points| points.iter().map(|(x, y)| point(*x, *y)).collect())
            .collect();

        let paths = array_bytes(
            &Type::PATH_ARRAY,
            vec![path_bytes(true, &first), path_bytes(false, &second)],
        );
        match decode(&Type::PATH_ARRAY, &paths) {
            Ok(PgValue::PathArray(decoded)) => assert_eq!(decoded, expected),
            other => panic!("expected a path array, got {other:?}"),
        }

        let polygons = array_bytes(
            &Type::POLYGON_ARRAY,
            vec![polygon_bytes(&first), polygon_bytes(&second)],
        );
        match decode(&Type::POLYGON_ARRAY, &polygons) {
            Ok(PgValue::PolygonArray(decoded)) => assert_eq!(decoded, expected),
            other => panic!("expected a polygon array, got {other:?}"),
        }
    }

    #[test]
    fn empty_path_and_polygon_arrays_decode_to_no_members() {
        match decode(
            &Type::PATH_ARRAY,
            &array_bytes(&Type::PATH_ARRAY, Vec::new()),
        ) {
            Ok(PgValue::PathArray(decoded)) => assert!(decoded.is_empty()),
            other => panic!("expected a path array, got {other:?}"),
        }
        match decode(
            &Type::POLYGON_ARRAY,
            &array_bytes(&Type::POLYGON_ARRAY, Vec::new()),
        ) {
            Ok(PgValue::PolygonArray(decoded)) => assert!(decoded.is_empty()),
            other => panic!("expected a polygon array, got {other:?}"),
        }
    }

    /// An `lseg` as Postgres sends it (`lseg_send`): its two points' four
    /// `float8`s. A `line` (`line_send`) is three: `A`, `B` and `C`.
    fn float8_bytes(floats: &[f64]) -> BytesMut {
        let mut out = BytesMut::new();
        for float in floats {
            out.put_f64(*float);
        }
        out
    }

    #[test]
    fn an_lseg_column_decodes_to_its_two_points() {
        // Read as a path, the first of these would declare a negative point
        // count and the second `i32::MIN`; both used to panic the decoder.
        for [x1, y1, x2, y2] in [[1.0, 2.0, 3.0, 4.0], [512.0, 0.0, -1.5, 1.0]] {
            match decode(&Type::LSEG, &float8_bytes(&[x1, y1, x2, y2])) {
                Ok(PgValue::Lseg(decoded)) => {
                    assert_eq!(decoded, (point(x1, y1), point(x2, y2)));
                }
                other => panic!("expected an lseg, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_lseg_array_column_decodes_to_one_pair_of_points_per_member() {
        let raw = array_bytes(
            &Type::LSEG_ARRAY,
            vec![
                float8_bytes(&[1.0, 2.0, 3.0, 4.0]),
                float8_bytes(&[0.0, 0.0, 1.0, 1.0]),
            ],
        );
        match decode(&Type::LSEG_ARRAY, &raw) {
            Ok(PgValue::LsegArray(decoded)) => assert_eq!(
                decoded,
                vec![
                    (point(1.0, 2.0), point(3.0, 4.0)),
                    (point(0.0, 0.0), point(1.0, 1.0)),
                ]
            ),
            other => panic!("expected an lseg array, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_lseg_is_an_error_naming_the_type() {
        let whole = float8_bytes(&[1.0, 2.0, 3.0, 4.0]);
        let mut trailing = whole.clone();
        trailing.put_u8(0);
        for (what, raw) in [
            ("no bytes", &[][..]),
            ("three floats", &whole[..24]),
            ("a truncated float", &whole[..31]),
            ("trailing bytes", &trailing[..]),
        ] {
            let err = decode(&Type::LSEG, raw).expect_err(what);
            assert!(err.contains("lseg"), "{what}: {err}");
        }
    }

    #[test]
    fn a_line_column_is_refused_rather_than_read_as_a_path() {
        // `{1,-1,0}` is the line `y = x`. Read as a path its bytes declare a
        // negative point count, which used to panic the decoder.
        let line = float8_bytes(&[1.0, -1.0, 0.0]);
        let err = decode(&Type::LINE, &line).expect_err("a line has no pg-value");
        assert!(err.contains("line & line[] are not supported"), "{err}");

        let lines = array_bytes(&Type::LINE_ARRAY, vec![line]);
        let err = decode(&Type::LINE_ARRAY, &lines).expect_err("nor has an array of them");
        assert!(err.contains("line & line[] are not supported"), "{err}");
    }

    #[test]
    fn a_malformed_path_or_polygon_is_an_error_naming_the_type() {
        let whole = polygon_bytes(&[(0.0, 0.0), (1.0, 1.0)]);
        let truncated = &whole[..whole.len() - 1];
        let mut trailing = whole.clone();
        trailing.put_u8(0);
        let mut negative = BytesMut::new();
        negative.put_i32(-1);
        let mut overstated = BytesMut::new();
        overstated.put_i32(i32::MAX);

        for (what, raw) in [
            ("no bytes", &[][..]),
            ("a short count", &whole[..3]),
            ("a truncated point", truncated),
            ("trailing bytes", &trailing[..]),
            ("a negative count", &negative[..]),
            ("a count with no points behind it", &overstated[..]),
        ] {
            let err = decode(&Type::POLYGON, raw).expect_err(what);
            assert!(err.contains("polygon"), "{what}: {err}");
        }

        // A path's count sits one byte in, behind its closed flag.
        for (what, raw) in [("no bytes", &[][..]), ("only the closed flag", &[1][..])] {
            let err = decode(&Type::PATH, raw).expect_err(what);
            assert!(err.contains("path"), "{what}: {err}");
        }
    }
}
