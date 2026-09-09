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

impl ResponseError for LegacyResponse {}

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
    use crate::app::cooling_device::CoolingRequestError;

    fn status_of(error: anyhow::Error) -> StatusCode {
        match LegacyResponse::from(error) {
            LegacyResponse::Error(status, _) => status,
            other => panic!("an error converted to {other:?}"),
        }
    }

    /// The whole point is that the two are told apart, so both directions are
    /// asserted. One test proving 400 would pass just as well if everything
    /// became 400, which would be a different bug with the same shape.
    #[test]
    fn a_callers_mistake_is_a_400_and_a_board_failure_is_a_500() {
        assert_eq!(
            status_of(CoolingRequestError::NoSuchDevice("nope".into()).into()),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(CoolingRequestError::SpeedTooHigh { given: 9, max: 6 }.into()),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(anyhow::anyhow!("the sysfs write failed")),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// The handlers wrap with `.context(...)`, so the typed error is the root
    /// cause rather than the top of the chain. A check that only looked at
    /// the outermost error would see a plain string and answer 500.
    #[test]
    fn the_status_survives_the_context_the_handler_adds() {
        let wrapped = anyhow::Error::from(CoolingRequestError::NoSuchDevice("nope".into()))
            .context("hold fan at step");
        assert_eq!(status_of(wrapped), StatusCode::BAD_REQUEST);
    }
}
