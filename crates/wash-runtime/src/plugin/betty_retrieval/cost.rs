//! What a fetched row costs the host while it holds it, and the caps on the
//! rows it holds.
//!
//! A row's cost is an upper bound of the memory its decoded form takes, not
//! its bytes on the wire: a `text[]` element is 4 bytes on the wire and a
//! 24-byte `String` once decoded, and a numeric of 10 wire bytes can print as
//! 131,072 characters.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_postgres::types::{FromSql, Kind, Type};

use super::bindings::wasmcloud::postgres::types::PgValue;

/// What the host holds for each row beyond its values' bytes: the row as
/// tokio-postgres keeps it, and the list of `pg-value`s it becomes.
pub(crate) const ROW_OVERHEAD: usize = size_of::<tokio_postgres::Row>() + size_of::<Vec<PgValue>>();

/// What the host holds for each value beyond its bytes: where it lies in its
/// row, and the `pg-value` it becomes.
pub(crate) const COLUMN_OVERHEAD: usize =
    size_of::<Option<std::ops::Range<usize>>>() + size_of::<PgValue>();

/// The most one row may cost; a larger one fails its query.
pub(crate) const ROW_MAX_BYTES: usize = 64 * 1024 * 1024;

/// The most the rows of one auto-committed query waiting for the guest may
/// cost together.
pub(crate) const ROW_BUFFER_MAX_BYTES: usize = 64 * 1024 * 1024;

/// What a row of `columns` values, `value_bytes` of them on the wire, costs.
/// Rows of NULLs or empty strings come with no value bytes, and still cost
/// the host something each.
pub(crate) fn cost(columns: usize, value_bytes: usize) -> usize {
    ROW_OVERHEAD
        .saturating_add(columns.saturating_mul(COLUMN_OVERHEAD))
        .saturating_add(value_bytes)
}

/// How many times its wire bytes a json or jsonb value is charged.
///
/// Decoding builds a `serde_json::Value` of about 32 bytes a node, and the
/// text has one node per 2 to 4 bytes of numbers and short strings; the tree
/// is printed back and dropped, so it is held for one value at a time. This is
/// a fixed factor, not a bound: a document of nothing but `[1],[1],...` builds
/// more (the operator doc says so).
pub(crate) const JSON_FACTOR: usize = 10;

/// What `row` costs: its decoded size, at most.
pub(crate) fn row_cost(row: &tokio_postgres::Row) -> usize {
    let value_bytes = (0..row.len())
        .map(|i| row.try_get::<_, ColumnBytes>(i).map_or(0, |len| len.0))
        .fold(0_usize, usize::saturating_add);
    cost(row.len(), value_bytes)
}

/// What a column holds once decoded, beyond the `pg-value` that holds it,
/// read from its wire form without decoding it.
struct ColumnBytes(usize);

impl<'a> FromSql<'a> for ColumnBytes {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self(column_bytes(ty, raw)))
    }

    fn from_sql_null(_: &Type) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self(0))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

/// The most the host holds for a non-null column of type `ty` whose wire
/// bytes are `raw`, once decoded.
pub(crate) fn column_bytes(ty: &Type, raw: &[u8]) -> usize {
    match ty.kind() {
        Kind::Array(element) => array_bytes(element, raw),
        Kind::Domain(base) => column_bytes(base, raw),
        _ => scalar_bytes(ty, raw),
    }
}

/// A non-array value: its wire bytes, except where decoding grows them.
fn scalar_bytes(ty: &Type, raw: &[u8]) -> usize {
    if *ty == Type::JSON || *ty == Type::JSONB {
        raw.len().saturating_mul(JSON_FACTOR)
    } else if *ty == Type::NUMERIC {
        numeric_text_bound(raw)
    } else if *ty == Type::MONEY {
        // "-92233720368547758.08"
        24
    } else if *ty == Type::UUID {
        // 16 bytes print as 36 characters.
        36
    } else if *ty == Type::INET || *ty == Type::CIDR {
        // The longest, "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128".
        64
    } else if *ty == Type::PATH || *ty == Type::POLYGON {
        // A float8 is 8 bytes on the wire and 16 as the guest's tuple.
        raw.len().saturating_mul(2)
    } else {
        raw.len()
    }
}

/// An array: what each member occupies in the guest's list (the header gives
/// the count), plus what each member points to.
fn array_bytes(element: &Type, raw: &[u8]) -> usize {
    let slots = element_count(raw).saturating_mul(element_size(element));
    if is_inline(element) {
        return slots;
    }
    array_elements(raw).fold(slots, |sum, member| {
        sum.saturating_add(scalar_bytes(element, member))
    })
}

