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
//! Routes for legacy API present in versions <= 2.0.0 of the firmware.
use crate::api::into_legacy_response::LegacyResponse;
use crate::api::into_legacy_response::{LegacyResult, Null};
use crate::app::bmc_application::NodeInfo;
use crate::app::bmc_application::{BmcApplication, UsbConfig};
use crate::app::bmc_info::{
    get_fs_stat, get_ipv4_address, get_mac_address, get_net_interfaces, get_storage_info,
};
use crate::app::firmware_catalog;
use crate::app::firmware_info::get_firmware_slots;
use crate::app::firmware_sources;
use crate::app::health_info::get_health;
use crate::app::metrics_token;
use crate::app::switch_info::get_switch_ports;
use crate::app::thermal_info::get_thermal_state;
use crate::app::transfer_action::InitializeTransfer;
use crate::app::transfer_action::UpgradeCommand;
use crate::app::update_check;
use crate::hal::{NodeId, UsbMode, UsbRoute};
use crate::serial_service::serial::SerialConnections;
use crate::serial_service::{legacy_serial_get_handler, legacy_serial_set_handler};
use crate::streaming_data_service::data_transfer::DataTransfer;
use crate::streaming_data_service::StreamingDataService;
use actix_files::file_extension_to_mime;
use actix_multipart::Multipart;
use actix_web::guard::{fn_guard, GuardContext};
use actix_web::http::{header, StatusCode};
use actix_web::{get, post, web, HttpResponse, Responder};
use anyhow::Context;
use async_compression::tokio::bufread::GzipEncoder;
use async_compression::Level;
use board_info::{self, BoardInfoAttribute};
use humansize::{format_size, DECIMAL};
use serde_json::json;
use std::collections::HashMap;
use std::ffi::c_ulong;
use std::io;
use std::ops::Deref;
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio_stream::StreamExt;
use tokio_util::io::ReaderStream;

use super::get_node_param;
type Query = web::Query<std::collections::HashMap<String, String>>;

/// version 1:
///
/// * get requests with type&opt queries
///
/// version 1.1:
///
/// * enabled HTTPS
/// * chunked upload of flash images
const API_VERSION: &str = "1.1";

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("")
            .route(
                web::get()
                    .guard(fn_guard(flash_status_guard))
                    .to(handle_flash_status),
            )
            .route(
                web::get()
                    .guard(fn_guard(flash_guard))
                    .to(handle_transfer_request),
            )
            .route(
                web::post()
                    .guard(fn_guard(set_node_info_guard))
                    .to(set_node_aux_info),
            )
            .route(web::get().to(api_entry)),
    )
    .service(handle_file_upload)
    .service(cancel_file_upload)
    .service(backup_handler);
}

/// The value of one query parameter. A `GuardContext` hands out the raw query
/// string and nothing else, so the guards below did their matching with
/// `contains`, which reads a value as a prefix: `type=firmware_slots`
/// contains `type=firmware`, and would be routed to the transfer machinery
/// before `api_entry` ever saw it. This compares the whole value.
fn query_param<'q>(query: &'q str, key: &str) -> Option<&'q str> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(value)
    })
}

/// Whether this request is the `flash`/`firmware` transfer machinery rather
/// than an `api_entry` type, for the given `opt`.
fn is_transfer_request(context: &GuardContext<'_>, opt: &str) -> bool {
    let Some(query) = context.head().uri.query() else {
        return false;
    };
    query_param(query, "opt") == Some(opt)
        && matches!(query_param(query, "type"), Some("flash" | "firmware"))
}

fn flash_status_guard(context: &GuardContext<'_>) -> bool {
    is_transfer_request(context, "get")
}

fn flash_guard(context: &GuardContext<'_>) -> bool {
    is_transfer_request(context, "set")
}

fn set_node_info_guard(context: &GuardContext<'_>) -> bool {
    let Some(query) = context.head().uri.query() else {
        return false;
    };
    query_param(query, "opt") == Some("set") && query_param(query, "type") == Some("node_info")
}

