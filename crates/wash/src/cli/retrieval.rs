//! Shared construction of a [`BettyRetrievalConfig`] from `wash host`'s
//! `--retrieval-*` flags and `wash dev`'s `dev.retrieval_*` config keys, so
//! the two front ends apply the same defaults, overrides and validation
//! instead of drifting apart.

use std::path::PathBuf;
use std::time::Duration;

use wash_runtime::plugin::betty_retrieval::{BettyRetrievalConfig, ModelSelection};

/// Everything either front end can say about which model to run.
#[derive(Debug, Clone, Default)]
pub(crate) struct ModelSettings {
    /// A model name, or a path to a descriptor.
    pub model: Option<String>,
    /// The older, path-only setting. Used when `model` is unset.
    pub model_config: Option<PathBuf>,
    pub catalog: Option<String>,
    pub cache: Option<PathBuf>,
    pub mirror: Option<String>,
}

/// Which model to run, or `None` when no model was configured at all.
///
/// `model` wins over `model_config` so that an operator can point a host at a
/// different model without first removing the path key a config file already
/// carries — which is the whole point of naming one in the environment.
pub(crate) fn model_selection(settings: ModelSettings) -> Option<ModelSelection> {
    let spec = settings
        .model
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
        .or_else(|| {
            settings
                .model_config
                .map(|path| path.to_string_lossy().into_owned())
        })?;
    Some(ModelSelection {
        spec,
        catalog: settings.catalog.filter(|c| !c.trim().is_empty()),
        cache: settings.cache,
        mirror: settings.mirror.filter(|m| !m.trim().is_empty()),
    })
}

/// Tuning overrides layered onto [`BettyRetrievalConfig::new`]'s defaults.
/// `None` leaves the plugin's own default for that field untouched.
#[derive(Debug, Clone, Default)]
pub(crate) struct BettyRetrievalOverrides {
    pub pool_size: Option<usize>,
    pub connect_timeout_secs: Option<u64>,
    pub ef_search: Option<u32>,
    pub max_scan_tuples: Option<u32>,
    pub embed_threads: Option<usize>,
}

/// Build and validate a [`BettyRetrievalConfig`] from a required database URL
/// and model config path plus optional tuning overrides.
///
/// Used by both `wash host --retrieval-*` and `wash dev`'s `dev.retrieval_*`
/// keys, which each resolve their own "URL and model config must be set
/// together" requirement before calling this — it only fills defaults,
/// applies overrides and validates.
pub(crate) fn build_betty_retrieval_config(
    database_url: String,
    model: ModelSelection,
    overrides: BettyRetrievalOverrides,
) -> anyhow::Result<BettyRetrievalConfig> {
    let mut config = BettyRetrievalConfig::with_model(database_url, model);
    if let Some(pool_size) = overrides.pool_size {
        config.pool_size = pool_size;
    }
    if let Some(connect_timeout_secs) = overrides.connect_timeout_secs {
        config.connect_timeout = Duration::from_secs(connect_timeout_secs);
    }
    if let Some(ef_search) = overrides.ef_search {
        config.ef_search = ef_search;
    }
    if let Some(max_scan_tuples) = overrides.max_scan_tuples {
        config.max_scan_tuples = max_scan_tuples;
    }
    if let Some(embed_threads) = overrides.embed_threads {
        config.embed_threads = embed_threads;
    }
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database_url() -> String {
        "postgres://genius:genius@127.0.0.1:55433/genius_retrieval".to_string()
    }

    fn model_config() -> ModelSelection {
        ModelSelection::from_path(PathBuf::from(
            "models/granite-embedding-107m-multilingual.json",
        ))
    }

    #[test]
    fn unset_overrides_keep_the_plugins_defaults() {
        let config = build_betty_retrieval_config(
            database_url(),
            model_config(),
            BettyRetrievalOverrides::default(),
        )
        .expect("the plugin's defaults are valid");
        assert_eq!(config.pool_size, 8);
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.ef_search, 200);
        assert_eq!(config.max_scan_tuples, 20_000);
        assert_eq!(config.embed_threads, 4);
    }

    #[test]
    fn applies_every_override() {
        let config = build_betty_retrieval_config(
            database_url(),
            model_config(),
            BettyRetrievalOverrides {
                pool_size: Some(16),
                connect_timeout_secs: Some(5),
                ef_search: Some(100),
                max_scan_tuples: Some(500),
                embed_threads: Some(2),
            },
        )
        .expect("valid overrides build a config");
        assert_eq!(config.pool_size, 16);
        assert_eq!(config.connect_timeout, Duration::from_secs(5));
        assert_eq!(config.ef_search, 100);
        assert_eq!(config.max_scan_tuples, 500);
        assert_eq!(config.embed_threads, 2);
    }

    #[test]
    fn rejects_a_zero_pool_size() {
        let err = build_betty_retrieval_config(
            database_url(),
            model_config(),
            BettyRetrievalOverrides {
                pool_size: Some(0),
                ..Default::default()
            },
        )
        .expect_err("a zero pool size must be refused by the plugin's own validate()")
        .to_string();
        assert!(err.contains("pool_size"), "{err}");
    }

    #[test]
    fn a_named_model_wins_over_a_descriptor_path() {
        // The point of naming one in the environment: it takes effect without
        // first removing the path a config file already carries.
        let selection = model_selection(ModelSettings {
            model: Some("multilingual-e5-small".to_string()),
            model_config: Some(PathBuf::from("models/granite.json")),
            catalog: Some("https://models.example/catalog".to_string()),
            ..Default::default()
        })
        .expect("a model was configured");
        assert_eq!(selection.spec, "multilingual-e5-small");
        assert_eq!(
            selection.catalog.as_deref(),
            Some("https://models.example/catalog")
        );
    }

    #[test]
    fn the_descriptor_path_is_used_when_no_model_is_named() {
        let selection = model_selection(ModelSettings {
            model_config: Some(PathBuf::from("models/granite.json")),
            ..Default::default()
        })
        .expect("a model was configured");
        assert_eq!(selection.spec, "models/granite.json");
        assert!(selection.catalog.is_none());
    }

    #[test]
    fn an_empty_model_setting_does_not_count_as_naming_one() {
        // An env var set to the empty string is how a shell says "unset" by
        // accident; it must not shadow the path key or look like a model.
        let selection = model_selection(ModelSettings {
            model: Some("   ".to_string()),
            model_config: Some(PathBuf::from("models/granite.json")),
            ..Default::default()
        })
        .expect("the path still configures a model");
        assert_eq!(selection.spec, "models/granite.json");

        assert!(
            model_selection(ModelSettings::default()).is_none(),
            "nothing configured is nothing to run"
        );
    }
}