/// Whether a member of an array of `element` holds all it is in the slot the
/// list gives it: numbers, dates, times, and the fixed geometry.
fn is_inline(element: &Type) -> bool {
    matches!(
        *element,
        Type::BOOL
            | Type::INT2
            | Type::INT4
            | Type::INT8
            | Type::OID
            | Type::FLOAT4
            | Type::FLOAT8
            | Type::DATE
            | Type::TIME
            | Type::TIMESTAMP
            | Type::TIMESTAMPTZ
            | Type::INTERVAL
            | Type::PG_LSN
            | Type::MACADDR
            | Type::MACADDR8
            | Type::POINT
            | Type::BOX
            | Type::LSEG
            | Type::LINE
            | Type::CIRCLE
    )
}

/// The number of elements of an array in binary form: the product of its
/// dimensions, and never more than its bytes can hold at four a piece (each
/// element has a four-byte length, `-1` for NULL), so a header that lies
/// costs only what its bytes could be.
fn element_count(raw: &[u8]) -> usize {
    const HEADER: usize = 12;
    let room = raw.len().saturating_sub(HEADER) / 4;
    let Some(ndim) = be_i32(raw, 0).and_then(|n| usize::try_from(n).ok()) else {
        return room;
    };
    if ndim == 0 {
        return 0;
    }
    let mut count = 1_usize;
    for dim in 0..ndim.min(raw.len() / 8) {
        match be_i32(raw, HEADER + dim * 8).and_then(|n| usize::try_from(n).ok()) {
            Some(len) => count = count.saturating_mul(len),
            None => return room,
        }
    }
    count.min(room)
}

/// The elements of a binary array, as the bytes of each one (none for NULL).
fn array_elements(raw: &[u8]) -> impl Iterator<Item = &[u8]> {
    let ndim = be_i32(raw, 0)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);
    let mut at = 12 + ndim.min(raw.len() / 8) * 8;
    std::iter::from_fn(move || {
        let len = be_i32(raw, at)?;
        at += 4;
        let Ok(len) = usize::try_from(len) else {
            return Some(&[][..]);
        };
        let element = raw.get(at..at.checked_add(len)?)?;
        at += len;
        Some(element)
    })
}

fn be_i32(raw: &[u8], at: usize) -> Option<i32> {
    let bytes = raw.get(at..at.checked_add(4)?)?;
    Some(i32::from_be_bytes(bytes.try_into().ok()?))
}

/// The characters a binary numeric prints as, at most: four to each base-10000
/// digit before the point, its display scale after, and a sign and a point.
/// Ten wire bytes (`1e131071`) print as 131,072 characters.
fn numeric_text_bound(raw: &[u8]) -> usize {
    let (Some(weight), Some(dscale)) = (
        raw.get(2..4)
            .and_then(|b| b.try_into().ok())
            .map(i16::from_be_bytes),
        raw.get(6..8)
            .and_then(|b| b.try_into().ok())
            .map(u16::from_be_bytes),
    ) else {
        return raw.len();
    };
    let whole = usize::try_from(i32::from(weight) + 1).unwrap_or(0) * 4;
    // "-Infinity" is the longest name.
    (whole + usize::from(dscale) + 2).max(9).max(raw.len())
}

/// What one member of an array of `element` occupies in the guest's list,
/// beyond the bytes it points to.
fn element_size(element: &Type) -> usize {
    match *element {
        Type::BOOL
        | Type::INT2
        | Type::INT4
        | Type::INT8
        | Type::OID
        | Type::FLOAT4
        | Type::FLOAT8
        | Type::DATE
        | Type::TIME
        | Type::PG_LSN
        | Type::MACADDR
        | Type::MACADDR8 => 16,
        // A `String` or a `Vec`.
        Type::TEXT
        | Type::NAME
        | Type::XML
        | Type::JSON
        | Type::JSONB
        | Type::UUID
        | Type::INET
        | Type::CIDR
        | Type::NUMERIC
        | Type::MONEY
        | Type::BYTEA
        | Type::PATH
        | Type::POLYGON
        | Type::INT2_VECTOR => 24,
        // An `(Option<u32>, Vec<u8>)` or `(u32, Vec<u8>)`.
        Type::VARCHAR | Type::BPCHAR | Type::BIT | Type::VARBIT => 32,
        // Timestamps, intervals, geometry: records of records.
        _ => 64,
    }
}

/// The rows of one auto-committed query that wait between its fetch task and
/// the guest, held to [`ROW_BUFFER_MAX_BYTES`] together.
pub(crate) struct RowBuffer {
    room: Arc<Semaphore>,
}

