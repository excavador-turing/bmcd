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
//! The repair permanently erases whatever is on that volume and reboots the
//! BMC, which on a v2.4 board power-cycles the four compute modules. It is a
//! `POST` with an explicit `{"confirm": true}` declared as JSON, behind the
//! same authentication as every other mutation, and it is worked out again
//! from the board each time it is asked for -- a page that was open an hour
//! ago does not get to remove a volume that has since been mounted.
use crate::api::address::actor_name;
use crate::api::into_legacy_response::LegacyResponse;
use crate::app::bmc_application::BmcApplication;
use crate::app::storage_status::{RepairError, StorageService};
use actix_web::error::JsonPayloadError;
use actix_web::http::StatusCode;
use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use std::future::Future;
use std::time::Duration;

/// How long the response has to reach the browser before the board goes down.
pub const REBOOT_DELAY: Duration = Duration::from_secs(2);

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/storage")
            .route("", web::get().to(get_storage))
            .service(
                web::resource("/repair")
                    .app_data(repair_body_config())
                    .route(web::post().to(post_repair)),
            ),
    );
}

#[derive(Debug, Deserialize)]
struct RepairRequest {
    #[serde(default)]
    confirm: bool,
}

const CONFIRM_REQUIRED: &str =
    "Repair removes the settings volume, which permanently erases the saved settings, and \
     reboots the board. Send {\"confirm\": true} to proceed.";

const JSON_REQUIRED: &str =
    "Send the request as JSON: Content-Type: application/json with the body {\"confirm\": true}.";

/// The body is strict: it has to be declared JSON, which a browser form
/// cannot do cross-site without a preflight. A `text/plain` or form-encoded
/// body that happens to parse is not accepted, so a page on another origin
/// cannot trigger the repair with the Basic credentials the browser caches.
/// Both refusals use the legacy body.
fn repair_body_config() -> web::JsonConfig {
    web::JsonConfig::default()
        .limit(1024)
        .error_handler(|error, _| {
            let refusal = match error {
                JsonPayloadError::ContentType => {
                    LegacyResponse::Error(StatusCode::UNSUPPORTED_MEDIA_TYPE, JSON_REQUIRED.into())
                }
                _ => LegacyResponse::bad_request(CONFIRM_REQUIRED),
            };
            refusal.into()
        })
}

fn refuse(e: RepairError) -> LegacyResponse {
    match e {
        RepairError::Blocked(_) => {
            LegacyResponse::Error(StatusCode::CONFLICT, e.to_string().into())
        }
        RepairError::Command(_) | RepairError::Timeout(_) | RepairError::Join(_) => {
            LegacyResponse::Error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string().into())
        }
    }
}

async fn get_storage(storage: web::Data<StorageService>) -> HttpResponse {
    HttpResponse::Ok().json(storage.status_async().await)
}

/// Remove the volume, and reboot a moment later -- as one piece of work that
/// does not depend on the request surviving.
///
/// The removal and the reboot run in a single detached task. A client that
/// gives up while `ubirmvol` runs drops this future, not that task: the
/// volume that was removed is followed by the reboot that recreates it, and
/// the guard is not left set on a board that never goes down. The caller
/// hears the removal's result over a channel, for the 202 or the error.
async fn repair_and_reboot<F, Fut>(
    storage: StorageService,
    confirm: bool,
    actor: String,
    reboot: F,
    delay: Duration,
) -> Result<(), LegacyResponse>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    if !confirm {
        return Err(LegacyResponse::bad_request(CONFIRM_REQUIRED));
    }
    let (sent, received) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let removed = storage.repair_async().await;
        let succeeded = removed.is_ok();
        // Nobody listening is fine: the work does not need the answer.
        let _ = sent.send(removed);
        if !succeeded {
            return;
        }
        tracing::warn!(
            "settings volume removed at the request of {actor}; the saved settings are gone \
             and the board reboots in {}s to recreate the volume",
            delay.as_secs()
        );
        tokio::time::sleep(delay).await;
        tracing::warn!("rebooting to recreate the settings volume");
        if let Err(e) = reboot().await {
            tracing::error!("the reboot after the settings repair failed: {e:#}");
        }
    });
    match received.await {
        Ok(result) => result.map_err(refuse),
        Err(_) => Err(LegacyResponse::Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the repair stopped before it reported".into(),
        )),
    }
}

