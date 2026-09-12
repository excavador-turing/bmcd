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
use actix_web::{http::StatusCode, HttpResponse, HttpResponseBuilder, Responder, ResponseError};
use serde_json::json;
use std::{borrow::Cow, fmt::Display};

use crate::serial_service::serial_handler::SerialError;

/// Specifies the different repsonses that this legacy API can return. Implements
/// `From<LegacyResponse>` to enforce the legacy json format in the return body.
#[derive(Debug, PartialEq)]
pub enum LegacyResponse {
    Success(Option<serde_json::Value>),
    Error(StatusCode, Cow<'static, str>),
    UartData(String),
}

impl LegacyResponse {
    pub fn bad_request<S: Into<Cow<'static, str>>>(msg: S) -> Self {
        LegacyResponse::Error(StatusCode::BAD_REQUEST, msg.into())
    }

    pub fn not_implemented<S: Into<Cow<'static, str>>>(msg: S) -> Self {
        LegacyResponse::Error(StatusCode::NOT_IMPLEMENTED, msg.into())
    }

    pub fn stub() -> Self {
        LegacyResponse::Success(None)
    }

    pub fn ok(value: serde_json::Value) -> Self {
        LegacyResponse::Success(Some(value))
    }
}

impl<T: Into<LegacyResponse>, E: Into<LegacyResponse>> From<Result<T, E>> for LegacyResponse {
    fn from(value: Result<T, E>) -> Self {
        value.map_or_else(|e| e.into(), |ok| ok.into())
    }
}

impl From<(StatusCode, &'static str)> for LegacyResponse {
    fn from(value: (StatusCode, &'static str)) -> Self {
        LegacyResponse::Error(value.0, value.1.into())
    }
}

impl From<(StatusCode, String)> for LegacyResponse {
    fn from(value: (StatusCode, String)) -> Self {
        LegacyResponse::Error(value.0, value.1.into())
    }
}

impl From<serde_json::Value> for LegacyResponse {
    fn from(value: serde_json::Value) -> Self {
        LegacyResponse::Success(Some(value))
    }
}

impl From<()> for LegacyResponse {
    fn from(_: ()) -> Self {
        LegacyResponse::Success(None)
    }
}

impl From<anyhow::Error> for LegacyResponse {
    fn from(e: anyhow::Error) -> Self {
        // A caller's mistake is a 400; everything else is a 500. Decided by
        // downcasting to a typed error rather than by reading the message,
        // because the message is for a person and would take the status with
        // it the first time somebody reworded it.
        //
        // The status is load-bearing on the path form, where a refusal is
        // `application/problem+json` and a client branches on it: 500 is the
        // canonical retryable status, so telling a client 500 for "that
        // device does not exist" asks it to retry an answer that will never
        // change.
        let status = if e
            .root_cause()
            .is::<crate::app::cooling_device::CoolingRequestError>()
        {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };

        LegacyResponse::Error(
            status,
            format!("Failed to {}: {}", e, e.root_cause()).into(),
        )
    }
}

impl From<serde_json::Error> for LegacyResponse {
    fn from(value: serde_json::Error) -> Self {
        LegacyResponse::Error(StatusCode::INTERNAL_SERVER_ERROR, value.to_string().into())
    }
}

impl From<SerialError> for LegacyResponse {
    fn from(value: SerialError) -> Self {
        LegacyResponse::Error(StatusCode::INTERNAL_SERVER_ERROR, value.to_string().into())
    }
}

/// A refusal keeps the status it was built with.
///
/// The empty impl this replaces inherited actix's default, which answers 500
/// to everything. That was invisible while nothing returned a `LegacyResponse`
/// as an `Err`: handlers reached by the legacy dispatcher come back through
/// `From<Result<T, E>>` and are rendered by `Responder`, which has always
/// honoured the carried status.
///
/// `api::access` returns `Result<HttpResponse, LegacyResponse>` directly to
/// actix, which routes an `Err` through here — so its refusals all answered
/// 500. Measured on bmc-2 the hour it was flashed: "the current password is
/// wrong" and "the new password is too short" both came back 500, which tells
/// a client the board is broken rather than that the request was.
///
/// The body is built by the same `From<LegacyResponse> for HttpResponse` the
/// success path uses, so a refusal has one shape wherever it came from.
impl ResponseError for LegacyResponse {
    fn status_code(&self) -> StatusCode {
        match self {
            LegacyResponse::Error(status, _) => *status,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_response(&self) -> HttpResponse {
        match self {
            LegacyResponse::Error(status, msg) => {
                LegacyResponse::Error(*status, msg.clone()).into()
            }
            other => {
                LegacyResponse::Error(StatusCode::INTERNAL_SERVER_ERROR, other.to_string().into())
                    .into()
            }
        }
    }
}

impl Display for LegacyResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LegacyResponse::Success(s) => write!(
                f,
                "{}",
                s.as_ref().map(|json| json.to_string()).unwrap_or_default()
            ),
            LegacyResponse::UartData(s) => write!(f, "{}", s),
            LegacyResponse::Error(_, msg) => write!(f, "{}", msg),
        }
    }
}

