//! Bind parameters: a value passes through, and an embed parameter becomes a
//! pgvector vector before its statement reaches the database.

use std::sync::Arc;

use bytes::BytesMut;
use genius_embed::Embedder;
use pgvector::Vector;
use sha2::{Digest as _, Sha256};
use tokio_postgres::types::{Format, IsNull, Kind, ToSql, Type, WrongType};

use super::bindings::betty_blocks::retrieval::types::{Error, Param, Role};
use super::bindings::wasmcloud::postgres::types::PgValue;
use super::errors::{self, ParamEncodeError};

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
        // array.
        if let Bound::Value(value) = self
            && !has_array_levels(ty, array_levels(value))
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

/// Every parameter as something tokio-postgres can bind, in order.
///
/// Called before a connection is checked out or a transaction is locked, so
/// an embedding never holds a connection idle.
pub(crate) async fn resolve(
    embedder: &Arc<dyn Embedder>,
    params: Vec<Param>,
) -> Result<Vec<Bound>, Error> {
    let mut bound = Vec::with_capacity(params.len());
    for param in params {
        bound.push(match param {
            Param::Value(value) => Bound::Value(value),
            Param::EmbedText((text, role)) => {
                Bound::Vector(embed_one(embedder, text, embed_role(role)).await?)
            }
            Param::EmbedTexts((texts, role)) => {
                Bound::Vectors(embed_many(embedder, texts, embed_role(role)).await?)
            }
        });
    }
    Ok(bound)
}

async fn embed_one(
    embedder: &Arc<dyn Embedder>,
    text: String,
    role: genius_embed::Role,
) -> Result<Vector, Error> {
    let embedder = Arc::clone(embedder);
    tokio::task::spawn_blocking(move || {
        embedder.embed(&text, role).map(Vector::from).map_err(|e| {
            // Named by the hash `entity.text_sha256` stores: the model's own
            // error does not say which text it failed on.
            let sha = Sha256::digest(text.as_bytes());
            Error::Embed(format!("embed text with sha {sha:x}: {e:#}"))
        })
    })
    .await
    .map_err(|e| Error::Embed(format!("embedding task failed: {e}")))?
}

async fn embed_many(
    embedder: &Arc<dyn Embedder>,
    texts: Vec<String>,
    role: genius_embed::Role,
) -> Result<Vec<Vector>, Error> {
    let embedder = Arc::clone(embedder);
    let vectors = tokio::task::spawn_blocking(move || {
        genius_embed::embed_distinct(embedder.as_ref(), &texts, role)
            .map_err(|e| Error::Embed(format!("{e:#}")))
    })
    .await
    .map_err(|e| Error::Embed(format!("embedding task failed: {e}")))??;
    Ok(vectors.into_iter().map(Vector::from).collect())
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

    #[test]
    fn each_wit_role_maps_onto_the_same_model_role() {
        assert_eq!(embed_role(Role::Query), genius_embed::Role::Query);
        assert_eq!(embed_role(Role::Passage), genius_embed::Role::Passage);
    }

    #[tokio::test]
    async fn a_value_passes_through_without_an_embedding() {
        let (counting, embedder) = counting();
        let bound = resolve(
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
        let bound = resolve(
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
        let bound = resolve(
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
        let bound = resolve(
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
            let message = embed_message(resolve(&embedder, vec![param]).await)
                .expect("an embedding failure is an embed error");
            assert!(
                message.starts_with(&format!("embed text with sha {ABC_SHA256}: ")),
                "{message}"
            );
            assert!(message.contains("the model fell over"), "{message}");
        }
    }
}
