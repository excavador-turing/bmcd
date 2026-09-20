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

//! The on-board switch, as something a client can read and check.
//!
//! Two endpoints, and neither changes anything. They exist before the ones
//! that do, on purpose.
//!
//! `presets` is the daemon expanding its own presets. An interface that
//! expanded `Split` itself would eventually disagree with the board about
//! what `Split` means, and the way that disagreement shows up is a board
//! nobody can reach. So the board is the only thing that decides, and every
//! client previews the same table.
//!
//! `validate` is the same rules the apply path will run, offered before
//! anything is applied, so an interface can grey out a button and say why
//! instead of letting somebody press it and be refused. The rules live in
//! [`crate::app::switch_document`]; this is the way to ask them a question.

use crate::api::into_legacy_response::LegacyResponse;
use crate::app::switch_change::{DEFAULT_WINDOW, MAX_WINDOW, MIN_WINDOW};
use crate::app::switch_document::{
    Preset, Refusal, SecondUplink, SwitchDocument, Warning, SPLIT_MANAGEMENT_VID, SPLIT_NODE_VID,
    VID_MAX, VID_MIN,
};
use crate::app::switch_service::{SwitchError, SwitchService};
use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/network/switch")
            .route("", web::get().to(get_switch))
            .route("", web::put().to(put_switch))
            .route("/confirm", web::post().to(post_confirm))
            .route("/revert", web::post().to(post_revert))
            .route("/presets", web::get().to(get_presets))
            .route("/validate", web::post().to(post_validate)),
    );
}

/// A preset, expanded, with whatever the board would say about it.
#[derive(Debug, Serialize)]
struct PresetView {
    /// `flat`, `split`, `trunk`.
    name: &'static str,
    /// One sentence for the person choosing.
    summary: &'static str,
    /// The document this preset means, port by port.
    document: SwitchDocument,
    warnings: Vec<Warning>,
}

/// What `validate` will accept: a preset to expand, or a document outright.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Proposal {
    Preset(Preset),
    Document(SwitchDocument),
}

#[derive(Debug, Serialize)]
struct Verdict {
    /// The document as the board reads it, whether it came as a preset or
    /// whole. Returned so a client can show what it is actually asking for.
    document: SwitchDocument,
    /// `None` means the board would apply this.
    refusal: Option<Refusal>,
    warnings: Vec<Warning>,
}

/// The VLAN identifiers a preset needs from the operator, and the range they
/// must fall in. Sent so an interface does not hard-code 1 and 4094.
#[derive(Debug, Serialize)]
struct Limits {
    vid_min: u16,
    vid_max: u16,
    /// The confirm window: what you get if you say nothing, and the range you
    /// may choose from.
    window_default_s: u64,
    window_min_s: u64,
    window_max_s: u64,
    /// What `Split` uses internally. Shown so a client can explain that these
    /// exist without inviting anyone to change them: under `Split` no tag
    /// leaves the board, so the numbers are invisible everywhere else.
    split_internal_vids: [u16; 2],
}

#[derive(Debug, Serialize)]
struct PresetsView {
    limits: Limits,
    presets: Vec<PresetView>,
}

/// The identifiers used when expanding `Trunk` for the preview.
///
/// `Trunk` needs two numbers that only the operator knows, because the router
/// has to match them. For a preview any pair does, and 10/20 read as examples
/// rather than as defaults somebody should keep.
const EXAMPLE_MANAGEMENT_VID: u16 = 10;
const EXAMPLE_NODE_VID: u16 = 20;

async fn get_presets() -> HttpResponse {
    let presets = [
        (
            "flat",
            "One network. Every module, the BMC and both uplinks share it, and the switch does \
             not look at VLANs at all. This is how a board ships.",
            Preset::Flat,
        ),
        (
            "split",
            "Two networks that never meet: the BMC out of ge0, the modules out of ge1. Nothing \
             is tagged, so the other end needs no VLAN configuration at all.",
            Preset::Split,
        ),
        (
            "trunk",
            "One cable carrying both networks, tagged, for a router that knows the VLANs. The \
             identifiers are yours, because the router has to match them.",
            Preset::Trunk {
                management_vid: EXAMPLE_MANAGEMENT_VID,
                node_vid: EXAMPLE_NODE_VID,
                second_uplink: SecondUplink::Redundant,
            },
        ),
    ];

    let view = PresetsView {
        limits: Limits {
            vid_min: VID_MIN,
            vid_max: VID_MAX,
            window_default_s: DEFAULT_WINDOW.as_secs(),
            window_min_s: MIN_WINDOW.as_secs(),
            window_max_s: MAX_WINDOW.as_secs(),
            split_internal_vids: [SPLIT_MANAGEMENT_VID, SPLIT_NODE_VID],
        },
        presets: presets
            .into_iter()
            .map(|(name, summary, preset)| {
                let document = preset.expand();
                let warnings = document.warnings();
                PresetView {
                    name,
                    summary,
                    document,
                    warnings,
                }
            })
            .collect(),
    };

    HttpResponse::Ok().json(view)
}