/// Remove the settings volume, then reboot a moment later. `202`, because the
/// work that matters happens after the response.
async fn post_repair(
    request: HttpRequest,
    storage: web::Data<StorageService>,
    bmc: web::Data<BmcApplication>,
    body: web::Json<RepairRequest>,
) -> Result<HttpResponse, LegacyResponse> {
    let actor = actor_name(&request);
    tracing::warn!("settings repair requested by {actor}");
    let bmc = bmc.into_inner();
    repair_and_reboot(
        storage.get_ref().clone(),
        body.confirm,
        actor,
        move || async move { bmc.reboot(false).await },
        REBOOT_DELAY,
    )
    .await?;
    Ok(HttpResponse::Accepted().json(serde_json::json!({ "rebooting": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::storage_status::tests::{Board, SlowExec, PERSISTENT_LINE, SAFEMODE_LINE};
    use actix_web::test::TestRequest;
    use actix_web::{FromRequest, ResponseError};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    async fn legacy_body(response: HttpResponse) -> (StatusCode, String) {
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

    /// Counts the reboots it is asked for.
    #[derive(Clone, Default)]
    struct Reboots(Arc<AtomicUsize>);

    impl Reboots {
        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }

        /// Wait (bounded) for the detached task to get as far as rebooting.
        async fn wait_for(&self, n: usize) {
            for _ in 0..200 {
                if self.count() >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    async fn repair(
        service: &StorageService,
        confirm: bool,
        reboots: &Reboots,
    ) -> Result<(), LegacyResponse> {
        let counter = reboots.0.clone();
        repair_and_reboot(
            service.clone(),
            confirm,
            "tester".to_string(),
            move || async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            Duration::from_millis(10),
        )
        .await
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

    // --- the body is strict ------------------------------------------------

    async fn extract(
        content_type: Option<&str>,
        body: &str,
    ) -> Result<RepairRequest, actix_web::Error> {
        let mut request = TestRequest::post()
            .uri("/api/bmc/storage/repair")
            .app_data(repair_body_config())
            .set_payload(body.to_string());
        if let Some(ct) = content_type {
            request = request.insert_header(("content-type", ct));
        }
        let (request, mut payload) = request.to_http_parts();
        web::Json::<RepairRequest>::from_request(&request, &mut payload)
            .await
            .map(web::Json::into_inner)
    }

    #[actix_web::test]
    async fn only_a_body_declared_as_json_is_read() {
        for ct in ["application/json", "application/json; charset=utf-8"] {
            let ok = extract(Some(ct), r#"{"confirm": true}"#).await.unwrap();
            assert!(ok.confirm, "{ct}");
        }
        // The cross-site form tricks: the body parses as JSON, the type says
        // it is not.
        for ct in [
            Some("text/plain"),
            Some("text/plain;charset=UTF-8"),
            Some("application/x-www-form-urlencoded"),
            Some("multipart/form-data; boundary=x"),
            None,
        ] {
            let error = extract(ct, r#"{"confirm": true}"#)
                .await
                .expect_err("not declared as JSON");
            let (status, message) = legacy_body(error.error_response()).await;
            assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{ct:?}");
            assert!(message.contains("application/json"), "{message}");
        }
    }

    #[actix_web::test]
    async fn a_body_that_is_not_the_request_is_a_400_in_the_legacy_shape() {
        for body in ["", "not json", r#"{"confirm": "yes"}"#, r#"{"confirm": 1}"#] {
            let error = extract(Some("application/json"), body)
                .await
                .expect_err("not the request");
            let (status, message) = legacy_body(error.error_response()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
            assert!(message.contains("\"confirm\": true"), "{message}");
        }
        // An empty object parses and simply does not confirm.
        let r = extract(Some("application/json"), "{}").await.unwrap();
        assert!(!r.confirm);
    }

    // --- the decision ------------------------------------------------------

    #[actix_web::test]
    async fn a_repair_without_confirm_is_a_400_and_runs_nothing() {
        let board = Board::broken();
        let reboots = Reboots::default();
        let error = repair(&board.service, false, &reboots)
            .await
            .expect_err("not confirmed");
        let (status, message) = legacy_body(error.error_response()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(message.contains("\"confirm\": true"), "{message}");
        assert!(message.contains("permanently erases"), "{message}");
        assert!(board.exec.calls.lock().unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(reboots.count(), 0);
    }

    #[actix_web::test]
    async fn a_repair_that_is_not_offered_is_a_409_with_the_blocker_and_runs_nothing() {
        let requested = Board::new(
            SAFEMODE_LINE,
            Some("reason=requested\n"),
            "ro safemode",
            &["rootfs", "overlay"],
        );
        // KEY1 wins over a file that says the mount failed.
        let key1_over_file = Board::new(
            SAFEMODE_LINE,
            Some("reason=mount_failed\n"),
            "ro safemode=1",
            &["rootfs", "overlay"],
        );
        let healthy = Board::new(PERSISTENT_LINE, None, "ro", &["rootfs", "overlay"]);
        let no_volume = Board::new(SAFEMODE_LINE, None, "ro", &["rootfs"]);
        for (board, expect) in [
            (requested, "Safemode was requested at power-on (KEY1)."),
            (key1_over_file, "Safemode was requested at power-on (KEY1)."),
            (healthy, "nothing to repair"),
            (
                no_volume,
                "There is no settings volume to remove; reboot the BMC to recreate it.",
            ),
        ] {
            let reboots = Reboots::default();
            let error = repair(&board.service, true, &reboots)
                .await
                .expect_err("not repairable");
            let (status, message) = legacy_body(error.error_response()).await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert!(message.contains(expect), "{message}");
            // The message is the blocker the report gives, word for word.
            assert_eq!(
                Some(message),
                board.service.status().repair_blocker,
                "409 and GET disagree"
            );
            assert!(board.exec.calls.lock().unwrap().is_empty());
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(reboots.count(), 0);
        }
    }

    // --- removal and reboot are one piece of work --------------------------

    #[actix_web::test]
    async fn a_confirmed_repair_removes_the_volume_and_then_reboots() {
        let board = Board::broken();
        let reboots = Reboots::default();
        repair(&board.service, true, &reboots)
            .await
            .expect("repairable");
        assert_eq!(
            board.exec.changes(),
            vec!["ubirmvol /dev/ubi0 -N overlay".to_string()]
        );
        reboots.wait_for(1).await;
        assert_eq!(reboots.count(), 1);
    }

    /// The client gives up while `ubirmvol` is still running. The handler's
    /// future is dropped; the volume still goes, so the reboot that recreates
    /// it must still come.
    #[actix_web::test]
    async fn a_dropped_request_still_ends_in_the_reboot() {
        let board = Board::broken();
        let exec = SlowExec::new(Duration::from_millis(150));
        let service = board.service_with(exec.clone());
        let reboots = Reboots::default();

        let gave_up =
            tokio::time::timeout(Duration::from_millis(20), repair(&service, true, &reboots)).await;
        assert!(gave_up.is_err(), "the removal was still running");
        assert_eq!(reboots.count(), 0);

        reboots.wait_for(1).await;
        assert_eq!(reboots.count(), 1, "the reboot did not follow the removal");
        assert_eq!(exec.calls.lock().unwrap().len(), 1);
    }

    #[actix_web::test]
    async fn a_failed_ubirmvol_is_a_500_carrying_the_commands_message_and_no_reboot() {
        let board = Board::broken();
        *board.exec.fail_prefix.lock().unwrap() = Some("ubirmvol".to_string());
        let reboots = Reboots::default();
        let error = repair(&board.service, true, &reboots)
            .await
            .expect_err("ubirmvol fails");
        let (status, message) = legacy_body(error.error_response()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            message.starts_with("`ubirmvol /dev/ubi0 -N overlay` failed: "),
            "{message}"
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(reboots.count(), 0);
    }

    #[actix_web::test]
    async fn a_hung_ubirmvol_is_a_500_and_no_reboot() {
        let board = Board::broken();
        let service = board
            .service_with(SlowExec::new(Duration::from_millis(300)))
            .with_timeout(Duration::from_millis(30));
        let reboots = Reboots::default();
        let error = repair(&service, true, &reboots)
            .await
            .expect_err("timed out");
        let (status, message) = legacy_body(error.error_response()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(message.contains("did not finish"), "{message}");
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(reboots.count(), 0);
    }
}
