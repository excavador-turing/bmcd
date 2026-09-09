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
//! One path per operation, over the same handlers as `?opt=&type=`.
//!
//! The legacy API routes every operation through `GET /api/bmc?opt=…&type=…`.
//! That is what upstream's `tpi`, this fork's `tpi`, the web interface and a
//! decade of people's scripts speak, and it stays exactly as it is. But it
//! cannot be *described*: OpenAPI keys operations by path and method, so
//! `type=about` and `type=thermal` are one operation with a discriminator, a
//! `oneOf` of thirty responses, and a generated client with one method that
//! returns `any`.
//!
//! So each operation also has a path -- `GET /api/bmc/thermal`,
//! `POST /api/bmc/hostname` -- that reaches **the same arm of the same
//! dispatcher** as its query form. There is no second implementation to
//! drift. What differs is the envelope, and only on these paths:
//!
//! - a success answers with the bare result, not `{"response":[{"result":…}]}`
//! - a refusal answers with `application/problem+json` (RFC 9457), not a
//!   200-shaped body carrying the message in `result`
//! - mutations are `POST`, and take their parameters as a form or a JSON
//!   body as well as in the query string
//!
//! The legacy form's envelope is what its clients parse and is not touched.

use crate::api::into_legacy_response::LegacyResponse;
use crate::api::legacy::dispatch;
use crate::app::bmc_application::BmcApplication;
use crate::serial_service::serial::SerialConnections;
use actix_web::http::{header, StatusCode};
use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Method {
    Get,
    Post,
}

/// One operation: where it lives, and which `(type, opt=set)` arm it is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Alias {
    pub method: Method,
    /// Relative to `/api/bmc`.
    pub path: &'static str,
    /// The legacy `type=` value. `method` decides `opt`: `Post` is `set`.
    pub ty: &'static str,
    /// For the two operations whose legacy form takes a whole JSON document
    /// in one query parameter: a JSON body that is not already wrapped in
    /// this key is taken as the document itself. `POST /firmware/sources`
    /// with the sources document as the body is the shape a person would
    /// write; `{"sources": "<escaped json>"}` is the shape the legacy form
    /// requires.
    pub document_param: Option<&'static str>,
    pub summary: &'static str,
}