/// Ask the board what it thinks of a configuration, without applying it.
///
/// A refusal here is a **200 with a refusal in it**, not a 400. The caller
/// asked a question and got an answer; it did not make a bad request. The
/// apply path is where the same refusal becomes a rejection.
async fn post_validate(body: web::Json<Proposal>) -> Result<HttpResponse, LegacyResponse> {
    let document = match body.into_inner() {
        Proposal::Preset(preset) => preset.expand(),
        Proposal::Document(document) => document,
    };

    let refusal = document.refusal();
    let warnings = document.warnings();

    Ok(HttpResponse::Ok().json(Verdict {
        document,
        refusal,
        warnings,
    }))
}

#[derive(Debug, Deserialize)]
struct ApplyRequest {
    #[serde(flatten)]
    proposal: Proposal,
    /// Seconds to wait for a confirmation. Absent means the board's default.
    #[serde(default)]
    window_s: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ConfirmRequest {
    token: String,
}

fn actor_name(request: &HttpRequest) -> String {
    use crate::authentication::authentication_context::Actor;
    match request.extensions().get::<Actor>() {
        Some(Actor::User { name, .. }) => name.clone(),
        Some(Actor::Loopback) => "loopback".to_string(),
        None => "unattributed".to_string(),
    }
}

/// Did this request come from the board itself?
///
/// `/api/bmc` drops authentication for the loopback interface, which is how
/// the on-board `tpi` works without credentials. For everything else that is a
/// convenience. For a confirmation it is a hole, and the one below closes it.
fn from_the_board_itself(request: &HttpRequest) -> bool {
    use crate::authentication::authentication_context::Actor;
    matches!(request.extensions().get::<Actor>(), Some(Actor::Loopback))
}

/// What the board says to a confirmation that came from its own shell.
///
/// Written out here, once, because the same words belong in the interface and
/// in the guide, and three copies of this sentence would diverge.
const CONFIRM_FROM_THE_BOARD: &str =
    "A confirmation has to arrive over the network you just changed; that is what it proves. \
     This one came from the board itself, which proves nothing. Confirm from the interface, or \
     from `tpi` on another machine.";

fn refuse(e: SwitchError) -> LegacyResponse {
    match e {
        SwitchError::Change(_) => LegacyResponse::bad_request(e.to_string()),
        other => LegacyResponse::from(anyhow::anyhow!("{other}")),
    }
}

async fn get_switch(switch: web::Data<SwitchService>) -> HttpResponse {
    HttpResponse::Ok().json(switch.view().await)
}

/// Apply a configuration, and start waiting to be proved right.
///
/// The answer is `202`, not `200`: the change is on the switch but it is not
/// yours to keep yet. Confirm within the window or the board puts the previous
/// one back by itself.
async fn put_switch(
    request: HttpRequest,
    switch: web::Data<SwitchService>,
    body: web::Json<ApplyRequest>,
) -> Result<HttpResponse, LegacyResponse> {
    let body = body.into_inner();
    let document = match body.proposal {
        Proposal::Preset(preset) => preset.expand(),
        Proposal::Document(document) => document,
    };

    // The refusals run here too, and here they ARE a rejection. `validate`
    // answers a question; this one is being asked to do something.
    if let Some(refusal) = document.refusal() {
        return Err(LegacyResponse::bad_request(refusal.reason));
    }

    let window = body
        .window_s
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_WINDOW);

    let pending = switch.apply(document, window).await.map_err(refuse)?;

    tracing::info!(
        "switch configuration applied by {}, awaiting confirmation within {}s",
        actor_name(&request),
        pending.window_s
    );

    Ok(HttpResponse::Accepted().json(pending))
}

