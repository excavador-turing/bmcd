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
use humantime::format_duration;
use std::{fmt::Display, str::Utf8Error, time::Duration};
use thiserror::Error;
use tokio::time::Instant;

/// What is left of a ban, in whole seconds, rounded up and never below one.
///
/// This is the number `Retry-After` carries and the number the message quotes,
/// computed once so a client that reads the header and a person who reads the
/// body are never told two different things. RFC 9110 lets `Retry-After` be
/// delta-seconds, which is what this is; it may not be zero or negative, so a
/// ban with milliseconds left is reported as one second rather than as "come
/// back now".
fn ban_seconds_remaining(deadline: Instant) -> u64 {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0);
    seconds.max(1)
}

#[derive(Error, Debug, PartialEq)]
pub enum AuthenticationError {
    #[error("error trying to parse credentials: {0}")]
    ParseError(String),
    #[error("credentials incorrect")]
    IncorrectCredentials,
    #[error("token expired {} ago",
            format_duration(Instant::now().duration_since(*.0)))]
    TokenExpired(Instant),
    #[error("token {0} is not registered")]
    NoMatch(String),
    #[error("cannot parse authorization header: {0}")]
    HttpParseError(String),
    #[error("{0} authentication not supported")]
    SchemeNotSupported(String),
    #[error("no authorization header provided")]
    Empty,
    #[error("Exceeded allowed authentication attempts. Access blocked for {}. \
            The credentials were not checked, so this says nothing about whether \
            they are correct.",
            format_duration(Duration::from_secs(ban_seconds_remaining(*.0))))]
    ExceededAllowedAttempts(Instant),
}

impl From<serde_json::Error> for AuthenticationError {
    fn from(value: serde_json::Error) -> Self {
        Self::ParseError(value.to_string())
    }
}

impl From<base64::DecodeError> for AuthenticationError {
    fn from(value: base64::DecodeError) -> Self {
        Self::ParseError(value.to_string())
    }
}

impl From<Utf8Error> for AuthenticationError {
    fn from(value: Utf8Error) -> Self {
        Self::ParseError(value.to_string())
    }
}

impl AuthenticationError {
    /// Seconds a caller must wait before its credentials will be looked at
    /// again, or `None` for every error that is not a ban.
    ///
    /// A ban is the one authentication failure that is not about the
    /// credential offered, and the one the caller can do something useful
    /// about -- wait. The response code is chosen on this being `Some`, so an
    /// arm added here changes what goes on the wire.
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            Self::ExceededAllowedAttempts(deadline) => Some(ban_seconds_remaining(*deadline)),
            _ => None,
        }
    }

    pub fn into_basic_error(self) -> SchemedAuthError {
        SchemedAuthError(Some(Scheme::Basic), self)
    }

    pub fn into_bearer_error(self) -> SchemedAuthError {
        SchemedAuthError(Some(Scheme::Bearer), self)
    }

    pub fn into_unknown_error(self) -> SchemedAuthError {
        SchemedAuthError(None, self)
    }
}

#[derive(Debug, PartialEq)]
pub enum Scheme {
    Basic,
    Bearer,
}

#[derive(Debug, PartialEq)]
pub struct SchemedAuthError(Option<Scheme>, pub AuthenticationError);

impl SchemedAuthError {
    /// See [`AuthenticationError::retry_after`]. The scheme makes no
    /// difference to a ban: it was imposed before any credential was read.
    pub fn retry_after(&self) -> Option<u64> {
        self.1.retry_after()
    }

    pub fn challenge(&self, realm: &str) -> String {
        match self.0 {
            Some(Scheme::Basic) => format!(r#"Basic realm="{}""#, realm),
            Some(Scheme::Bearer) => format!(
                r#"Bearer realm="{}" error="invalid_token" error_description="{}""#,
                realm, self.1
            ),
            None => format!(r#"realm="{}""#, realm),
        }
    }
}

impl Display for SchemedAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Retry-After` is delta-seconds, and RFC 9110 has no way to spell zero
    /// or negative usefully -- "come back now" is exactly the advice that
    /// walks a caller back into the ban. A deadline is only ever constructed
    /// in the future, but the gap between imposing a ban and writing the
    /// response is real time, so one that has already passed must still read
    /// as a whole second.
    #[test]
    fn a_ban_never_asks_for_a_zero_or_negative_wait() {
        let past = AuthenticationError::ExceededAllowedAttempts(
            Instant::now() - Duration::from_secs(3600),
        );
        assert_eq!(past.retry_after(), Some(1));

        let now = AuthenticationError::ExceededAllowedAttempts(Instant::now());
        assert_eq!(now.retry_after(), Some(1));
    }

    /// Rounded up rather than truncated, so a caller that waits exactly as
    /// long as it was told finds the ban gone instead of a fraction of a
    /// second of it left.
    #[test]
    fn a_part_second_counts_as_a_whole_one() {
        let error = AuthenticationError::ExceededAllowedAttempts(
            Instant::now() + Duration::from_millis(60_500),
        );
        assert_eq!(error.retry_after(), Some(61));
    }

    /// And nothing else is a ban. This is the whole of what keeps every other
    /// authentication failure on the answer it has always given: the response
    /// code is chosen on this being `Some`.
    #[test]
    fn no_other_error_asks_anyone_to_wait() {
        for error in [
            AuthenticationError::IncorrectCredentials,
            AuthenticationError::Empty,
            AuthenticationError::NoMatch("token".to_string()),
            AuthenticationError::TokenExpired(Instant::now()),
            AuthenticationError::ParseError("nonsense".to_string()),
            AuthenticationError::HttpParseError("nonsense".to_string()),
            AuthenticationError::SchemeNotSupported("Digest".to_string()),
        ] {
            assert_eq!(error.retry_after(), None, "{error}");
        }
    }

    /// The message a person reads carries the same number as the header a
    /// client reads, which is why both come from one helper.
    #[test]
    fn the_message_quotes_the_time_the_header_gives() {
        let error =
            AuthenticationError::ExceededAllowedAttempts(Instant::now() + Duration::from_secs(600));
        let seconds = error.retry_after().expect("a ban says when to come back");
        let message = error.to_string();

        assert!(message.starts_with("Exceeded allowed authentication attempts."));
        assert!(
            message.contains(&format_duration(Duration::from_secs(seconds)).to_string()),
            "{message:?} does not quote {seconds}s"
        );
    }
}