/// A row larger than [`ROW_MAX_BYTES`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RowTooLarge;

impl RowBuffer {
    pub(crate) fn new() -> Self {
        Self {
            room: Arc::new(Semaphore::new(ROW_BUFFER_MAX_BYTES)),
        }
    }

    /// Room for a row that costs `cost`, once the guest has read enough of
    /// the rows before it. Holding the permit holds the room.
    pub(crate) async fn reserve(&self, cost: usize) -> Result<OwnedSemaphorePermit, RowTooLarge> {
        if cost > ROW_MAX_BYTES {
            return Err(RowTooLarge);
        }
        let cost = u32::try_from(cost).map_err(|_| RowTooLarge)?;
        Arc::clone(&self.room)
            .acquire_many_owned(cost)
            .await
            // Nothing closes the semaphore.
            .map_err(|_closed| RowTooLarge)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::plugin::betty_retrieval::bindings::wasmcloud::postgres::types::PgValue;

    #[test]
    fn a_row_costs_its_bytes_and_what_holds_it() {
        assert_eq!(cost(0, 0), ROW_OVERHEAD);
        assert_eq!(cost(3, 10), ROW_OVERHEAD + 3 * COLUMN_OVERHEAD + 10);
        assert_eq!(cost(usize::MAX, usize::MAX), usize::MAX, "saturates");
    }

    /// The operator doc gives these two numbers.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn a_row_and_a_value_each_cost_96_bytes_on_a_64_bit_host() {
        assert_eq!(ROW_OVERHEAD, 96);
        assert_eq!(COLUMN_OVERHEAD, 96);
    }

    /// The cap used to count value bytes alone, which a row of NULLs has
    /// none of.
    #[test]
    fn a_row_of_nulls_costs_something() {
        assert!(cost(1, 0) >= ROW_OVERHEAD + COLUMN_OVERHEAD);
    }

    #[tokio::test]
    async fn a_row_over_the_most_one_row_may_be_is_refused() {
        let buffer = RowBuffer::new();
        assert_eq!(
            buffer.reserve(ROW_MAX_BYTES + 1).await.err(),
            Some(RowTooLarge)
        );
        let _room = buffer
            .reserve(ROW_MAX_BYTES)
            .await
            .expect("a row of exactly the most fits an empty buffer");
    }

