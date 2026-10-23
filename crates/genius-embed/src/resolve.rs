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
use std::time::Duration;

use anyhow::{bail, Context as _, Result};

use crate::modelcfg::{sha256_file, ModelConfig};

/// What to resolve, and where from.
#[derive(Clone)]
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

/// `Debug` prints the catalog and mirror without their credentials or query
/// string: either can carry a token.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("spec", &self.spec)
            .field("catalog", &self.catalog.as_deref().map(redact_url))
            .field("cache", &self.cache)
            .field("mirror", &self.mirror.as_deref().map(redact_url))
            .finish()
    }
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

/// How the HTTP side behaves. The defaults are what a host uses; a test
/// shrinks them.
#[derive(Clone)]
struct Fetch {
    /// Time to open the connection.
    connect: Duration,
    /// Time from sending the request to the response headers.
    response: Duration,
    /// Time to read a whole body. An artifact is hundreds of megabytes, so
    /// this is a total budget, not an idle timeout.
    body: Duration,
    /// The most bytes one artifact may have.
    max_artifact_bytes: u64,
    /// Runs in `fetch_one` once a cached artifact is known to be stale.
    #[cfg(test)]
    after_stale_check: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl Default for Fetch {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            response: Duration::from_secs(30),
            body: Duration::from_secs(10 * 60),
            max_artifact_bytes: 2 * 1024 * 1024 * 1024,
            #[cfg(test)]
            after_stale_check: None,
        }
    }
}

/// The most bytes a catalog entry may have: a descriptor is a few hundred.
const MAX_CATALOG_BYTES: u64 = 1024 * 1024;

/// Resolve `spec` to a model on disk, fetching it if that is what it takes.
///
/// A `spec` naming an existing file is a path, whatever else it looks like, so
/// nothing that worked before changes meaning. Anything else is a name.
pub fn resolve(request: &Request) -> Result<Resolved> {
    resolve_with(request, &Fetch::default())
}

fn resolve_with(request: &Request, fetch: &Fetch) -> Result<Resolved> {
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
    resolve_named(spec, request, fetch)
}

/// A spec that was meant as a path but is not there should say so, rather than
/// being looked up as an absurd model name and failing with a catalog error.
fn looks_like_path(spec: &str) -> bool {
    spec.contains('/') || spec.contains(std::path::MAIN_SEPARATOR) || spec.ends_with(".json")
}

