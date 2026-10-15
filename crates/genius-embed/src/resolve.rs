//! Turning what an operator wrote into a model that is on disk and verified.
//!
//! The setting an operator gives is either a PATH to a descriptor — how this
//! has always worked, and still the whole story for a model placed by hand —
//! or a NAME, looked up in a catalog and fetched if the bytes are not already
//! cached. A name is what makes changing models an environment change rather
//! than a rebuild: the catalog holds one small file per model, and adding a
//! model is adding a file to it.
//!
//! Two rules hold this together, and both exist because the same failure is
//! silent otherwise. A fetched descriptor MUST pin both digests: the
//! embedding-space id is derived from the artifact bytes, so bytes that can
//! change between restarts would change the space without changing anything a
//! reader can see. And a cache hit is a full digest check, not a file-exists
//! check, so a half-written or tampered artifact is refused rather than
//! loaded.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};

use crate::modelcfg::{sha256_file, ModelConfig};

/// What to resolve, and where from.
#[derive(Debug, Clone)]
pub struct Request {
    /// A path to a descriptor, or a model name to look up in `catalog`.
    pub spec: String,
    /// Where names are looked up: a directory, or an `http(s)` base URL.
    /// Required only when `spec` is a name.
    pub catalog: Option<String>,
    /// Where fetched artifacts live. Each model gets `<cache>/<model_id>/`.
    pub cache: Option<PathBuf>,
    /// Serve artifact downloads from here instead of the descriptor's own
    /// `source.base_url` — for a machine that cannot reach the public host.
    /// The file names are unchanged, so the digests still decide.
    pub mirror: Option<String>,
}

/// How a resolved model came to be on disk, for the line the host logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `spec` was a path; nothing was fetched.
    Path,
    /// Every artifact was already cached, with the right digest.
    Cache,
    /// At least one artifact was downloaded.
    Download,
    /// The catalog could not be reached, and the descriptor and artifacts a
    /// previous fetch left in the cache still matched their pins.
    OfflineCache,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Path => "path",
            Origin::Cache => "cache",
            Origin::Download => "download",
            Origin::OfflineCache => "offline-cache",
        }
    }
}

/// A resolved model: a descriptor whose artifacts exist and match their pins.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub config: ModelConfig,
    /// The descriptor this came from, for error messages.
    pub descriptor: PathBuf,
    pub origin: Origin,
}

/// Resolve `spec` to a model on disk, fetching it if that is what it takes.
///
/// A `spec` naming an existing file is a path, whatever else it looks like, so
/// nothing that worked before changes meaning. Anything else is a name.
pub fn resolve(request: &Request) -> Result<Resolved> {
    let spec = request.spec.trim();
    if spec.is_empty() {
        bail!("no model was named: set a model name, or the path to a model descriptor");
    }
    let as_path = Path::new(spec);
    if as_path.is_file() {
        let config = ModelConfig::load_anchored(as_path)
            .with_context(|| format!("load the model descriptor {spec}"))?;
        config.verify_artifacts()?;
        return Ok(Resolved {
            config,
            descriptor: as_path.to_path_buf(),
            origin: Origin::Path,
        });
    }
    if looks_like_path(spec) {
        bail!(
            "the model descriptor {spec} does not exist. A setting containing a path \
             separator or ending in .json is read as a path; drop those to name a model \
             in the catalog instead."
        );
    }
    resolve_named(spec, request)
}

/// A spec that was meant as a path but is not there should say so, rather than
/// being looked up as an absurd model name and failing with a catalog error.
fn looks_like_path(spec: &str) -> bool {
    spec.contains('/') || spec.contains(std::path::MAIN_SEPARATOR) || spec.ends_with(".json")
}