#[get("/backup")]
async fn backup_handler() -> impl Responder {
    let archive = tokio::task::spawn_blocking(move || {
        let mut builder = tar::Builder::new(Vec::new());
        builder.mode(tar::HeaderMode::Deterministic);
        builder
            .append_dir_all(".", "/mnt/overlay/upper/")
            .and_then(|_| builder.finish())
            .and_then(|_| builder.into_inner())
    })
    .await
    .expect("error joining archiving task");

    match archive {
        Ok(buffer) => {
            let now = chrono::Local::now();
            let content_disposition = format!(
                r#"attachment; filename="tp2-backup-{}.tar.gz""#,
                now.format("%d-%m-%Y")
            );
            let encoder = GzipEncoder::with_quality(std::io::Cursor::new(buffer), Level::Best);
            HttpResponse::Ok()
                .insert_header(header::ContentType(file_extension_to_mime("gz")))
                .insert_header((header::CONTENT_DISPOSITION, content_disposition))
                .streaming(ReaderStream::new(encoder))
        }
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

async fn api_entry(
    bmc: web::Data<BmcApplication>,
    serial: web::Data<SerialConnections>,
    query: Query,
) -> impl Responder {
    let is_set = match query.get("opt").map(String::as_str) {
        Some("set") => true,
        Some("get") => false,
        _ => return LegacyResponse::bad_request("Missing `opt` parameter"),
    };

    let Some(ty) = query.get("type") else {
        return LegacyResponse::bad_request("Missing `type` parameter");
    };

    let bmc = bmc.as_ref();
    match (ty.as_ref(), is_set) {
        ("usb_boot", true) => usb_boot(bmc, query).await.into(),
        ("clear_usb_boot", true) => clear_usb_boot(bmc).into(),
        ("firmware_slots", false) => get_firmware_slot_info().await.into(),
        ("health", false) => get_health_info().await.into(),
        ("network", false) => get_network_info().await.into(),
        ("network", true) => reset_network(bmc).await.into(),
        ("nodeinfo", true) => set_node_info().into(),
        ("nodeinfo", false) => get_node_info(bmc).into(),
        ("node_info", false) => get_node_aux_info(bmc).await.into(),
        ("node_to_msd", true) => set_node_to_msd(bmc, query).await.into(),
        ("other", false) => get_system_information().await.into(),
        ("power", true) => set_node_power(bmc, query).await,
        ("power", false) => get_node_power(bmc).await.into(),
        ("reboot", true) => reboot(bmc, query).await.into(),
        ("reload", true) => reload_self().into(),
        ("reset", true) => reset_node(bmc, query).await.into(),
        ("sdcard", true) => format_sdcard().into(),
        ("sdcard", false) => get_sdcard_info(),
        ("uart", false) => legacy_serial_get_handler(serial, query).await.into(),
        ("uart", true) => legacy_serial_set_handler(serial, query).await.into(),
        ("usb", true) => set_usb_mode(bmc, query).await.into(),
        ("usb", false) => get_usb_mode(bmc).await.into(),
        ("usb_node1", true) => set_node1_usb_mode(bmc, query).await.into(),
        ("usb_node1", false) => get_node1_usb_mode(bmc).await,
        ("info", false) => get_info().await.into(),
        ("cooling", false) => get_cooling_info().await.into(),
        ("cooling", true) => set_cooling_info(bmc, query).await.into(),
        ("thermal", false) => get_thermal_info().await.into(),
        ("about", false) => get_about().await.into(),
        ("metrics_token", false) => get_metrics_token().await,
        ("update_check", false) => get_update_check().await.into(),
        ("firmware_sources", false) => get_firmware_sources().await.into(),
        ("firmware_sources", true) => set_firmware_sources(query).await,
        ("firmware_available", false) => get_firmware_available(query).await.into(),
        ("firmware_install", true) => install_firmware(query).await,
        ("metrics_token", true) => rotate_metrics_token().await,
        ("ntp", false) => get_ntp().await.into(),
        ("ntp", true) => set_ntp(query).await,
        ("hostname", false) => get_hostname().await.into(),
        ("hostname", true) => set_hostname(query).await,
        ("config", false) => get_config(bmc, query).await.into(),
        ("config", true) => set_config(bmc, query).await,
        _ => (
            StatusCode::BAD_REQUEST,
            format!("Invalid `type` parameter {}", ty),
        )
            .into(),
    }
}

#[allow(clippy::unused_unit)]
fn reload_self() -> impl Into<LegacyResponse> {
    tokio::task::spawn_blocking(move || {
        Command::new("sh")
            .arg("-c")
            .arg("/etc/init.d/S94bmcd restart")
            .status()
    });

    ()
}

/// Everything a person has configured, as one document.
///
/// `secrets=1` includes the metrics token, and that is what makes an export a
/// credential: applied to another board it can scrape it. The document says
/// `contains_secrets` on its face so the difference is visible in the file
/// rather than remembered from the request that produced it.
async fn get_config(bmc: &BmcApplication, query: Query) -> impl Into<LegacyResponse> {
    let with_secrets = query.contains_key("secrets");
    json!(crate::app::config_export::export(bmc, with_secrets).await)
}

/// Applies an exported document.
///
/// Reports per field rather than as one verdict. There is no way to roll a
/// hostname and a set of firmware sources back together, so a partial apply
/// reported as a failure would leave an operator unsure which half took.
async fn set_config(bmc: &BmcApplication, query: Query) -> LegacyResponse {
    let Some(body) = query.get("config") else {
        return LegacyResponse::bad_request("Missing `config` parameter");
    };

    let document: crate::app::config_export::ConfigExport = match serde_json::from_str(body) {
        Ok(parsed) => parsed,
        Err(e) => return LegacyResponse::bad_request(format!("`config` is not valid: {e}")),
    };

    match crate::app::config_export::import(bmc, &document).await {
        Ok(report) => LegacyResponse::Success(Some(json!(report))),
        Err(e) => LegacyResponse::bad_request(e),
    }
}

/// What the board calls itself, live and after the next reboot.
///
/// Both, because they differ when someone has run `hostname` by hand, and a
/// page that shows only one of them cannot explain why the board answers to a
/// name the settings do not show.
async fn get_hostname() -> impl Into<LegacyResponse> {
    let live = crate::app::hostname::current().await;
    let persisted = tokio::fs::read_to_string("/etc/hostname")
        .await
        .ok()
        .map(|s| s.trim().to_string());
    json!({ "hostname": live, "on_next_boot": persisted })
}

/// Renames the board.
///
/// The name is not only a label: it is what `about` reports, what the
/// interface puts in its header, what mdnsd advertises as `<name>.local`, and
/// the `instance` label on every metrics series. The last of those breaks the
/// continuity of a Prometheus series, which is why this is a deliberate act
/// with a confirmation in front of it rather than an editable field.
async fn set_hostname(query: Query) -> LegacyResponse {
    let Some(name) = query.get("name") else {
        return LegacyResponse::bad_request("Missing `name` parameter");
    };

    match crate::app::hostname::set(name).await {
        Ok(()) => LegacyResponse::Success(None),
        Err(e) => LegacyResponse::bad_request(e),
    }
}

/// Which time sources the board uses, and how its clock is doing on them.
///
/// Both in one answer because they are read together: a server list with no
/// sync state is a setting nobody can tell the effect of, and the effect is
/// the only reason to change it.
async fn get_ntp() -> impl Into<LegacyResponse> {
    let config = crate::app::ntp::load().await;
    let health = crate::app::health_info::get_health().await;
    json!({
        "servers": config.servers,
        // False on an image whose chrony.conf predates the `sourcedir` line.
        // Without it a saved list is written and silently never read, and the
        // page should say so rather than show a setting that does nothing.
        "configurable": crate::app::ntp::sourcedir_configured(),
        "clock": health.clock,
    })
}

/// Replaces the time sources and reloads chrony.
///
/// Validated before writing, like the firmware sources and for a sharper
/// reason: these lines go into another daemon's config file, so a value
/// carrying a newline would append directives of its own.
async fn set_ntp(query: Query) -> LegacyResponse {
    let Some(body) = query.get("servers") else {
        return LegacyResponse::bad_request(
            "Missing `servers` parameter: a comma-separated list, or empty for the image default",
        );
    };

    let servers: Vec<String> = body
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    if let Err(e) = crate::app::ntp::validate_all(&servers) {
        return LegacyResponse::bad_request(e);
    }

    match crate::app::ntp::store(&servers).await {
        Ok(()) => LegacyResponse::Success(None),
        // A reload that failed after a successful write is not a failed
        // write, and reporting it as one invites the caller to try again
        // against a board that is already correct.
        Err(e) => LegacyResponse::Error(StatusCode::INTERNAL_SERVER_ERROR, e.into()),
    }
}

/// The configured firmware sources.
async fn get_firmware_sources() -> impl Into<LegacyResponse> {
    json!(firmware_sources::load().await)
}

/// Replaces the configured sources.
///
/// Validated before it is written, not at install time: a list that saves and
/// then fails on use is much harder to understand than one refused as it is
/// written. The commonest mistake -- an http source pointed at a `.tpu` rather
/// than the directory holding version folders -- would otherwise list nothing,
/// and an empty list is indistinguishable from a source with no new versions.
async fn set_firmware_sources(query: Query) -> LegacyResponse {
    if !firmware_sources::storage_available() {
        return LegacyResponse::Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no overlay mounted, so sources cannot be stored".into(),
        );
    }

    let Some(body) = query.get("sources") else {
        return LegacyResponse::bad_request("Missing `sources` parameter");
    };

    let parsed: firmware_sources::Sources = match serde_json::from_str(body) {
        Ok(parsed) => parsed,
        Err(e) => return LegacyResponse::bad_request(format!("`sources` is not valid: {e}")),
    };

    if let Err(e) = firmware_sources::validate(&parsed) {
        return LegacyResponse::bad_request(e);
    }

    match firmware_sources::store(&parsed).await {
        Ok(()) => LegacyResponse::ok(json!(parsed)),
        Err(e) => LegacyResponse::Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not store the sources: {e}").into(),
        ),
    }
}

