//! Bind parameters: a value passes through, and an embed parameter becomes a
//! pgvector vector before its statement reaches the database.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::BytesMut;
use genius_embed::Embedder;
use pgvector::Vector;
use sha2::{Digest as _, Sha256};
use tokio_postgres::types::{Format, IsNull, Kind, ToSql, Type, WrongType};

use super::bindings::betty_blocks::retrieval::types::{Error, Param, Role};
use super::bindings::wasmcloud::postgres::types::PgValue;
use super::errors::{self, ParamEncodeError};
use super::share::WorkloadShares;
use tokio::sync::OwnedSemaphorePermit;

/// A resolved parameter, owning whatever its statement borrows.
#[derive(Debug)]
pub(crate) enum Bound {
    Value(PgValue),
    Vector(Vector),
    Vectors(Vec<Vector>),
}

impl Bound {
    fn inner(&self) -> &(dyn ToSql + Sync) {
        match self {
            Bound::Value(v) => v,
            Bound::Vector(v) => v,
            Bound::Vectors(v) => v,
        }
    }
}

/// Encodes exactly as the value it holds. A failure is wrapped in
/// [`ParamEncodeError`], so it is reported as the parameter's fault rather
/// than the connection's.
impl ToSql for Bound {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        self.to_sql_checked(ty, out)
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn to_sql_checked(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        // `PgValue` accepts every type, so without this a list would reach a
        // postgres-types slice encoder, which panics on a type that is not an
        // array, and a scalar would be written in its own format whatever the
        // parameter's type.
        if let Bound::Value(value) = self
            && (!has_array_levels(ty, array_levels_encoded(value)) || !scalar_fits(value, ty))
        {
            let wrong = WrongType::new::<PgValue>(ty.clone());
            return Err(ParamEncodeError::new(Box::new(wrong)).into());
        }
        self.inner()
            .to_sql_checked(ty, out)
            .map_err(|e| ParamEncodeError::new(e).into())
    }

    fn encode_format(&self, ty: &Type) -> Format {
        self.inner().encode_format(ty)
    }
}

/// How many array levels the type `value` is bound to must have for the stock
/// conversion to encode it without panicking.
///
/// A slice encoder asks its type for a member type before it looks at a single
/// member, so a list needs one level even when it is empty. It asks a member
/// to encode only once it has one, so a list of lists with no lists in it
/// needs that one level and no more: an empty `path-array` binds to `path[]`,
/// although a `path` is not an array and a `path-array` holding one must still
/// be refused.
fn array_levels_encoded(value: &PgValue) -> usize {
    let outer_is_empty = match value {
        PgValue::Int2VectorArray(lists) => lists.is_empty(),
        PgValue::PathArray(lists) | PgValue::PolygonArray(lists) => lists.is_empty(),
        _ => false,
    };
    if outer_is_empty {
        1
    } else {
        array_levels(value)
    }
}

/// How many nested lists the stock conversion encodes `value` as. `int2-vector`,
/// `path` and `polygon` count too: each of them is encoded as a list.
fn array_levels(value: &PgValue) -> usize {
    match value {
        PgValue::Int2VectorArray(_) | PgValue::PathArray(_) | PgValue::PolygonArray(_) => 2,
        PgValue::Int8Array(_)
        | PgValue::BoolArray(_)
        | PgValue::Float8Array(_)
        | PgValue::Float4Array(_)
        | PgValue::Int4Array(_)
        | PgValue::NumericArray(_)
        | PgValue::Int2Array(_)
        | PgValue::Int2Vector(_)
        | PgValue::BitArray(_)
        | PgValue::VarbitArray(_)
        | PgValue::ByteaArray(_)
        | PgValue::CharArray(_)
        | PgValue::VarcharArray(_)
        | PgValue::CidrArray(_)
        | PgValue::InetArray(_)
        | PgValue::MacaddrArray(_)
        | PgValue::Macaddr8Array(_)
        | PgValue::BoxArray(_)
        | PgValue::CircleArray(_)
        | PgValue::LineArray(_)
        | PgValue::LsegArray(_)
        | PgValue::Path(_)
        | PgValue::PointArray(_)
        | PgValue::Polygon(_)
        | PgValue::DateArray(_)
        | PgValue::IntervalArray(_)
        | PgValue::TimeArray(_)
        | PgValue::TimeTzArray(_)
        | PgValue::TimestampArray(_)
        | PgValue::TimestampTzArray(_)
        | PgValue::JsonArray(_)
        | PgValue::JsonbArray(_)
        | PgValue::MoneyArray(_)
        | PgValue::PgLsnArray(_)
        | PgValue::NameArray(_)
        | PgValue::TextArray(_)
        | PgValue::XmlArray(_)
        | PgValue::UuidArray(_) => 1,
        _ => 0,
    }
}

