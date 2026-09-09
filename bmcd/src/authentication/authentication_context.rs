// Copyright 2023 Turing Machines
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use super::authentication_errors::AuthenticationError;
use super::authentication_errors::SchemedAuthError;
use super::ban_patrol::BanPatrol;
use super::passwd_validator::PasswordValidator;
use super::passwd_validator::UnixValidator;
use base64::{engine::general_purpose, Engine as _};
use rand::distr::Alphanumeric;
use rand::rng;
use rand::Rng;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::marker::PhantomData;
use tokio::time::{Duration, Instant};

pub struct AuthenticationContext<P>
where
    P: PasswordValidator + 'static,
{
    /// Token to the user it was issued for, and when it was last used.
    ///
    /// The username is carried so that a request authenticated by a bearer
    /// token can still say who is acting. Without it every audit line for
    /// every logged-in operator reads the same, which is the gap this store
    /// existed on the wrong side of.
    token_store: HashMap<String, (String, Instant)>,
    passwds: HashMap<String, String>,
    password_validator: PhantomData<P>,
    expire_timeout: Duration,
    ban_patrol: BanPatrol,
}

impl<P> AuthenticationContext<P>
where
    P: PasswordValidator + 'static,
{
    pub fn with_unix_validator(
        password_entries: impl Iterator<Item = (String, String)>,
        expire_timeout: Duration,
        authentication_attempts: usize,
    ) -> AuthenticationContext<UnixValidator> {
        AuthenticationContext::<UnixValidator> {
            token_store: HashMap::new(),
            passwds: HashMap::from_iter(password_entries),
            password_validator: PhantomData::<UnixValidator>,
            expire_timeout,
            ban_patrol: BanPatrol::new(authentication_attempts),
        }
    }

    pub fn reload_password_cache(
        &mut self,
        password_entries: impl Iterator<Item = (String, String)>,
    ) {
        self.passwds = HashMap::from_iter(password_entries);
    }

    /// This function piggy-backs removes of expired tokens on an authentication
    /// request. This imposes a small penalty on each request. Its deemed not
    /// significant enough to justify optimization given the expected volume
    /// of incoming authentication requests.
    async fn new_and_remove_expired_tokens(&mut self, key: String, username: String) {
        self.token_store.retain(|_, (_, last_access)| {
            let duration = Instant::now().saturating_duration_since(*last_access);
            duration <= self.expire_timeout
        });

        self.token_store.insert(key, (username, Instant::now()));
    }

    async fn authorize_bearer(
        &mut self,
        peer: &str,
        token: &str,
    ) -> Result<String, AuthenticationError> {
        self.ban_patrol.patrole_ban(peer)?;

        let Some((username, last_access)) = self.token_store.get_mut(token) else {
            return Err(self
                .ban_patrol
                .penalize(peer)
                .err()
                .unwrap_or(AuthenticationError::NoMatch(token.to_string())));
        };

        let instant = *last_access;
        let duration = Instant::now().saturating_duration_since(instant);
        if duration < self.expire_timeout {
            let username = username.clone();
            *last_access = Instant::now();
            self.ban_patrol.clear_penalties(peer);
            return Ok(username);
        }

        self.token_store.remove(token);
        Err(AuthenticationError::TokenExpired(instant))
    }

    fn validate_credentials(
        &mut self,
        peer: &str,
        username: &str,
        password: &str,
    ) -> Result<(), AuthenticationError> {
        self.ban_patrol.patrole_ban(peer)?;

        match self
            .passwds
            .get(username)
            .ok_or(AuthenticationError::IncorrectCredentials)
            .and_then(|pass| P::validate(pass, password))
        {
            Ok(_) => {
                tracing::debug!("{username} validated successfully");
                self.ban_patrol.clear_penalties(peer);
                Ok(())
            }
            Err(AuthenticationError::IncorrectCredentials) => Err(self
                .ban_patrol
                .penalize(peer)
                .err()
                .unwrap_or(AuthenticationError::IncorrectCredentials)),
            Err(err) => Err(err),
        }
    }

    async fn authorize_basic(
        &mut self,
        peer: &str,
        credentials: &str,
    ) -> Result<String, AuthenticationError> {
        let decoded = general_purpose::STANDARD.decode(credentials)?;
        let utf8 = std::str::from_utf8(&decoded)?;
        let Some((user, pass)) = utf8.split_once(':') else {
            return Err(AuthenticationError::ParseError(
                "basic authentication formatted wrong".to_string(),
            ));
        };

        self.validate_credentials(peer, user, pass)?;
        Ok(user.to_string())
    }

    /// Authorise a request and say who it is.
    ///
    /// The username comes back rather than being discarded, because an audit
    /// line that cannot name the operator is a log line.
    pub async fn authorize_request(
        &mut self,
        peer: &str,
        http_authorization_line: &str,
    ) -> Result<Actor, SchemedAuthError> {
        match http_authorization_line.split_once(' ') {
            Some(("Bearer", token)) => self
                .authorize_bearer(peer, token)
                .await
                .map(|name| Actor::User {
                    name,
                    scheme: "bearer",
                })
                .map_err(AuthenticationError::into_bearer_error),
            Some(("Basic", credentials)) => self
                .authorize_basic(peer, credentials)
                .await
                .map(|name| Actor::User {
                    name,
                    scheme: "basic",
                })
                .map_err(AuthenticationError::into_basic_error),
            Some((auth, _)) => {
                Err(AuthenticationError::SchemeNotSupported(auth.to_string()).into_unknown_error())
            }
            None => Err(
                AuthenticationError::HttpParseError(http_authorization_line.to_string())
                    .into_basic_error(),
            ),
        }
    }

    pub async fn authenticate_request(
        &mut self,
        peer: &str,
        body: &[u8],
    ) -> Result<Session, AuthenticationError> {
        let credentials = serde_json::from_slice::<Login>(body)?;

        self.validate_credentials(peer, &credentials.username, &credentials.password)?;

        let token: String = rng()
            .sample_iter(&Alphanumeric)
            .take(64)
            .map(char::from)
            .collect();
        self.new_and_remove_expired_tokens(token.clone(), credentials.username.clone())
            .await;

        Ok(Session {
            id: token, // according Redfish spec, id refers to the session id.
            // which is not equal to the access-token. for now use
            // the token.
            name: "User Session".to_string(),
            description: "User Session".to_string(),
            username: credentials.username,
        })
    }
}