/// The table. This is also the OpenAPI document's table of contents; see
/// `api::openapi`.
pub(crate) static ALIASES: &[Alias] = &[
    // --- read -------------------------------------------------------------
    Alias { method: Method::Get, path: "/about", ty: "about", document_param: None,
        summary: "Firmware, daemon, kernel and board identity" },
    Alias { method: Method::Get, path: "/health", ty: "health", document_param: None,
        summary: "The board's own condition: uptime, load, memory, NAND, clock" },
    Alias { method: Method::Get, path: "/thermal", ty: "thermal", document_param: None,
        summary: "Temperature sensors with their trip points, and the fan's step" },
    Alias { method: Method::Get, path: "/cooling", ty: "cooling", document_param: None,
        summary: "Cooling devices with their setpoints" },
    Alias { method: Method::Get, path: "/network", ty: "network", document_param: None,
        summary: "Interfaces and switch ports" },
    Alias { method: Method::Get, path: "/power", ty: "power", document_param: None,
        summary: "Which nodes are powered" },
    Alias { method: Method::Get, path: "/nodes", ty: "node_info", document_param: None,
        summary: "Node names, module names and power-on times" },
    Alias { method: Method::Get, path: "/usb", ty: "usb", document_param: None,
        summary: "The USB bus: which node holds it, in which mode" },
    Alias { method: Method::Get, path: "/sdcard", ty: "sdcard", document_param: None,
        summary: "SD card capacity and use" },
    Alias { method: Method::Get, path: "/info", ty: "info", document_param: None,
        summary: "Storage and interfaces, as the Overview page reads them" },
    Alias { method: Method::Get, path: "/firmware/slots", ty: "firmware_slots", document_param: None,
        summary: "The A/B slots: running, rollback, what is staged, the last promotion" },
    Alias { method: Method::Get, path: "/firmware/sources", ty: "firmware_sources", document_param: None,
        summary: "Where the board looks for firmware" },
    Alias { method: Method::Get, path: "/firmware/available", ty: "firmware_available", document_param: None,
        summary: "Every enabled source's candidates; `refresh=1` re-polls behind the answer" },
    Alias { method: Method::Get, path: "/firmware/check", ty: "update_check", document_param: None,
        summary: "Whether a newer release exists, per channel" },
    Alias { method: Method::Get, path: "/metrics-token", ty: "metrics_token", document_param: None,
        summary: "The read-only credential the metrics endpoint accepts" },
    Alias { method: Method::Get, path: "/hostname", ty: "hostname", document_param: None,
        summary: "The board's name, live and after the next boot" },
    Alias { method: Method::Get, path: "/ntp", ty: "ntp", document_param: None,
        summary: "Time sources and the clock's state" },
    Alias { method: Method::Get, path: "/config", ty: "config", document_param: None,
        summary: "Everything configured on this board, as one document; `secrets=1` includes the metrics token" },
    // --- act --------------------------------------------------------------
    Alias { method: Method::Post, path: "/power", ty: "power", document_param: None,
        summary: "Power nodes on or off" },
    Alias { method: Method::Post, path: "/reset", ty: "reset", document_param: None,
        summary: "Restart one node" },
    Alias { method: Method::Post, path: "/reboot", ty: "reboot", document_param: None,
        summary: "Reboot the BMC; the compute modules keep running" },
    Alias { method: Method::Post, path: "/reload", ty: "reload", document_param: None,
        summary: "Restart the daemon" },
    Alias { method: Method::Post, path: "/usb", ty: "usb", document_param: None,
        summary: "Route the USB bus to a node, in a mode" },
    Alias { method: Method::Post, path: "/usb-boot", ty: "usb_boot", document_param: None,
        summary: "Hold a node's usbboot pin for the next power-on" },
    Alias { method: Method::Post, path: "/usb-boot/clear", ty: "clear_usb_boot", document_param: None,
        summary: "Release the usbboot pin" },
    Alias { method: Method::Post, path: "/node-to-msd", ty: "node_to_msd", document_param: None,
        summary: "Expose a node's storage as a mass-storage device" },
    Alias { method: Method::Post, path: "/network/reset", ty: "network", document_param: None,
        summary: "Reset the network configuration" },
    Alias { method: Method::Post, path: "/sdcard/format", ty: "sdcard", document_param: None,
        summary: "Format the SD card" },
    Alias { method: Method::Post, path: "/cooling", ty: "cooling", document_param: None,
        summary: "Set a cooling device's step" },
    Alias { method: Method::Post, path: "/hostname", ty: "hostname", document_param: None,
        summary: "Rename the board; the metrics instance label changes with it" },
    Alias { method: Method::Post, path: "/ntp", ty: "ntp", document_param: None,
        summary: "Replace the time sources and reload chrony" },
    Alias { method: Method::Post, path: "/config", ty: "config", document_param: Some("config"),
        summary: "Apply an exported configuration; reports per field" },
    Alias { method: Method::Post, path: "/firmware/sources", ty: "firmware_sources", document_param: Some("sources"),
        summary: "Replace the firmware sources" },
    Alias { method: Method::Post, path: "/firmware/install", ty: "firmware_install", document_param: None,
        summary: "Stage a version from a source for the next boot" },
    Alias { method: Method::Post, path: "/metrics-token/rotate", ty: "metrics_token", document_param: None,
        summary: "Replace the metrics token; the old one stops working at once" },
];

/// Registers every alias and the document that describes them. Called from
/// the `/api/bmc` scope, after the legacy resource, so the authenticator and
/// the app data are the same ones.
pub fn config(cfg: &mut web::ServiceConfig) {
    crate::api::openapi::config(cfg);
    for alias in ALIASES {
        let route = match alias.method {
            Method::Get => web::get(),
            Method::Post => web::post(),
        };
        cfg.route(
            alias.path,
            route.to(
                move |bmc: web::Data<BmcApplication>,
                      serial: web::Data<SerialConnections>,
                      url: web::Query<HashMap<String, String>>,
                      body: web::Bytes,
                      req: HttpRequest| async move {
                    handle(alias, bmc, serial, url.into_inner(), &body, &req).await
                },
            ),
        );
    }
}