/// What every enabled source is offering.
///
/// `refresh=1` skips the cache, which is what a "check now" control needs: a
/// page that can only report what it thought half an hour ago cannot confirm a
/// release published since.
async fn get_firmware_available(query: Query) -> impl Into<LegacyResponse> {
    let force = query.contains_key("refresh");
    json!(firmware_catalog::get(force).await)
}

/// Stages a chosen version from a configured source.
///
/// Delegates to `tpi-selfupdate`, which downloads, verifies against the
/// publisher's `SHA256SUMS` where one exists, checks the image fits the UBI
/// slot, and arms `nextboot`. Reimplementing that here would give the
/// interface a second install path free to disagree with the command line --
/// and the command line is what a person falls back to when the interface is
/// the thing that broke.
///
/// Local candidates do NOT come through here: those are a file that already
/// exists, and the existing transfer endpoint installs them without a
/// download.
async fn install_firmware(query: Query) -> LegacyResponse {
    let Some(source_id) = query.get("source") else {
        return LegacyResponse::bad_request("Missing `source` parameter");
    };
    let Some(version) = query.get("version") else {
        return LegacyResponse::bad_request("Missing `version` parameter");
    };

    let sources = firmware_sources::load().await;
    let Some(source) = sources.sources.iter().find(|s| &s.id == source_id) else {
        return LegacyResponse::bad_request(format!("no source called {source_id:?}"));
    };

    // The same refusal the transfer endpoint makes, for the same reason:
    // osupdate writes into the volume nextboot points at.
    if !query.contains_key("force") {
        let slots = get_firmware_slots(firmware_version().await).await;
        if slots.update_staged == Some(true) {
            return LegacyResponse::Error(
                StatusCode::CONFLICT,
                "an update is already staged for the next boot; reboot to take it, \
                 or pass force=1 to replace it"
                    .into(),
            );
        }
    }

    let mut args: Vec<String> = vec!["--tag".into(), version.clone()];
    match source.kind {
        firmware_sources::SourceKind::Github => {
            args.push("--repo".into());
            args.push(source.location.clone());
        }
        firmware_sources::SourceKind::Http => {
            args.push("--url".into());
            args.push(source.location.clone());
        }
        firmware_sources::SourceKind::Local => {
            return LegacyResponse::bad_request(
                "a local image is installed through opt=set&type=firmware with local=1",
            );
        }
    }
    // Installing a version that is not newer is a deliberate act the caller
    // has already been warned about; the updater refuses it otherwise.
    if query.contains_key("allow_downgrade") {
        args.push("--allow-downgrade".into());
    }

    let result = tokio::task::spawn_blocking(move || {
        std::process::Command::new("/sbin/tpi-selfupdate")
            .args(&args)
            .output()
    })
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => LegacyResponse::ok(json!({
            "staged": version,
            "source": source_id,
        })),
        Ok(Ok(output)) => {
            // The updater logs to stderr; its last line is the reason it
            // stopped, and is far more useful than an exit code.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr
                .lines()
                .last()
                .unwrap_or("no output")
                .trim()
                .to_string();
            LegacyResponse::Error(StatusCode::BAD_REQUEST, detail.into())
        }
        Ok(Err(e)) => LegacyResponse::Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot run the updater: {e}").into(),
        ),
        Err(e) => LegacyResponse::Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("install task failed: {e}").into(),
        ),
    }
}

/// Whether a newer firmware release exists on either channel.
///
/// Answered by `tpi-selfupdate --check --json`, which already owns release
/// resolution, rather than by a second implementation here that would be
/// free to disagree with the updater that actually performs the upgrade.
/// Cached, because unauthenticated GitHub allows 60 requests an hour and a
/// browser left on the firmware page would spend them.
async fn get_update_check() -> impl Into<LegacyResponse> {
    json!(update_check::get().await)
}

/// Hands the metrics token to an administrator so a scrape can be configured.
///
/// Reachable only through `/api/bmc`, which means only a shadow account can
/// read it -- and such an account can already power a node off and flash the
/// firmware, so handing it this secret grants nothing it did not have. The
/// point of the token is the reverse direction: what the token can do is
/// read `/metrics` and nothing else.
///
/// Generates one on first read rather than at install time, so a board that
/// is never scraped never carries a credential.
async fn get_metrics_token() -> LegacyResponse {
    if !metrics_token::storage_available() {
        return LegacyResponse::Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no overlay mounted, so a metrics token cannot be stored".into(),
        );
    }
    match metrics_token::ensure().await {
        Ok(token) => LegacyResponse::ok(json!({
            "username": metrics_token::TOKEN_USERNAME,
            "token": token.token,
            "created_at": token.created_at,
            "path": metrics_token::TOKEN_PATH,
        })),
        Err(e) => LegacyResponse::Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not read or create the metrics token: {e}").into(),
        ),
    }
}

/// Replaces the metrics token. The previous one stops working immediately --
/// that is what rotation means, and it is why this is a separate credential:
/// doing it cannot lock anyone out of the web interface, and rotating the
/// root password cannot break a scrape.
async fn rotate_metrics_token() -> LegacyResponse {
    if !metrics_token::storage_available() {
        return LegacyResponse::Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no overlay mounted, so a metrics token cannot be stored".into(),
        );
    }
    match metrics_token::rotate().await {
        Ok(token) => LegacyResponse::ok(json!({
            "username": metrics_token::TOKEN_USERNAME,
            "token": token.token,
            "created_at": token.created_at,
            "rotated": true,
        })),
        Err(e) => LegacyResponse::Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not rotate the metrics token: {e}").into(),
        ),
    }
}

async fn get_about() -> impl Into<LegacyResponse> {
    let bmcd_version = env!("CARGO_PKG_VERSION");
    let build_time = build_time::build_time_utc!("%Y-%m-%d %H:%M:%S-00:00");

    let mut buildroot = "unknown".to_string();
    let mut version = "unknown".to_string();

    if let Ok(os_release) = read_os_release().await {
        if let Some(release) = buildroot_release(&os_release) {
            buildroot = release;
        }
        if let Some(ver) = os_release.get("VERSION") {
            version = ver.to_string();
        }
    }

    let hostname = read_hostname().await.unwrap_or_default();
    // A board that cannot read this still has an About page worth rendering,
    // so an unreadable kernel is "unknown" rather than a failed request.
    let kernel = read_kernel_release()
        .await
        .unwrap_or_else(|_| "unknown".to_string());
    let (board_model, board_revision, board_serial) = read_board_info().await.unwrap_or_default();

    json!(
        {
            "board_model": board_model,
            "board_revision": board_revision,
            "board_serial": board_serial,
            "hostname": hostname,
            "api": API_VERSION,
            "version": version,
            "bmcd_version": bmcd_version,
            // feeds the "Build version" field of the web UI about page, which
            // renders "vundefined" without it.
            "build_version": bmcd_version,
            "buildtime": build_time,
            "buildroot": buildroot,
            // The one field an operator wants after a kernel bump, and the
            // only way to answer it before this was `uname -r` over SSH.
            "kernel": kernel,
        }
    )
}