impl Responder for LegacyResponse {
    type Body = <HttpResponse as Responder>::Body;

    fn respond_to(self, _req: &actix_web::HttpRequest) -> HttpResponse<Self::Body> {
        self.into()
    }
}

pub type LegacyResult<T> = Result<T, LegacyResponse>;

impl From<LegacyResponse> for HttpResponse {
    fn from(value: LegacyResponse) -> Self {
        let (response, result, is_uart) = match value {
            LegacyResponse::Success(None) => (
                StatusCode::OK,
                serde_json::Value::String("ok".to_string()),
                false,
            ),
            LegacyResponse::Success(Some(body)) => (StatusCode::OK, body, false),
            LegacyResponse::UartData(d) => (StatusCode::OK, serde_json::Value::String(d), true),
            LegacyResponse::Error(status_code, msg) => (
                status_code,
                serde_json::Value::String(msg.into_owned()),
                false,
            ),
        };

        let keyname = if is_uart { "uart" } else { "result" };

        let msg = json! {{
            "response": [{ keyname: result }]
        }};

        HttpResponseBuilder::new(response).json(msg)
    }
}

#[derive(Default)]
pub struct Null;

impl Responder for Null {
    type Body = <HttpResponse as Responder>::Body;

    fn respond_to(self, _: &actix_web::HttpRequest) -> HttpResponse<Self::Body> {
        HttpResponse::Ok().into()
    }
}

impl From<()> for Null {
    fn from(_: ()) -> Self {
        Null {}
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    /// A refusal must answer with the status it was built with.
    ///
    /// This is the regression that shipped in 2.36.0 and was caught on a real
    /// board an hour later: every refusal from `api::access` answered 500,
    /// because the `ResponseError` impl was empty and actix's default is 500.
    /// A wrong password reported as a server fault is a client that retries.
    #[test]
    fn a_refusal_keeps_its_own_status() {
        assert_eq!(
            LegacyResponse::bad_request("no").status_code(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            LegacyResponse::Error(StatusCode::FORBIDDEN, "nope".into()).status_code(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            LegacyResponse::not_implemented("later").status_code(),
            StatusCode::NOT_IMPLEMENTED
        );
        // And the rendered response agrees with the declared status, which is
        // the half a client actually sees.
        assert_eq!(
            LegacyResponse::bad_request("no").error_response().status(),
            StatusCode::BAD_REQUEST
        );
    }

    /// Anything that is not an error still reads as one here, because reaching
    /// `ResponseError` at all means it was returned as an `Err`.
    #[test]
    fn a_success_returned_as_an_error_is_a_server_fault() {
        assert_eq!(
            LegacyResponse::Success(None).status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
