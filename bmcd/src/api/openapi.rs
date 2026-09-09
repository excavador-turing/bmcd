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
//! The API, described: OpenAPI 3.1 at `GET /api/bmc/openapi.json`.
//!
//! Built from `paths::ALIASES` -- the same table that registers the routes --
//! so an operation cannot be documented without existing, or exist on a path
//! without being documented. The daemon serves its own description so the
//! document always matches the board you are on, not the site you read.
//!
//! What this release describes: every operation's path, method, summary, the
//! parameters that are known, and the error shape. Response bodies are still
//! `{}`: typing them is the next step (schemas derived from the serde types
//! the handlers already return), and an untyped-but-honest response is better
//! than a typed one guessed from memory -- the day before this was written, a
//! client shipped three formatters against shapes the daemon has never sent.

use crate::api::paths::{Alias, Method, ALIASES};
use actix_web::{web, HttpResponse};
use serde_json::{json, Value};

/// Parameters this release knows for an operation, by legacy `type` and
/// method. Anything not listed here is still accepted by the handler; the
/// spec marks every operation `additionalProperties: true` for that reason.
fn known_params(alias: &Alias) -> &'static [(&'static str, &'static str)] {
    match (alias.ty, alias.method) {
        ("firmware_available", Method::Get) => &[(
            "refresh",
            "`1` starts a re-poll of every source behind the answer; the answer itself is what was cached",
        )],
        ("reset", Method::Post) => &[("node", "1-4")],
        ("usb", Method::Post) => &[
            ("node", "1-4: which node the bus is routed to"),
            ("mode", "0 host, 1 device, 2 flash"),
        ],
        ("usb_boot", Method::Post) | ("node_to_msd", Method::Post) => &[("node", "1-4")],
        ("cooling", Method::Post) => &[("device", "as reported by GET /cooling"), ("speed", "a step, 0 to max_speed")],
        ("hostname", Method::Post) => &[("name", "one DNS label: letters, digits, hyphens; no dots")],
        ("ntp", Method::Post) => &[("servers", "comma-separated, in preference order; empty restores the image's pool")],
        ("config", Method::Post) => &[("config", "the exported document; a JSON body that is the document itself is accepted as-is")],
        ("firmware_sources", Method::Post) => &[("sources", "the sources document; a JSON body that is the document itself is accepted as-is")],
        ("firmware_install", Method::Post) => &[
            ("source", "a source id from GET /firmware/sources"),
            ("version", "a version that source offers"),
            ("force", "`1` replaces an image that is already staged"),
            ("allow_downgrade", "`1` for a version that is not newer than the running one"),
        ],
        ("power", Method::Post) => &[
            ("node1", "0 or 1"),
            ("node2", "0 or 1"),
            ("node3", "0 or 1"),
            ("node4", "0 or 1"),
        ],
        _ => &[],
    }
}

/// Response schemas, derived from the very types the handlers serialise.
///
/// Deriving rather than writing them out is the whole point of this. The
/// failure being fixed is a client written against a shape the daemon has
/// never sent -- `tpi` shipped three such formatters -- and a hand-written
/// schema is that same failure with an extra step between it and the reader.
///
/// Not every operation is here. Several handlers assemble their answer with
/// `json!` out of several sources and have no single type to derive from;
/// those keep an untyped `200` whose description says so, and
/// [`UNTYPED`] lists them, so that leaving a new one undescribed is a
/// deliberate act rather than an omission.
fn response_schemas() -> Vec<(&'static str, Value)> {
    // `$ref` to a component, for an operation that answers with exactly one
    // type; an inline shape for the two that wrap one.
    vec![
        ("/thermal", component_ref("Thermal")),
        ("/health", component_ref("Health")),
        (
            "/cooling",
            json!({ "type": "array", "items": component_ref("CoolingDevice") }),
        ),
        (
            "/network",
            json!({
                "type": "object",
                "properties": { "ports": { "type": "array", "items": component_ref("SwitchPort") } },
                "required": ["ports"]
            }),
        ),
        ("/firmware/slots", component_ref("FirmwareSlots")),
        ("/firmware/sources", component_ref("Sources")),
        ("/firmware/check", component_ref("UpdateCheck")),
        ("/firmware/available", component_ref("Catalog")),
        ("/about", component_ref("About")),
        ("/info", component_ref("BoardInfo")),
        ("/hostname", component_ref("Hostname")),
        ("/ntp", component_ref("Ntp")),
        ("/config", component_ref("ConfigExport")),
        // Four nodes, always. The daemon's own type is `[NodeInfo; 4]`, and
        // saying so lets a generated client index it without a length check
        // that can never fail.
        (
            "/nodes",
            json!({
                "type": "array", "items": component_ref("NodeInfo"),
                "minItems": 4, "maxItems": 4
            }),
        ),
        // Upstream's single-element arrays, described as they are rather than
        // flattened. A schema claiming an object here would be a lie that
        // compiles, and a client has to index them either way.
        ("/power", one_of_array("NodePower")),
        ("/usb", one_of_array("UsbState")),
        ("/sdcard", one_of_array("SdCard")),
    ]
}

