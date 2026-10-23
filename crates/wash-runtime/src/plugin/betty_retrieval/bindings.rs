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
        Date, HashableF64, MacAddressEui48, MacAddressEui64, Offset, PgValue, Time, Timestamp,
        TimestampTz,
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

    /// `value` encoded as `ty` on a thread of its own, giving up after a
    /// second. A numeric in exponent form used to make that encoding raise ten
    /// to a power of about four billion, which never finished and took memory
    /// until the host was killed.
    fn encode_within_a_second(value: PgValue, ty: Type) -> Result<BytesMut, String> {
        use std::sync::mpsc::{RecvTimeoutError, channel};
        let what = format!("{value:?} as {ty}");
        let (done, outcome) = channel();
        std::thread::spawn(move || {
            let mut out = BytesMut::new();
            let encoded = value
                .to_sql(&ty, &mut out)
                .map(|_| out)
                .map_err(|e| e.to_string());
            let _ = done.send(encoded);
        });
        match outcome.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(encoded) => encoded,
            Err(RecvTimeoutError::Timeout) => panic!("encoding {what} took over a second"),
            Err(RecvTimeoutError::Disconnected) => panic!("encoding {what} panicked"),
        }
    }

    /// What Postgres reads from an encoded `numeric`, by an independent decoder.
    fn numeric_read_back(raw: &[u8]) -> String {
        pg_bigdecimal::PgNumeric::from_sql(&Type::NUMERIC, raw)
            .expect("an encoded numeric decodes")
            .n
            .expect("not NaN")
            .to_string()
    }

    #[test]
    fn a_numeric_in_exponent_form_encodes_its_value_quickly() {
        for (text, value) in [
            ("1e5", "100000"),
            ("1e+21", "1000000000000000000000"),
            ("1.5e3", "1500"),
            ("-2.5E2", "-250"),
            ("12e-1", "1.2"),
            ("1.50e1", "15.0"),
        ] {
            for variant in [
                PgValue::Numeric(text.to_string()),
                PgValue::Decimal(text.to_string()),
            ] {
                let raw = encode_within_a_second(variant, Type::NUMERIC)
                    .unwrap_or_else(|e| panic!("{text} must encode: {e}"));
                assert_eq!(numeric_read_back(&raw), value, "{text}");
            }
        }
    }

    #[test]
    fn a_numeric_array_with_an_exponent_member_encodes_its_values_quickly() {
        let raw = encode_within_a_second(
            PgValue::NumericArray(vec!["1e5".to_string(), "2.50".to_string()]),
            Type::NUMERIC_ARRAY,
        )
        .expect("the array encodes");
        let members: Vec<String> =
            Vec::<pg_bigdecimal::PgNumeric>::from_sql(&Type::NUMERIC_ARRAY, &raw)
                .expect("the encoded array decodes")
                .into_iter()
                .map(|member| member.n.expect("not NaN").to_string())
                .collect();
        assert_eq!(members, ["100000", "2.50"]);
    }

    /// Postgres holds at most 131072 digits before a numeric's point and
    /// 16383 after it. A value past either is refused before any of it is
    /// expanded, and an exponent past an `i64` is refused rather than
    /// overflowing.
    #[test]
    fn a_numeric_past_what_postgres_holds_is_refused_quickly() {
        for text in [
            "1e131072",
            "-9.9e131072",
            "1e-16384",
            "0.5e-16383",
            "1e99999999999999999999",
            "1e-9223372036854775808",
            "1e9223372036854775807",
        ] {
            let refused = encode_within_a_second(PgValue::Numeric(text.to_string()), Type::NUMERIC);
            assert!(refused.is_err(), "{text} must be refused");
        }
        let array = PgValue::NumericArray(vec!["1".to_string(), "1e131072".to_string()]);
        assert!(encode_within_a_second(array, Type::NUMERIC_ARRAY).is_err());
    }

    /// The numerics at Postgres's two limits, in its binary form: a digit
    /// count, a weight in base 10000, a sign, a display scale, then the digits.
    /// One followed by 131071 zeros is the digit 1000 at weight 32767; one at
    /// the 16383rd place after the point is the digit 10 at weight -4096.
    #[test]
    fn a_numeric_at_postgres_limits_still_encodes() {
        let raw = encode_within_a_second(PgValue::Numeric("1e131071".to_string()), Type::NUMERIC)
            .expect("131072 digits before the point fit");
        assert_eq!(raw[..], [0, 1, 0x7f, 0xff, 0, 0, 0, 0, 0x03, 0xe8]);
        let raw = encode_within_a_second(PgValue::Numeric("1e-16383".to_string()), Type::NUMERIC)
            .expect("16383 digits after the point fit");
        assert_eq!(raw[..], [0, 1, 0xf0, 0x00, 0, 0, 0x3f, 0xff, 0, 10]);
    }

    /// Every digit Postgres holds, before the point and after it, still
    /// encodes well inside the second: each digit is read once.
    #[test]
    fn a_numeric_with_every_digit_postgres_holds_encodes_quickly() {
        let widest = format!("{}.{}", "9".repeat(131_072), "9".repeat(16_383));
        let raw = encode_within_a_second(PgValue::Numeric(widest), Type::NUMERIC)
            .expect("the widest numeric encodes");
        // 32768 base-10000 digits before the point and 4096 after it.
        assert_eq!(raw[..8], [0x90, 0x00, 0x7f, 0xff, 0, 0, 0x3f, 0xff]);
        assert_eq!(raw.len(), 8 + 2 * 36_864);
    }

    #[test]
    fn nan_and_the_infinities_encode_as_their_own_signs() {
        for (text, sign) in [
            ("NaN", 0xc0_u8),
            ("nan", 0xc0),
            ("Infinity", 0xd0),
            ("+inf", 0xd0),
            ("-Infinity", 0xf0),
        ] {
            let raw = encode_within_a_second(PgValue::Numeric(text.to_string()), Type::NUMERIC)
                .unwrap_or_else(|e| panic!("{text} must encode: {e}"));
            assert_eq!(raw[..], [0, 0, 0, 0, sign, 0, 0, 0], "{text}");
        }
    }

    #[test]
    fn text_that_is_not_a_number_is_refused_as_a_numeric() {
        for text in [
            "", "-", ".", "e5", "1e", "1e+", "1.2.3", "1,5", " 1", "1 ", "0x10", "1_000",
        ] {
            assert!(
                encode_within_a_second(PgValue::Numeric(text.to_string()), Type::NUMERIC).is_err(),
                "{text:?} must be refused"
            );
        }
    }

    /// Every ordinary value encodes byte for byte as the `pg_bigdecimal`
    /// encoder, which handled these correctly, wrote it.
    #[test]
    fn an_ordinary_numeric_encodes_as_before() {
        use std::str::FromStr as _;
        for text in [
            "1",
            "-1",
            "7",
            "10000",
            "10000.5",
            "123.45",
            "1.50",
            "0.001",
            "-0.0000001",
            "0.00010000",
            "99999999",
            "100000000",
            "98765432109876543210.0123456789",
            "+42.4200",
            "000123.4500",
        ] {
            let mut expected = BytesMut::new();
            pg_bigdecimal::PgNumeric::new(Some(
                pg_bigdecimal::BigDecimal::from_str(text).expect("the reference parses it"),
            ))
            .to_sql(&Type::NUMERIC, &mut expected)
            .expect("the reference encodes it");
            let raw = encode_within_a_second(PgValue::Numeric(text.to_string()), Type::NUMERIC)
                .unwrap_or_else(|e| panic!("{text} must encode: {e}"));
            assert_eq!(raw, expected, "{text}");
        }
    }

    /// Zero has no digits, whatever its scale or sign.
    #[test]
    fn zero_encodes_with_no_digits_and_its_scale() {
        for (text, value) in [
            ("0", "0"),
            ("-0", "0"),
            ("0.00", "0.00"),
            ("0e5", "0"),
            ("0.0e-3", "0.0000"),
        ] {
            let raw = encode_within_a_second(PgValue::Numeric(text.to_string()), Type::NUMERIC)
                .unwrap_or_else(|e| panic!("{text} must encode: {e}"));
            assert_eq!(raw[..2], [0, 0], "{text}: no digits");
            assert_eq!(numeric_read_back(&raw), value, "{text}");
        }
    }

    /// `text` encoded as a `numeric`, the way the server sends one.
    fn numeric_bytes(text: &str) -> BytesMut {
        let mut out = BytesMut::new();
        PgValue::Numeric(text.to_string())
            .to_sql(&Type::NUMERIC, &mut out)
            .unwrap_or_else(|e| panic!("{text} encodes: {e}"));
        out
    }

    /// A `numeric` column was read as text, so `SELECT 1::numeric` gave
    /// `"\0\u{1}\0\0\0\0\0\0\0\u{1}"`.
    #[test]
    fn a_numeric_column_decodes_to_its_decimal_text() {
        for (text, read) in [
            ("1", "1"),
            ("123.45", "123.45"),
            ("-0.001", "-0.001"),
            ("1.50", "1.50"),
            ("0", "0"),
            ("0.00", "0.00"),
            ("10000", "10000"),
            ("5050", "5050"),
            ("1e5", "100000"),
            (
                "98765432109876543210.0123456789",
                "98765432109876543210.0123456789",
            ),
            ("-123456789.000000001", "-123456789.000000001"),
            ("NaN", "NaN"),
            ("Infinity", "Infinity"),
            ("-Infinity", "-Infinity"),
        ] {
            match decode(&Type::NUMERIC, &numeric_bytes(text)) {
                Ok(PgValue::Numeric(decoded)) => assert_eq!(decoded, read, "{text}"),
                other => panic!("{text}: expected a numeric, got {other:?}"),
            }
        }
    }

    /// Zero as `pg_bigdecimal` wrote it, at weight -2: a sender other than
    /// Postgres may not normalise it.
    #[test]
    fn a_numeric_zero_at_any_weight_decodes_to_zero() {
        let raw = [0, 0, 0xff, 0xfe, 0, 0, 0, 2];
        match decode(&Type::NUMERIC, &raw) {
            Ok(PgValue::Numeric(decoded)) => assert_eq!(decoded, "0.00"),
            other => panic!("expected a numeric, got {other:?}"),
        }
    }

    #[test]
    fn a_numeric_array_column_decodes_each_member() {
        let raw = array_bytes(
            &Type::NUMERIC_ARRAY,
            vec![
                numeric_bytes("1.5"),
                numeric_bytes("-2"),
                numeric_bytes("NaN"),
            ],
        );
        match decode(&Type::NUMERIC_ARRAY, &raw) {
            Ok(PgValue::NumericArray(decoded)) => assert_eq!(decoded, ["1.5", "-2", "NaN"]),
            other => panic!("expected a numeric array, got {other:?}"),
        }
    }

    /// Decoding reads each digit once: the widest numeric Postgres holds
    /// comes back well inside a second. `pg_bigdecimal`'s decoder took 18
    /// seconds for 40,000 digits.
    #[test]
    fn the_widest_numeric_decodes_quickly() {
        use std::sync::mpsc::channel;
        let widest = format!("{}.{}", "9".repeat(131_072), "9".repeat(16_383));
        let raw = numeric_bytes(&widest);
        let (done, decoded) = channel();
        std::thread::spawn(move || {
            let _ = done.send(decode(&Type::NUMERIC, &raw));
        });
        match decoded.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(Ok(PgValue::Numeric(text))) => assert!(text == widest, "the widest numeric changed"),
            Ok(other) => panic!(
                "expected a numeric, got {:?}",
                other.map(|_| "another value")
            ),
            Err(e) => panic!("decoding the widest numeric did not finish within a second: {e}"),
        }
    }

    #[test]
    fn a_malformed_numeric_is_an_error_not_a_value() {
        let mut too_long = numeric_bytes("1.5").to_vec();
        too_long.push(0);
        for (what, raw) in [
            ("no bytes", vec![]),
            ("a short header", vec![0, 1, 0, 0]),
            ("a missing digit", vec![0, 1, 0, 0, 0, 0, 0, 0]),
            (
                "a digit past 9999",
                vec![0, 1, 0, 0, 0, 0, 0, 0, 0x27, 0x10],
            ),
            ("an unknown sign", vec![0, 0, 0, 0, 0x12, 0x34, 0, 0]),
            ("a trailing byte", too_long),
        ] {
            assert!(decode(&Type::NUMERIC, &raw).is_err(), "{what}");
        }
    }

    /// Postgres sends `money` as an `int8` of cents (two places for `C` and
    /// most locales of `lc_monetary`).
    #[test]
    fn a_money_column_decodes_to_its_amount() {
        for (cents, amount) in [
            (12_345_i64, "123.45"),
            (-5, "-0.05"),
            (0, "0.00"),
            (100, "1.00"),
            (i64::MIN, "-92233720368547758.08"),
            (i64::MAX, "92233720368547758.07"),
        ] {
            match decode(&Type::MONEY, &cents.to_be_bytes()) {
                Ok(PgValue::Money(decoded)) => assert_eq!(decoded, amount, "{cents}"),
                other => panic!("{cents}: expected money, got {other:?}"),
            }
        }
        let raw = array_bytes(
            &Type::MONEY_ARRAY,
            [1_234_i64, -1]
                .iter()
                .map(|cents| BytesMut::from(&cents.to_be_bytes()[..]))
                .collect(),
        );
        match decode(&Type::MONEY_ARRAY, &raw) {
            Ok(PgValue::MoneyArray(decoded)) => assert_eq!(decoded, ["12.34", "-0.01"]),
            other => panic!("expected a money array, got {other:?}"),
        }
        assert!(
            decode(&Type::MONEY, &[0, 1, 2]).is_err(),
            "three bytes are no money"
        );
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
