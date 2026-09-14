//! Bind parameters: a value passes through, and an embed parameter becomes a
//! pgvector vector before its statement reaches the database.

use std::sync::Arc;

use genius_embed::Embedder;
use pgvector::Vector;
use sha2::{Digest as _, Sha256};
use tokio_postgres::types::ToSql;

use super::bindings::betty_blocks::retrieval::types::{Error, Param, Role};
use super::bindings::wasmcloud::postgres::types::PgValue;

/// A resolved parameter, owning whatever its statement borrows.
pub(crate) enum Bound {
    Value(PgValue),
    Vector(Vector),
    Vectors(Vec<Vector>),
}

impl Bound {
    pub(crate) fn as_sql(&self) -> &(dyn ToSql + Sync) {
        match self {
            Bound::Value(v) => v,
            Bound::Vector(v) => v,
            Bound::Vectors(v) => v,
        }
    }
}

/// `bound` as the slice tokio-postgres binds, in parameter order.
pub(crate) fn as_sql(bound: &[Bound]) -> Vec<&(dyn ToSql + Sync)> {
    bound.iter().map(Bound::as_sql).collect()
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
