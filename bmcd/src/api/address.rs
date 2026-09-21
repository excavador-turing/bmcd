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
//! `/api/bmc/network/address`: the board's own address, applied then
//! confirmed over the new one.
//!
//! The same four verbs as the switch, and the same rule about where a
//! confirmation may come from -- see [`super::network`] for why a
//! confirmation over loopback is refused. It proves nothing here either: a
//! request from a shell on the board never used the address at all.
use crate::api::into_legacy_response::LegacyResponse;
use crate::app::address_change::{DEFAULT_WINDOW, MAX_WINDOW, MIN_WINDOW};
use crate::app::address_document::{AddressDocument, Refusal, Warning, MAX_PREFIX, MIN_PREFIX};
use crate::app::address_service::{AddressError, AddressService};
use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/network/address")
            .route("", web::get().to(get_address))
            .route("", web::put().to(put_address))
            .route("/confirm", web::post().to(post_confirm))
            .route("/revert", web::post().to(post_revert))
            .route("/validate", web::post().to(post_validate))
            .route("/limits", web::get().to(get_limits)),
    );
}

#[derive(Debug, Deserialize)]
struct ApplyRequest {
    #[serde(flatten)]
    document: AddressDocument,
    window_s: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ConfirmRequest {
    token: String,
}

#[derive(Debug, Serialize)]
struct Verdict {
    document: AddressDocument,
    refusal: Option<Refusal>,
    warnings: Vec<Warning>,
}

/// What a client may ask for, so it never hard-codes a bound.
#[derive(Debug, Serialize)]
struct Limits {
    prefix_min: u8,
    prefix_max: u8,
    window_default_s: u64,
    window_min_s: u64,
    window_max_s: u64,
}

const CONFIRM_FROM_THE_BOARD: &str =
    "A confirmation has to arrive at the address you just set; that is what it proves. This \
     one came from the board itself, which never used the address at all. Confirm from the \
     interface, or from `tpi` on another machine.";

fn actor_name(request: &HttpRequest) -> String {
    use crate::authentication::authentication_context::Actor;
    match request.extensions().get::<Actor>() {
        Some(Actor::User { name, .. }) => name.clone(),
        Some(Actor::Loopback) => "loopback".to_string(),
        _ => "an unknown actor".to_string(),
    }
}

fn from_the_board_itself(request: &HttpRequest) -> bool {
    use crate::authentication::authentication_context::Actor;
    matches!(request.extensions().get::<Actor>(), Some(Actor::Loopback))
}

fn refuse(e: AddressError) -> LegacyResponse {
    match e {
        AddressError::Change(_) => LegacyResponse::bad_request(e.to_string()),
        other => LegacyResponse::from(anyhow::anyhow!("{other}")),
    }
}

async fn get_address(address: web::Data<AddressService>) -> HttpResponse {
    HttpResponse::Ok().json(address.view().await)
}

async fn get_limits() -> HttpResponse {
    HttpResponse::Ok().json(Limits {
        prefix_min: MIN_PREFIX,
        prefix_max: MAX_PREFIX,
        window_default_s: DEFAULT_WINDOW.as_secs(),
        window_min_s: MIN_WINDOW.as_secs(),
        window_max_s: MAX_WINDOW.as_secs(),
    })
}

/// Ask the board what it thinks of a document, without applying it.
async fn post_validate(body: web::Json<AddressDocument>) -> HttpResponse {
    let document = body.into_inner();
    HttpResponse::Ok().json(Verdict {
        refusal: document.refusal(),
        warnings: document.warnings(),
        document,
    })
}

/// Apply an address, and start waiting to be proved right. `202`, not 200.
async fn put_address(
    request: HttpRequest,
    address: web::Data<AddressService>,
    body: web::Json<ApplyRequest>,
) -> Result<HttpResponse, LegacyResponse> {
    let body = body.into_inner();
    if let Some(refusal) = body.document.refusal() {
        return Err(LegacyResponse::bad_request(refusal.reason));
    }
    let window = body
        .window_s
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_WINDOW);
    let pending = address.apply(body.document, window).await.map_err(refuse)?;
    tracing::info!(
        "address applied by {}, awaiting confirmation within {}s",
        actor_name(&request),
        pending.window_s
    );
    Ok(HttpResponse::Accepted().json(pending))
}