/// An array that always holds exactly one object, which is upstream's shape
/// for `power`, `usb` and `sdcard`.
fn one_of_array(name: &str) -> Value {
    json!({
        "type": "array", "items": component_ref(name),
        "minItems": 1, "maxItems": 1
    })
}

/// The operations whose answer this document does not describe.
///
/// Each of these handlers builds its response with `json!` from more than one
/// source, so there is no type to derive a schema from. Listed rather than
/// left implicit: a test requires every GET alias to be either described or
/// named here, which is what stops the list quietly growing.
/// Empty, and meant to stay that way.
///
/// Ten operations sat here until SQU-177: their handlers assembled an answer
/// with `json!` from several sources, so there was no type to derive a schema
/// from. Each now has one, reconstructed from what the handler already sent.
///
/// The list survives because the test that reads it is what stops a new
/// undescribed operation appearing quietly. Adding a path here is a
/// deliberate act with a reason attached; leaving one out fails the build.
const UNTYPED: &[&str] = &[];

fn component_ref(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{}", name) })
}

/// Every named type the schemas above refer to, ready for `components`.
///
/// `schema_for!` emits the root type plus a `$defs` of everything it nests,
/// and refs of the form `#/$defs/Name`. OpenAPI keeps its schemas at
/// `#/components/schemas/`, so both the definitions and every `$ref` are
/// moved across.
fn components() -> serde_json::Map<String, Value> {
    let roots = [
        (
            "Thermal",
            serde_json::to_value(schemars::schema_for!(crate::app::thermal_info::Thermal)),
        ),
        (
            "Health",
            serde_json::to_value(schemars::schema_for!(crate::app::health_info::Health)),
        ),
        (
            "CoolingDevice",
            serde_json::to_value(schemars::schema_for!(
                crate::app::cooling_device::CoolingDevice
            )),
        ),
        (
            "SwitchPort",
            serde_json::to_value(schemars::schema_for!(crate::app::switch_info::SwitchPort)),
        ),
        (
            "FirmwareSlots",
            serde_json::to_value(schemars::schema_for!(
                crate::app::firmware_info::FirmwareSlots
            )),
        ),
        (
            "Sources",
            serde_json::to_value(schemars::schema_for!(crate::app::firmware_sources::Sources)),
        ),
        (
            "UpdateCheck",
            serde_json::to_value(schemars::schema_for!(crate::app::update_check::UpdateCheck)),
        ),
        (
            "Catalog",
            serde_json::to_value(schemars::schema_for!(crate::app::firmware_catalog::Catalog)),
        ),
        (
            "ConfigExport",
            serde_json::to_value(schemars::schema_for!(
                crate::app::config_export::ConfigExport
            )),
        ),
        (
            "NodeInfo",
            serde_json::to_value(schemars::schema_for!(crate::app::bmc_application::NodeInfo)),
        ),
        (
            "About",
            serde_json::to_value(schemars::schema_for!(crate::api::responses::About)),
        ),
        (
            "BoardInfo",
            serde_json::to_value(schemars::schema_for!(crate::api::responses::BoardInfo)),
        ),
        (
            "Hostname",
            serde_json::to_value(schemars::schema_for!(crate::api::responses::Hostname)),
        ),
        (
            "Ntp",
            serde_json::to_value(schemars::schema_for!(crate::api::responses::Ntp)),
        ),
        (
            "NodePower",
            serde_json::to_value(schemars::schema_for!(crate::api::responses::NodePower)),
        ),
        (
            "UsbState",
            serde_json::to_value(schemars::schema_for!(crate::api::responses::UsbState)),
        ),
        (
            "SdCard",
            serde_json::to_value(schemars::schema_for!(crate::api::responses::SdCard)),
        ),
    ];

    let mut out = serde_json::Map::new();
    for (name, schema) in roots {
        let mut schema = schema.expect("a generated schema is valid JSON");
        // The nested types come along as $defs; they become components too.
        if let Some(Value::Object(defs)) = schema.as_object_mut().and_then(|o| o.remove("$defs")) {
            for (def_name, def) in defs {
                out.entry(def_name).or_insert(retarget(def));
            }
        }
        if let Some(object) = schema.as_object_mut() {
            // Meta-keys that mean nothing inside an OpenAPI components entry.
            object.remove("$schema");
            object.remove("title");
        }
        out.insert(name.to_string(), retarget(schema));
    }
    out
}