fn resolve_named(name: &str, request: &Request) -> Result<Resolved> {
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!(
            "{name:?} is not a model name: names hold letters, digits, dashes, \
             underscores and dots, so that a name cannot reach outside the catalog"
        );
    }
    let catalog = request.catalog.as_deref().map(str::trim).unwrap_or("");
    if catalog.is_empty() {
        bail!(
            "the model {name:?} was asked for by name, but no catalog was given. \
             Set the catalog to a directory or an http(s) base URL holding \
             {name}.json, or name a descriptor by path instead."
        );
    }
    let cache = request.cache.clone().unwrap_or_else(default_cache);

    let descriptor_dir = cache.join(name);
    std::fs::create_dir_all(&descriptor_dir)
        .with_context(|| format!("create the model cache {}", descriptor_dir.display()))?;
    let descriptor = descriptor_dir.join(format!("{name}.json"));

    // A catalog that cannot be reached is not the end when an earlier fetch
    // left the descriptor in the cache: its artifacts are checked against its
    // pins below, and only then accepted. Without this, a catalog outage would
    // stop every restart of a host whose model is already on disk.
    let offline = match read_catalog_entry(catalog, name) {
        Ok(raw) => {
            // Written to the cache before parsing, so that what was resolved
            // is inspectable next to the artifacts it describes even when it
            // is wrong.
            write_replacing(&descriptor, &raw)?;
            None
        }
        Err(unreachable) if descriptor.is_file() => Some(unreachable),
        Err(unreachable) => return Err(unreachable),
    };

    let mut config = ModelConfig::parse(&descriptor)
        .with_context(|| format!("parse the catalog entry for {name}"))?;
    // A catalog entry's paths say where each artifact sits UNDER ITS SOURCE —
    // published repositories really do keep a graph in `onnx/` and its
    // tokenizer at the root — so they are kept as written for the download.
    // On disk everything lands flat beside the descriptor: a catalog is a
    // published file and cannot know this machine's layout.
    let model_relative = config.model_path.clone();
    let tokenizer_relative = config.tokenizer_path.clone();
    config.model_path = descriptor_dir.join(file_name(&model_relative, "model_path")?);
    config.tokenizer_path = descriptor_dir.join(file_name(&tokenizer_relative, "tokenizer_path")?);

    let (model_pin, tokenizer_pin) = match (&config.artifact_sha256, &config.tokenizer_sha256) {
        (Some(model), Some(tokenizer)) => (model.clone(), tokenizer.clone()),
        _ => bail!(
            "the catalog entry for {name} does not pin both artifact_sha256 and \
             tokenizer_sha256. A model fetched by name must pin its bytes: the \
             embedding-space id is derived from them, so unpinned bytes could change \
             under a restart and silently make every stored application incomparable."
        ),
    };

    let base = match (request.mirror.as_deref(), config.source.as_ref()) {
        (Some(mirror), _) => mirror.trim_end_matches('/').to_string(),
        (None, Some(source)) => source.base_url.trim_end_matches('/').to_string(),
        (None, None) => bail!(
            "the catalog entry for {name} has no source.base_url, so its artifacts \
             cannot be fetched. Add one, or place the artifacts and name the \
             descriptor by path."
        ),
    };

    if let Some(unreachable) = offline {
        for (what, path, pin) in [
            ("model", &config.model_path, &model_pin),
            ("tokenizer", &config.tokenizer_path, &tokenizer_pin),
        ] {
            let cached = path.is_file() && sha256_file(path)? == *pin;
            if !cached {
                return Err(unreachable.context(format!(
                    "the catalog could not be reached, and the cache holds no {what} artifact \
                     for {name} that matches its pin to fall back on"
                )));
            }
        }
        config
            .verify_artifacts()
            .with_context(|| format!("verify the cached artifacts for {name}"))?;
        return Ok(Resolved {
            config,
            descriptor,
            origin: Origin::OfflineCache,
        });
    }

    let mut origin = Origin::Cache;
    for (what, path, relative, pin) in [
        (
            "model",
            config.model_path.clone(),
            model_relative,
            model_pin,
        ),
        (
            "tokenizer",
            config.tokenizer_path.clone(),
            tokenizer_relative,
            tokenizer_pin,
        ),
    ] {
        if fetch_one(&base, &path, &relative, &pin, what, name)? {
            origin = Origin::Download;
        }
    }

    config
        .verify_artifacts()
        .with_context(|| format!("verify the artifacts fetched for {name}"))?;
    Ok(Resolved {
        config,
        descriptor,
        origin,
    })
}