async fn post_confirm(
    request: HttpRequest,
    address: web::Data<AddressService>,
    body: web::Json<ConfirmRequest>,
) -> Result<HttpResponse, LegacyResponse> {
    if from_the_board_itself(&request) {
        tracing::warn!("an address confirmation arrived over loopback and was refused");
        return Err(LegacyResponse::forbidden(CONFIRM_FROM_THE_BOARD));
    }
    address
        .confirm(&body.into_inner().token)
        .await
        .map_err(refuse)?;
    tracing::info!("address confirmed by {}", actor_name(&request));
    Ok(HttpResponse::Ok().json(address.view().await))
}

/// Open to loopback, like the switch's: this is the direction that cannot
/// strand anybody, and a console is where you would reach for it.
async fn post_revert(
    request: HttpRequest,
    address: web::Data<AddressService>,
) -> Result<HttpResponse, LegacyResponse> {
    address.revert().await.map_err(refuse)?;
    tracing::info!("address change reverted by {}", actor_name(&request));
    Ok(HttpResponse::Ok().json(address.view().await))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authentication::authentication_context::Actor;
    use actix_web::http::StatusCode;
    use actix_web::test::TestRequest;

    fn request_from(actor: Option<Actor>) -> HttpRequest {
        let request = TestRequest::default().to_http_request();
        if let Some(actor) = actor {
            request.extensions_mut().insert(actor);
        }
        request
    }

    fn service() -> web::Data<AddressService> {
        let dir = tempdir::TempDir::new("address-confirm").expect("temp dir");
        web::Data::new(AddressService::load(dir.path().join("interfaces")))
    }

    #[actix_web::test]
    async fn a_confirmation_from_the_board_itself_is_refused() {
        let error = post_confirm(
            request_from(Some(Actor::Loopback)),
            service(),
            web::Json(ConfirmRequest {
                token: "anything".into(),
            }),
        )
        .await
        .expect_err("loopback must be refused");
        let LegacyResponse::Error(status, message) = error else {
            panic!("a refusal, not a success");
        };
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(message.contains("came from the board itself"), "{message}");
        assert!(message.contains("another machine"), "{message}");
    }

    #[actix_web::test]
    async fn a_confirmation_over_the_network_reaches_the_state_machine() {
        let user = Actor::User {
            name: "oleg".into(),
            scheme: "session",
        };
        let error = post_confirm(
            request_from(Some(user)),
            service(),
            web::Json(ConfirmRequest {
                token: "anything".into(),
            }),
        )
        .await
        .expect_err("nothing is pending");
        let LegacyResponse::Error(status, message) = error else {
            panic!("a refusal, not a success");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(message.contains("no address change waiting"), "{message}");
    }

    #[actix_web::test]
    async fn validate_answers_with_the_refusal_rather_than_refusing() {
        let doc: AddressDocument = serde_json::from_str(
            r#"{"mode":"static","address":"192.168.1.20","prefix":24,"gateway":"10.0.0.1"}"#,
        )
        .unwrap();
        let response = post_validate(web::Json(doc)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = actix_web::body::to_bytes(response.into_body()).await.unwrap();
        let verdict: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(verdict["refusal"]["reason"]
            .as_str()
            .unwrap()
            .contains("not on 192.168.1.0/24"));
    }

    #[actix_web::test]
    async fn the_limits_are_published() {
        let response = get_limits().await;
        let body = actix_web::body::to_bytes(response.into_body()).await.unwrap();
        let limits: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(limits["window_default_s"], 30);
        assert_eq!(limits["prefix_max"], 30);
    }
}
