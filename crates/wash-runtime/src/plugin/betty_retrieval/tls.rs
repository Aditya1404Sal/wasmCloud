//! How the plugin reaches its database: the url as tokio-postgres reads it,
//! and TLS, which the plugin sets up itself.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use tokio_postgres::config::SslMode;
use url::Url;

/// The database url, split into what tokio-postgres reads and what the plugin
/// reads itself.
///
/// tokio-postgres accepts only `disable`, `prefer` and `require` for
/// `sslmode`, and refuses libpq's `sslrootcert`. So both are taken out of the
/// url first. `require`, `verify-ca` and `verify-full` all connect with TLS,
/// and rustls always checks the certificate chain and the host name, so all
/// three verify fully. The trust roots are the public web roots and, when
/// one is given, a CA file. The connection keeps itself alive (see
/// [`KeepAlive`]).
pub(crate) struct DatabaseConnection {
    pub(crate) pg: tokio_postgres::Config,
    /// `None` without TLS.
    pub(crate) tls: Option<rustls::ClientConfig>,
}

impl DatabaseConnection {
    /// No error names the url or any part of it: it carries the password.
    /// An option is named by its key, and a CA file by its path.
    pub(crate) fn from_url(database_url: &str, ca_file: Option<&Path>) -> anyhow::Result<Self> {
        let mut url = Url::parse(database_url).context("database_url is not a valid url")?;
        let mut sslmode = None;
        let mut sslrootcert = None;
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .into_owned()
            .filter(|(key, value)| match key.as_str() {
                "sslmode" => {
                    sslmode = Some(value.clone());
                    false
                }
                "sslrootcert" => {
                    sslrootcert = Some(PathBuf::from(value));
                    false
                }
                _ => true,
            })
            .collect();
        let url_sets = |key: &str| kept.iter().any(|(set, _)| set == key);
        let keep_alive = KeepAlive {
            idle: !url_sets("keepalives_idle"),
            interval: !url_sets("keepalives_interval"),
            retries: !url_sets("keepalives_retries"),
            user_timeout: !url_sets("tcp_user_timeout"),
        };
        if kept.is_empty() {
            url.set_query(None);
        } else {
            url.query_pairs_mut().clear().extend_pairs(kept);
        }
        // The client's errors name an option, never its value; the cause,
        // which says which option, is in the error's source.
        let mut pg: tokio_postgres::Config = url.as_str().parse().map_err(|e| {
            let cause = std::error::Error::source(&e)
                .map(|cause| format!(": {cause}"))
                .unwrap_or_default();
            anyhow!(
                "database_url is not a connection string the postgres client accepts: {e}{cause}"
            )
        })?;
        keep_alive.apply(&mut pg);

        let tls = match sslmode.as_deref() {
            None | Some("prefer" | "allow") => {
                pg.ssl_mode(SslMode::Prefer);
                false
            }
            Some("disable") => {
                pg.ssl_mode(SslMode::Disable);
                false
            }
            Some("require" | "verify-ca" | "verify-full") => {
                pg.ssl_mode(SslMode::Require);
                true
            }
            Some(other) => bail!(
                "database_url has sslmode {other:?}; use disable, prefer, require, verify-ca or \
                 verify-full"
            ),
        };
        let ca_file = ca_file.map(Path::to_path_buf).or(sslrootcert);
        let tls = if tls {
            crate::init_crypto();
            Some(
                rustls::ClientConfig::builder()
                    .with_root_certificates(trust_roots(ca_file.as_deref())?)
                    .with_no_client_auth(),
            )
        } else {
            None
        };
        Ok(Self { pg, tls })
    }
}

/// TCP keepalives after 30 seconds of quiet, then every 10 seconds, giving up
/// after 3 unanswered, and on Linux a 60-second TCP user timeout: a connection
/// the network dropped without a word is noticed within about a minute,
/// rather than when TCP gives up on it, about 15 minutes. Each applies unless
/// the url sets its option.
struct KeepAlive {
    idle: bool,
    interval: bool,
    retries: bool,
    user_timeout: bool,
}

