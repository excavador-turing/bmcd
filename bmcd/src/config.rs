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