async fn get_info() -> impl Into<LegacyResponse> {
    let storage = get_storage_info();
    let ips = get_net_interfaces().await;
    json!(
        {
            "ip": ips,
            "storage": storage,
        }
    )
}

/// The A/B firmware slots: which UBI volume the board booted from, which one
/// a rollback would land on, whether an update is staged for the next boot,
/// and what the promotion script last said.
///
/// Not `type=firmware`. That name has been taken since before this fork by
/// the transfer machinery -- `opt=get&type=firmware` is the status of a
/// running firmware upload -- and a client polling an upgrade it started must
/// keep getting that answer.
async fn get_firmware_slot_info() -> impl Into<LegacyResponse> {
    json!(get_firmware_slots(firmware_version().await).await)
}

/// The condition of the BMC itself: uptime, load, memory, what is left of
/// the NAND, and whether the board knows what time it is. Always a 200 -- a
/// board that can answer none of it says so field by field.
async fn get_health_info() -> impl Into<LegacyResponse> {
    json!(get_health().await)
}

/// Link state of the on-board Ethernet switch. Read-only, and read straight
/// from `/sys/class/net`: the switch driver registers a netdev per port, so
/// the kernel already has all of this and the daemon simply never passed it
/// on.
///
/// Every port is always listed, absent ones included. A kernel where the
/// switch driver does not probe leaves the BMC perfectly reachable over its
/// own interface while all four compute modules are cut off, and the only
/// visible difference is that these six netdevs are not there.
async fn get_network_info() -> impl Into<LegacyResponse> {
    json!({ "ports": get_switch_ports().await })
}

/// Every temperature the kernel can read and every cooling device it can
/// drive. Always a 200: a board with neither answers with two empty lists,
/// which is a fact about the board and not a failure of the request.
async fn get_thermal_info() -> impl Into<LegacyResponse> {
    json!(get_thermal_state().await)
}

async fn reboot(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    bmc.reboot(query.contains_key("fel"))
        .await
        .map_err(Into::into)
}

async fn reset_node(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    let node = get_node_param(&query)?;
    Ok(bmc.reset_node(node).await?)
}

async fn usb_boot(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    let node = get_node_param(&query)?;
    bmc.usb_boot(node, true).await.map_err(Into::into)
}

fn clear_usb_boot(bmc: &BmcApplication) -> impl Into<LegacyResponse> {
    bmc.clear_usb_boot().context("clear USB boot mode")
}

async fn reset_network(bmc: &BmcApplication) -> impl Into<LegacyResponse> {
    bmc.rtl_reset().await.context("reset network switch")
}

fn set_node_info() -> impl Into<LegacyResponse> {
    // In previous versions of the firmware this was dead code
    LegacyResponse::not_implemented("Method type `set` on parameter `nodeinfo` is deprecated")
}

fn get_node_info(_bmc: &BmcApplication) -> impl Into<LegacyResponse> {
    // TODO: implement serial listening in BmcApplication
    let (n1, n2, n3, n4) = (0, 0, 0, 0);

    json! {
       [{
            "node1": n1,
            "node2": n2,
            "node3": n3,
            "node4": n4,
       }]
    }
}
async fn set_node_aux_info(
    bmc: web::Data<BmcApplication>,
    payload: web::Json<HashMap<NodeId, NodeInfo>>,
) -> impl Responder {
    bmc.set_node_info(payload.into_inner()).await?;
    Ok::<Null, LegacyResponse>(Null)
}

async fn get_node_aux_info(bmc: &BmcApplication) -> LegacyResult<serde_json::Value> {
    let infos = bmc.get_node_infos().await?;
    Ok(serde_json::to_value(infos)?)
}

async fn set_node_to_msd(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    let node = get_node_param(&query)?;
    bmc.node_in_msd(node).await?;
    Ok(())
}

async fn read_os_release() -> std::io::Result<HashMap<String, String>> {
    let buffer = tokio::fs::read("/etc/os-release").await?;
    let mut lines = buffer.lines();
    let mut results = HashMap::new();

    while let Some(line) = lines.next_line().await? {
        if let Some((key, value)) = line.split_once('=') {
            results.insert(key.to_string(), value.to_string());
        }
    }
    Ok(results)
}

/// The firmware version of the running image, out of /etc/os-release. Same
/// key `get_about` sends as `version`, with the quotes os-release puts around
/// a value stripped -- `get_about` passes them through, and something may be
/// matching on that, so it is left as it is.
pub(crate) async fn firmware_version() -> Option<String> {
    let os_release = read_os_release().await.ok()?;
    os_release
        .get("VERSION")
        .map(|version| version.trim_matches('"').to_string())
}

/// The Buildroot release the image was built from, out of a parsed
/// /etc/os-release.
///
/// `PRETTY_NAME` carries the Turing Pi firmware release, not the Buildroot
/// one, so a board built on Buildroot 2025.02.17 reported "Turing Pi v2.2.0"
/// as its buildroot release. post_build.sh writes the actual release to
/// `BUILDROOT_VERSION`; images built before that key existed still only have
/// `PRETTY_NAME` to offer.
fn buildroot_release(os_release: &HashMap<String, String>) -> Option<String> {
    os_release
        .get("BUILDROOT_VERSION")
        .or_else(|| os_release.get("PRETTY_NAME"))
        .map(|release| release.trim_matches('"').to_string())
}

/// The kernel release, as `uname -r` reports it -- `6.12.109`.
///
/// `/proc/sys/kernel/osrelease` is the sibling of the hostname file read
/// below and holds exactly the release, so there is nothing to parse. The
/// alternative, `/proc/version`, wraps the same string in a banner carrying
/// the build host and a build timestamp; an About page needs neither, and
/// `/info` has already been one unauthenticated surface too many.
async fn read_kernel_release() -> io::Result<String> {
    let release = tokio::fs::read_to_string("/proc/sys/kernel/osrelease")
        .await?
        .trim_end_matches(['\0', '\n'])
        .to_string();

    Ok(release)
}

async fn read_hostname() -> io::Result<String> {
    let hostname = tokio::fs::read_to_string("/proc/sys/kernel/hostname")
        .await?
        .trim_end_matches(['\0', '\n'])
        .to_string();

    Ok(hostname)
}