    #[tokio::test]
    async fn rows_wait_for_room_until_the_guest_reads_the_ones_before() {
        let buffer = RowBuffer::new();
        let first = buffer
            .reserve(ROW_BUFFER_MAX_BYTES / 2 + 1)
            .await
            .expect("the first row fits");
        let second = buffer.reserve(ROW_BUFFER_MAX_BYTES / 2 + 1);
        tokio::pin!(second);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut second)
                .await
                .is_err(),
            "two rows over half the buffer each must not both wait in it"
        );
        drop(first);
        let _room = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .expect("the room the first row held is free once it is read")
            .expect("the second row fits then");
    }

    /// An array as Postgres sends it in binary: header, one dimension, and
    /// each element behind its four-byte length (-1 for NULL).
    fn wire_array(elem: &Type, elements: &[Option<&[u8]>]) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend(1_i32.to_be_bytes());
        raw.extend(0_i32.to_be_bytes());
        raw.extend(elem.oid().to_be_bytes());
        raw.extend(i32::try_from(elements.len()).unwrap().to_be_bytes());
        raw.extend(1_i32.to_be_bytes());
        for element in elements {
            match element {
                Some(bytes) => {
                    raw.extend(i32::try_from(bytes.len()).unwrap().to_be_bytes());
                    raw.extend(*bytes);
                }
                None => raw.extend((-1_i32).to_be_bytes()),
            }
        }
        raw
    }

    /// A text[] of empty strings is 4 wire bytes an element and a 24-byte
    /// `String` decoded: the cap counted the 4.
    #[test]
    fn a_text_array_costs_what_it_decodes_to() {
        let count = 1000;
        let raw = wire_array(&Type::TEXT, &vec![Some(&b""[..]); count]);
        let cost = column_bytes(&Type::TEXT_ARRAY, &raw);
        assert!(cost >= count * size_of::<String>(), "{cost}");
        // Decoded for real, it is not more than it was charged.
        let decoded = Vec::<String>::from_sql(&Type::TEXT_ARRAY, &raw).unwrap();
        assert!(cost >= decoded.capacity() * size_of::<String>());
    }

    /// `SELECT array_fill(''::text, ARRAY[15000000])`: 60 MB on the wire and
    /// about 360 MB decoded, which the 64 MiB cap has to refuse.
    #[test]
    fn a_text_array_of_many_empty_strings_is_over_the_decoded_budget() {
        let count = 15_000_000;
        // The array header says how many; its elements need not be built.
        let mut raw = wire_array(&Type::TEXT, &[]);
        let wire = 20 + 4 * count;
        raw.resize(wire, 0);
        raw[12..16].copy_from_slice(&i32::try_from(count).unwrap().to_be_bytes());
        for slot in raw[20..].chunks_exact_mut(4) {
            slot.copy_from_slice(&0_i32.to_be_bytes());
        }
        let cost = cost(1, column_bytes(&Type::TEXT_ARRAY, &raw));
        assert!(
            cost >= count * size_of::<String>(),
            "{cost} bytes for {count} Strings"
        );
        assert!(cost > ROW_MAX_BYTES, "{cost} must be refused as one row");
        assert!(wire < ROW_MAX_BYTES, "the wire bytes alone would pass");
    }

    #[test]
    fn a_varchar_array_costs_its_tuples() {
        let raw = wire_array(&Type::VARCHAR, &vec![Some(&b""[..]); 100]);
        assert!(column_bytes(&Type::VARCHAR_ARRAY, &raw) >= 100 * 32);
    }

    #[test]
    fn an_array_header_that_lies_still_costs_no_more_than_its_bytes_allow() {
        // Claims 2^31-1 elements in 20 bytes.
        let mut raw = wire_array(&Type::TEXT, &[]);
        raw[12..16].copy_from_slice(&i32::MAX.to_be_bytes());
        let cost = column_bytes(&Type::TEXT_ARRAY, &raw);
        assert!(cost < 1024, "{cost}");
        // A header too short to read costs its bytes.
        assert!(column_bytes(&Type::TEXT_ARRAY, &[0, 0]) < 1024);
    }

    /// A json or jsonb is parsed into a `serde_json::Value` (32 bytes a node
    /// where the text has a byte or two) and printed back, so it costs a
    /// fixed multiple of its wire bytes.
    #[test]
    fn a_json_value_is_charged_for_its_decoded_form() {
        let text = format!("[{}]", vec!["0"; 100_000].join(","));
        let mut jsonb = vec![1_u8];
        jsonb.extend(text.as_bytes());
        for (ty, raw) in [
            (&Type::JSONB, jsonb),
            (&Type::JSON, text.clone().into_bytes()),
        ] {
            let cost = column_bytes(ty, &raw);
            assert!(cost >= raw.len() * JSON_FACTOR, "{ty}: {cost}");
        }
        let array = wire_array(&Type::JSONB, &[Some(&[1, b'1'][..]), Some(&[1, b'2'][..])]);
        // Two members of two bytes each, and the slots that hold them.
        assert!(column_bytes(&Type::JSONB_ARRAY, &array) >= 4 * JSON_FACTOR + 2 * 24);
    }

    /// 1e131071 is ten bytes on the wire and 131,072 characters printed.
    #[test]
    fn a_numeric_is_charged_for_the_digits_it_prints() {
        let mut raw = Vec::new();
        raw.extend(1_u16.to_be_bytes()); // ndigits
        raw.extend(32_767_i16.to_be_bytes()); // weight
        raw.extend(0_u16.to_be_bytes()); // sign
        raw.extend(0_u16.to_be_bytes()); // dscale
        raw.extend(1_i16.to_be_bytes());
        assert!(column_bytes(&Type::NUMERIC, &raw) >= 131_072);
        let array = wire_array(&Type::NUMERIC, &[Some(&raw)]);
        assert!(column_bytes(&Type::NUMERIC_ARRAY, &array) >= 131_072);
        // A small one stays small.
        let one = [0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
        assert!(column_bytes(&Type::NUMERIC, &one) < 64);
    }

    /// Values that decode to what they were on the wire cost what they did.
    #[test]
    fn a_plain_value_costs_its_wire_bytes() {
        assert_eq!(column_bytes(&Type::TEXT, b"hello"), 5);
        assert_eq!(column_bytes(&Type::INT8, &[0; 8]), 8);
        assert_eq!(column_bytes(&Type::BYTEA, &[0; 1000]), 1000);
        // A sentence-length text[] is within a small margin of its wire size
        // plus the strings' headers.
        let words: Vec<&[u8]> = vec![b"a few words per element of this list"; 100];
        let raw = wire_array(
            &Type::TEXT,
            &words.iter().map(|w| Some(*w)).collect::<Vec<_>>(),
        );
        let cost = column_bytes(&Type::TEXT_ARRAY, &raw);
        assert!(cost <= raw.len() + 100 * 24, "{cost} vs {}", raw.len());
    }

    /// The per-element sizes the arrays are charged are not below what the
    /// generated types occupy.
    #[test]
    fn an_array_element_is_charged_at_least_its_decoded_size() {
        use super::super::bindings::wasmcloud::postgres::types::{
            Interval, Time, TimeTz, Timestamp, TimestampTz,
        };
        assert!(element_size(&Type::TIMESTAMP) >= size_of::<Timestamp>());
        assert!(element_size(&Type::TIMESTAMPTZ) >= size_of::<TimestampTz>());
        assert!(element_size(&Type::TIMETZ) >= size_of::<TimeTz>());
        assert!(element_size(&Type::TIME) >= size_of::<Time>());
        assert!(element_size(&Type::INTERVAL) >= size_of::<Interval>());
        assert!(element_size(&Type::TEXT) >= size_of::<String>());
        assert!(element_size(&Type::VARCHAR) >= size_of::<(Option<u32>, Vec<u8>)>());
        assert!(element_size(&Type::FLOAT8) >= size_of::<(u64, i16, i8)>());
        assert!(element_size(&Type::BOX) >= size_of::<[((u64, i16, i8), (u64, i16, i8)); 2]>());
        assert!(element_size(&Type::INT8) >= size_of::<i64>());
    }

    /// The same bounds against rows a server really sends: what a row is
    /// charged is not less than the decoded values hold, and is far more than
    /// the wire bytes for the shapes that grow. Needs a database.
    #[tokio::test]
    #[ignore = "needs a database; set BETTY_RETRIEVAL_TEST_DATABASE_URL"]
    async fn a_row_off_the_wire_costs_what_it_decodes_to() {
        use super::super::bindings::conversions::row_to_values;
        let url = std::env::var("BETTY_RETRIEVAL_TEST_DATABASE_URL").expect("database url");
        let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .expect("connect");
        tokio::spawn(connection);
        let wire = |row: &tokio_postgres::Row| -> usize {
            (0..row.len())
                .map(|i| row.try_get::<_, Wire>(i).map_or(0, |w| w.0))
                .sum()
        };
        for (sql, grows_at_least) in [
            ("SELECT array_fill(''::text, ARRAY[100000])", 24),
            ("SELECT array_fill(''::varchar, ARRAY[100000])", 32),
            ("SELECT array_fill(1::int8, ARRAY[100000])", 0),
            ("SELECT array_fill('x'::text, ARRAY[100000])", 24),
            (
                "SELECT (SELECT jsonb_agg(0) FROM generate_series(1, 50000))",
                0,
            ),
            ("SELECT 1e131071::numeric", 0),
            ("SELECT 'hello world, a plain row'::text, 42::int8", 0),
        ] {
            let row = client.query_one(sql, &[]).await.expect(sql);
            let cost = row_cost(&row);
            let on_wire = wire(&row);
            let values = row_to_values(&row).expect("decodes");
            // What the decoded value holds, for the shapes this test selects.
            let held: usize = values
                .iter()
                .map(|v| match v {
                    PgValue::TextArray(items) => {
                        items.capacity() * size_of::<String>()
                            + items.iter().map(String::len).sum::<usize>()
                    }
                    PgValue::VarcharArray(items) => {
                        items.capacity() * size_of::<(Option<u32>, Vec<u8>)>()
                            + items.iter().map(|(_, b)| b.len()).sum::<usize>()
                    }
                    PgValue::Int8Array(items) => items.capacity() * 8,
                    PgValue::Jsonb(text) | PgValue::Numeric(text) | PgValue::Text(text) => {
                        text.len()
                    }
                    _ => 0,
                })
                .sum();
            let charged = cost - ROW_OVERHEAD - row.len() * COLUMN_OVERHEAD;
            assert!(charged >= held, "{sql}: charged {charged}, holds {held}");
            if grows_at_least > 0 {
                assert!(charged >= 100_000 * grows_at_least, "{sql}");
            }
            eprintln!("{sql}: wire {on_wire}, held {held}, charged {charged}");
        }
    }

    /// A column's wire length, as the cap used to count it.
    struct Wire(usize);

    impl<'a> FromSql<'a> for Wire {
        fn from_sql(
            _: &Type,
            raw: &'a [u8],
        ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
            Ok(Self(raw.len()))
        }
        fn accepts(_: &Type) -> bool {
            true
        }
    }
}