/// Keep the change.
///
/// This must arrive on a NEW connection, and that is the whole proof. After an
/// apply the old path no longer exists, so any authenticated request that
/// reaches this daemon came through the new configuration. Nothing else needs
/// checking, and nothing else could be checked as cheaply.
///
/// ## Which is why a confirmation from the board itself is refused
///
/// The proof is the arrival, not the request. A confirmation sent over
/// loopback -- from `tpi` on the board, from a shell, from a script running
/// there -- crossed no switch port, so it says nothing whatever about whether
/// the configuration that was just applied works. It persists it anyway.
///
/// This is not theoretical. On 2026-09-20 a document reasoned to be equivalent
/// to Flat was applied to bmc-2 and confirmed from the board's own ssh
/// session. The owner lost the interface; the window that exists for exactly
/// that case never got to run, because the confirmation had already arrived.
/// The reasoning being sound was not the point -- a proof from loopback is
/// empty regardless of what it is proving.
///
/// Apply and revert stay open to loopback. Applying is how you would recover a
/// board from its console, and reverting is the safe direction: it puts back
/// the configuration that was already proved once. Only the step that makes a
/// change permanent needs to have come from somewhere.
async fn post_confirm(
    request: HttpRequest,
    switch: web::Data<SwitchService>,
    body: web::Json<ConfirmRequest>,
) -> Result<HttpResponse, LegacyResponse> {
    if from_the_board_itself(&request) {
        tracing::warn!(
            "a switch confirmation arrived over loopback and was refused; it would have proved \
             nothing"
        );
        return Err(LegacyResponse::forbidden(CONFIRM_FROM_THE_BOARD));
    }

    switch
        .confirm(&body.into_inner().token)
        .await
        .map_err(refuse)?;
    tracing::info!("switch configuration confirmed by {}", actor_name(&request));
    Ok(HttpResponse::Ok().json(switch.view().await))
}

/// Put a pending change back now, rather than waiting out its window.
///
/// Open to loopback, unlike confirm: this is the direction that cannot strand
/// anybody, and a console is where you would reach for it.
async fn post_revert(
    request: HttpRequest,
    switch: web::Data<SwitchService>,
) -> Result<HttpResponse, LegacyResponse> {
    switch.revert().await.map_err(refuse)?;
    tracing::info!("switch change reverted by {}", actor_name(&request));
    Ok(HttpResponse::Ok().json(switch.view().await))
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

    fn service() -> web::Data<SwitchService> {
        let dir = tempdir::TempDir::new("switch-confirm").expect("temp dir");
        web::Data::new(SwitchService::load(dir.path().join("switch.json")))
    }

    fn confirm_body() -> web::Json<ConfirmRequest> {
        web::Json(ConfirmRequest {
            token: "whatever".to_string(),
        })
    }

    /// The refusal this endpoint exists to make.
    ///
    /// Nothing is pending, so a confirmation that got past the gate would fail
    /// with "no change is waiting" -- a 400. Getting a 403 instead is the
    /// proof that it never got that far, and that the gate is ahead of the
    /// state machine rather than tangled in it.
    #[actix_web::test]
    async fn a_confirmation_from_the_board_itself_is_refused() {
        let error = post_confirm(
            request_from(Some(Actor::Loopback)),
            service(),
            confirm_body(),
        )
        .await
        .expect_err("loopback proves nothing");

        let LegacyResponse::Error(status, message) = error else {
            panic!("a refusal, not a success");
        };
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            message.contains("came from the board itself"),
            "the message has to say what to do instead: {message}"
        );
        assert!(message.contains("another machine"), "{message}");
    }

    /// The control. Without it the test above would pass just as well if
    /// confirm were refused to everybody.
    #[actix_web::test]
    async fn a_confirmation_over_the_network_reaches_the_state_machine() {
        let user = Actor::User {
            name: "root".to_string(),
            scheme: "bearer",
        };
        let error = post_confirm(request_from(Some(user)), service(), confirm_body())
            .await
            .expect_err("nothing is pending on a fresh board");

        let LegacyResponse::Error(status, message) = error else {
            panic!("a refusal, not a success");
        };
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "it reached the state machine and was turned down there: {message}"
        );
        assert!(message.contains("no change is waiting"), "{message}");
    }

    /// An unauthenticated request is not loopback, and must not be treated as
    /// though it were: the gate is about where the request came from, and
    /// "nobody said" is not "from the board".
    #[actix_web::test]
    async fn a_request_with_no_actor_is_not_mistaken_for_loopback() {
        let error = post_confirm(request_from(None), service(), confirm_body())
            .await
            .expect_err("nothing is pending on a fresh board");

        let LegacyResponse::Error(status, _) = error else {
            panic!("a refusal, not a success");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// Reverting from a console is how somebody digs themselves out, so the
    /// gate must not have been put on the whole module.
    #[actix_web::test]
    async fn a_revert_from_the_board_itself_is_allowed_through() {
        let error = post_revert(request_from(Some(Actor::Loopback)), service())
            .await
            .expect_err("nothing is pending on a fresh board");

        let LegacyResponse::Error(status, message) = error else {
            panic!("a refusal, not a success");
        };
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "loopback may revert; it simply had nothing to revert: {message}"
        );
    }
}