fn resolve_named(name: &str, request: &Request, fetch: &Fetch) -> Result<Resolved> {
    if name.chars().all(|c| c == '.')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!(
            "{name:?} is not a model name: names hold letters, digits, dashes, \
             underscores and dots (and not only dots), so that a name cannot reach \
             outside the catalog"
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
    sweep_stale_partials(&descriptor_dir);

    // A catalog that cannot be reached is not the end when an earlier fetch
    // left the descriptor in the cache: its artifacts are checked against its
    // pins below, and only then accepted. Without this, a catalog outage would
    // stop every restart of a host whose model is already on disk.
    let (raw, offline) = match read_catalog_entry(catalog, name, fetch) {
        Ok(raw) => {
            // Written to the cache before parsing, so that what was resolved
            // is inspectable next to the artifacts it describes even when it
            // is wrong.
            write_replacing(&descriptor, &raw)?;
            (raw, None)
        }
        Err(unreachable) => match std::fs::read(&descriptor) {
            Ok(raw) => (raw, Some(unreachable)),
            Err(_) => return Err(unreachable),
        },
    };

    // Parsed from the bytes in hand, not read back from the shared path:
    // another host sharing this cache may replace that file at any moment.
    let mut config = ModelConfig::parse_bytes(&raw, &descriptor.display().to_string())
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
        if fetch_one(&base, &path, &relative, &pin, what, name, fetch)? {
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
    fetch: &Fetch,
) -> Result<bool> {
    if path.exists() {
        let actual = sha256_file(path)?;
        if actual == pin {
            return Ok(false);
        }
        #[cfg(test)]
        if let Some(hook) = &fetch.after_stale_check {
            hook();
        }
        // Not an error: a truncated download from a previous run looks exactly
        // like this, and refusing to replace it would wedge the cache. It is
        // not deleted here either: the rename below replaces it atomically,
        // and a delete would race a second host that is doing the same, or
        // remove the file that host has just put in place.
    }
    refuse_plain_http(base, "source")?;
    let url = format!(
        "{base}/{}",
        relative.to_string_lossy().trim_start_matches('/')
    );
    let shown = redact_url(&url);
    // Streamed into a file of its own and checked BEFORE it is renamed into
    // place: an interrupted fetch leaves no short file, two hosts sharing the
    // cache never write into each other's file, and the final name only ever
    // holds bytes that matched the pin. The guard removes the file on every
    // way out but the rename.
    let temp = Partial::beside(path);
    let actual = download_to_file(&url, temp.path(), fetch)
        .with_context(|| format!("fetch the {what} artifact for {name}"))?;
    if actual != pin {
        bail!(
            "the {what} artifact fetched for {name} from {shown} does not match the digest \
             its catalog entry pins\n  expected {pin}\n  actual   {actual}\n\
             The bytes at that address are not the ones this model was validated with."
        );
    }
    temp.persist(path)?;
    Ok(true)
}

/// A file written beside its destination under a name of its own, removed
/// when dropped unless it was moved into place.
struct Partial {
    path: PathBuf,
    keep: bool,
}

impl Partial {
    fn beside(path: &Path) -> Self {
        Self {
            path: temporary_beside(path),
            keep: false,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Move into place, replacing whatever is there in one step.
    fn persist(mut self, dest: &Path) -> Result<()> {
        std::fs::rename(&self.path, dest)
            .with_context(|| format!("move {} into place", self.path.display()))?;
        self.keep = true;
        Ok(())
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// How long a `.partial` file may sit untouched before it is taken for the
/// leftover of a host that died. Longer than any download is allowed to take.
const STALE_PARTIAL_AGE: Duration = Duration::from_secs(60 * 60);

/// Remove the temp files an earlier, killed run left in `dir`. A recent one
/// may belong to a host downloading right now, so it is left alone.
fn sweep_stale_partials(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with('.') && name.ends_with(".partial")) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > STALE_PARTIAL_AGE);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A name beside `path` that no other process or call picks: the process id,
/// the time and a counter, so two hosts sharing one cache never write the
/// same file.
fn temporary_beside(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let file = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    // The counter separates two calls in one process within one clock tick,
    // which two threads resolving at once can produce.
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_file_name(format!(
        ".{file}.{}.{nanos}.{sequence}.partial",
        std::process::id()
    ))
}

/// Replace `path` with `bytes` in one rename, so a reader never sees it half
/// written.
fn write_replacing(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = Partial::beside(path);
    std::fs::write(temp.path(), bytes)
        .with_context(|| format!("write {}", temp.path().display()))?;
    temp.persist(path)
}

fn file_name(path: &Path, field: &str) -> Result<String> {
    path.file_name()
        .and_then(|f| f.to_str())
        .map(str::to_string)
        .with_context(|| format!("{field} {} has no file name", path.display()))
}

/// Read `<catalog>/<name>.json`, from a directory or over http(s).
fn read_catalog_entry(catalog: &str, name: &str, fetch: &Fetch) -> Result<Vec<u8>> {
    if is_url(catalog) {
        refuse_plain_http(catalog, "catalog")?;
        let url = format!("{}/{name}.json", catalog.trim_end_matches('/'));
        return download(&url, fetch).with_context(|| {
            format!(
                "read the catalog entry for {name}. The catalog must hold {name}.json; \
                 what it holds decides which models can be named."
            )
        });
    }
    let path = Path::new(catalog).join(format!("{name}.json"));
    std::fs::read(&path).with_context(|| {
        format!(
            "read the catalog entry {}/{name}.json. The catalog must hold {name}.json; \
             what it holds decides which models can be named.",
            redact_url(catalog).trim_end_matches('/')
        )
    })
}

/// Whether `s` begins `scheme://`, with a scheme as RFC 3986 spells one. A
/// directory path has a `/` or `\\` before any `://`, so it never matches.
pub fn has_scheme(s: &str) -> bool {
    let Some((scheme, _)) = s.trim_start().split_once("://") else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn is_url(s: &str) -> bool {
    let head = s
        .trim_start()
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    head.starts_with("http://") || head.starts_with("https://")
}

/// A catalog, mirror or source address with its credentials, query string and
/// fragment removed, for an error, a log line or a `Debug` print. Those parts
/// are where a token or a password goes; the rest is what an operator needs to
/// see. Any `scheme://` address is treated so, not only http(s): an
/// `s3://key:secret@bucket` is refused later, but it is printed first. A
/// directory is returned as written, and an address that does not parse is
/// not echoed at all.
pub fn redact_url(address: &str) -> String {
    if !is_url(address) && !has_scheme(address) {
        return address.to_string();
    }
    match url::Url::parse(address.trim()) {
        Ok(mut parsed) => {
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.set_query(None);
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Err(_) => "<unparseable address>".to_string(),
    }
}

/// Refuse a plain `http` address on another machine. The catalog decides the
/// digests every artifact is checked against, so over plain http anything on
/// the network path could supply both the files and the digests that would
/// pass them. This machine's loopback is allowed, for a catalog served
/// locally.
///
/// The address is parsed, not sliced: `http://models.example?@localhost` names
/// `models.example`, and the scheme is case-insensitive.
fn refuse_plain_http(address: &str, what: &str) -> Result<()> {
    let Ok(parsed) = url::Url::parse(address.trim()) else {
        bail!("the model {what} is not a valid http(s) address");
    };
    match parsed.scheme() {
        "https" => return Ok(()),
        "http" => {}
        other => bail!("the model {what} uses the scheme {other:?}; use https"),
    }
    let loopback = match parsed.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if loopback {
        return Ok(());
    }
    let host = parsed.host_str().unwrap_or_default();
    bail!(
        "the model {what} at {host} uses plain http; use https. The catalog's digests are \
         what every download is checked against, so over plain http anything on the network \
         path could supply both the files and the digests that pass them."
    )
}

/// The client for one request: an https address must not be redirected to
/// plain http, and nothing may wait forever on a peer that has stopped
/// answering.
fn agent(url: &str, fetch: &Fetch) -> ureq::Agent {
    let https = url::Url::parse(url.trim()).is_ok_and(|parsed| parsed.scheme() == "https");
    ureq::Agent::config_builder()
        .https_only(https)
        .timeout_connect(Some(fetch.connect))
        .timeout_recv_response(Some(fetch.response))
        .timeout_recv_body(Some(fetch.body))
        .build()
        .new_agent()
}

fn get(url: &str, fetch: &Fetch) -> Result<ureq::http::Response<ureq::Body>> {
    agent(url, fetch)
        .get(url)
        .call()
        .with_context(|| format!("GET {}", redact_url(url)))
}

/// One blocking GET of a small document, such as a catalog entry.
fn download(url: &str, fetch: &Fetch) -> Result<Vec<u8>> {
    let mut response = get(url, fetch)?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_CATALOG_BYTES)
        .read_to_vec()
        .with_context(|| format!("read the response from {}", redact_url(url)))
}

/// One blocking GET of a model artifact, streamed into `dest` while its
/// SHA-256 is computed, so that a 400 MB graph is never held in memory and a
/// body over the cap stops at the cap. Returns the digest of what was written.
fn download_to_file(url: &str, dest: &Path, fetch: &Fetch) -> Result<String> {
    use std::io::{Read as _, Write as _};

    use sha2::{Digest as _, Sha256};

    let cap = fetch.max_artifact_bytes;
    let shown = redact_url(url);
    let too_big = || anyhow::anyhow!("the response from {shown} is larger than {cap} bytes");
    let mut response = get(url, fetch)?;
    if response
        .body()
        .content_length()
        .is_some_and(|len| len > cap)
    {
        return Err(too_big());
    }
    let mut reader = response
        .body_mut()
        .with_config()
        .limit(cap.saturating_add(1))
        .reader();
    let mut file =
        std::fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("read the response from {shown}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > cap {
            return Err(too_big());
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .with_context(|| format!("write {}", dest.display()))?;
    }
    file.flush()
        .with_context(|| format!("write {}", dest.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
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

    /// Like `serve`, for `n` requests answered concurrently, so two resolves
    /// can share one server.
    fn serve_many(dir: PathBuf, n: usize) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead as _, BufReader, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = std::thread::spawn(move || {
            let mut workers = Vec::new();
            for _ in 0..n {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let dir = dir.clone();
                workers.push(std::thread::spawn(move || {
                    let mut line = String::new();
                    if BufReader::new(&stream).read_line(&mut line).is_err() {
                        return;
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let body =
                        std::fs::read(dir.join(path.trim_start_matches('/'))).unwrap_or_default();
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
                }));
            }
            for worker in workers {
                let _ = worker.join();
            }
        });
        (base, handle)
    }

    fn request(catalog: &str, cache: PathBuf) -> Request {
        Request {
            spec: "demo".to_string(),
            catalog: Some(catalog.to_string()),
            cache: Some(cache),
            mirror: None,
        }
    }

    #[test]
    fn a_server_that_never_answers_fails_the_resolve_instead_of_hanging_it() {
        let scratch = Scratch::new("stalled");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        // Accepts, reads nothing, answers nothing, and keeps the sockets open.
        let held = std::thread::spawn(move || {
            let mut open = Vec::new();
            while let Ok((stream, _)) = listener.accept() {
                open.push(stream);
            }
        });
        let fetch = Fetch {
            connect: Duration::from_millis(300),
            response: Duration::from_millis(300),
            body: Duration::from_millis(300),
            ..Fetch::default()
        };
        let cache = scratch.path().join("cache");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(resolve_with(&request(&base, cache), &fetch));
        });
        let outcome = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("resolve must give up on a server that never answers");
        assert!(outcome.is_err());
        drop(held);
    }

    #[test]
    fn two_hosts_resolving_one_cache_with_stale_artifacts_both_succeed() {
        let scratch = Scratch::new("race");
        let published = scratch.path().join("published");
        std::fs::create_dir_all(&published).expect("published");
        let cache = scratch.path().join("cache");
        let (base, server) = serve_many(published.clone(), 6);
        write_model(&published, "demo", Some(&base));
        // A truncated download from an earlier run: both artifacts are stale.
        let cached = cache.join("demo");
        std::fs::create_dir_all(&cached).expect("cache dir");
        std::fs::write(cached.join("model.onnx"), b"onnx").expect("stale model");
        std::fs::write(cached.join("tokenizer.json"), b"tok").expect("stale tokenizer");

        // Both hosts have seen the stale file before either goes on, which is
        // the interleaving that used to make the second one fail.
        // A barrier that gives up after a few seconds, so that a host that
        // fails early cannot leave the other waiting for it forever.
        let arrived = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetch = Fetch {
            after_stale_check: Some({
                let arrived = arrived.clone();
                std::sync::Arc::new(move || {
                    use std::sync::atomic::Ordering;
                    let me = arrived.fetch_add(1, Ordering::SeqCst) + 1;
                    let round_ends_at = me.div_ceil(2) * 2;
                    let give_up = std::time::Instant::now() + Duration::from_secs(3);
                    while arrived.load(Ordering::SeqCst) < round_ends_at
                        && std::time::Instant::now() < give_up
                    {
                        std::thread::yield_now();
                    }
                })
            }),
            ..Fetch::default()
        };
        let hosts: Vec<_> = (0..2)
            .map(|_| {
                let (base, cache, fetch) = (base.clone(), cache.clone(), fetch.clone());
                std::thread::spawn(move || resolve_with(&request(&base, cache), &fetch))
            })
            .collect();
        for host in hosts {
            let resolved = host
                .join()
                .expect("host thread")
                .expect("each host resolves");
            assert_eq!(resolved.config.model_id, "demo");
        }
        server.join().expect("server");
        assert_eq!(
            std::fs::read(cached.join("model.onnx")).expect("model"),
            b"onnx bytes"
        );
    }

    #[test]
    fn a_catalog_address_of_any_scheme_is_printed_without_its_userinfo() {
        for (secret, kept) in [
            ("s3://k:secret@bucket/models?sig=abc", "s3://bucket/models"),
            ("S3://k:secret@bucket", "s3://bucket"),
            ("ftp://anon:secret@host:2121/c#f", "ftp://host:2121/c"),
            ("git+ssh://deploy:secret@host/repo", "git+ssh://host/repo"),
        ] {
            let shown = redact_url(secret);
            assert_eq!(shown, kept);
            for leak in ["secret", "k:", "abc"] {
                assert!(!shown.contains(leak), "{shown}");
            }
        }
        // Not echoed when it will not parse, as for http.
        assert_eq!(redact_url("s3://k:secret@[bad"), "<unparseable address>");
        // A directory is returned as written, whatever it holds.
        for dir in ["/srv/catalog", "rel/a://b@c", "C:\\models", "models"] {
            assert_eq!(redact_url(dir), dir);
        }
        let req = Request {
            spec: "demo".to_string(),
            catalog: Some("s3://k:secret@b".to_string()),
            cache: None,
            mirror: Some("gs://u:hunter2@b".to_string()),
        };
        let printed = format!("{req:?} {req:#?}");
        for leak in ["secret", "hunter2"] {
            assert!(!printed.contains(leak), "{printed}");
        }
        let err = resolve(&req).expect_err("s3 is not a catalog");
        let msg = format!("{err:#} {err:?}");
        for leak in ["secret", "hunter2"] {
            assert!(!msg.contains(leak), "{msg}");
        }
    }

    #[test]
    fn a_catalog_or_mirror_address_is_printed_without_credentials_or_query() {
        let secret = "http://user:hunter2@127.0.0.1:1/cat?token=s3cr3t#frag";
        let shown = redact_url(secret);
        for leak in ["user", "hunter2", "s3cr3t", "frag"] {
            assert!(!shown.contains(leak), "{shown}");
        }
        assert!(shown.contains("127.0.0.1:1/cat"), "{shown}");

        let scratch = Scratch::new("redact");
        let req = Request {
            spec: "demo".to_string(),
            catalog: Some(secret.to_string()),
            cache: Some(scratch.path().join("cache")),
            mirror: Some("https://m:pw9@mirror.example/x?sig=abc123".to_string()),
        };
        let printed = format!("{req:?} {req:#?}");
        for leak in ["hunter2", "s3cr3t", "pw9", "abc123"] {
            assert!(!printed.contains(leak), "{printed}");
        }
        let err = resolve(&req).expect_err("nothing listens on port 1");
        let msg = format!("{err:#} {err:?}");
        for leak in ["hunter2", "s3cr3t", "user:"] {
            assert!(!msg.contains(leak), "{msg}");
        }
        assert!(
            msg.contains("GET "),
            "the error still names the request: {msg}"
        );

        // A fetched artifact that does not match: the source is named, without
        // its credentials.
        let published = scratch.path().join("published");
        std::fs::create_dir_all(&published).expect("published");
        let (base, server) = serve(published.clone());
        let with_creds = base.replacen("http://", "http://user:hunter2@", 1);
        write_model(&published, "demo", Some(&with_creds));
        std::fs::write(published.join("model.onnx"), b"other").expect("swap");
        let err = resolve(&request(&base, scratch.path().join("cache2")))
            .expect_err("wrong bytes are refused");
        drop(server);
        let msg = format!("{err:#}");
        assert!(msg.contains("does not match"), "{msg}");
        for leak in ["hunter2", "s3cr3t"] {
            assert!(!msg.contains(leak), "{msg}");
        }
    }

    #[test]
    fn a_body_over_the_cap_fails_and_leaves_nothing_behind() {
        let scratch = Scratch::new("cap");
        let published = scratch.path().join("published");
        std::fs::create_dir_all(&published).expect("published");
        let cache = scratch.path().join("cache");
        let (base, server) = serve_many(published.clone(), 3);
        write_model(&published, "demo", Some(&base));
        let fetch = Fetch {
            max_artifact_bytes: 4,
            ..Fetch::default()
        };
        let err = resolve_with(&request(&base, cache.clone()), &fetch)
            .expect_err("a body over the cap is refused");
        drop(server);
        let msg = format!("{err:#}");
        assert!(msg.contains("larger than"), "{msg}");
        let left: Vec<_> = std::fs::read_dir(cache.join("demo"))
            .expect("the cache dir")
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter(|name| name != "demo.json")
            .collect();
        assert!(left.is_empty(), "nothing but the descriptor: {left:?}");
    }

    #[test]
    fn a_truncated_body_leaves_no_partial_file() {
        use std::io::{BufRead as _, BufReader, Write as _};
        let scratch = Scratch::new("truncated");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut line = String::new();
                let _ = BufReader::new(&stream).read_line(&mut line);
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                if path.ends_with(".json") && !path.contains("tokenizer") {
                    let entry = serde_json::json!({
                        "model_id": "demo", "model_path": "model.onnx",
                        "tokenizer_path": "tokenizer.json",
                        "artifact_sha256": "0".repeat(64),
                        "tokenizer_sha256": "1".repeat(64),
                        "pooling": "cls", "license": "Apache-2.0",
                        "source": { "base_url": format!("http://{}", stream.local_addr().expect("a")) },
                    })
                    .to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{entry}",
                        entry.len()
                    );
                } else {
                    // Promises 100 bytes, sends 5, hangs up.
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort"
                    );
                }
            }
        });
        let cache = scratch.path().join("cache");
        resolve(&request(&base, cache.clone())).expect_err("a short body is refused");
        drop(server);
        let left: Vec<_> = std::fs::read_dir(cache.join("demo"))
            .expect("the cache dir")
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter(|name| name != "demo.json")
            .collect();
        assert!(left.is_empty(), "nothing but the descriptor: {left:?}");
    }

    #[test]
    fn stale_partial_files_are_swept_but_a_fresh_one_is_left_to_its_writer() {
        let scratch = Scratch::new("sweep");
        let catalog = scratch.path().join("catalog");
        let cache = scratch.path().join("cache");
        std::fs::create_dir_all(&catalog).expect("catalog");
        let cached = cache.join("demo");
        std::fs::create_dir_all(&cached).expect("cache dir");
        write_model(&cached, "demo", None);
        write_model(&catalog, "demo", Some("http://127.0.0.1:1/never"));
        let old = cached.join(".model.onnx.1.2.partial");
        let fresh = cached.join(".model.onnx.3.4.partial");
        std::fs::write(&old, b"abandoned").expect("old");
        std::fs::write(&fresh, b"in flight").expect("fresh");
        std::fs::File::options()
            .write(true)
            .open(&old)
            .expect("open")
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(2 * 60 * 60))
            .expect("age it");

        resolve(&request(&catalog.display().to_string(), cache)).expect("resolves from cache");
        assert!(!old.exists(), "an abandoned partial file is removed");
        assert!(fresh.exists(), "one a host may be writing is not");
    }

    #[test]
    fn a_name_of_only_dots_is_not_a_model_name() {
        let scratch = Scratch::new("dots");
        for bad in [".", "..", "...", ".json"] {
            let mut req = request("/tmp", scratch.path().join("cache"));
            req.spec = bad.to_string();
            let err = resolve(&req).expect_err("dots name nothing");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("is not a model name") || msg.contains("read as a path"),
                "{bad:?}: {msg}"
            );
        }
    }

    #[test]
    fn every_way_of_spelling_a_remote_plain_http_address_is_refused() {
        let scratch = Scratch::new("bypass");
        for catalog in [
            "http://models.example?@localhost",
            "http://models.example#@localhost",
            "http://localhost@models.example/c",
            "http://127.0.0.1@models.example/c",
            "HTTP://models.example/c",
            "Http://models.example/c",
            "http://MODELS.example/c",
        ] {
            let err = resolve(&request(catalog, scratch.path().join("cache")))
                .expect_err("remote plain http must be refused");
            let msg = format!("{err:#}");
            assert!(msg.contains("plain http"), "{catalog}: {msg}");
            assert!(!msg.contains("GET "), "{catalog}: {msg}");
        }
    }

    #[test]
    fn an_upper_case_scheme_is_still_a_url_and_loopback_http_is_still_allowed() {
        let scratch = Scratch::new("scheme-case");
        for catalog in [
            "HTTPS://127.0.0.1:1/cat",
            "HTTP://127.0.0.1:1/cat",
            "http://[::1]:1/c",
        ] {
            let err = resolve(&request(catalog, scratch.path().join("cache")))
                .expect_err("nothing listens on port 1");
            let msg = format!("{err:#}");
            assert!(msg.contains("GET "), "{catalog}: {msg}");
        }
    }
}
