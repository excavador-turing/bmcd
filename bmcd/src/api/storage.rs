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
//! `/api/bmc/storage`: is the board keeping its settings, and the repair for
//! when it is not.
//!
//! Two endpoints. `GET` says what the board is running on and whether the
//! repair is on offer; `POST /repair` removes the settings volume that would
//! not mount and reboots, so the next boot recreates it. What the words mean,
//! and why the mount table rather than a file decides them, is in
//! [`crate::app::storage_status`].
//!
//! The repair erases whatever is on that volume and reboots the BMC, which
//! on a v2.4 board power-cycles the four compute modules. It is a `POST`
//! with an explicit `{"confirm": true}`, behind the same authentication as
//! every other mutation, and it is worked out again from the board each time
//! it is asked for -- a page that was open an hour ago does not get to remove
//! a volume that has since been mounted.
use crate::api::address::actor_name;
use crate::api::into_legacy_response::LegacyResponse;
use crate::app::bmc_application::BmcApplication;
use crate::app::storage_status::{RepairError, StorageService};
use actix_web::http::StatusCode;
use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use std::time::Duration;

/// How long the response has to reach the browser before the board goes down.
pub const REBOOT_DELAY: Duration = Duration::from_secs(2);

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/storage")
            .route("", web::get().to(get_storage))
            .route("/repair", web::post().to(post_repair)),
    );
}

#[derive(Debug, Deserialize)]
struct RepairRequest {
    #[serde(default)]
    confirm: bool,
}

const CONFIRM_REQUIRED: &str =
    "Repair removes the settings volume, which erases the saved settings, and reboots the \
     board. Send {\"confirm\": true} to proceed.";

async fn get_storage(storage: web::Data<StorageService>) -> HttpResponse {
    HttpResponse::Ok().json(storage.status_async().await)
}

/// Everything about a repair request except the reboot: the body, the
/// decision, and the command. Separate so that it can be run against a board
/// that is not there.
async fn run_repair(storage: &StorageService, body: &[u8]) -> Result<(), LegacyResponse> {
    let confirmed = serde_json::from_slice::<RepairRequest>(body)
        .map(|r| r.confirm)
        .unwrap_or(false);
    if !confirmed {
        return Err(LegacyResponse::bad_request(CONFIRM_REQUIRED));
    }
    storage.repair_async().await.map_err(|e| match e {
        RepairError::Blocked(_) => {
            LegacyResponse::Error(StatusCode::CONFLICT, e.to_string().into())
        }
        RepairError::Command(_) | RepairError::Join(_) => {
            LegacyResponse::Error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string().into())
        }
    })
}

/// Remove the settings volume, then reboot a moment later. `202`, because the
/// work that matters happens after the response.
async fn post_repair(
    request: HttpRequest,
    storage: web::Data<StorageService>,
    bmc: web::Data<BmcApplication>,
    body: web::Bytes,
) -> Result<HttpResponse, LegacyResponse> {
    let actor = actor_name(&request);
    run_repair(&storage, &body).await?;
    tracing::warn!(
        "settings volume removed at the request of {actor}; the saved settings are gone and the \
         board reboots in {}s to recreate the volume",
        REBOOT_DELAY.as_secs()
    );
    let bmc = bmc.into_inner();
    tokio::spawn(async move {
        tokio::time::sleep(REBOOT_DELAY).await;
        tracing::warn!("rebooting to recreate the settings volume");
        if let Err(e) = bmc.reboot(false).await {
            tracing::error!("the reboot after the settings repair failed: {e:#}");
        }
    });
    Ok(HttpResponse::Accepted().json(serde_json::json!({ "rebooting": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::storage_status::tests::{Board, PERSISTENT_LINE, SAFEMODE_LINE};
    use actix_web::ResponseError;

    async fn legacy_body(error: LegacyResponse) -> (StatusCode, String) {
        let response = error.error_response();
        let status = response.status();
        let bytes = actix_web::body::to_bytes(response.into_body())
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        (
            status,
            body["response"][0]["result"].as_str().unwrap().to_string(),
        )
    }

    #[actix_web::test]
    async fn the_report_is_the_published_shape() {
        let board = Board::broken();
        let response = get_storage(web::Data::new(board.service.clone())).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = actix_web::body::to_bytes(response.into_body())
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["state"], "temporary");
        assert_eq!(json["reason"], "mount_failed");
        assert_eq!(json["detail"], "UBIFS error");
        assert_eq!(json["backing"], "ubi");
        assert_eq!(json["repairable"], true);
        assert!(json["repair_blocker"].is_null());
    }

    #[actix_web::test]
    async fn a_repair_without_confirm_is_a_400_and_runs_nothing() {
        let board = Board::broken();
        for body in [
            &b""[..],
            b"{}",
            br#"{"confirm": false}"#,
            br#"{"confirm": "yes"}"#,
            b"not json",
        ] {
            let error = run_repair(&board.service, body)
                .await
                .expect_err("not confirmed");
            let (status, message) = legacy_body(error).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
            assert!(message.contains("\"confirm\": true"), "{message}");
        }
        assert!(board.exec.calls.lock().unwrap().is_empty());
    }

    #[actix_web::test]
    async fn a_repair_that_is_not_offered_is_a_409_with_the_blocker_and_runs_nothing() {
        let requested = Board::new(
            SAFEMODE_LINE,
            Some("reason=requested\n"),
            "ro safemode",
            &["rootfs", "overlay"],
        );
        let healthy = Board::new(PERSISTENT_LINE, None, "ro", &["rootfs", "overlay"]);
        let no_volume = Board::new(SAFEMODE_LINE, None, "ro", &["rootfs"]);
        for (board, expect) in [
            (requested, "Safemode was requested at power-on (KEY1)."),
            (healthy, "nothing to repair"),
            (
                no_volume,
                "There is no settings volume to remove; a normal reboot recreates it.",
            ),
        ] {
            let error = run_repair(&board.service, br#"{"confirm": true}"#)
                .await
                .expect_err("not repairable");
            let (status, message) = legacy_body(error).await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert!(message.contains(expect), "{message}");
            // The message is the blocker the report gives, word for word.
            assert_eq!(
                Some(message),
                board.service.status().repair_blocker,
                "409 and GET disagree"
            );
            assert!(board.exec.calls.lock().unwrap().is_empty());
        }
    }

    #[actix_web::test]
    async fn a_confirmed_repair_of_a_broken_board_removes_the_volume() {
        let board = Board::broken();
        run_repair(&board.service, br#"{"confirm": true}"#)
            .await
            .expect("repairable");
        assert_eq!(
            board.exec.changes(),
            vec!["ubirmvol /dev/ubi0 -N overlay".to_string()]
        );
    }

    #[actix_web::test]
    async fn a_failed_ubirmvol_is_a_500_carrying_the_command_and_what_it_printed() {
        let board = Board::broken();
        *board.exec.fail_prefix.lock().unwrap() = Some("ubirmvol".to_string());
        let error = run_repair(&board.service, br#"{"confirm": true}"#)
            .await
            .expect_err("ubirmvol fails");
        let (status, message) = legacy_body(error).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            message.starts_with("`ubirmvol /dev/ubi0 -N overlay` failed: "),
            "{message}"
        );
    }
}