/// Ensure one artifact is present with the right digest. Returns whether it
/// had to be downloaded. `path` is where it belongs on this machine;
/// `relative` is where it sits under the source.
fn fetch_one(
    base: &str,
    path: &Path,
    relative: &Path,
    pin: &str,
    what: &str,
    name: &str,
) -> Result<bool> {
    if path.exists() {
        let actual = sha256_file(path)?;
        if actual == pin {
            return Ok(false);
        }
        // Not an error: a truncated download from a previous run looks exactly
        // like this, and refusing to replace it would wedge the cache.
        std::fs::remove_file(path)
            .with_context(|| format!("remove the stale {what} artifact {}", path.display()))?;
    }
    refuse_plain_http(base, "source")?;
    let url = format!(
        "{base}/{}",
        relative.to_string_lossy().trim_start_matches('/')
    );
    let bytes = download(&url)?;
    // Written under a name of its own and checked BEFORE it is renamed into
    // place: an interrupted fetch leaves no short file, two hosts sharing the
    // cache never write into each other's file, and the final name only ever
    // holds bytes that matched the pin.
    let temp = temporary_beside(path);
    std::fs::write(&temp, &bytes).with_context(|| format!("write {}", temp.display()))?;
    let actual = sha256_file(&temp)?;
    if actual != pin {
        std::fs::remove_file(&temp).ok();
        bail!(
            "the {what} artifact fetched for {name} from {url} does not match the digest \
             its catalog entry pins\n  expected {pin}\n  actual   {actual}\n\
             The bytes at that address are not the ones this model was validated with."
        );
    }
    std::fs::rename(&temp, path).with_context(|| format!("move {} into place", temp.display()))?;
    Ok(true)
}

/// A name beside `path` that no other process or call picks: the process id
/// and the time, so two hosts sharing one cache never write the same file.
fn temporary_beside(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let file = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{file}.{}.{nanos}.partial", std::process::id()))
}

