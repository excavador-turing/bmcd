// Copyright 2023 Turing Machines
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use config::FileFormat;
use serde::Deserialize;
use serde_with::serde_as;
use serde_with::DurationSeconds;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_YAML: &str = include_str!("../../default_config.yaml");

#[derive(Debug, Deserialize)]
pub struct Config {
    pub tls: Tls,
    pub store: Store,
    pub authentication: Authentication,
    pub host: String,
    pub port: u16,
    /// Where `/metrics` is served, on its own plain-HTTP listener.
    ///
    /// Defaulted rather than required, so a configuration file written for an
    /// older bmcd still parses. It binds the same `host` as the API, so
    /// restricting the daemon to a management address covers both listeners
    /// with one setting.
    #[serde(default = "default_metrics_port")]
    pub metrics_port: u16,
    pub www: PathBuf,
    pub redirect_http: bool,
    pub log: Log,
}

#[serde_as]
#[derive(Debug, Deserialize)]
pub struct Store {
    #[serde_as(as = "Option<DurationSeconds<u64>>")]
    pub write_timeout: Option<Duration>,
}

#[serde_as]
#[derive(Debug, Deserialize)]
pub struct Authentication {
    pub authentication_attempts: usize,
    #[serde_as(as = "DurationSeconds<u64>")]
    pub token_expires: Duration,
}

#[derive(Debug, Deserialize)]
pub struct Tls {
    pub private_key: PathBuf,
    pub certificate: PathBuf,

    /// PEM bundle of certificate authorities whose CLIENT certificates this
    /// daemon will accept as proof that a request came from a trusted proxy.
    ///
    /// Absent -- the default, and what a board ships with -- means no client
    /// certificate is requested and [`Tls::identity_header`] is never
    /// believed. A board on a bench behaves exactly as it did before this
    /// existed.
    ///
    /// Set, it enables the one path by which a request can be authorised
    /// without a password or a session token: a proxy that holds a
    /// certificate from this CA, telling us which human it already
    /// authenticated. That is the whole of SQU-136's daemon half, and the
    /// reason the fleet interface can be exposed when a board cannot.
    #[serde(default)]
    pub client_ca: Option<PathBuf>,

    /// Header naming the human the proxy authenticated.
    ///
    /// Believed ONLY on a connection that presented a certificate this
    /// daemon verified against [`Tls::client_ca`]. Without that, the header
    /// is ordinary attacker-controlled input -- anyone on the management LAN
    /// can set it -- so the certificate is what makes it mean anything, and
    /// the two are useless apart.
    #[serde(default = "default_identity_header")]
    pub identity_header: String,
}

fn default_identity_header() -> String {
    "x-forwarded-email".to_string()
}

/// Where the interface stores a CA it was given (`api/access.rs`).
///
/// A FIXED path, and that is what makes the feature possible at all. This
/// daemon reads its configuration with the `config` crate, which has no
/// writer, and `config.yaml` is the operator's file with the operator's
/// comments in it. Rewriting it from an HTTP handler would mean shipping a
/// YAML serialiser and losing every comment on the first save.
///
/// So the interface changes a FILE and never the configuration. `config.yaml`
/// still wins when it names a path: `effective_client_ca` prefers it, and the
/// handlers refuse to touch anything when it is set, telling the operator to
/// edit the file they already chose to use.
pub const MANAGED_CLIENT_CA: &str = "/etc/ssl/certs/bmcd_client_ca.pem";

/// The one thing besides the bundle that the interface can set.
///
/// A separate file rather than a second copy of the configuration: it holds
/// exactly one setting, it is written by one handler, and a board that has
/// never used the interface does not have it at all.
pub const ACCESS_OVERRIDES: &str = "/etc/bmcd/access.json";

/// Settings the interface owns, as stored.
#[derive(Debug, Default, Deserialize, serde::Serialize)]
pub struct AccessOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_header: Option<String>,
}

impl AccessOverrides {
    /// Absent or unreadable reads as "nothing overridden". A board whose
    /// sidecar is corrupt falls back to its configuration rather than
    /// refusing to describe itself.
    pub fn load() -> Self {
        std::fs::read(ACCESS_OVERRIDES)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default()
    }

    pub fn store(&self) -> std::io::Result<()> {
        let path = Path::new(ACCESS_OVERRIDES);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_atomically(path, &serde_json::to_vec_pretty(self)?)
    }
}

/// Write through a temporary file in the SAME directory, then rename.
///
/// A half-written CA bundle is a board that asks for a client certificate and
/// can verify none, discovered at the next login rather than now; a rename is
/// atomic on the same filesystem, so the file is either the old one or the
/// new one and never half of either.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

pub fn write_client_ca(pem: &[u8]) -> std::io::Result<()> {
    write_atomically(Path::new(MANAGED_CLIENT_CA), pem)
}

pub fn remove_client_ca() -> std::io::Result<()> {
    match std::fs::remove_file(MANAGED_CLIENT_CA) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Which of the three sources decided a setting, so the interface can say so
/// instead of presenting every value as equally changeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// config.yaml said so. Not changeable from the interface.
    Config,
    /// The interface stored it.
    Override,
    /// Nobody said; this is the built-in.
    Default,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Config => write!(f, "config"),
            Source::Override => write!(f, "override"),
            Source::Default => write!(f, "default"),
        }
    }
}

impl Tls {
    /// The CA to verify client certificates against, or `None` for a board
    /// that trusts no proxy.
    ///
    /// EXISTENCE, not configuration, decides the managed path. A configured
    /// path that does not exist makes the daemon fail to start -- which is
    /// why neither setting ships in the firmware's default config.yaml -- and
    /// a board whose CA was removed must come back up rather than refuse to.
    pub fn effective_client_ca(&self) -> Option<PathBuf> {
        if let Some(path) = self.client_ca.as_ref() {
            return Some(path.clone());
        }
        let managed = PathBuf::from(MANAGED_CLIENT_CA);
        managed.exists().then_some(managed)
    }

    /// The header name and where it came from.
    pub fn effective_identity_header(&self, overrides: &AccessOverrides) -> (String, Source) {
        if self.identity_header != default_identity_header() {
            return (self.identity_header.clone(), Source::Config);
        }
        match overrides.identity_header.as_deref() {
            Some(name) if !name.is_empty() => (name.to_string(), Source::Override),
            _ => (self.identity_header.clone(), Source::Default),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Log {
    pub stdout: bool,
    pub directive: String,
    pub coloring: bool,
}

impl Config {
    pub fn load(config_file: &Path) -> anyhow::Result<Self> {
        let config = config::Config::builder()
            .add_source(config::File::from_str(DEFAULT_YAML, FileFormat::Yaml))
            .add_source(
                config::File::new(&config_file.to_string_lossy(), FileFormat::Yaml).required(false),
            )
            .build()?;

        Ok(config.try_deserialize()?)
    }
}

/// Port 9110, chosen rather than inherited.
///
/// Not 443, which serves the API and the web interface. Not 9100, which means
/// node-exporter to every scrape config and dashboard in this estate, so a
/// BMC answering there would be read as a node with the wrong metric
/// families. Nothing else here uses 9110, and the `job` and `instance` labels
/// are what actually disambiguate a target.
fn default_metrics_port() -> u16 {
    9110
}