/// Model, hardware revision and factory serial, all three out of the board's
/// 24c02 EEPROM at i2c 0x50. `board_info` already reads and lays out that
/// EEPROM for the first two; the serial is the next field along in the same
/// 50-byte header, so this is one more `value_of` and not a second reader.
pub(crate) async fn read_board_info() -> io::Result<(String, String, Option<String>)> {
    let info = ::board_info::BoardInfo::load()?;
    let board_model = info.value_of(&BoardInfoAttribute::ProductName);
    let board_revision = info.value_of(&BoardInfoAttribute::HwVersion);
    let board_serial = trim_eeprom_field(info.value_of(&BoardInfoAttribute::FactorySerial));
    Ok((board_model, board_revision, board_serial))
}

/// Strips the padding off a fixed-width EEPROM text field. The factory writes
/// 16 bytes whatever the serial is, padded with NUL, and an erased EEPROM
/// reads back as 0xff, which `from_utf8_lossy` turns into replacement
/// characters. A field with nothing left after that is `None`: a board whose
/// EEPROM was never programmed should say it has no serial, not report a
/// string of padding as one.
fn trim_eeprom_field(value: String) -> Option<String> {
    let trimmed = value.trim_matches(|c: char| c == '\0' || c == '\u{fffd}' || c.is_whitespace());
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// function is here for backwards compliance. Data is mostly a duplication of [`get_about`]
async fn get_system_information() -> impl Into<LegacyResponse> {
    let build_time = build_time::build_time_utc!("%Y-%m-%d %H:%M:%S-00:00");
    let ipv4 = get_ipv4_address().unwrap_or("Unknown".to_owned());
    let mac = get_mac_address("br0").await;

    let mut info = json!(
        {
            "api": API_VERSION,
            "buildtime": build_time,
            "ip": ipv4,
            "mac": mac,
        }
    );

    if let Ok(os_release) = read_os_release().await {
        let obj = info.as_object_mut().unwrap();
        if let Some(release) = buildroot_release(&os_release) {
            obj.insert(
                "buildroot".to_string(),
                serde_json::value::to_value(release).unwrap(),
            );
        }
        if let Some(build_version) = os_release.get("VERSION") {
            obj.insert(
                "version".to_string(),
                serde_json::value::to_value(build_version).unwrap(),
            );
        }
    }

    json!([info])
}

async fn set_node_power(bmc: &BmcApplication, query: Query) -> LegacyResponse {
    let mut mask = 0;
    let mut states = 0;

    for idx in 0..4 {
        let param = format!("node{}", idx + 1);
        let req_status = match query.get(&param).map(String::as_str) {
            Some("0") => false,
            Some("1") => true,
            Some(x) => {
                let msg = format!("Invalid value `{}` for parameter `{}`", x, param);
                return (StatusCode::BAD_REQUEST, msg).into();
            }
            None => continue,
        };
        let bit = 1 << idx;

        mask |= bit;

        if req_status {
            states |= bit;
        }
    }

    bmc.activate_slot(states, mask)
        .await
        .context("set power state")
        .into()
}

async fn get_node_power(bmc: &BmcApplication) -> impl Into<LegacyResponse> {
    let n1 = get_node_power_status(bmc, NodeId::Node1).await;
    let n2 = get_node_power_status(bmc, NodeId::Node2).await;
    let n3 = get_node_power_status(bmc, NodeId::Node3).await;
    let n4 = get_node_power_status(bmc, NodeId::Node4).await;

    json!(
     [{
        "node1": n1,
        "node2": n2,
        "node3": n3,
        "node4": n4,
    }]
    )
}

async fn get_node_power_status(bmc: &BmcApplication, node: NodeId) -> String {
    let Ok(status) = bmc.get_node_power(node).await else {
        return "Unknown".to_owned();
    };

    u8::from(status).to_string()
}

fn format_sdcard() -> impl Into<LegacyResponse> {
    LegacyResponse::not_implemented("microSD card formatting is not implemented")
}

/// function is here for backwards compliance. Data is mostly a duplication of [`get_info`]
fn get_sdcard_info() -> LegacyResponse {
    match get_fs_stat("/mnt/sdcard") {
        Ok((total, free)) => {
            let used = total - free;
            json!(
                 [{
                    "total": total,
                    "use": used,
                    "free": free,
                }]
            )
            .into()
        }
        Err(_) => (
            StatusCode::BAD_REQUEST,
            "Failed to get microSD card info: {}",
        )
            .into(),
    }
}

async fn set_node1_usb_mode(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    bmc.set_node1_usb_route(query.contains_key("alternative_port"))
        .await
        .map_err(Into::into)
}

async fn get_node1_usb_mode(bmc: &BmcApplication) -> LegacyResponse {
    LegacyResponse::ok(bmc.get_node1_usb_route().await.into())
}

/// switches the USB configuration.
/// API values are mapped to the `UsbConfig` as followed:
///
/// | i32 | Mode         | Route |
/// |-----|--------------|-------|
/// | 0   | Host         | USB-A |
/// | 1   | Device       | USB-A |
/// | 2   | Flash host   | USB-A |
/// | 3   | Flash device | USB-A |
/// | 4   | Host         | BMC   |
/// | 5   | Device       | BMC   |
/// | 6   | Flash host   | BMC   |
/// | 7   | Flash device | BMC   |
///
async fn set_usb_mode(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    let node = get_node_param(&query)?;
    let mode_str = query
        .get("mode")
        .ok_or(LegacyResponse::bad_request("Missing `mode` parameter"))?;

    let mode_num = i32::from_str(mode_str)
        .map_err(|_| LegacyResponse::bad_request("Parameter `mode` is not a number"))?;

    let mode = UsbMode::from_api_mode(mode_num);

    let route = if (mode_num >> 2) & 0x1 == 1 {
        UsbRoute::Bmc
    } else {
        UsbRoute::AlternativePort
    };

    let cfg = match (mode, route) {
        (UsbMode::Device, UsbRoute::AlternativePort) => UsbConfig::UsbA(node),
        (UsbMode::Device, UsbRoute::Bmc) => UsbConfig::Bmc(node),
        (UsbMode::Host, route) => UsbConfig::Node(node, route),
        (UsbMode::Flash, route) => UsbConfig::Flashing(node, route),
    };

    bmc.configure_usb(cfg)
        .await
        .context("set USB mode")
        .map_err(Into::into)
}

/// gets the USB configuration from the POV of the configured node.
async fn get_usb_mode(bmc: &BmcApplication) -> impl Into<LegacyResponse> {
    let (config, bus_type) = bmc.get_usb_mode().await;

    let (node, mode, route) = match config {
        UsbConfig::UsbA(node) => (node, UsbMode::Device, UsbRoute::AlternativePort),
        UsbConfig::Bmc(node) => (node, UsbMode::Device, UsbRoute::Bmc),
        UsbConfig::Node(node, route) => (node, UsbMode::Host, route),
        UsbConfig::Flashing(node, route) => (node, UsbMode::Flash, route),
    };

    json!(
        [{
            "mode": mode,
            "node": node.to_string(),
            "route": route,
            "bus_type": bus_type,
        }]
    )
}

async fn set_cooling_info(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    let device = query
        .get("device")
        .ok_or(LegacyResponse::bad_request("Missing `device` parameter"))?;
    let speed_str = query
        .get("speed")
        .ok_or(LegacyResponse::bad_request("Missing `speed` parameter"))?;

    // check if the speed is a valid number within the range of the device
    let speed = c_ulong::from_str(speed_str)
        .map_err(|_| LegacyResponse::bad_request("`speed` parameter is not a number"))?;

    // set the speed
    bmc.set_cooling_speed(device, speed)
        .await
        .context("set Cooling state")
        .map_err(Into::into)
}

async fn get_cooling_info() -> LegacyResult<serde_json::Value> {
    let info = BmcApplication::get_cooling_devices().await?;
    Ok(json!(info))
}

async fn handle_flash_status(flash: web::Data<StreamingDataService>) -> LegacyResult<String> {
    Ok(serde_json::to_string(flash.status().await.deref())?)
}

async fn handle_transfer_request(
    ss: web::Data<StreamingDataService>,
    bmc: web::Data<BmcApplication>,
    query: Query,
) -> LegacyResult<String> {
    let (process_name, upgrade_command) = match query.get("type").map(|c| c.as_str()) {
        // Park the image instead of installing it: write it to the card and
        // stop. Nothing is staged, so none of the refusals below apply -- a
        // board with an image already armed can still be given another to
        // choose from later.
        Some("firmware") if query.contains_key("park") => {
            ("firmware park service".to_string(), UpgradeCommand::OsPark)
        }
        Some("firmware") => {
            // Refuse to stage a second image over one that is already armed.
            //
            // `osupdate` writes into the volume `nextboot` points at, so
            // starting a second upgrade destroys the image the board is about
            // to boot while leaving the environment pointing at it -- the one
            // state in this whole design with nothing to fall back to.
            // `tpi-selfupdate` has always refused it; the API had not, so the
            // web interface could do what the command line would not.
            //
            // `force` exists because the refusal must not become a trap: a
            // board whose staged image is known bad needs a way forward that
            // is not a reboot onto it.
            if !query.contains_key("force") {
                let slots = get_firmware_slots(firmware_version().await).await;
                if slots.update_staged == Some(true) {
                    let staged = slots
                        .staged
                        .as_ref()
                        .and_then(|s| s.version.clone())
                        .unwrap_or_else(|| "an image".to_string());
                    return Err(LegacyResponse::Error(
                        StatusCode::CONFLICT,
                        format!(
                            "{staged} is already staged for the next boot; reboot to take it, \
                             or pass force=1 to replace it"
                        )
                        .into(),
                    ));
                }
            }
            (
                "firmware upgrade service".to_string(),
                UpgradeCommand::OsUpgrade,
            )
        }
        Some("flash") => {
            let node = get_node_param(&query)?;
            (
                format!("{node} os install service"),
                UpgradeCommand::Module(node, bmc.clone().into_inner()),
            )
        }
        _ => {
            return Err(LegacyResponse::bad_request(
                "`type` should equal 'firmware' or 'flash'",
            ))
        }
    };

    let data_transfer = create_data_transfer(&query).await?;
    let do_crc = !query.contains_key("skip_crc");
    let transfer_request =
        InitializeTransfer::new(process_name, upgrade_command, data_transfer, do_crc);

    let handle = ss.request_transfer(transfer_request.try_into()?).await?;
    let json = json!({"handle": handle});
    Ok(json.to_string())
}

async fn create_data_transfer(query: &Query) -> LegacyResult<DataTransfer> {
    let file = query.get("file").ok_or(LegacyResponse::bad_request(
        "Invalid `file` query parameter",
    ))?;

    if query.contains_key("local") {
        return Ok(DataTransfer::local(PathBuf::from(file)));
    }

    let sha256 = try_map_sha256(query.get("sha256"))?;

    if file.starts_with("http") {
        let url = reqwest::Url::parse(file).map_err(|e| {
            LegacyResponse::bad_request(format!(
                "{file} could not be parsed to a url object: {:#}",
                e
            ))
        })?;
        return Ok(DataTransfer::url(url, sha256).await?);
    }

    let size = query.get("length").ok_or((
        StatusCode::LENGTH_REQUIRED,
        "Invalid `length` query parameter",
    ))?;

    let size = u64::from_str(size)
        .map_err(|_| LegacyResponse::bad_request("`length` parameter is not a number"))?;

    Ok(DataTransfer::remote(PathBuf::from(&file), size, 16, sha256))
}

pub fn try_map_sha256(value: Option<&String>) -> LegacyResult<Option<bytes::Bytes>> {
    let sha = if let Some(sha256) = value {
        let bytes = hex::decode(sha256)
            .map_err(|e| {
                LegacyResponse::bad_request(format!(
                    "`sha256` parameter contains invalid hex values: {}",
                    e
                ))
            })?
            .into();
        Some(bytes)
    } else {
        None
    };

    Ok(sha)
}

#[get("/upload/{handle}/cancel")]
async fn cancel_file_upload(ss: web::Data<StreamingDataService>) -> impl Responder {
    ss.cancel_all().await;
    HttpResponse::Ok().finish()
}

#[post("/upload/{handle}")]
async fn handle_file_upload(
    handle: web::Path<u32>,
    ss: web::Data<StreamingDataService>,
    mut payload: Multipart,
) -> impl Responder {
    let (sender, size) = ss.take_sender(*handle).await?;
    let Some(Ok(mut field)) = payload.next().await else {
        return Err(LegacyResponse::bad_request("Multipart form invalid"));
    };

    let mut bytes_send: u64 = 0;
    while let Some(Ok(chunk)) = field.next().await {
        let length = chunk.len();
        if sender.send(chunk).await.is_err() {
            return Err(return_transfer_error(ss).await.into());
        }

        bytes_send += length as u64;
    }

    if bytes_send != size {
        ss.cancel_all().await;
        return Err(LegacyResponse::bad_request(format!(
            "missing {} bytes",
            format_size(size - bytes_send, DECIMAL)
        )));
    }

    Ok(Null)
}

/// When the channel gets dropped, give the worker some time to shutdown so that the
/// actual error message can be bubbled up.
async fn return_transfer_error(ss: web::Data<StreamingDataService>) -> impl Into<LegacyResponse> {
    let msg = ss.try_get_error(Duration::from_secs(5)).await;
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        msg.unwrap_or("transfer canceled".to_string()),
    )
}