/// Replace `path` with `bytes` in one rename, so a reader never sees it half
/// written.
fn write_replacing(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = temporary_beside(path);
    std::fs::write(&temp, bytes).with_context(|| format!("write {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("move {} into place", temp.display()))
}

fn file_name(path: &Path, field: &str) -> Result<String> {
    path.file_name()
        .and_then(|f| f.to_str())
        .map(str::to_string)
        .with_context(|| format!("{field} {} has no file name", path.display()))
}

/// Read `<catalog>/<name>.json`, from a directory or over http(s).
fn read_catalog_entry(catalog: &str, name: &str) -> Result<Vec<u8>> {
    if is_url(catalog) {
        refuse_plain_http(catalog, "catalog")?;
        let url = format!("{}/{name}.json", catalog.trim_end_matches('/'));
        return download(&url).with_context(|| {
            format!(
                "read the catalog entry for {name}. The catalog must hold {name}.json; \
                 what it holds decides which models can be named."
            )
        });
    }
    let path = Path::new(catalog).join(format!("{name}.json"));
    std::fs::read(&path).with_context(|| {
        format!(
            "read the catalog entry {}. The catalog must hold {name}.json; what it \
             holds decides which models can be named.",
            path.display()
        )
    })
}

fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Refuse a plain `http` address on another machine. The catalog decides the
/// digests every artifact is checked against, so over plain http anything on
/// the network path could supply both the files and the digests that would
/// pass them. This machine's loopback is allowed, for a catalog served
/// locally.
fn refuse_plain_http(address: &str, what: &str) -> Result<()> {
    let Some(rest) = address.strip_prefix("http://") else {
        return Ok(());
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = match host_port.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => host_port.split(':').next().unwrap_or_default(),
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if loopback {
        return Ok(());
    }
    bail!(
        "the model {what} at {host} uses plain http; use https. The catalog's digests are \
         what every download is checked against, so over plain http anything on the network \
         path could supply both the files and the digests that pass them."
    )
}

/// One blocking GET. A model artifact is hundreds of megabytes, so the body is
/// read with a limit far above that rather than the client's small default,
/// which would otherwise truncate the graph and fail the digest check with a
/// misleading message.
fn download(url: &str) -> Result<Vec<u8>> {
    const MAX_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
    // An https address must not be redirected to plain http, for the same
    // reason a plain http one is refused.
    let agent = ureq::Agent::config_builder()
        .https_only(url.starts_with("https://"))
        .build()
        .new_agent();
    let mut response = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_ARTIFACT_BYTES)
        .read_to_vec()
        .with_context(|| format!("read the response from {url}"))
}

/// Where fetched models live when no cache is named: beside the user's other
/// caches, so a second checkout does not download the same hundreds of
/// megabytes again.
fn default_cache() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("genius-embed/models");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home).join(".cache/genius-embed/models");
    }
    PathBuf::from(".genius-embed-models")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "genius-embed-resolve-{tag}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("create scratch");
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A descriptor plus its two artifacts, digests pinned to the real bytes.
    fn write_model(dir: &Path, name: &str, source: Option<&str>) -> PathBuf {
        let model = dir.join("model.onnx");
        let tokenizer = dir.join("tokenizer.json");
        std::fs::write(&model, b"onnx bytes").expect("write model");
        std::fs::write(&tokenizer, b"tokenizer bytes").expect("write tokenizer");
        let entry = serde_json::json!({
            "model_id": name,
            "model_path": "model.onnx",
            "tokenizer_path": "tokenizer.json",
            "artifact_sha256": sha256_file(&model).expect("digest"),
            "tokenizer_sha256": sha256_file(&tokenizer).expect("digest"),
            "pooling": "cls",
            "license": "Apache-2.0",
            "source": source.map(|base_url| serde_json::json!({ "base_url": base_url })),
        });
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, serde_json::to_vec_pretty(&entry).expect("json")).expect("write");
        path
    }

    #[test]
    fn a_spec_that_is_a_path_resolves_without_a_catalog() {
        let scratch = Scratch::new("path");
        let descriptor = write_model(scratch.path(), "local-model", None);
        let resolved = resolve(&Request {
            spec: descriptor.display().to_string(),
            catalog: None,
            cache: None,
            mirror: None,
        })
        .expect("a descriptor path needs nothing else");
        assert_eq!(resolved.origin, Origin::Path);
        assert_eq!(resolved.config.model_id, "local-model");
    }

    #[test]
    fn a_path_that_is_missing_says_so_rather_than_being_read_as_a_name() {
        let err = resolve(&Request {
            spec: "models/not-here.json".to_string(),
            catalog: Some("/tmp".to_string()),
            cache: None,
            mirror: None,
        })
        .expect_err("a path that is not there cannot resolve");
        let msg = format!("{err:#}");
        assert!(msg.contains("does not exist"), "{msg}");
        assert!(msg.contains("read as a path"), "{msg}");
    }

    #[test]
    fn a_name_needs_a_catalog() {
        let err = resolve(&Request {
            spec: "granite".to_string(),
            catalog: None,
            cache: None,
            mirror: None,
        })
        .expect_err("a name without a catalog cannot resolve");
        assert!(format!("{err:#}").contains("no catalog was given"));
    }

    #[test]
    fn a_name_cannot_reach_outside_the_catalog() {
        for bad in ["../secrets", "a/b", "name with space"] {
            let err = resolve(&Request {
                spec: bad.to_string(),
                catalog: Some("/tmp".to_string()),
                cache: None,
                mirror: None,
            })
            .expect_err("a traversal or odd name is refused");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("is not a model name") || msg.contains("read as a path"),
                "{bad:?}: {msg}"
            );
        }
    }

    #[test]
    fn a_cached_model_resolves_from_a_directory_catalog_without_fetching() {
        let scratch = Scratch::new("cache");
        let catalog = scratch.path().join("catalog");
        let cache = scratch.path().join("cache");
        std::fs::create_dir_all(&catalog).expect("catalog");
        // The cache already holds the artifacts, as it would after one fetch.
        let cached = cache.join("demo");
        std::fs::create_dir_all(&cached).expect("cache dir");
        write_model(&cached, "demo", None);
        // The catalog entry points at a source nothing will reach: a cache hit
        // must not touch the network.
        write_model(&catalog, "demo", Some("http://127.0.0.1:1/never"));
        std::fs::remove_file(catalog.join("model.onnx")).expect("tidy");
        std::fs::remove_file(catalog.join("tokenizer.json")).expect("tidy");

        let resolved = resolve(&Request {
            spec: "demo".to_string(),
            catalog: Some(catalog.display().to_string()),
            cache: Some(cache.clone()),
            mirror: None,
        })
        .expect("a full cache resolves offline");
        assert_eq!(resolved.origin, Origin::Cache);
        assert_eq!(resolved.config.model_id, "demo");
        assert_eq!(resolved.config.model_path, cached.join("model.onnx"));
    }

    /// A throwaway HTTP server for one test: it serves whatever files the test
    /// wrote, so the fetch path is exercised end to end — catalog entry over
    /// http, artifacts over http, digests checked against what arrived —
    /// without reaching anything outside this machine.
    fn serve(dir: PathBuf) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead as _, BufReader, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = std::thread::spawn(move || {
            // Three requests: the catalog entry, then the two artifacts.
            for _ in 0..3 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    return;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let file = dir.join(path.trim_start_matches('/'));
                let body = std::fs::read(&file).unwrap_or_default();
                let status = if body.is_empty() {
                    "404 Not Found"
                } else {
                    "200 OK"
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        (base, handle)
    }

    #[test]
    fn a_plain_http_catalog_or_source_off_this_machine_is_refused_before_any_request() {
        let scratch = Scratch::new("http");
        let err = resolve(&Request {
            spec: "demo".to_string(),
            catalog: Some("http://models.example/catalog".to_string()),
            cache: Some(scratch.path().join("cache")),
            mirror: None,
        })
        .expect_err("a plain http catalog on another machine must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("https"), "{msg}");
        assert!(!msg.contains("GET "), "refused before any request: {msg}");

        // A directory catalog whose entry fetches from plain http elsewhere.
        let catalog = scratch.path().join("catalog");
        std::fs::create_dir_all(&catalog).expect("catalog");
        write_model(&catalog, "demo", Some("http://files.example/models"));
        for mirror in [None, Some("http://mirror.example".to_string())] {
            let err = resolve(&Request {
                spec: "demo".to_string(),
                catalog: Some(catalog.display().to_string()),
                cache: Some(scratch.path().join("cache")),
                mirror,
            })
            .expect_err("plain http artifacts from another machine must be refused");
            let msg = format!("{err:#}");
            assert!(msg.contains("https"), "{msg}");
            assert!(!msg.contains("GET "), "refused before any request: {msg}");
        }
    }

    #[test]
    fn a_url_catalog_that_cannot_be_reached_falls_back_to_the_cached_descriptor() {
        let scratch = Scratch::new("offline");
        let published = scratch.path().join("published");
        std::fs::create_dir_all(&published).expect("published");
        let cache = scratch.path().join("cache");
        let (base, server) = serve(published.clone());
        write_model(&published, "demo", Some(&base));
        resolve(&Request {
            spec: "demo".to_string(),
            catalog: Some(base),
            cache: Some(cache.clone()),
            mirror: None,
        })
        .expect("the first resolve fetches");
        server.join().expect("server");

        // The catalog is down now; the cache holds the descriptor and both
        // artifacts, and they still match the descriptor's pins.
        let resolved = resolve(&Request {
            spec: "demo".to_string(),
            catalog: Some("http://127.0.0.1:1/catalog".to_string()),
            cache: Some(cache.clone()),
            mirror: None,
        })
        .expect("a cached model resolves while its catalog is down");
        assert_eq!(resolved.origin, Origin::OfflineCache);
        assert_eq!(resolved.config.model_path, cache.join("demo/model.onnx"));

        // Without the artifacts, the catalog's failure is what the error says.
        std::fs::remove_file(cache.join("demo/model.onnx")).expect("drop the model");
        let err = resolve(&Request {
            spec: "demo".to_string(),
            catalog: Some("http://127.0.0.1:1/catalog".to_string()),
            cache: Some(cache),
            mirror: None,
        })
        .expect_err("no catalog and no cached model cannot resolve");
        assert!(format!("{err:#}").contains("catalog"), "{err:#}");
    }

    #[test]
    fn a_digest_pinned_in_upper_case_matches_the_artifact() {
        // PowerShell's Get-FileHash prints digests in upper case. Compared as
        // text, a good cached artifact would be deleted and fetched again on
        // every start, and then refused as tampered.
        let scratch = Scratch::new("uppercase");
        let catalog = scratch.path().join("catalog");
        let cache = scratch.path().join("cache");
        std::fs::create_dir_all(&catalog).expect("catalog");
        let cached = cache.join("demo");
        std::fs::create_dir_all(&cached).expect("cache dir");
        write_model(&cached, "demo", None);
        let entry = write_model(&catalog, "demo", Some("http://127.0.0.1:1/never"));
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&entry).expect("read")).expect("json");
        for key in ["artifact_sha256", "tokenizer_sha256"] {
            let upper = json[key].as_str().expect("pinned").to_ascii_uppercase();
            json[key] = serde_json::Value::String(upper);
        }
        std::fs::write(&entry, serde_json::to_vec(&json).expect("json")).expect("write");

        let resolved = resolve(&Request {
            spec: "demo".to_string(),
            catalog: Some(catalog.display().to_string()),
            cache: Some(cache),
            mirror: None,
        })
        .expect("an upper-case pin of the right digest matches");
        assert_eq!(resolved.origin, Origin::Cache);
    }

    #[test]
    fn a_download_with_the_wrong_bytes_leaves_no_file_behind() {
        let scratch = Scratch::new("wrong-bytes");
        let published = scratch.path().join("published");
        std::fs::create_dir_all(&published).expect("published");
        let cache = scratch.path().join("cache");
        let (base, server) = serve(published.clone());
        write_model(&published, "demo", Some(&base));
        // Pinned to the bytes above, then served different ones.
        std::fs::write(published.join("model.onnx"), b"other bytes").expect("swap");
        let err = resolve(&Request {
            spec: "demo".to_string(),
            catalog: Some(base),
            cache: Some(cache.clone()),
            mirror: None,
        })
        .expect_err("bytes that do not match the pin are refused");
        assert!(format!("{err:#}").contains("does not match"), "{err:#}");
        drop(server);
        let left: Vec<_> = std::fs::read_dir(cache.join("demo"))
            .expect("the cache dir")
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter(|name| name != "demo.json")
            .collect();
        assert!(
            left.is_empty(),
            "nothing but the descriptor is left: {left:?}"
        );
    }

    #[test]
    fn a_name_resolves_from_a_url_catalog_by_fetching_and_checking_the_bytes() {
        let scratch = Scratch::new("fetch");
        let published = scratch.path().join("published");
        std::fs::create_dir_all(&published).expect("published");
        let cache = scratch.path().join("cache");

        // What the server will serve. The entry's source points back at the
        // same server, so both the catalog read and the artifact fetch go over
        // http.
        let (base, server) = serve(published.clone());
        write_model(&published, "demo", Some(&base));

        let resolved = resolve(&Request {
            spec: "demo".to_string(),
            catalog: Some(base.clone()),
            cache: Some(cache.clone()),
            mirror: None,
        })
        .expect("a published model resolves by name");
        server.join().expect("server");

        assert_eq!(resolved.origin, Origin::Download);
        assert_eq!(resolved.config.model_id, "demo");
        // Fetched beside the descriptor in the cache, not wherever the catalog
        // entry said its files were.
        assert_eq!(resolved.config.model_path, cache.join("demo/model.onnx"));
        assert_eq!(
            std::fs::read(cache.join("demo/model.onnx")).expect("cached model"),
            b"onnx bytes"
        );
        // Nothing is left half-written.
        assert!(!cache.join("demo/model.partial").exists());

        // Second resolve, with no server running at all: the cache answers.
        let again = resolve(&Request {
            spec: "demo".to_string(),
            catalog: Some(scratch.path().join("published").display().to_string()),
            cache: Some(cache),
            mirror: None,
        })
        .expect("a cached model resolves offline");
        assert_eq!(again.origin, Origin::Cache);
    }

    #[test]
    fn an_artifact_published_under_a_subdirectory_is_fetched_from_there() {
        // Real repositories keep a graph in `onnx/` and its tokenizer at the
        // root. Taking only file names would ask for the graph at the root and
        // get a 404 — which is exactly what happened the first time a second
        // model was added to a catalog.
        let scratch = Scratch::new("subdir");
        let published = scratch.path().join("published");
        std::fs::create_dir_all(published.join("onnx")).expect("published");
        let cache = scratch.path().join("cache");

        let (base, server) = serve(published.clone());
        // The graph is published one level down; the tokenizer is not.
        std::fs::write(published.join("onnx/model.onnx"), b"onnx bytes").expect("model");
        std::fs::write(published.join("tokenizer.json"), b"tokenizer bytes").expect("tokenizer");
        let entry = serde_json::json!({
            "model_id": "nested",
            "model_path": "onnx/model.onnx",
            "tokenizer_path": "tokenizer.json",
            "artifact_sha256": sha256_file(&published.join("onnx/model.onnx")).expect("digest"),
            "tokenizer_sha256": sha256_file(&published.join("tokenizer.json")).expect("digest"),
            "pooling": "mean",
            "license": "Apache-2.0",
            "source": { "base_url": base.clone() },
        });
        std::fs::write(
            published.join("nested.json"),
            serde_json::to_vec(&entry).expect("json"),
        )
        .expect("write");

        let resolved = resolve(&Request {
            spec: "nested".to_string(),
            catalog: Some(base),
            cache: Some(cache.clone()),
            mirror: None,
        })
        .expect("a nested artifact resolves");
        server.join().expect("server");

        assert_eq!(resolved.origin, Origin::Download);
        // Published one level down, cached flat: the catalog cannot know this
        // machine's layout.
        assert_eq!(resolved.config.model_path, cache.join("nested/model.onnx"));
        assert_eq!(
            std::fs::read(cache.join("nested/model.onnx")).expect("cached"),
            b"onnx bytes"
        );
    }

    #[test]
    fn a_catalog_entry_without_pins_is_refused() {
        let scratch = Scratch::new("unpinned");
        let catalog = scratch.path().join("catalog");
        std::fs::create_dir_all(&catalog).expect("catalog");
        let entry = serde_json::json!({
            "model_id": "loose",
            "model_path": "model.onnx",
            "tokenizer_path": "tokenizer.json",
            "pooling": "cls",
            "license": "Apache-2.0",
            "source": { "base_url": "http://127.0.0.1:1/never" },
        });
        std::fs::write(
            catalog.join("loose.json"),
            serde_json::to_vec(&entry).expect("json"),
        )
        .expect("write");

        let err = resolve(&Request {
            spec: "loose".to_string(),
            catalog: Some(catalog.display().to_string()),
            cache: Some(scratch.path().join("cache")),
            mirror: None,
        })
        .expect_err("a fetched model must pin its bytes");
        let msg = format!("{err:#}");
        assert!(msg.contains("does not pin both"), "{msg}");
        assert!(msg.contains("incomparable"), "{msg}");
    }

    #[test]
    fn a_cached_artifact_with_the_wrong_bytes_is_not_accepted() {
        let scratch = Scratch::new("tampered");
        let catalog = scratch.path().join("catalog");
        let cache = scratch.path().join("cache");
        std::fs::create_dir_all(&catalog).expect("catalog");
        let cached = cache.join("demo");
        std::fs::create_dir_all(&cached).expect("cache dir");
        write_model(&cached, "demo", None);
        write_model(&catalog, "demo", Some("http://127.0.0.1:1/never"));
        std::fs::remove_file(catalog.join("model.onnx")).expect("tidy");
        std::fs::remove_file(catalog.join("tokenizer.json")).expect("tidy");
        // Someone replaced the cached graph. It must not be loaded, and with
        // the source unreachable the resolve must fail rather than proceed.
        std::fs::write(cached.join("model.onnx"), b"different bytes").expect("tamper");

        let err = resolve(&Request {
            spec: "demo".to_string(),
            catalog: Some(catalog.display().to_string()),
            cache: Some(cache),
            mirror: None,
        })
        .expect_err("a wrong-digest cache entry is refetched, and the fetch fails here");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("GET http://127.0.0.1:1/never/model.onnx"),
            "{msg}"
        );
    }
}
