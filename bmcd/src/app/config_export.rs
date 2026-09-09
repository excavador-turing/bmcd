// Copyright 2026 Oleg Tsarev
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
//!
//! Everything a person has configured, as one document.
//!
//! Board B is a named milestone, and the first thing anyone will want is "make
//! it like board A". Today that is five settings re-entered by hand plus a
//! metrics token re-pasted into a scrape config.
//!
//! ## An allow-list, never "everything on the overlay"
//!
//! The overlay also carries `htoprc` and a zero-byte `crond.reboot`, residue
//! of tools that were removed. Copying the overlay wholesale would clone that
//! junk to a board that never had those tools. Every field below is named.
//!
//! ## The token is what makes an export sensitive
//!
//! The metrics token is a credential. An export carrying it can scrape any
//! board it is applied to, so it is included only when asked for and the
//! document says on its face which kind it is.
//!
//! ## The node store is read through its own API
//!
//! `bmcd.bin` is the daemon's key/value store -- a bincode database with its
//! own header and magic, not a settings file. Nothing here decodes it: the
//! values are read through `get_node_infos()`, which is the one place that
//! already knows those types.

use crate::app::bmc_application::{BmcApplication, NodeInfo};
use crate::hal::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Bumped when a field changes meaning, not when one is added: an importer
/// ignores what it does not know and reports what it could not apply.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSettings {
    pub id: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uart_baud: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigExport {
    pub format_version: u32,
    /// When and from what. Not read back on import; it is what makes a file
    /// found on a workstation six months later identifiable.
    pub exported_at: String,
    pub exported_from: ExportOrigin,
    /// True when `secrets` is present. On its face, because the difference
    /// decides how the file must be handled.
    pub contains_secrets: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ntp_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware_sources: Option<crate::app::firmware_sources::Sources>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nodes: Option<Vec<NodeSettings>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Secrets>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportOrigin {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board_serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
}

/// The credential tier. Separate from the settings above so that "export
/// without secrets" is a different shape and not a field someone has to
/// remember to strip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Secrets {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics_token: Option<String>,
}

/// What can be applied, and what could not.
#[derive(Debug, Default, Serialize)]
pub struct ImportReport {
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
    pub failed: Vec<String>,
}

/// Reads the board's settings.
///
/// Nothing here fails the whole export. A board with no overlay still has a
/// hostname; a daemon that cannot read one setting should still hand over the
/// other four, because the alternative is an operator with nothing at all in
/// front of a board B.
pub async fn export(bmc: &BmcApplication, with_secrets: bool) -> ConfigExport {
    let hostname = crate::app::hostname::current().await;
    let ntp = crate::app::ntp::load().await;
    let sources = crate::app::firmware_sources::load().await;

    let nodes = match bmc.get_node_infos().await {
        Ok(infos) => Some(
            infos
                .iter()
                .enumerate()
                .map(|(index, info)| NodeSettings {
                    id: index as u8 + 1,
                    name: info.name.clone(),
                    module_name: info.module_name.clone(),
                    uart_baud: info.uart_baud,
                })
                .collect(),
        ),
        Err(e) => {
            tracing::warn!("config export: cannot read the node settings: {e}");
            None
        }
    };

    let secrets = if with_secrets {
        Some(Secrets {
            metrics_token: crate::app::metrics_token::peek().await,
        })
    } else {
        None
    };

    ConfigExport {
        format_version: FORMAT_VERSION,
        exported_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        exported_from: ExportOrigin {
            hostname: hostname.clone(),
            board_serial: crate::api::legacy::read_board_info()
                .await
                .ok()
                .and_then(|(_, _, serial)| serial),
            firmware: crate::api::legacy::firmware_version().await,
        },
        contains_secrets: secrets.is_some(),
        hostname,
        // An empty list is the shipped default, and exporting it as "no NTP
        // configuration" is right: an import should then leave the target's
        // own default alone rather than clear it.
        ntp_servers: (!ntp.servers.is_empty()).then_some(ntp.servers),
        firmware_sources: Some(sources),
        nodes,
        secrets,
    }
}