#[cfg(test)]
mod test {

    use super::*;
    use crate::app::thermal_info::{Cooler, Thermal, ThermalSensor};

    #[test]
    fn buildroot_release_prefers_the_buildroot_key() {
        let os_release = HashMap::from([
            (
                "PRETTY_NAME".to_string(),
                "\"Turing Pi v2.2.0\"".to_string(),
            ),
            (
                "BUILDROOT_VERSION".to_string(),
                "\"2025.02.17\"".to_string(),
            ),
        ]);
        assert_eq!(
            buildroot_release(&os_release),
            Some("2025.02.17".to_string())
        );
    }

    #[test]
    fn buildroot_release_falls_back_to_the_pretty_name() {
        let os_release = HashMap::from([(
            "PRETTY_NAME".to_string(),
            "\"Turing Pi v2.2.0\"".to_string(),
        )]);
        assert_eq!(
            buildroot_release(&os_release),
            Some("Turing Pi v2.2.0".to_string())
        );
        assert_eq!(buildroot_release(&HashMap::new()), None);
    }

    #[test]
    fn eeprom_fields_lose_their_padding() {
        assert_eq!(
            trim_eeprom_field("XZCT250200139\0\0\0".to_string()),
            Some("XZCT250200139".to_string())
        );
        assert_eq!(
            trim_eeprom_field("TuringPi2\0\0\0\0\0\0\0".to_string()),
            Some("TuringPi2".to_string())
        );
        // never programmed: all NUL, or an erased EEPROM read as 0xff
        assert_eq!(trim_eeprom_field("\0".repeat(16)), None);
        assert_eq!(trim_eeprom_field("\u{fffd}".repeat(16)), None);
    }