/// Types Postgres reads as fixed-width binary. A value written in another
/// format of the same width is read as a different value, with no error.
const FIXED_WIDTH: &[Type] = &[
    Type::BOOL,
    Type::INT2,
    Type::INT4,
    Type::INT8,
    Type::OID,
    Type::FLOAT4,
    Type::FLOAT8,
    Type::MONEY,
    Type::DATE,
    Type::TIME,
    Type::TIMESTAMP,
    Type::TIMESTAMPTZ,
    Type::INTERVAL,
    Type::NUMERIC,
    Type::UUID,
];

/// Whether a scalar `value` may be written for a parameter of type `ty`.
///
/// The stock conversion writes each scalar in its own format and never looks
/// at `ty`. That is right only where the formats agree: a number, bool, date
/// or time must meet its own type (`timestamp` and `timestamptz` share one
/// encoding, so they meet either), and text must not meet a fixed-width type.
/// Text still binds to every type Postgres stores as text, such as `json`,
/// `xml` and enums. A domain is checked as its base type. Lists and the
/// other variants are left to [`has_array_levels`] and their own encoders.
fn scalar_fits(value: &PgValue, ty: &Type) -> bool {
    let ty = base_type(ty);
    match value {
        PgValue::BigInt(_) | PgValue::Int8(_) | PgValue::BigSerial(_) | PgValue::Serial8(_) => {
            <i64 as ToSql>::accepts(ty)
        }
        PgValue::Integer(_)
        | PgValue::Int(_)
        | PgValue::Int4(_)
        | PgValue::Serial(_)
        | PgValue::Serial4(_) => <i32 as ToSql>::accepts(ty),
        PgValue::SmallInt(_) | PgValue::Int2(_) | PgValue::SmallSerial(_) | PgValue::Serial2(_) => {
            <i16 as ToSql>::accepts(ty)
        }
        PgValue::Double(_) | PgValue::Float8(_) => <f64 as ToSql>::accepts(ty),
        PgValue::Real(_) | PgValue::Float4(_) => <f32 as ToSql>::accepts(ty),
        PgValue::Bool(_) | PgValue::Boolean(_) => <bool as ToSql>::accepts(ty),
        PgValue::Date(_) => *ty == Type::DATE,
        PgValue::Time(_) => *ty == Type::TIME,
        PgValue::Timestamp(_) | PgValue::TimestampTz(_) => {
            matches!(*ty, Type::TIMESTAMP | Type::TIMESTAMPTZ)
        }
        PgValue::Text(_) => !FIXED_WIDTH.contains(ty),
        _ => true,
    }
}

/// `ty`, or the type a domain over it is built on, all the way down.
fn base_type(ty: &Type) -> &Type {
    match ty.kind() {
        Kind::Domain(base) => base_type(base),
        _ => ty,
    }
}

/// Whether `ty` is an array `levels` deep: an array whose members are arrays,
/// and so on.
fn has_array_levels(ty: &Type, levels: usize) -> bool {
    (0..levels)
        .try_fold(ty, |ty, _| match ty.kind() {
            Kind::Array(member) => Some(member),
            _ => None,
        })
        .is_some()
}

/// `bound` as the slice tokio-postgres binds, in parameter order.
pub(crate) fn as_sql(bound: &[Bound]) -> Vec<&(dyn ToSql + Sync)> {
    bound.iter().map(|b| b as &(dyn ToSql + Sync)).collect()
}

/// Refuses a statement whose placeholders and bound parameters differ in
/// number, before it runs.
pub(crate) fn check_count(expected: usize, bound: usize) -> Result<(), Error> {
    if expected == bound {
        return Ok(());
    }
    Err(errors::invalid_params(format!(
        "statement expects {expected} parameters but {bound} were bound"
    )))
}

pub(crate) fn embed_role(role: Role) -> genius_embed::Role {
    match role {
        Role::Query => genius_embed::Role::Query,
        Role::Passage => genius_embed::Role::Passage,
    }
}

/// What one statement may ask the model to embed, and one statement's
/// embedding at a time for each workload.
///
/// Every workload on the host embeds with the one model, one text at a time.
/// Without these, one statement with a huge list, a huge text, or a stream of
/// statements from one workload would keep every other workload's embedding
/// waiting.
pub(crate) struct EmbedGate {
    max_texts: usize,
    max_text_bytes: usize,
    /// One slot for each workload, with no time limit: a workload's next
    /// statement waits for its last embedding to end.
    running: WorkloadShares,
}

impl EmbedGate {
    pub(crate) fn new(max_texts: usize, max_text_bytes: usize) -> Self {
        Self {
            max_texts,
            max_text_bytes,
            running: WorkloadShares::new(1, None),
        }
    }