impl KeepAlive {
    const IDLE: Duration = Duration::from_secs(30);
    const INTERVAL: Duration = Duration::from_secs(10);
    const RETRIES: u32 = 3;
    const USER_TIMEOUT: Duration = Duration::from_secs(60);

    fn apply(&self, pg: &mut tokio_postgres::Config) {
        if self.idle {
            pg.keepalives_idle(Self::IDLE);
        }
        if self.interval {
            pg.keepalives_interval(Self::INTERVAL);
        }
        if self.retries {
            pg.keepalives_retries(Self::RETRIES);
        }
        if self.user_timeout {
            pg.tcp_user_timeout(Self::USER_TIMEOUT);
        }
    }
}

/// The public web roots compiled into the binary, and the certificates in
/// `ca_file` when there is one. A file that cannot be read, or that holds no
/// usable certificate, is refused: trusting nothing from it would read as a
/// server whose certificate does not match.
fn trust_roots(ca_file: Option<&Path>) -> anyhow::Result<rustls::RootCertStore> {
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let Some(path) = ca_file else {
        return Ok(roots);
    };
    let shown = path.display();
    let data = std::fs::read(path).with_context(|| format!("read the CA file {shown}"))?;
    let certs = CertificateDer::pem_slice_iter(&data)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("the CA file {shown} is not PEM-encoded certificates: {e}"))?;
    if certs.is_empty() {
        bail!("the CA file {shown} holds no certificate");
    }
    for cert in certs {
        roots
            .add(cert)
            .map_err(|e| anyhow!("a certificate in {shown} cannot be a trust root: {e}"))?;
    }
    Ok(roots)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    const PASSWORD: &str = "hunter2";

    fn url(query: &str) -> String {
        format!("postgres://genius:{PASSWORD}@db.internal:5432/retrieval{query}")
    }

    /// A PEM file holding one self-signed CA certificate, in `dir`.
    fn ca_file(dir: &Path) -> PathBuf {
        let ca = rcgen::generate_simple_self_signed(vec!["private-ca.test".to_string()])
            .expect("generate a test CA");
        let path = dir.join("ca.pem");
        std::fs::write(&path, ca.cert.pem()).expect("write the test CA");
        path
    }

    fn public_roots() -> usize {
        webpki_roots::TLS_SERVER_ROOTS.len()
    }

    #[test]
    fn require_verify_ca_and_verify_full_all_connect_with_tls() {
        for mode in ["require", "verify-ca", "verify-full"] {
            let connection = DatabaseConnection::from_url(&url(&format!("?sslmode={mode}")), None)
                .unwrap_or_else(|e| panic!("sslmode={mode} must be accepted: {e:#}"));
            assert!(connection.tls.is_some(), "sslmode={mode} must use TLS");
            assert_eq!(
                connection.pg.get_ssl_mode(),
                tokio_postgres::config::SslMode::Require,
                "sslmode={mode}"
            );
        }
    }

    #[test]
    fn disable_prefer_and_no_sslmode_connect_without_tls() {
        for (query, mode) in [
            ("?sslmode=disable", tokio_postgres::config::SslMode::Disable),
            ("?sslmode=prefer", tokio_postgres::config::SslMode::Prefer),
            ("", tokio_postgres::config::SslMode::Prefer),
        ] {
            let connection = DatabaseConnection::from_url(&url(query), None)
                .unwrap_or_else(|e| panic!("{query:?} must be accepted: {e:#}"));
            assert!(connection.tls.is_none(), "{query:?} must not use TLS");
            assert_eq!(connection.pg.get_ssl_mode(), mode, "{query:?}");
        }
    }

    #[test]
    fn the_options_tokio_postgres_knows_still_reach_it() {
        let connection =
            DatabaseConnection::from_url(&url("?sslmode=require&application_name=retrieval"), None)
                .expect("a url with a known option parses");
        assert_eq!(connection.pg.get_application_name(), Some("retrieval"));
        assert_eq!(connection.pg.get_dbname(), Some("retrieval"));
        assert_eq!(connection.pg.get_password(), Some(PASSWORD.as_bytes()));
    }

    #[test]
    fn an_sslmode_without_a_meaning_here_is_refused_without_printing_the_url() {
        let err = DatabaseConnection::from_url(&url("?sslmode=sometimes"), None)
            .err()
            .expect("an unknown sslmode must be refused")
            .to_string();
        assert!(err.contains("sometimes"), "{err}");
        assert!(!err.contains(PASSWORD), "{err}");
        assert!(!err.contains("db.internal"), "{err}");
    }

    #[test]
    fn an_option_tokio_postgres_does_not_know_is_refused_without_printing_the_url() {
        let err = DatabaseConnection::from_url(&url("?sslcert=/client.pem"), None)
            .err()
            .expect("an unknown option must be refused")
            .to_string();
        assert!(err.contains("sslcert"), "{err}");
        assert!(!err.contains(PASSWORD), "{err}");
    }

    #[test]
    fn a_ca_file_adds_its_certificate_to_the_public_roots() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let ca = ca_file(dir.path());
        assert_eq!(
            trust_roots(Some(&ca)).expect("the CA loads").len(),
            public_roots() + 1
        );
        assert_eq!(
            trust_roots(None).expect("public roots").len(),
            public_roots()
        );
    }

    #[test]
    fn sslrootcert_in_the_url_names_a_ca_file_and_the_setting_wins_over_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let ca = ca_file(dir.path());
        let missing = dir.path().join("missing.pem");
        let with_root = |path: &Path| {
            url(&format!(
                "?sslmode=verify-full&sslrootcert={}",
                path.display()
            ))
        };

        DatabaseConnection::from_url(&with_root(&ca), None)
            .expect("sslrootcert names a readable CA file");
        let err = DatabaseConnection::from_url(&with_root(&missing), None)
            .err()
            .expect("the CA file sslrootcert names is read")
            .to_string();
        assert!(err.contains("missing.pem"), "{err}");
        DatabaseConnection::from_url(&with_root(&missing), Some(&ca))
            .expect("with the setting given, sslrootcert's file is not the one read");
    }

    /// TCP keepalives and, where the platform has it, a TCP user timeout find
    /// a connection the network dropped without a word. Without them such a
    /// connection is noticed only once TCP gives up, about 15 minutes.
    #[test]
    fn a_connection_the_network_drops_silently_is_noticed_within_a_minute() {
        use std::time::Duration;
        let pg = DatabaseConnection::from_url(&url(""), None)
            .expect("a plain url parses")
            .pg;
        assert!(pg.get_keepalives());
        assert_eq!(pg.get_keepalives_idle(), Duration::from_secs(30));
        assert_eq!(pg.get_keepalives_interval(), Some(Duration::from_secs(10)));
        assert_eq!(pg.get_keepalives_retries(), Some(3));
        assert_eq!(pg.get_tcp_user_timeout(), Some(&Duration::from_secs(60)));
    }

    #[test]
    fn keepalive_options_the_url_sets_win() {
        use std::time::Duration;
        let pg = DatabaseConnection::from_url(
            &url(
                "?keepalives_idle=7200&keepalives_interval=2&keepalives_retries=9&\
                 tcp_user_timeout=5",
            ),
            None,
        )
        .expect("a url with keepalive options parses")
        .pg;
        assert_eq!(pg.get_keepalives_idle(), Duration::from_secs(7200));
        assert_eq!(pg.get_keepalives_interval(), Some(Duration::from_secs(2)));
        assert_eq!(pg.get_keepalives_retries(), Some(9));
        assert_eq!(pg.get_tcp_user_timeout(), Some(&Duration::from_secs(5)));

        let pg = DatabaseConnection::from_url(&url("?keepalives=0"), None)
            .expect("a url that turns keepalives off parses")
            .pg;
        assert!(!pg.get_keepalives());
    }

    #[test]
    fn a_ca_file_that_holds_no_certificate_is_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let not_pem = dir.path().join("ca.pem");
        std::fs::write(&not_pem, "not a certificate").expect("write the file");
        let err = DatabaseConnection::from_url(&url("?sslmode=require"), Some(&not_pem))
            .err()
            .expect("a file with no certificate must be refused")
            .to_string();
        assert!(err.contains("ca.pem"), "{err}");
    }
}