    use crate::app::firmware_info::{FirmwareSlots, Promotion, Slot, StagedImage};

    /// The exact bytes `opt=get&type=firmware_slots` puts on the wire for the
    /// board as it reads today: `rootfs` running out of volume 1 with an
    /// image of 37019648 bytes, `rootfs_prev` waiting in volume 3, nothing
    /// staged. `firmware_info`'s own tests cover the read that produces this
    /// value; this one pins the shape a client sees.
    ///
    /// The keys are sorted rather than declaration-ordered because
    /// `LegacyResponse` carries its body as a `serde_json::Value` and
    /// serde_json's `preserve_order` feature is off, so every endpoint in
    /// this daemon answers with sorted keys.
    #[actix_web::test]
    async fn firmware_slots_answer_with_both_volumes() {
        let slots = FirmwareSlots {
            present: true,
            running: Some(Slot {
                volume: "rootfs".to_string(),
                volume_id: 1,
                size_bytes: Some(37019648),
                version: Some("v2.2.0-unstable-hive.5".to_string()),
            }),
            rollback: Some(Slot {
                volume: "rootfs_prev".to_string(),
                volume_id: 3,
                size_bytes: Some(37011456),
                version: None,
            }),
            update_staged: Some(false),
            nextboot: None,
            last_promotion: Some(Promotion {
                timestamp: "Mon Sep  7 19:30:22 UTC 2026".to_string(),
                message: "switch ports present: node1 node2 node3 node4".to_string(),
            }),
            staged: Some(StagedImage {
                version: Some("v2.4.0".to_string()),
                sha256: Some("00707f1f".to_string()),
                staged_at: Some("2026-09-08T03:03:30Z".to_string()),
                source: Some("tpi-selfupdate".to_string()),
                file: None,
            }),
        };

        let response = HttpResponse::from(LegacyResponse::from(json!(slots)));
        assert_eq!(response.status(), StatusCode::OK);

        let body = actix_web::body::to_bytes(response.into_body())
            .await
            .expect("body");
        assert_eq!(
            std::str::from_utf8(&body).expect("utf8"),
            concat!(
                r#"{"response":[{"result":{"#,
                r#""last_promotion":{"message":"switch ports present: node1 node2 node3 node4","#,
                r#""timestamp":"Mon Sep  7 19:30:22 UTC 2026"},"#,
                r#""nextboot":null,"present":true,"#,
                r#""rollback":{"size_bytes":37011456,"version":null,"volume":"rootfs_prev","volume_id":3},"#,
                r#""running":{"size_bytes":37019648,"version":"v2.2.0-unstable-hive.5","volume":"rootfs","volume_id":1},"#,
                r#""staged":{"file":null,"sha256":"00707f1f","source":"tpi-selfupdate","#,
                r#""staged_at":"2026-09-08T03:03:30Z","version":"v2.4.0"},"#,
                r#""update_staged":false"#,
                r#"}}]}"#,
            )
        );
    }

    /// A board with no UBI, no `fw_printenv` and no promotion log -- an older
    /// firmware, or a board that boots from something else entirely. 200 and
    /// nulls, never a 500, and never an invented slot: `update_staged` is
    /// null rather than false because "no update is staged" and "the U-Boot
    /// environment could not be read" are different answers.
    #[actix_web::test]
    async fn firmware_slots_answer_200_when_there_is_no_ubi() {
        let slots = FirmwareSlots {
            present: false,
            running: None,
            rollback: None,
            update_staged: None,
            nextboot: None,
            last_promotion: None,
            staged: None,
        };

        let response = HttpResponse::from(LegacyResponse::from(json!(slots)));
        assert_eq!(response.status(), StatusCode::OK);

        let body = actix_web::body::to_bytes(response.into_body())
            .await
            .expect("body");
        assert_eq!(
            std::str::from_utf8(&body).expect("utf8"),
            concat!(
                r#"{"response":[{"result":{"last_promotion":null,"nextboot":null,"#,
                r#""present":false,"rollback":null,"running":null,"staged":null,"#,
                r#""update_staged":null}}]}"#,
            )
        );
    }

    use crate::app::health_info::{Clock, Health, Load, Memory, Nand, Rtc};

    /// The exact bytes `opt=get&type=health` puts on the wire. The uptime,
    /// the memory total and the four NAND counts are the board's own numbers;
    /// the rest is shaped like it. `health_info`'s tests cover the reads that
    /// produce this value.
    ///
    /// Worth seeing before writing a client: a microsecond offset comes out
    /// as `-3.077e-6`. serde_json prints the shortest round-trip form of an
    /// `f64`, which for small magnitudes is scientific notation -- valid
    /// JSON, and not what a hand-written parser always expects.
    #[actix_web::test]
    async fn health_answers_with_the_boards_condition() {
        let health = Health {
            uptime_seconds: Some(172.43),
            load: Load {
                present: true,
                one_minute: Some(0.08),
                five_minutes: Some(0.03),
                fifteen_minutes: Some(0.01),
            },
            memory: Memory {
                present: true,
                total_bytes: Some(121634816),
                free_bytes: Some(20971520),
                available_bytes: Some(62914560),
            },
            nand: Nand {
                present: true,
                total_eraseblocks: Some(2040),
                available_eraseblocks: Some(5),
                bad_eraseblocks: Some(0),
                reserved_eraseblocks: Some(40),
                eraseblock_size_bytes: Some(126976),
                available_bytes: Some(634880),
            },
            clock: Clock {
                rtc: vec![
                    Rtc {
                        device: "rtc0".to_string(),
                        name: Some("sun6i-rtc".to_string()),
                    },
                    Rtc {
                        device: "rtc1".to_string(),
                        name: Some("pcf8563".to_string()),
                    },
                ],
                synchronised: Some(true),
                source: Some("192.168.77.1".to_string()),
                stratum: Some(3),
                offset_seconds: Some(-0.000003077),
                measured_by: Some("chronyc tracking".to_string()),
            },
        };

        let response = HttpResponse::from(LegacyResponse::from(json!(health)));
        assert_eq!(response.status(), StatusCode::OK);

        let body = actix_web::body::to_bytes(response.into_body())
            .await
            .expect("body");
        assert_eq!(
            std::str::from_utf8(&body).expect("utf8"),
            concat!(
                r#"{"response":[{"result":{"#,
                r#""clock":{"measured_by":"chronyc tracking","offset_seconds":-3.077e-6,"#,
                r#""rtc":[{"device":"rtc0","name":"sun6i-rtc"},{"device":"rtc1","name":"pcf8563"}],"#,
                r#""source":"192.168.77.1","stratum":3,"synchronised":true},"#,
                r#""load":{"fifteen_minutes":0.01,"five_minutes":0.03,"one_minute":0.08,"present":true},"#,
                r#""memory":{"available_bytes":62914560,"free_bytes":20971520,"present":true,"total_bytes":121634816},"#,
                r#""nand":{"available_bytes":634880,"available_eraseblocks":5,"bad_eraseblocks":0,"#,
                r#""eraseblock_size_bytes":126976,"present":true,"reserved_eraseblocks":40,"total_eraseblocks":2040},"#,
                r#""uptime_seconds":172.43"#,
                r#"}}]}"#,
            )
        );
    }