/// Rewrites `#/$defs/X` to `#/components/schemas/X`, everywhere it appears.
fn retarget(node: Value) -> Value {
    match node {
        Value::String(s) => Value::String(s.replace("#/$defs/", "#/components/schemas/")),
        Value::Array(items) => Value::Array(items.into_iter().map(retarget).collect()),
        Value::Object(fields) => {
            Value::Object(fields.into_iter().map(|(k, v)| (k, retarget(v))).collect())
        }
        other => other,
    }
}

/// The document. Regenerated on every request; it is a few kilobytes and
/// the alternative is a cache that outlives a version bump.
pub fn document() -> Value {
    let mut paths = serde_json::Map::new();
    let schemas = response_schemas();

    for alias in ALIASES {
        let method = match alias.method {
            Method::Get => "get",
            Method::Post => "post",
        };
        let params = known_params(alias);

        let mut operation = json!({
            "summary": alias.summary,
            "operationId": operation_id(alias),
            "description": format!(
                "Legacy form: `GET /api/bmc?opt={}&type={}` with the same parameters in the query string. \
                 That form answers `{{\"response\":[{{\"result\":…}}]}}` and puts a refusal's message in `result`; \
                 this path answers the bare result and a refusal as `application/problem+json`.",
                if alias.method == Method::Post { "set" } else { "get" },
                alias.ty
            ),
            "responses": {
                "200": {
                    "description": "The result.",
                    "content": { "application/json": { "schema": {} } }
                },
                "204": { "description": "Done, with nothing to report." },
                "default": {
                    "description": "A refusal, RFC 9457.",
                    "content": { "application/problem+json": { "schema": { "$ref": "#/components/schemas/Problem" } } }
                }
            }
        });

        if alias.method == Method::Get {
            let query: Vec<Value> = params
                .iter()
                .map(|(name, description)| {
                    json!({ "name": name, "in": "query", "required": false,
                            "description": description, "schema": { "type": "string" } })
                })
                .collect();
            if !query.is_empty() {
                operation["parameters"] = Value::Array(query);
            }
        } else {
            let properties: serde_json::Map<String, Value> = params
                .iter()
                .map(|(name, description)| {
                    (
                        name.to_string(),
                        json!({ "type": "string", "description": description }),
                    )
                })
                .collect();
            operation["requestBody"] = json!({
                "required": false,
                "description": "Parameters may also be given in the query string; the query string wins on a clash.",
                "content": {
                    "application/x-www-form-urlencoded": {
                        "schema": { "type": "object", "properties": properties, "additionalProperties": true }
                    },
                    "application/json": {
                        "schema": { "type": "object", "properties": properties, "additionalProperties": true }
                    }
                }
            });
        }

        match schemas.iter().find(|(path, _)| *path == alias.path) {
            Some((_, schema)) if alias.method == Method::Get => {
                operation["responses"]["200"]["content"]["application/json"]["schema"] =
                    schema.clone();
            }
            _ if UNTYPED.contains(&alias.path) => {
                operation["responses"]["200"]["description"] = json!(
                    "The result. Its shape is not described here: this handler assembles \
                     its answer from several sources rather than serialising one type."
                );
            }
            _ => {
                operation["responses"]["200"]["description"] =
                    json!("The result. Its shape is not described in this document.");
            }
        }

        let entry = paths
            .entry(format!("/api/bmc{}", alias.path))
            .or_insert_with(|| json!({}));
        entry[method] = operation;
    }

    let mut schema_components = components();
    schema_components.insert(
        "Problem".to_string(),
        json!({
            "type": "object",
            "description": "RFC 9457. `detail` is the same message the legacy form puts in `result`.",
            "properties": {
                "type": { "type": "string" },
                "title": { "type": "string" },
                "status": { "type": "integer" },
                "detail": { "type": "string" }
            },
            "required": ["title", "status"]
        }),
    );

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Turing Pi 2 BMC",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "The board management controller of a Turing Pi 2, as built by the excavador-turing fork. \
                            Every operation here is also reachable in the legacy form `GET /api/bmc?opt=&type=`, \
                            which older clients speak and which is not going away.",
            "contact": { "name": "excavador-turing", "url": "https://github.com/excavador-turing" },
            "license": { "name": "Apache-2.0" }
        },
        "servers": [{ "url": "/", "description": "This board" }],
        "security": [{ "bearer": [] }, { "basic": [] }],
        "paths": Value::Object(paths),
        "components": {
            "securitySchemes": {
                "bearer": { "type": "http", "scheme": "bearer",
                            "description": "A token from POST /api/bmc/authenticate." },
                "basic": { "type": "http", "scheme": "basic",
                           "description": "The board's root credentials. Loopback on the board itself needs neither." }
            },
            "schemas": Value::Object(schema_components)
        }
    })
}