/// Applies a document, field by field, and says what happened to each.
///
/// Deliberately not transactional. There is no way to roll back a hostname and
/// a set of firmware sources together, and pretending otherwise would mean
/// reporting a partial apply as a failure — leaving an operator unsure which
/// half took. Each field reports its own outcome instead.
///
/// The network configuration is not here and never will be: it is per-board by
/// definition, and it already has its own reset path.
pub async fn import(bmc: &BmcApplication, document: &ConfigExport) -> Result<ImportReport, String> {
    if document.format_version != FORMAT_VERSION {
        return Err(format!(
            "this export is format {} and this daemon reads format {FORMAT_VERSION}",
            document.format_version
        ));
    }

    let mut report = ImportReport::default();

    if let Some(name) = &document.hostname {
        match crate::app::hostname::set(name).await {
            Ok(()) => report.applied.push(format!("hostname {name}")),
            Err(e) => report.failed.push(format!("hostname: {e}")),
        }
    } else {
        report.skipped.push("hostname: not in the export".into());
    }

    if let Some(servers) = &document.ntp_servers {
        match crate::app::ntp::store(servers).await {
            Ok(()) => report
                .applied
                .push(format!("{} time server(s)", servers.len())),
            Err(e) => report.failed.push(format!("time servers: {e}")),
        }
    } else {
        report
            .skipped
            .push("time servers: not in the export, so the board keeps its own".into());
    }

    if let Some(sources) = &document.firmware_sources {
        match crate::app::firmware_sources::validate(sources) {
            Ok(()) => match crate::app::firmware_sources::store(sources).await {
                Ok(()) => report
                    .applied
                    .push(format!("{} firmware source(s)", sources.sources.len())),
                Err(e) => report.failed.push(format!("firmware sources: {e}")),
            },
            Err(e) => report.failed.push(format!("firmware sources: {e}")),
        }
    } else {
        report
            .skipped
            .push("firmware sources: not in the export".into());
    }

    if let Some(nodes) = &document.nodes {
        let mut infos: HashMap<NodeId, NodeInfo> = HashMap::new();
        let mut rejected = Vec::new();
        for node in nodes {
            let Some(id) = node_id(node.id) else {
                rejected.push(node.id);
                continue;
            };
            infos.insert(
                id,
                NodeInfo {
                    name: node.name.clone(),
                    module_name: node.module_name.clone(),
                    // Never imported: it is when THIS board last powered the
                    // node on, and carrying it to another board would report
                    // an uptime that never happened.
                    power_on_time: None,
                    uart_baud: node.uart_baud,
                },
            );
        }
        if !rejected.is_empty() {
            report
                .failed
                .push(format!("node id(s) {rejected:?} are not 1-4"));
        }
        if !infos.is_empty() {
            let count = infos.len();
            match bmc.set_node_info(infos).await {
                Ok(()) => report.applied.push(format!("{count} node name(s)")),
                Err(e) => report.failed.push(format!("node names: {e}")),
            }
        }
    } else {
        report.skipped.push("node names: not in the export".into());
    }

    match document
        .secrets
        .as_ref()
        .and_then(|s| s.metrics_token.as_ref())
    {
        Some(token) => match crate::app::metrics_token::adopt(token).await {
            Ok(()) => report.applied.push("metrics token".into()),
            Err(e) => report.failed.push(format!("metrics token: {e}")),
        },
        None => report
            .skipped
            .push("metrics token: not in the export, so the board keeps its own".into()),
    }

    Ok(report)
}

fn node_id(id: u8) -> Option<NodeId> {
    match id {
        1 => Some(NodeId::Node1),
        2 => Some(NodeId::Node2),
        3 => Some(NodeId::Node3),
        4 => Some(NodeId::Node4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(secrets: Option<Secrets>) -> ConfigExport {
        ConfigExport {
            format_version: FORMAT_VERSION,
            exported_at: "2026-09-09T00:00:00Z".into(),
            exported_from: ExportOrigin {
                hostname: Some("hive-a-bmc".into()),
                board_serial: Some("XZCT250200139".into()),
                firmware: Some("v2.8.1".into()),
            },
            contains_secrets: secrets.is_some(),
            hostname: Some("hive-a-bmc".into()),
            ntp_servers: Some(vec!["192.168.77.1".into()]),
            firmware_sources: None,
            nodes: Some(vec![NodeSettings {
                id: 1,
                name: Some("hive-1".into()),
                module_name: Some("RK1".into()),
                uart_baud: None,
            }]),
            secrets,
        }
    }

    /// An export without secrets must not carry the key at all, so a file that
    /// looks safe cannot turn out to have an empty credential field that a
    /// later version fills in.
    #[test]
    fn an_export_without_secrets_has_no_secrets_key() {
        let json = serde_json::to_string(&document(None)).expect("serialises");
        // The key, not the substring: `contains_secrets` contains "secrets",
        // and asserting on that passed for the wrong reason.
        assert!(!json.contains("\"secrets\""), "{json}");
        assert!(json.contains("\"contains_secrets\":false"), "{json}");
    }

    #[test]
    fn an_export_with_secrets_says_so_on_its_face() {
        let json = serde_json::to_string(&document(Some(Secrets {
            metrics_token: Some("deadbeef".into()),
        })))
        .expect("serialises");
        assert!(json.contains("\"contains_secrets\":true"), "{json}");
        assert!(json.contains("metrics_token"), "{json}");
    }

    #[test]
    fn a_document_round_trips() {
        let original = document(None);
        let json = serde_json::to_string(&original).expect("serialises");
        let parsed: ConfigExport = serde_json::from_str(&json).expect("parses");
        assert_eq!(parsed.hostname.as_deref(), Some("hive-a-bmc"));
        assert_eq!(parsed.ntp_servers, Some(vec!["192.168.77.1".to_string()]));
        assert_eq!(
            parsed.nodes.expect("nodes")[0].name.as_deref(),
            Some("hive-1")
        );
    }

    /// A field this daemon has never heard of must not fail the import: an
    /// export from a newer board should still restore the four settings both
    /// versions understand.
    #[test]
    fn an_unknown_field_is_ignored_rather_than_fatal() {
        let json = r#"{
            "format_version": 1,
            "exported_at": "2026-09-09T00:00:00Z",
            "exported_from": {},
            "contains_secrets": false,
            "hostname": "hive-a-bmc",
            "something_from_the_future": {"a": 1}
        }"#;
        let parsed: ConfigExport = serde_json::from_str(json).expect("parses");
        assert_eq!(parsed.hostname.as_deref(), Some("hive-a-bmc"));
        assert!(parsed.ntp_servers.is_none());
    }

    #[test]
    fn only_nodes_one_to_four_exist() {
        assert!(node_id(1).is_some());
        assert!(node_id(4).is_some());
        assert!(node_id(0).is_none());
        assert!(node_id(5).is_none());
    }
}