    /// A board that can answer none of it: no `/proc` readings, no UBI, no
    /// RTC, no chrony. 200 and a field-by-field "not there", never a 500 and
    /// never a zero that would read as a measurement.
    #[actix_web::test]
    async fn health_answers_200_when_nothing_can_be_read() {
        let health = Health {
            uptime_seconds: None,
            load: Load {
                present: false,
                one_minute: None,
                five_minutes: None,
                fifteen_minutes: None,
            },
            memory: Memory {
                present: false,
                total_bytes: None,
                free_bytes: None,
                available_bytes: None,
            },
            nand: Nand {
                present: false,
                total_eraseblocks: None,
                available_eraseblocks: None,
                bad_eraseblocks: None,
                reserved_eraseblocks: None,
                eraseblock_size_bytes: None,
                available_bytes: None,
            },
            clock: Clock {
                rtc: Vec::new(),
                synchronised: None,
                source: None,
                stratum: None,
                offset_seconds: None,
                measured_by: None,
            },
        };

        let response = HttpResponse::from(LegacyResponse::from(json!(health)));
        assert_eq!(response.status(), StatusCode::OK);

        let body = actix_web::body::to_bytes(response.into_body())
            .await
            .expect("body");
        assert_eq!(
            std::str::from_utf8(&body).expect("utf8"),
            concat!(
                r#"{"response":[{"result":{"clock":{"measured_by":null,"offset_seconds":null,"rtc":[],"#,
                r#""source":null,"stratum":null,"synchronised":null},"#,
                r#""load":{"fifteen_minutes":null,"five_minutes":null,"one_minute":null,"present":false},"#,
                r#""memory":{"available_bytes":null,"free_bytes":null,"present":false,"total_bytes":null},"#,
                r#""nand":{"available_bytes":null,"available_eraseblocks":null,"bad_eraseblocks":null,"#,
                r#""eraseblock_size_bytes":null,"present":false,"reserved_eraseblocks":null,"total_eraseblocks":null},"#,
                r#""uptime_seconds":null}}]}"#,
            )
        );
    }

    #[actix_web::test]
    async fn test_node_info() {
        let json = serde_json::json! {
            {
                "Node1": {
                    "module_name": "Raspberry Pi CM4"
                },
                "Node3": {
                    "name": "New jeston"
                }
            }
        };
        let _: HashMap<NodeId, NodeInfo> = serde_json::from_value(json).unwrap();
    }

    /// The exact bytes `opt=get&type=thermal` puts on the wire, for the board
    /// as it reads today: the SoC sensor at 52539 millidegrees, and the fan
    /// the kernel drives from it on step 4 of 6 with the duty behind each of
    /// those steps out of the board's device tree. `thermal_info`'s own tests
    /// assert that sysfs and that device tree read back as this `Thermal`, so
    /// between them the chain from the files to the response body is covered.
    ///
    /// The keys come out sorted rather than in the order the structs declare
    /// them. That is not this endpoint's doing: `LegacyResponse` carries its
    /// body as a `serde_json::Value`, whose map is a `BTreeMap` unless
    /// serde_json's `preserve_order` feature is on, and it is not -- so every
    /// endpoint in this daemon has always answered with sorted keys. It is
    /// asserted literally here so that anyone writing a client against this
    /// shape is reading what the daemon actually sends.
    #[actix_web::test]
    async fn thermal_answers_with_sensors_and_cooling() {
        let thermal = Thermal {
            sensors: vec![ThermalSensor {
                name: "bmc-thermal".to_string(),
                temperature_c: Some(52.5),
                present: true,
            }],
            cooling: vec![Cooler {
                name: "pwm-fan".to_string(),
                cur_state: Some(4),
                max_state: Some(6),
                present: true,
                levels: Some(vec![0, 16, 32, 64, 102, 170, 254]),
                max_level: Some(254),
            }],
        };

        let response = HttpResponse::from(LegacyResponse::from(json!(thermal)));
        assert_eq!(response.status(), StatusCode::OK);

        let body = actix_web::body::to_bytes(response.into_body())
            .await
            .expect("body");
        assert_eq!(
            std::str::from_utf8(&body).expect("utf8"),
            concat!(
                r#"{"response":[{"result":{"#,
                r#""cooling":[{"cur_state":4,"levels":[0,16,32,64,102,170,254],"#,
                r#""max_level":254,"max_state":6,"name":"pwm-fan","present":true}],"#,
                r#""sensors":[{"name":"bmc-thermal","present":true,"temperature_c":52.5}]"#,
                r#"}}]}"#,
            )
        );
    }

    /// A board that cannot measure anything -- any image older than the one
    /// that describes the sensor in the device tree, or a v2.4 board with no
    /// fan. Two empty lists and a 200. Never a 500, and never a temperature
    /// that would read as a real zero degrees.
    #[actix_web::test]
    async fn thermal_answers_200_with_empty_lists_when_there_is_nothing_to_read() {
        let thermal = Thermal {
            sensors: Vec::new(),
            cooling: Vec::new(),
        };

        let response = HttpResponse::from(LegacyResponse::from(json!(thermal)));
        assert_eq!(response.status(), StatusCode::OK);

        let body = actix_web::body::to_bytes(response.into_body())
            .await
            .expect("body");
        assert_eq!(
            std::str::from_utf8(&body).expect("utf8"),
            r#"{"response":[{"result":{"cooling":[],"sensors":[]}}]}"#
        );
    }
}