/// `getThermal`, `postFirmwareInstall`: the path, camel-cased, after the
/// method. Stable across releases as long as the path is, which is the
/// promise a generated client needs.
fn operation_id(alias: &Alias) -> String {
    let mut out = String::from(match alias.method {
        Method::Get => "get",
        Method::Post => "post",
    });
    let mut upper = true;
    for c in alias.path.chars() {
        if c == '/' || c == '-' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

pub async fn serve() -> HttpResponse {
    HttpResponse::Ok().json(document())
}

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.route("/openapi.json", web::get().to(serve));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_alias_is_an_operation_and_every_operation_is_an_alias() {
        let doc = document();
        let paths = doc["paths"].as_object().expect("paths");
        let mut operations = 0;
        for (_, methods) in paths {
            operations += methods.as_object().expect("methods").len();
        }
        assert_eq!(operations, ALIASES.len());
    }

    #[test]
    fn operation_ids_are_unique_and_shaped_for_a_generator() {
        let mut seen = std::collections::HashSet::new();
        for alias in ALIASES {
            let id = operation_id(alias);
            assert!(id.chars().all(|c| c.is_ascii_alphanumeric()), "{id}");
            assert!(seen.insert(id.clone()), "duplicate operationId {id}");
        }
        assert_eq!(
            operation_id(&Alias {
                method: Method::Post,
                path: "/firmware/install",
                ty: "",
                document_param: None,
                summary: ""
            }),
            "postFirmwareInstall"
        );
        assert_eq!(
            operation_id(&Alias {
                method: Method::Get,
                path: "/metrics-token",
                ty: "",
                document_param: None,
                summary: ""
            }),
            "getMetricsToken"
        );
    }

    #[test]
    fn a_refusal_is_described_on_every_operation() {
        let doc = document();
        for (_, methods) in doc["paths"].as_object().unwrap() {
            for (_, op) in methods.as_object().unwrap() {
                assert_eq!(
                    op["responses"]["default"]["content"]["application/problem+json"]["schema"]
                        ["$ref"],
                    "#/components/schemas/Problem"
                );
            }
        }
    }

    #[test]
    fn the_document_is_openapi_3_1() {
        assert_eq!(document()["openapi"], "3.1.0");
    }

    /// The one that stops the undescribed list growing by accident.
    ///
    /// Every read operation is either described by a schema or named in
    /// `UNTYPED`. Adding an endpoint and saying nothing about what it answers
    /// with fails here, which is the only moment anybody is thinking about it.
    #[test]
    fn every_read_operation_is_described_or_declared_undescribed() {
        let described: Vec<&str> = response_schemas().iter().map(|(path, _)| *path).collect();

        let missing: Vec<&str> = ALIASES
            .iter()
            .filter(|alias| alias.method == Method::Get)
            .map(|alias| alias.path)
            .filter(|path| !described.contains(path) && !UNTYPED.contains(path))
            .collect();

        assert!(
            missing.is_empty(),
            "these read operations describe no response and are not in UNTYPED: {:?}",
            missing
        );
    }

    /// `UNTYPED` must not name something that is described, or an operation
    /// that no longer exists -- either way it is a stale claim about the API.
    #[test]
    fn nothing_is_both_described_and_declared_undescribed() {
        let described: Vec<&str> = response_schemas().iter().map(|(path, _)| *path).collect();
        let read_paths: Vec<&str> = ALIASES
            .iter()
            .filter(|alias| alias.method == Method::Get)
            .map(|alias| alias.path)
            .collect();

        for path in UNTYPED {
            assert!(
                !described.contains(path),
                "{path} is described and also listed as undescribed"
            );
            assert!(
                read_paths.contains(path),
                "{path} is listed as undescribed and is not a read operation"
            );
        }
    }

    /// A `$ref` that points at nothing renders as an empty box in every
    /// viewer and generates a client that will not compile.
    #[test]
    fn every_ref_resolves_to_a_component() {
        let doc = document();
        let components = doc["components"]["schemas"]
            .as_object()
            .expect("components.schemas is an object");

        let mut refs = Vec::new();
        collect_refs(&doc, &mut refs);
        assert!(!refs.is_empty(), "the document carries no $ref at all");

        for reference in refs {
            let name = reference
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("$ref outside components: {reference}"));
            assert!(
                components.contains_key(name),
                "$ref names a component that does not exist: {reference}"
            );
        }
    }

    fn collect_refs(node: &Value, out: &mut Vec<String>) {
        match node {
            Value::Object(fields) => {
                for (key, value) in fields {
                    if key == "$ref" {
                        if let Some(text) = value.as_str() {
                            out.push(text.to_string());
                        }
                    } else {
                        collect_refs(value, out);
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| collect_refs(item, out)),
            _ => {}
        }
    }

    /// The document's schemas, resolvable as one JSON Schema.
    ///
    /// `$ref`s point at `#/components/schemas/…`, so the whole `components`
    /// object is handed to the validator as the document root and the schema
    /// under test is referenced into it.
    fn validator_for(component: &str) -> (Value, jsonschema::Validator) {
        let doc = document();
        let root = json!({
            "$ref": format!("#/components/schemas/{component}"),
            "components": doc["components"].clone(),
        });
        let validator = jsonschema::validator_for(&root)
            .unwrap_or_else(|e| panic!("the published schema for {component} is not valid: {e}"));
        (root, validator)
    }

    fn assert_valid(component: &str, instance: &Value) {
        let (_, validator) = validator_for(component);
        let errors: Vec<String> = validator
            .iter_errors(instance)
            .map(|e| e.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "what the daemon serialises does not match the {component} schema it publishes:\n  {}\ninstance: {}",
            errors.join("\n  "),
            serde_json::to_string_pretty(instance).unwrap()
        );
    }

    /// The check this whole ticket is for.
    ///
    /// A schema is derived from a type; a response is serialised from the
    /// same type. Those two agree only as far as `schemars` and `serde` agree,
    /// and they part company over exactly the attributes this daemon uses:
    /// `skip_serializing_if`, `rename_all`, and serde `default`s. Where they
    /// part, the published document describes a shape the board never sends —
    /// which is the failure `tpi`'s three dead formatters were made of.
    ///
    /// So: build what the board would send, and hold it against what the
    /// document promises.
    #[test]
    fn a_board_with_no_promotion_log_still_matches_the_firmware_slots_schema() {
        // `promotion_history` carries skip_serializing_if, so this instance
        // omits the field entirely. A schema that lists it as required is
        // wrong about every board that has never taken an OTA update.
        let slots = crate::app::firmware_info::FirmwareSlots {
            present: true,
            running: Some(crate::app::firmware_info::Slot {
                volume: "rootfs".to_string(),
                volume_id: 0,
                size_bytes: Some(37019648),
                version: Some("v2.14.0".to_string()),
            }),
            rollback: Some(crate::app::firmware_info::Slot {
                volume: "rootfs_prev".to_string(),
                volume_id: 1,
                size_bytes: Some(37011456),
                version: None,
            }),
            update_staged: Some(false),
            nextboot: None,
            last_promotion: None,
            promotion_history: None,
            staged: None,
        };

        let sent = serde_json::to_value(&slots).unwrap();
        assert!(
            sent.get("promotion_history").is_none(),
            "the fixture no longer exercises the omitted field this test exists for"
        );
        assert_valid("FirmwareSlots", &sent);
    }

    /// The same type with everything present, so the schema is not merely
    /// permissive about absence.
    #[test]
    fn a_board_mid_update_matches_the_firmware_slots_schema() {
        let slots = crate::app::firmware_info::FirmwareSlots {
            present: true,
            running: None,
            rollback: None,
            update_staged: Some(true),
            nextboot: Some("ubi0:rootfs_new".to_string()),
            last_promotion: Some(crate::app::firmware_info::Promotion {
                timestamp: "23:29:57".to_string(),
                message: "promoted".to_string(),
            }),
            promotion_history: Some(crate::app::firmware_info::PromotionHistory {
                attempts: 15,
                rolled_back: 1,
                promoted: 14,
            }),
            staged: Some(crate::app::firmware_info::StagedImage {
                version: Some("v2.14.0".to_string()),
                sha256: Some("551f68b0".to_string()),
                staged_at: Some("2026-09-09T15:04:57Z".to_string()),
                source: Some("upload".to_string()),
                file: None,
            }),
        };
        assert_valid("FirmwareSlots", &serde_json::to_value(&slots).unwrap());
    }

    /// `PortKind` is `rename_all = "lowercase"`. A schema that spells the
    /// variants `Node`/`Uplink` describes a board that does not exist.
    #[test]
    fn a_switch_port_matches_its_schema_including_the_renamed_enum() {
        let port = crate::app::switch_info::SwitchPort {
            name: "node1".to_string(),
            kind: crate::app::switch_info::PortKind::Node,
            present: true,
            link: Some(true),
            operstate: Some("up".to_string()),
            speed_mbps: Some(1000),
            duplex: Some("full".to_string()),
            rx_bytes: Some(1234),
            tx_bytes: Some(5678),
            rx_errors: Some(0),
            tx_errors: Some(0),
        };

        let sent = serde_json::to_value(&port).unwrap();
        assert_eq!(
            sent["kind"], "node",
            "the fixture no longer exercises the rename"
        );
        assert_valid("SwitchPort", &sent);
    }

    /// A port the driver never probed: every optional field absent at once.
    #[test]
    fn an_unprobed_switch_port_matches_its_schema() {
        let port = crate::app::switch_info::SwitchPort {
            name: "node4".to_string(),
            kind: crate::app::switch_info::PortKind::Uplink,
            present: false,
            link: None,
            operstate: None,
            speed_mbps: None,
            duplex: None,
            rx_bytes: None,
            tx_bytes: None,
            rx_errors: None,
            tx_errors: None,
        };
        assert_valid("SwitchPort", &serde_json::to_value(&port).unwrap());
    }

    /// A held fan and a governed one, against the schema `/cooling` publishes.
    #[test]
    fn a_cooling_device_matches_its_schema_held_or_not() {
        for overridden in [false, true] {
            let device = crate::app::cooling_device::CoolingDevice {
                device: "system fan".to_string(),
                speed: 4,
                max_speed: 6,
                zone: Some("thermal_zone0".to_string()),
                overridden,
            };
            assert_valid("CoolingDevice", &serde_json::to_value(&device).unwrap());
        }
    }

    /// `use` is a Rust keyword, so the field is `used` and renamed on the
    /// wire. A schema that published `used` would describe a key the board
    /// has never sent -- and this is the exact class of mistake the contract
    /// tests exist for.
    #[test]
    fn the_sdcard_schema_uses_the_wire_name_not_the_rust_name() {
        let card = crate::api::responses::SdCard {
            total: 31_914_983_424,
            used: 1_234_567_890,
            free: 30_680_415_534,
        };

        let sent = serde_json::to_value(&card).unwrap();
        assert!(
            sent.get("use").is_some(),
            "the fixture no longer exercises the rename"
        );
        assert!(sent.get("used").is_none());
        assert_valid("SdCard", &sent);
    }

    /// A board whose EEPROM has no serial. Every other field on `About` is a
    /// plain string that falls back to "unknown", so this is the only one
    /// that can be absent and the only one worth a case of its own.
    #[test]
    fn about_matches_its_schema_with_and_without_a_serial() {
        for serial in [Some("XZCT250200139".to_string()), None] {
            let about = crate::api::responses::About {
                board_model: "Turing Pi 2".to_string(),
                board_revision: "2.5.2".to_string(),
                board_serial: serial,
                hostname: "hive-bmc".to_string(),
                api: "1.1".to_string(),
                version: "v2.15.0".to_string(),
                bmcd_version: "2.28.0".to_string(),
                build_version: "2.28.0".to_string(),
                buildtime: "2026-09-09 17:00:00-00:00".to_string(),
                buildroot: "2025.02.17".to_string(),
                kernel: "6.12.109".to_string(),
            };
            assert_valid("About", &serde_json::to_value(&about).unwrap());
        }
    }

    /// The board reports `"Unknown"` for a rail it could not read, so these
    /// are strings and not booleans. A schema saying boolean would generate a
    /// client that cannot represent the third case.
    #[test]
    fn node_power_is_strings_because_a_rail_can_be_unreadable() {
        let power = crate::api::responses::NodePower {
            node1: "1".to_string(),
            node2: "0".to_string(),
            node3: "Unknown".to_string(),
            node4: "1".to_string(),
        };
        assert_valid("NodePower", &serde_json::to_value(&power).unwrap());
    }

    /// Both halves absent: a board mid-rename whose /etc/hostname is missing
    /// and whose live name could not be read.
    #[test]
    fn hostname_matches_its_schema_when_neither_half_is_readable() {
        for (live, next) in [
            (Some("hive-bmc".to_string()), Some("hive-a-bmc".to_string())),
            (None, None),
        ] {
            let hostname = crate::api::responses::Hostname {
                hostname: live,
                on_next_boot: next,
            };
            assert_valid("Hostname", &serde_json::to_value(&hostname).unwrap());
        }
    }

    /// The schemas are derived, so this checks the derivation reached the
    /// board's own fields rather than producing an empty object.
    #[test]
    fn the_thermal_schema_carries_the_fields_the_daemon_sends() {
        let doc = document();
        let thermal = &doc["components"]["schemas"]["Thermal"];

        for field in ["sensors", "cooling"] {
            assert!(
                !thermal["properties"][field].is_null(),
                "Thermal's schema is missing `{field}`; the derive is not reaching the type"
            );
        }

        let cooler = &doc["components"]["schemas"]["Cooler"];
        for field in ["cur_state", "max_state", "levels", "max_level"] {
            assert!(
                !cooler["properties"][field].is_null(),
                "Cooler's schema is missing `{field}`"
            );
        }
    }
}