/// Who a request is, for the audit line.
///
/// `Loopback` is named rather than folded into "authenticated", because it is
/// the bypass: `/api/bmc` skips authentication entirely for requests from the
/// board itself, which is how the on-board `tpi` works without credentials and
/// how the promotion gate reads its metrics token. An audit trail that cannot
/// tell that apart from a real credential is not an audit trail — and this is
/// the line somebody reading one would most want to find.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Actor {
    Loopback,
    User { name: String, scheme: &'static str },
}

impl std::fmt::Display for Actor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Actor::Loopback => write!(f, "loopback (unauthenticated)"),
            Actor::User { name, scheme } => write!(f, "{} ({})", name, scheme),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Session {
    pub id: String,
    name: String,
    description: String,
    username: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct Login {
    username: String,
    password: String,
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::ops::Sub;

    /// Every token in a test context belongs to this user, so a test can
    /// assert that authorising by token still names who is acting.
    pub const TEST_TOKEN_OWNER: &str = "token_owner";

    pub fn build_test_context(
        token_data: impl IntoIterator<Item = (String, Instant)>,
        user_data: impl IntoIterator<Item = (String, String)>,
    ) -> AuthenticationContext<UnixValidator> {
        let token_data = token_data
            .into_iter()
            .map(|(token, seen)| (token, (TEST_TOKEN_OWNER.to_string(), seen)));

        AuthenticationContext {
            token_store: HashMap::from_iter(token_data),
            passwds: HashMap::from_iter(user_data),
            password_validator: PhantomData::<UnixValidator>,
            expire_timeout: Duration::from_secs(20),
            ban_patrol: BanPatrol::new(10),
        }
    }

    #[actix_web::test]
    async fn test_token_failures() {
        let now = Instant::now();
        let twenty_sec_ago = now.sub(Duration::from_secs(20));
        let mut context = build_test_context(
            [("123".to_string(), now), ("2".to_string(), twenty_sec_ago)],
            Vec::new(),
        );

        assert_eq!(
            context
                .authorize_request("peer", "Bearer 1234")
                .await
                .unwrap_err()
                .1,
            AuthenticationError::NoMatch("1234".to_string())
        );

        assert_eq!(
            context
                .authorize_request("peer", "Bearer 2")
                .await
                .unwrap_err()
                .1,
            AuthenticationError::TokenExpired(twenty_sec_ago)
        );

        // After expired error, the token gets removed. Subsequent calls for that token will
        // therefore return "NoMatch"
        assert_eq!(
            context
                .authorize_request("peer", "Bearer 2")
                .await
                .unwrap_err()
                .1,
            AuthenticationError::NoMatch("2".to_string())
        );
    }

    #[actix_web::test]
    async fn test_happy_flow() {
        let mut context = build_test_context(
            [
                ("123".to_string(), Instant::now()),
                ("2".to_string(), Instant::now().sub(Duration::from_secs(20))),
            ],
            Vec::new(),
        );
        // Not merely `Ok`: a bearer token has to come back naming its user,
        // or every audit line written for a logged-in operator reads the same
        // and the trail cannot answer who did it.
        assert_eq!(
            Ok(Actor::User {
                name: TEST_TOKEN_OWNER.to_string(),
                scheme: "bearer",
            }),
            context.authorize_request("peer1", "Bearer 123").await
        );
    }

    #[actix_web::test]
    async fn authentication_errors() {
        let mut context = build_test_context(
            Vec::new(),
            [("test_user".to_string(), "password".to_string())],
        );

        assert!(matches!(
            context
                .authenticate_request("peer1", b"{not a valid json")
                .await
                .unwrap_err(),
            AuthenticationError::ParseError(_)
        ));

        let json = serde_json::to_vec(&Login {
            username: "John".to_string(),
            password: "1234".to_string(),
        })
        .unwrap();

        assert_eq!(
            context
                .authenticate_request("peer", &json)
                .await
                .unwrap_err(),
            AuthenticationError::IncorrectCredentials
        );
        let json = serde_json::to_vec(&Login {
            username: "test_user".to_string(),
            password: "1234".to_string(),
        })
        .unwrap();

        assert_eq!(
            context
                .authenticate_request("peer", &json)
                .await
                .unwrap_err(),
            AuthenticationError::IncorrectCredentials
        );
    }

    #[actix_web::test]
    async fn pass_authentication() {
        let mut context = build_test_context(
            Vec::new(),
            [("test_user".to_string(), "password".to_string())],
        );
        let json = serde_json::to_vec(&Login {
            username: "test_user".to_string(),
            password: "password".to_string(),
        })
        .unwrap();

        assert_eq!(
            context
                .authenticate_request("peer", &json)
                .await
                .unwrap_err(),
            AuthenticationError::IncorrectCredentials
        );
    }
}