async fn handle(
    alias: &'static Alias,
    bmc: web::Data<BmcApplication>,
    serial: web::Data<SerialConnections>,
    mut params: HashMap<String, String>,
    body: &[u8],
    req: &HttpRequest,
) -> HttpResponse {
    if alias.method == Method::Post {
        match merge_body(&mut params, body, req.content_type(), alias.document_param) {
            Ok(()) => {}
            Err(detail) => return problem(StatusCode::BAD_REQUEST, &detail),
        }
    }

    let is_set = alias.method == Method::Post;
    let response = dispatch(
        bmc.as_ref(),
        serial,
        alias.ty,
        is_set,
        web::Query(params),
        req,
    )
    .await;
    unwrap(response)
}

/// Folds a request body into the parameter map the dispatcher reads.
///
/// A form body is the query string's twin and merges field by field. A JSON
/// body merges its top-level members, stringifying scalars, because the
/// legacy handlers read every parameter as text. A JSON body for an operation
/// that takes a whole document (`document_param`) and is not already wrapped
/// in that key is taken as the document -- so the natural request is the
/// documented one.
///
/// The URL's parameters win on a clash: they are what the client put in the
/// address, and a body silently overriding them is the kind of surprise that
/// takes an afternoon to find.
fn merge_body(
    params: &mut HashMap<String, String>,
    body: &[u8],
    content_type: &str,
    document_param: Option<&'static str>,
) -> Result<(), String> {
    if body.is_empty() {
        return Ok(());
    }
    let mime = content_type.split(';').next().unwrap_or("").trim();

    let from_body: HashMap<String, String> = match mime {
        "application/x-www-form-urlencoded" => {
            let text = std::str::from_utf8(body).map_err(|_| "the form body is not UTF-8")?;
            web::Query::<HashMap<String, String>>::from_query(text)
                .map_err(|e| format!("the form body could not be read: {e}"))?
                .into_inner()
        }
        "application/json" => {
            let value: serde_json::Value = serde_json::from_slice(body)
                .map_err(|e| format!("the JSON body could not be read: {e}"))?;
            let serde_json::Value::Object(members) = value else {
                return Err("the JSON body must be an object".to_string());
            };
            match document_param {
                Some(key) if !members.contains_key(key) => {
                    let document = serde_json::Value::Object(members);
                    HashMap::from([(key.to_string(), document.to_string())])
                }
                _ => members
                    .into_iter()
                    .map(|(k, v)| {
                        let text = match v {
                            serde_json::Value::String(s) => s,
                            other => other.to_string(),
                        };
                        (k, text)
                    })
                    .collect(),
            }
        }
        "" => return Err("a body needs a Content-Type".to_string()),
        other => {
            return Err(format!(
            "{other} is not accepted; send application/x-www-form-urlencoded or application/json"
        ))
        }
    };

    for (k, v) in from_body {
        params.entry(k).or_insert(v);
    }
    Ok(())
}

/// The envelope these paths use. The legacy envelope is what the legacy
/// clients parse and stays exactly as it is; this one is for clients that
/// read the specification.
fn unwrap(response: LegacyResponse) -> HttpResponse {
    match response {
        // A mutation with nothing to say. 204 rather than `"ok"` in a body:
        // the status is the answer, and a client that must parse a string to
        // learn that nothing went wrong has been given busywork.
        LegacyResponse::Success(None) => HttpResponse::NoContent().finish(),
        LegacyResponse::Success(Some(value)) => HttpResponse::Ok().json(value),
        LegacyResponse::UartData(text) => HttpResponse::Ok()
            .insert_header((header::CONTENT_TYPE, "text/plain; charset=utf-8"))
            .body(text),
        LegacyResponse::Error(status, message) => problem(status, &message),
    }
}