    /// Drop `workload`'s slot once it is unbound.
    pub(crate) fn forget(&self, workload: &str) {
        self.running.forget(workload);
    }

    /// Refuse a statement over either cap before anything is embedded. A text
    /// is named by its SHA-256, never by its content.
    fn check(&self, params: &[Param]) -> Result<(), Error> {
        let texts = || {
            params.iter().flat_map(|param| match param {
                Param::Value(_) => [].iter(),
                Param::EmbedText((text, _)) => std::slice::from_ref(text).iter(),
                Param::EmbedTexts((texts, _)) => texts.iter(),
            })
        };
        let count = texts().count();
        if count > self.max_texts {
            return Err(errors::invalid_params(format!(
                "the statement has {count} texts to embed; at most {} are allowed",
                self.max_texts
            )));
        }
        if let Some(text) = texts().find(|text| text.len() > self.max_text_bytes) {
            return Err(errors::invalid_params(format!(
                "the text with sha {:x} is {} bytes; at most {} are allowed",
                Sha256::digest(text.as_bytes()),
                text.len(),
                self.max_text_bytes
            )));
        }
        Ok(())
    }
}

/// Sets its flag when dropped: the future that awaits an embedding is gone,
/// so the blocking task embedding for it can stop.
struct AbandonOnDrop(Arc<AtomicBool>);

impl Drop for AbandonOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Every parameter as something tokio-postgres can bind, in order.
///
/// Called before a connection is checked out or a transaction is locked, so
/// an embedding never holds a connection idle. A statement over a cap is
/// refused first; then the statement waits for `workload`'s embedding slot,
/// which its blocking tasks hold until they end.
pub(crate) async fn resolve(
    embedder: &Arc<dyn Embedder>,
    gate: &EmbedGate,
    workload: &Arc<str>,
    params: Vec<Param>,
) -> Result<Vec<Bound>, Error> {
    gate.check(&params)?;
    let slot = if params.iter().any(|param| !matches!(param, Param::Value(_))) {
        Some(Arc::new(gate.running.acquire(workload).await?))
    } else {
        None
    };
    let mut bound = Vec::with_capacity(params.len());
    for param in params {
        bound.push(match param {
            Param::Value(value) => Bound::Value(value),
            Param::EmbedText((text, role)) => Bound::Vector(
                embed_texts(embedder, vec![text], embed_role(role), slot.clone())
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| Error::Embed("the model answered no vector".to_string()))?,
            ),
            Param::EmbedTexts((texts, role)) => {
                Bound::Vectors(embed_texts(embedder, texts, embed_role(role), slot.clone()).await?)
            }
        });
    }
    Ok(bound)
}

/// Each text's vector, in order, embedding each distinct text once on a
/// blocking thread. The thread holds `slot` until it ends, and stops before
/// its next text once the caller has gone.
async fn embed_texts(
    embedder: &Arc<dyn Embedder>,
    texts: Vec<String>,
    role: genius_embed::Role,
    slot: Option<Arc<OwnedSemaphorePermit>>,
) -> Result<Vec<Vector>, Error> {
    let embedder = Arc::clone(embedder);
    let abandoned = Arc::new(AtomicBool::new(false));
    let _abandon = AbandonOnDrop(Arc::clone(&abandoned));
    let vectors = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        embed_distinct(embedder.as_ref(), &texts, role, &abandoned)
    })
    .await
    .map_err(|e| Error::Embed(format!("embedding task failed: {e}")))??;
    Ok(vectors.into_iter().map(Vector::from).collect())
}

