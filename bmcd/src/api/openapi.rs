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
        ("config", Method::Get) => &[("secrets", "`1` includes the metrics token, which makes the document a credential")],
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

/// The document. Regenerated on every request; it is a few kilobytes and
/// the alternative is a cache that outlives a version bump.
pub fn document() -> Value {
    let mut paths = serde_json::Map::new();

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
                    "description": "The result. Its shape is not yet described in this document; see the tool that consumes it.",
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

        let entry = paths
            .entry(format!("/api/bmc{}", alias.path))
            .or_insert_with(|| json!({}));
        entry[method] = operation;
    }

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
            "schemas": {
                "Problem": {
                    "type": "object",
                    "description": "RFC 9457. `detail` is the same message the legacy form puts in `result`.",
                    "properties": {
                        "type": { "type": "string" },
                        "title": { "type": "string" },
                        "status": { "type": "integer" },
                        "detail": { "type": "string" }
                    },
                    "required": ["title", "status"]
                }
            }
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
}