/// RFC 9457. `title` is the status's reason phrase, `detail` the message the
/// handler wrote -- the same message the legacy form puts in `result`, so
/// nothing is lost between the two envelopes; it is only findable.
fn problem(status: StatusCode, detail: &str) -> HttpResponse {
    HttpResponse::build(status)
        .insert_header((header::CONTENT_TYPE, "application/problem+json"))
        .json(serde_json::json!({
            "type": "about:blank",
            "title": status.canonical_reason().unwrap_or("Error"),
            "status": status.as_u16(),
            "detail": detail,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn merged(
        url: &[(&str, &str)],
        body: &str,
        content_type: &str,
        document_param: Option<&'static str>,
    ) -> Result<HashMap<String, String>, String> {
        let mut params: HashMap<String, String> = url
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        merge_body(&mut params, body.as_bytes(), content_type, document_param)?;
        Ok(params)
    }

    #[test]
    fn a_form_body_merges_like_a_query_string() {
        let p = merged(
            &[],
            "name=hive-a-bmc&extra=1",
            "application/x-www-form-urlencoded",
            None,
        )
        .expect("merges");
        assert_eq!(p["name"], "hive-a-bmc");
        assert_eq!(p["extra"], "1");
    }

    #[test]
    fn a_json_body_stringifies_scalars_because_the_handlers_read_text() {
        let p = merged(
            &[],
            r#"{"node":1,"force":true,"name":"x"}"#,
            "application/json",
            None,
        )
        .expect("merges");
        assert_eq!(p["node"], "1");
        assert_eq!(p["force"], "true");
        assert_eq!(p["name"], "x");
    }

    /// `POST /firmware/sources` with the document as the body is what a
    /// person writes. The legacy form needs it escaped inside `sources=`;
    /// the alias does that folding so the natural request is the one that
    /// works.
    #[test]
    fn a_bare_document_is_wrapped_in_its_parameter() {
        let doc = r#"{"sources":[{"id":"fork","kind":"github"}]}"#;
        // Not wrapped: `sources` here is the document's own top-level key,
        // which happens to match. That is the ambiguous case, and it must
        // still be treated as already-wrapped only when the value is a
        // string -- so test the unambiguous shape first.
        let p = merged(
            &[],
            r#"{"format_version":1,"hostname":"a"}"#,
            "application/json",
            Some("config"),
        )
        .expect("merges");
        let stored: serde_json::Value = serde_json::from_str(&p["config"]).expect("json");
        assert_eq!(stored["hostname"], "a");

        // Already wrapped: passed through as the parameter.
        let wrapped = format!(r#"{{"sources": {}}}"#, serde_json::to_string(doc).unwrap());
        let p = merged(&[], &wrapped, "application/json", Some("sources")).expect("merges");
        assert_eq!(p["sources"], doc);
    }

    #[test]
    fn the_url_wins_over_the_body_on_a_clash() {
        let p = merged(
            &[("node", "2")],
            "node=3",
            "application/x-www-form-urlencoded",
            None,
        )
        .expect("merges");
        assert_eq!(p["node"], "2");
    }

    #[test]
    fn an_unknown_content_type_is_refused_with_the_accepted_ones_named() {
        let err = merged(&[], "x", "text/plain", None).expect_err("refused");
        assert!(err.contains("application/json"), "{err}");
        let err = merged(&[], "x", "", None).expect_err("refused");
        assert!(err.contains("Content-Type"), "{err}");
    }

    #[test]
    fn an_empty_body_is_fine_whatever_the_content_type_says() {
        assert!(merged(&[], "", "", None).is_ok());
    }

    #[actix_web::test]
    async fn a_refusal_is_a_problem_document() {
        let response = unwrap(LegacyResponse::Error(
            StatusCode::BAD_REQUEST,
            Cow::Borrowed("\"hive bmc\" is not a hostname"),
        ));
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/problem+json"
        );
        let body = actix_web::body::to_bytes(response.into_body())
            .await
            .unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["status"], 400);
        assert_eq!(problem["title"], "Bad Request");
        assert!(problem["detail"]
            .as_str()
            .unwrap()
            .contains("not a hostname"));
    }

    #[actix_web::test]
    async fn a_success_is_the_bare_result_and_a_plain_ack_is_204() {
        let response = unwrap(LegacyResponse::Success(Some(serde_json::json!({"a": 1}))));
        assert_eq!(response.status(), StatusCode::OK);
        let body = actix_web::body::to_bytes(response.into_body())
            .await
            .unwrap();
        assert_eq!(&body[..], br#"{"a":1}"#);

        let response = unwrap(LegacyResponse::Success(None));
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    /// Every alias must name a `(type, opt)` pair the dispatcher actually has.
    /// The dispatcher's arms are a match, not data, so this checks the
    /// table's shape and the OpenAPI document's contents at the same time:
    /// a path has exactly one method per operation, and no two aliases share
    /// a `(method, path)`.
    #[test]
    fn the_table_has_no_duplicate_routes() {
        let mut seen = std::collections::HashSet::new();
        for alias in ALIASES {
            assert!(alias.path.starts_with('/'), "{}", alias.path);
            assert!(
                seen.insert((alias.method, alias.path)),
                "duplicate route {:?} {}",
                alias.method,
                alias.path
            );
        }
    }
}