/// As `genius_embed::embed_distinct`, stopping before the next text once
/// `abandoned` is set.
fn embed_distinct(
    embedder: &dyn Embedder,
    texts: &[String],
    role: genius_embed::Role,
    abandoned: &AtomicBool,
) -> Result<Vec<Vec<f32>>, Error> {
    let mut distinct: Vec<&str> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    let positions: Vec<usize> = texts
        .iter()
        .map(|text| {
            *index.entry(text.as_str()).or_insert_with(|| {
                distinct.push(text.as_str());
                distinct.len() - 1
            })
        })
        .collect();
    let mut vectors = Vec::with_capacity(distinct.len());
    for text in distinct {
        if abandoned.load(Ordering::Relaxed) {
            return Err(Error::Embed(
                "the statement was cancelled before its texts were embedded".to_string(),
            ));
        }
        vectors.push(embedder.embed(text, role).map_err(|e| {
            // Named by the hash `entity.text_sha256` stores: the model's own
            // error does not say which text it failed on.
            let sha = Sha256::digest(text.as_bytes());
            Error::Embed(format!("embed text with sha {sha:x}: {e:#}"))
        })?);
    }
    positions
        .into_iter()
        .map(|i| {
            vectors
                .get(i)
                .cloned()
                .ok_or_else(|| Error::Embed("an embedding went missing".to_string()))
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::super::bindings::wasmcloud::postgres::types::Error as PgError;
    use super::*;

    /// A pure function of each text and role, so a reordered output shows,
    /// counting every forward pass.
    #[derive(Default)]
    struct CountingEmbedder {
        calls: AtomicUsize,
    }

    impl CountingEmbedder {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Embedder for CountingEmbedder {
        fn embed(&self, text: &str, role: genius_embed::Role) -> anyhow::Result<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vector_for(text, role))
        }

        fn dim(&self) -> usize {
            2
        }
    }

    fn vector_for(text: &str, role: genius_embed::Role) -> Vec<f32> {
        let role = match role {
            genius_embed::Role::Query => 1.0,
            genius_embed::Role::Passage => 2.0,
        };
        vec![text.bytes().map(f32::from).sum(), role]
    }

    struct FailingEmbedder;

    impl Embedder for FailingEmbedder {
        fn embed(&self, _text: &str, _role: genius_embed::Role) -> anyhow::Result<Vec<f32>> {
            anyhow::bail!("the model fell over")
        }

        fn dim(&self) -> usize {
            2
        }
    }

    /// A resolved parameter in a form a test can compare.
    #[derive(Debug, PartialEq)]
    enum Resolved {
        Text(String),
        OtherValue,
        Vector(Vec<f32>),
        Vectors(Vec<Vec<f32>>),
    }

    fn describe(bound: &[Bound]) -> Vec<Resolved> {
        bound
            .iter()
            .map(|b| match b {
                Bound::Value(PgValue::Text(text)) => Resolved::Text(text.clone()),
                Bound::Value(_) => Resolved::OtherValue,
                Bound::Vector(v) => Resolved::Vector(v.to_vec()),
                Bound::Vectors(vs) => Resolved::Vectors(vs.iter().map(Vector::to_vec).collect()),
            })
            .collect()
    }

    fn embed_message(resolved: Result<Vec<Bound>, Error>) -> Option<String> {
        match resolved {
            Err(Error::Embed(message)) => Some(message),
            _ => None,
        }
    }

    fn counting() -> (Arc<CountingEmbedder>, Arc<dyn Embedder>) {
        let counting = Arc::new(CountingEmbedder::default());
        let embedder: Arc<dyn Embedder> = counting.clone();
        (counting, embedder)
    }

    fn workload(id: &str) -> Arc<str> {
        Arc::from(id)
    }

    /// One workload's statement, under limits too wide to matter.
    async fn resolve_alone(
        embedder: &Arc<dyn Embedder>,
        params: Vec<Param>,
    ) -> Result<Vec<Bound>, Error> {
        resolve(
            embedder,
            &EmbedGate::new(1_000, 64 * 1024),
            &workload("w"),
            params,
        )
        .await
    }

    /// Counts its calls, how many run at once at most, and takes 40 ms each.
    #[derive(Default)]
    struct SlowEmbedder {
        calls: AtomicUsize,
        running: AtomicUsize,
        most_at_once: AtomicUsize,
    }

    impl Embedder for SlowEmbedder {
        fn embed(&self, text: &str, role: genius_embed::Role) -> anyhow::Result<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_at_once.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(40));
            self.running.fetch_sub(1, Ordering::SeqCst);
            Ok(vector_for(text, role))
        }

        fn dim(&self) -> usize {
            2
        }
    }

    fn texts(count: usize) -> Vec<String> {
        (0..count).map(|i| format!("text {i}")).collect()
    }

    #[tokio::test]
    async fn a_statement_with_more_texts_than_the_cap_is_invalid_params_before_any_embedding() {
        let (counting, embedder) = counting();
        let params = vec![
            Param::EmbedTexts((vec!["a".to_string(), "b".to_string()], Role::Passage)),
            Param::EmbedText(("c".to_string(), Role::Query)),
        ];
        let err = resolve(&embedder, &EmbedGate::new(2, 1024), &workload("w"), params)
            .await
            .expect_err("three texts are over a cap of two");
        assert!(
            matches!(&err, Error::Postgres(PgError::InvalidParams(m))
                if m == "the statement has 3 texts to embed; at most 2 are allowed"),
            "{err:?}"
        );
        assert_eq!(counting.calls(), 0, "nothing is embedded before the check");
    }

    #[tokio::test]
    async fn a_text_longer_than_the_cap_is_invalid_params_named_by_its_sha256() {
        let (counting, embedder) = counting();
        let long = "x".repeat(11);
        let err = resolve(
            &embedder,
            &EmbedGate::new(10, 10),
            &workload("w"),
            vec![Param::EmbedText((long.clone(), Role::Query))],
        )
        .await
        .expect_err("eleven bytes are over a cap of ten");
        let sha = format!("{:x}", Sha256::digest(long.as_bytes()));
        assert!(
            matches!(&err, Error::Postgres(PgError::InvalidParams(m))
                if m == &format!("the text with sha {sha} is 11 bytes; at most 10 are allowed")),
            "{err:?}"
        );
        assert_eq!(counting.calls(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abandoned_embedding_stops_at_the_next_text() {
        let slow = Arc::new(SlowEmbedder::default());
        let embedder: Arc<dyn Embedder> = slow.clone();
        let gate = Arc::new(EmbedGate::new(1_000, 1024));
        let task = tokio::spawn(async move {
            resolve(
                &embedder,
                &gate,
                &workload("w"),
                vec![Param::EmbedTexts((texts(20), Role::Passage))],
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        task.abort();
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let calls = slow.calls.load(Ordering::SeqCst);
        assert!(
            calls < 6,
            "an abandoned embedding went on to {calls} of 20 texts"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_workload_embeds_one_statement_at_a_time() {
        let slow = Arc::new(SlowEmbedder::default());
        let embedder: Arc<dyn Embedder> = slow.clone();
        let gate = EmbedGate::new(1_000, 1024);
        let same = workload("w");
        let one = || {
            resolve(
                &embedder,
                &gate,
                &same,
                vec![Param::EmbedTexts((texts(3), Role::Passage))],
            )
        };
        let (first, second) = tokio::join!(one(), one());
        first.expect("the first statement embeds");
        second.expect("the second statement embeds after it");
        assert_eq!(slow.most_at_once.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn another_workload_does_not_wait_for_ones_embedding() {
        let slow = Arc::new(SlowEmbedder::default());
        let embedder: Arc<dyn Embedder> = slow.clone();
        let gate = EmbedGate::new(1_000, 1024);
        let statement = |id: &str| {
            let workload = workload(id);
            let embedder = &embedder;
            let gate = &gate;
            async move {
                resolve(
                    embedder,
                    gate,
                    &workload,
                    vec![Param::EmbedTexts((texts(3), Role::Passage))],
                )
                .await
            }
        };
        let (a, b) = tokio::join!(statement("a"), statement("b"));
        a.expect("a embeds");
        b.expect("b embeds");
        assert_eq!(slow.most_at_once.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn each_wit_role_maps_onto_the_same_model_role() {
        assert_eq!(embed_role(Role::Query), genius_embed::Role::Query);
        assert_eq!(embed_role(Role::Passage), genius_embed::Role::Passage);
    }

    #[tokio::test]
    async fn a_value_passes_through_without_an_embedding() {
        let (counting, embedder) = counting();
        let bound = resolve_alone(
            &embedder,
            vec![Param::Value(PgValue::Text("orders".to_string()))],
        )
        .await
        .expect("a value resolves");
        assert_eq!(describe(&bound), vec![Resolved::Text("orders".to_string())]);
        assert_eq!(counting.calls(), 0);
    }

    #[tokio::test]
    async fn embed_text_becomes_one_vector_for_its_role() {
        let (counting, embedder) = counting();
        let text = "which models hold orders";
        let bound = resolve_alone(
            &embedder,
            vec![Param::EmbedText((text.to_string(), Role::Query))],
        )
        .await
        .expect("an embed-text parameter resolves");
        assert_eq!(
            describe(&bound),
            vec![Resolved::Vector(vector_for(
                text,
                genius_embed::Role::Query
            ))]
        );
        assert_eq!(counting.calls(), 1);
    }

    #[tokio::test]
    async fn embed_texts_keeps_input_order_and_embeds_each_distinct_text_once() {
        let (counting, embedder) = counting();
        // Not a palindrome, so a reversed output cannot pass.
        let texts = ["alpha", "beta", "alpha", "gamma"];
        let bound = resolve_alone(
            &embedder,
            vec![Param::EmbedTexts((
                texts.iter().map(ToString::to_string).collect(),
                Role::Passage,
            ))],
        )
        .await
        .expect("an embed-texts parameter resolves");
        let expected = texts
            .iter()
            .map(|text| vector_for(text, genius_embed::Role::Passage))
            .collect();
        assert_eq!(describe(&bound), vec![Resolved::Vectors(expected)]);
        assert_eq!(counting.calls(), 3, "each distinct text is embedded once");
    }

    #[tokio::test]
    async fn parameters_resolve_in_their_own_order() {
        let (_, embedder) = counting();
        let bound = resolve_alone(
            &embedder,
            vec![
                Param::EmbedTexts((vec!["beta".to_string()], Role::Passage)),
                Param::Value(PgValue::Text("orders".to_string())),
                Param::EmbedText(("alpha".to_string(), Role::Query)),
            ],
        )
        .await
        .expect("mixed parameters resolve");
        assert_eq!(
            describe(&bound),
            vec![
                Resolved::Vectors(vec![vector_for("beta", genius_embed::Role::Passage)]),
                Resolved::Text("orders".to_string()),
                Resolved::Vector(vector_for("alpha", genius_embed::Role::Query)),
            ]
        );
    }

    #[test]
    fn a_bound_value_encodes_exactly_as_the_value_it_holds() {
        let value = PgValue::Text("orders".to_string());
        let mut direct = BytesMut::new();
        let mut through = BytesMut::new();
        assert!(value.to_sql_checked(&Type::TEXT, &mut direct).is_ok());
        assert!(
            Bound::Value(value)
                .to_sql_checked(&Type::TEXT, &mut through)
                .is_ok()
        );
        assert_eq!(through, direct);
    }

    #[test]
    fn a_parameter_that_cannot_encode_is_tagged_as_the_parameters_fault() {
        let err = Bound::Vector(Vector::from(vec![0.5]))
            .to_sql_checked(&Type::TEXT, &mut BytesMut::new())
            .err()
            .expect("a vector does not encode as text");
        assert!(err.is::<ParamEncodeError>(), "{err}");
        assert!(
            err.source()
                .is_some_and(|source| source.is::<tokio_postgres::types::WrongType>()),
            "{err}"
        );
    }

    #[test]
    fn a_list_bound_to_a_type_with_fewer_array_levels_is_wrong_type_rather_than_a_panic() {
        let point = ((0, 0, 1), (0, 0, 1));
        for (value, ty) in [
            (PgValue::Int4Array(vec![1, 2]), Type::INT4),
            (PgValue::TextArray(vec!["orders".to_string()]), Type::TEXT),
            (PgValue::Int2Vector(vec![1]), Type::INT2),
            (PgValue::Path(vec![point]), Type::PATH),
            (PgValue::Int2VectorArray(vec![vec![1]]), Type::INT2_ARRAY),
            (PgValue::PathArray(vec![vec![point]]), Type::POINT_ARRAY),
        ] {
            let mut out = BytesMut::new();
            let err = Bound::Value(value)
                .to_sql_checked(&ty, &mut out)
                .err()
                .expect("a list must not encode as a type with fewer array levels");
            assert!(err.is::<ParamEncodeError>(), "{ty}: {err}");
            assert!(
                err.source().is_some_and(|source| source.is::<WrongType>()),
                "{ty}: {err}"
            );
            assert!(out.is_empty(), "{ty}: a refused parameter writes nothing");
        }
    }

    fn midnight(
        on: (i32, u32, u32),
    ) -> super::super::bindings::wasmcloud::postgres::types::Timestamp {
        use super::super::bindings::wasmcloud::postgres::types::{Date, Time, Timestamp};
        Timestamp {
            date: Date::Ymd(on),
            time: Time {
                hour: 0,
                min: 0,
                sec: 0,
                micro: 0,
            },
        }
    }

    /// A type made as Postgres reports a domain or an enum it does not know
    /// statically.
    fn custom_type(name: &str, kind: Kind) -> Type {
        Type::new(name.to_string(), 4_000_000, kind, "public".to_string())
    }

    /// Each of these encodes to as many bytes as the type it is bound to
    /// expects, so Postgres would store a different value with no error:
    /// `int8(1)` as the float8 4.9e-324, `int8(1700000000)` as a timestamptz
    /// at 2000-01-01 00:28:20, `"1234"` as the int4 825373492.
    #[test]
    fn a_scalar_bound_to_another_type_of_its_width_is_refused_not_misread() {
        let int_domain = custom_type("positive_count", Kind::Domain(Type::INT4));
        for (value, ty) in [
            (PgValue::Int8(1), Type::FLOAT8),
            (PgValue::BigInt(1_700_000_000), Type::TIMESTAMPTZ),
            (PgValue::Int8(1), Type::TIMESTAMP),
            (PgValue::Float8((1, 0, 1)), Type::INT8),
            (PgValue::Int4(1), Type::FLOAT4),
            (PgValue::Int4(1), Type::DATE),
            (PgValue::Int(1), Type::INT8),
            (PgValue::Int2(1), Type::INT4),
            (PgValue::Bool(true), Type::INT2),
            (PgValue::Timestamp(midnight((2026, 10, 15))), Type::INT8),
            (PgValue::Text("1234".to_string()), Type::INT4),
            (PgValue::Text("12345678".to_string()), Type::FLOAT8),
            (PgValue::Text("0123456789abcdef".to_string()), Type::UUID),
            (PgValue::Text("1234".to_string()), int_domain),
        ] {
            let mut out = BytesMut::new();
            let err = Bound::Value(value.clone())
                .to_sql_checked(&ty, &mut out)
                .err()
                .unwrap_or_else(|| panic!("{value:?} must not bind to {ty}"));
            assert!(err.is::<ParamEncodeError>(), "{ty}: {err}");
            assert!(
                err.source().is_some_and(|source| source.is::<WrongType>()),
                "{ty}: {err}"
            );
            assert!(out.is_empty(), "{ty}: a refused parameter writes nothing");
        }
    }

    #[test]
    fn a_scalar_binds_to_its_own_type_and_text_to_the_types_stored_as_text() {
        let count_domain = custom_type("order_count", Kind::Domain(Type::INT8));
        let mood = custom_type("mood", Kind::Enum(vec!["calm".to_string()]));
        for (value, ty) in [
            (PgValue::Int8(1), Type::INT8),
            (PgValue::Serial8(1), Type::INT8),
            (PgValue::Int8(1), count_domain),
            (PgValue::Int4(1), Type::INT4),
            (PgValue::Serial4(1), Type::INT4),
            (PgValue::Int2(1), Type::INT2),
            (PgValue::Float8((1, 0, 1)), Type::FLOAT8),
            (PgValue::Float4((1, 0, 1)), Type::FLOAT4),
            (PgValue::Bool(true), Type::BOOL),
            // One encoding for both: a naive timestamp is read as UTC.
            (
                PgValue::Timestamp(midnight((2026, 10, 15))),
                Type::TIMESTAMP,
            ),
            (
                PgValue::Timestamp(midnight((2026, 10, 15))),
                Type::TIMESTAMPTZ,
            ),
            (PgValue::Text("orders".to_string()), Type::TEXT),
            (PgValue::Text("orders".to_string()), Type::VARCHAR),
            (PgValue::Text("{}".to_string()), Type::JSON),
            (PgValue::Text("<a/>".to_string()), Type::XML),
            (PgValue::Text("calm".to_string()), mood),
        ] {
            Bound::Value(value.clone())
                .to_sql_checked(&ty, &mut BytesMut::new())
                .unwrap_or_else(|e| panic!("{value:?} must bind to {ty}: {e}"));
        }
    }

    /// An empty array of `ty`, as postgres-types encodes any empty slice: one
    /// dimension, the member type's oid, no members.
    fn empty_array(ty: &Type) -> BytesMut {
        let mut out = BytesMut::new();
        Vec::<i32>::new()
            .to_sql(ty, &mut out)
            .expect("an empty slice encodes for any array type");
        out
    }

    #[test]
    fn an_empty_list_of_lists_binds_to_an_array_one_level_deep() {
        for (value, ty) in [
            // `path` and `polygon` are not arrays, so `path[]` and `polygon[]`
            // are one level deep: the guard used to refuse these two outright.
            (PgValue::PathArray(Vec::new()), Type::PATH_ARRAY),
            (PgValue::PolygonArray(Vec::new()), Type::POLYGON_ARRAY),
            (PgValue::Int2VectorArray(Vec::new()), Type::INT2_ARRAY),
            // Whatever the members would have been, there are none to encode.
            (PgValue::PathArray(Vec::new()), Type::POINT_ARRAY),
            (PgValue::PolygonArray(Vec::new()), Type::TEXT_ARRAY),
        ] {
            let mut out = BytesMut::new();
            let encoded = Bound::Value(value).to_sql_checked(&ty, &mut out);
            assert!(
                matches!(encoded, Ok(IsNull::No)),
                "{ty}: {:?}",
                encoded.err()
            );
            assert_eq!(out, empty_array(&ty), "{ty}");
        }
    }

    #[test]
    fn an_empty_list_still_binds_to_an_array_as_deep_as_its_variant() {
        for (value, ty) in [
            (PgValue::Int4Array(Vec::new()), Type::INT4_ARRAY),
            (PgValue::TextArray(Vec::new()), Type::TEXT_ARRAY),
            (PgValue::PointArray(Vec::new()), Type::POINT_ARRAY),
            (PgValue::Int2Vector(Vec::new()), Type::INT2_VECTOR),
            (
                PgValue::Int2VectorArray(Vec::new()),
                Type::INT2_VECTOR_ARRAY,
            ),
        ] {
            let mut direct = BytesMut::new();
            let mut through = BytesMut::new();
            assert!(value.to_sql_checked(&ty, &mut direct).is_ok(), "{ty}");
            assert!(
                Bound::Value(value)
                    .to_sql_checked(&ty, &mut through)
                    .is_ok(),
                "{ty}"
            );
            assert_eq!(through, direct, "{ty}");
        }
    }

    #[test]
    fn an_empty_list_bound_to_a_type_that_is_not_an_array_is_still_refused() {
        // The slice encoder panics on the type before it counts the members.
        for (value, ty) in [
            (PgValue::Int4Array(Vec::new()), Type::INT4),
            (PgValue::TextArray(Vec::new()), Type::TEXT),
            (PgValue::Int2Vector(Vec::new()), Type::INT2),
            (PgValue::PathArray(Vec::new()), Type::PATH),
            (PgValue::PolygonArray(Vec::new()), Type::POLYGON),
            (PgValue::Int2VectorArray(Vec::new()), Type::INT2),
        ] {
            let mut out = BytesMut::new();
            let err = Bound::Value(value)
                .to_sql_checked(&ty, &mut out)
                .err()
                .expect("an empty list must not encode as a type that is not an array");
            assert!(err.is::<ParamEncodeError>(), "{ty}: {err}");
            assert!(
                err.source().is_some_and(|source| source.is::<WrongType>()),
                "{ty}: {err}"
            );
            assert!(out.is_empty(), "{ty}: a refused parameter writes nothing");
        }
    }

    #[test]
    fn a_list_of_lists_with_a_member_still_needs_both_array_levels() {
        let point = ((0, 0, 1), (0, 0, 1));
        for (value, ty) in [
            // One member is enough: encoding it is what would panic.
            (PgValue::PathArray(vec![vec![point]]), Type::PATH_ARRAY),
            (
                PgValue::PolygonArray(vec![vec![point]]),
                Type::POLYGON_ARRAY,
            ),
            // The member being empty does not help; it is still encoded.
            (PgValue::PathArray(vec![Vec::new()]), Type::PATH_ARRAY),
            (PgValue::Int2VectorArray(vec![Vec::new()]), Type::INT2_ARRAY),
        ] {
            let mut out = BytesMut::new();
            let err = Bound::Value(value)
                .to_sql_checked(&ty, &mut out)
                .err()
                .expect("a list of lists with a member needs two array levels");
            assert!(err.is::<ParamEncodeError>(), "{ty}: {err}");
            assert!(
                err.source().is_some_and(|source| source.is::<WrongType>()),
                "{ty}: {err}"
            );
            assert!(out.is_empty(), "{ty}: a refused parameter writes nothing");
        }
    }

    #[test]
    fn an_empty_path_or_polygon_is_an_error_whatever_it_is_bound_to() {
        // The stock conversion refuses these itself, before any encoder runs.
        for (value, ty) in [
            (PgValue::Path(Vec::new()), Type::POINT_ARRAY),
            (PgValue::Polygon(Vec::new()), Type::POINT_ARRAY),
            (PgValue::Path(Vec::new()), Type::PATH),
            (PgValue::Polygon(Vec::new()), Type::POLYGON),
        ] {
            let err = Bound::Value(value)
                .to_sql_checked(&ty, &mut BytesMut::new())
                .err()
                .expect("an empty path or polygon does not encode");
            assert!(err.is::<ParamEncodeError>(), "{ty}: {err}");
        }
    }

    #[test]
    fn a_list_bound_to_a_type_with_as_many_array_levels_encodes_as_the_list_alone() {
        for (value, ty) in [
            (PgValue::Int4Array(vec![1, 2]), Type::INT4_ARRAY),
            (PgValue::Int2Vector(vec![1, 2]), Type::INT2_VECTOR),
            (
                PgValue::Int2VectorArray(vec![vec![1, 2]]),
                Type::INT2_VECTOR_ARRAY,
            ),
        ] {
            let mut direct = BytesMut::new();
            let mut through = BytesMut::new();
            assert!(value.to_sql_checked(&ty, &mut direct).is_ok(), "{ty}");
            assert!(
                Bound::Value(value)
                    .to_sql_checked(&ty, &mut through)
                    .is_ok(),
                "{ty}"
            );
            assert_eq!(through, direct, "{ty}");
        }
    }

    #[test]
    fn a_parameter_count_mismatch_is_invalid_params() {
        assert!(check_count(2, 2).is_ok());
        assert!(matches!(
            check_count(2, 1),
            Err(Error::Postgres(PgError::InvalidParams(m)))
                if m == "statement expects 2 parameters but 1 were bound"
        ));
    }

    #[tokio::test]
    async fn an_embedding_failure_names_the_text_by_its_sha256() {
        // SHA-256 of "abc", the FIPS 180-2 test vector.
        const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let embedder: Arc<dyn Embedder> = Arc::new(FailingEmbedder);
        for param in [
            Param::EmbedText(("abc".to_string(), Role::Query)),
            Param::EmbedTexts((vec!["abc".to_string()], Role::Passage)),
        ] {
            let message = embed_message(resolve_alone(&embedder, vec![param]).await)
                .expect("an embedding failure is an embed error");
            assert!(
                message.starts_with(&format!("embed text with sha {ABC_SHA256}: ")),
                "{message}"
            );
            assert!(message.contains("the model fell over"), "{message}");
        }
    }
}
