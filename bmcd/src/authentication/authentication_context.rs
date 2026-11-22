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
    /// The on-board local token ([`super::local_token`]). Accepted from a
    /// loopback peer only, never expires, and is not dropped by a password
    /// change: the token store above is, this is not.
    local_token: Option<String>,
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
            local_token: None,
        }
    }

    /// Install the token written for the on-board `tpi`.
    pub fn set_local_token(&mut self, token: String) {
        self.local_token = Some(token);
    }

    /// Whether `candidate` is the local token. The caller has checked the
    /// peer is loopback; this never looks at the address.
    pub fn is_local_token(&self, candidate: &str) -> bool {
        self.local_token
            .as_deref()
            .is_some_and(|token| super::local_token::matches(token, candidate))
    }

    pub fn reload_password_cache(
        &mut self,
        password_entries: impl Iterator<Item = (String, String)>,
    ) {
        let next = HashMap::from_iter(password_entries);
        // A changed hash is a password change, including one made with
        // `passwd` on the board. Tokens minted from the old password stop.
        if next != self.passwds {
            self.token_store.clear();
        }
        self.passwds = next;
    }

    /// Drops one bearer token. Unknown tokens are already gone.
    pub fn revoke_bearer(&mut self, token: &str) {
        self.token_store.remove(token);
    }

    /// Drops every session. Used when this daemon itself changes the password,
    /// before the shadow watcher has run.
    pub fn clear_sessions(&mut self) {
        self.token_store.clear();
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

        // An unknown token is refused but not penalised. The store is in
        // memory, so a restart forgets every session, and an expired token is
        // removed from it below -- a client still holding either is not
        // guessing, and a page sends several requests at once. Counting those
        // banned the operator's address, and their correct password with it.
        // A token is 64 random alphanumerics; the ban buys nothing against
        // guessing one. A ban that already stands still applies, above.
        let Some((username, last_access)) = self.token_store.get_mut(token) else {
            return Err(AuthenticationError::NoMatch(token.to_string()));
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

    /// The Basic credential's user and the stored hash, under the lock.
    ///
    /// The password is not checked here: `sha512_crypt` takes long enough on
    /// the board that running it under the only authentication mutex stalls
    /// every other request. [`Self::check_password`] runs it after the lock
    /// is released, and [`Self::finish_basic`] records the result.
    fn basic_hash(
        &mut self,
        peer: &str,
        credentials: &str,
    ) -> Result<PendingAuthorization, AuthenticationError> {
        let decoded = general_purpose::STANDARD.decode(credentials)?;
        let utf8 = std::str::from_utf8(&decoded)?;
        let Some((user, pass)) = utf8.split_once(':') else {
            return Err(AuthenticationError::ParseError(
                "basic authentication formatted wrong".to_string(),
            ));
        };
        let hash = self.password_hash(peer, user)?;
        Ok(PendingAuthorization::Password {
            username: user.to_string(),
            password: pass.to_string(),
            hash,
        })
    }

    /// Authorise a request as far as the map can, under the lock.
    ///
    /// A bearer token is decided here. A Basic credential comes back as
    /// [`PendingAuthorization::Password`]: the caller releases the lock, runs
    /// [`Self::check_password`], and takes the lock again for
    /// [`Self::finish_basic`].
    pub async fn begin_authorization(
        &mut self,
        peer: &str,
        http_authorization_line: &str,
    ) -> Result<PendingAuthorization, SchemedAuthError> {
        match http_authorization_line.split_once(' ') {
            Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => self
                .authorize_bearer(peer, token)
                .await
                .map(|name| {
                    PendingAuthorization::Decided(Actor::User {
                        name,
                        scheme: "bearer",
                    })
                })
                .map_err(AuthenticationError::into_bearer_error),
            Some((scheme, credentials)) if scheme.eq_ignore_ascii_case("basic") => self
                .basic_hash(peer, credentials)
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

    /// Record a Basic password check, under the lock again.
    pub fn finish_basic(
        &mut self,
        peer: &str,
        username: String,
        outcome: Result<(), AuthenticationError>,
    ) -> Result<Actor, SchemedAuthError> {
        self.record_password_check(peer, outcome)
            .map(|()| Actor::User {
                name: username,
                scheme: "basic",
            })
            .map_err(AuthenticationError::into_basic_error)
    }

    /// The password check, on the blocking pool and without the lock.
    pub async fn check_password(hash: String, password: String) -> Result<(), AuthenticationError> {
        tokio::task::spawn_blocking(move || P::validate(&hash, &password))
            .await
            .unwrap_or_else(|e| Err(AuthenticationError::CheckFailed(e.to_string())))
    }

    /// The ban and penalty bookkeeping for a finished password check.
    ///
    /// Only a wrong password is a penalty. A check that could not run
    /// ([`AuthenticationError::CheckFailed`]) says nothing about the password
    /// offered and is passed back unchanged, neither counted nor cleared.
    fn record_password_check(
        &mut self,
        peer: &str,
        outcome: Result<(), AuthenticationError>,
    ) -> Result<(), AuthenticationError> {
        self.ban_patrol.patrole_ban(peer)?;
        match outcome {
            Ok(()) => {
                self.ban_patrol.clear_penalties(peer);
                Ok(())
            }
            Err(AuthenticationError::IncorrectCredentials) => Err(self
                .ban_patrol
                .penalize(peer)
                .err()
                .unwrap_or(AuthenticationError::IncorrectCredentials)),
            Err(other) => Err(other),
        }
    }

    /// Authorise a request and say who it is, all under one borrow.
    ///
    /// The handler runs [`Self::begin_authorization`] and
    /// [`Self::finish_basic`] itself, with the password check between them
    /// outside the lock. Only the tests use this.
    #[cfg(test)]
    pub async fn authorize_request(
        &mut self,
        peer: &str,
        http_authorization_line: &str,
    ) -> Result<Actor, SchemedAuthError> {
        match self
            .begin_authorization(peer, http_authorization_line)
            .await?
        {
            PendingAuthorization::Decided(actor) => Ok(actor),
            PendingAuthorization::Password {
                username,
                password,
                hash,
            } => self.finish_basic(peer, username, P::validate(&hash, &password)),
        }
    }

    pub fn credentials_from_body(body: &[u8]) -> Result<(String, String), AuthenticationError> {
        let credentials = serde_json::from_slice::<Login>(body)?;
        Ok((credentials.username, credentials.password))
    }

    /// The stored hash for `username`, after the ban check.
    ///
    /// An unknown user is penalised here. The password check itself is the
    /// caller's, so `sha512_crypt` does not run while the authentication
    /// mutex is held.
    pub fn password_hash(
        &mut self,
        peer: &str,
        username: &str,
    ) -> Result<String, AuthenticationError> {
        self.ban_patrol.patrole_ban(peer)?;
        match self.passwds.get(username) {
            Some(hash) => Ok(hash.clone()),
            None => Err(self
                .ban_patrol
                .penalize(peer)
                .err()
                .unwrap_or(AuthenticationError::IncorrectCredentials)),
        }
    }

    /// Records the password result and, on success, inserts the session.
    ///
    /// Called again under the mutex after the password check has finished.
    /// A check that could not run is reported and not counted as a guess.
    pub async fn establish_session(
        &mut self,
        peer: &str,
        username: String,
        outcome: Result<(), AuthenticationError>,
    ) -> Result<Session, AuthenticationError> {
        self.record_password_check(peer, outcome)?;

        let token: String = rng()
            .sample_iter(&Alphanumeric)
            .take(64)
            .map(char::from)
            .collect();
        self.new_and_remove_expired_tokens(token.clone(), username.clone())
            .await;

        Ok(Session {
            id: token, // according Redfish spec, id refers to the session id.
            // which is not equal to the access-token. for now use
            // the token.
            name: "User Session".to_string(),
            description: "User Session".to_string(),
            username,
        })
    }

    /// The three login steps in one call, without the lock release between
    /// them. Only the tests use it; the handler runs the steps itself.
    #[cfg(test)]
    pub async fn authenticate_request(
        &mut self,
        peer: &str,
        body: &[u8],
    ) -> Result<Session, AuthenticationError> {
        let (username, password) = Self::credentials_from_body(body)?;
        let hash = self.password_hash(peer, &username)?;
        let outcome = P::validate(&hash, &password);
        self.establish_session(peer, username, outcome).await
    }
}

/// What [`AuthenticationContext::begin_authorization`] left to do.
pub enum PendingAuthorization {
    /// Decided under the lock (a bearer token).
    Decided(Actor),
    /// A Basic credential whose password is still to be checked, outside the
    /// lock.
    Password {
        username: String,
        password: String,
        hash: String,
    },
}

/// Who a request is, for the audit line.
///
/// `Loopback` is named rather than folded into "authenticated": it is the
/// board acting on itself (the on-board `tpi` with the local token from
/// [`super::local_token`]), and a confirmation of a network change from there
/// proves nothing about whether the new address is reachable. Loopback without
/// that token gets a 401 like any other peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Actor {
    /// The board itself: a loopback peer with the on-board local token.
    Loopback,
    User {
        name: String,
        scheme: &'static str,
    },
}

impl std::fmt::Display for Actor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Actor::Loopback => write!(f, "loopback (local token)"),
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
            local_token: None,
        }
    }

    #[actix_web::test]
    async fn a_changed_password_hash_drops_existing_tokens() {
        let mut context = build_test_context(
            [("123".to_string(), Instant::now())],
            [("root".to_string(), "old-hash".to_string())],
        );
        context.reload_password_cache([("root".to_string(), "old-hash".to_string())].into_iter());
        assert!(context
            .authorize_request("peer", "Bearer 123")
            .await
            .is_ok());

        context.reload_password_cache([("root".to_string(), "new-hash".to_string())].into_iter());
        assert!(context
            .authorize_request("peer", "Bearer 123")
            .await
            .is_err());
    }

    #[actix_web::test]
    async fn logout_drops_only_that_token() {
        let mut context = build_test_context(
            [
                ("keep".to_string(), Instant::now()),
                ("drop".to_string(), Instant::now()),
            ],
            Vec::<(String, String)>::new(),
        );
        context.revoke_bearer("drop");
        assert!(context
            .authorize_request("peer", "Bearer keep")
            .await
            .is_ok());
        assert!(context
            .authorize_request("peer", "Bearer drop")
            .await
            .is_err());
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
