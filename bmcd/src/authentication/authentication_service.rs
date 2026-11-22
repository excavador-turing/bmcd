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
use super::{
    authentication_context::{Actor, AuthenticationContext, PendingAuthorization},
    authentication_errors::{AuthenticationError, SchemedAuthError},
    factory_password::{FactoryPassword, REFUSAL},
    passwd_validator::UnixValidator,
    request_policy,
    verified_proxy::VerifiedProxy,
    websocket_subprotocol::{bearer_token, is_websocket_handshake},
};
use actix_web::{
    body::{EitherBody, MessageBody},
    dev::{Service, ServiceRequest, ServiceResponse},
    http::{
        header::{self},
        Method,
    },
    Error, HttpMessage, HttpRequest, HttpResponse,
};
use futures::future::LocalBoxFuture;
use futures::StreamExt;
use serde::Serialize;
use std::time::Duration;
use std::{borrow::Cow, rc::Rc, sync::Arc};
use tokio::sync::Mutex;

/// This authentication service is designed to prepare for implementing "Redfish
/// Session Login Authentication" as good as possible. Redfish is not yet
/// implemented in this product, until then this session based, token
/// authentication service is used to provide authentication.
#[derive(Clone)]
pub struct AuthenticationService<S> {
    service: Rc<S>,
    context: Arc<Mutex<AuthenticationContext<UnixValidator>>>,
    authentication_path: &'static str,
    realm: &'static str,
    /// Header naming the human, believed only behind a verified client
    /// certificate. See [`crate::authentication::verified_proxy`].
    identity_header: Arc<str>,
    /// A board still on `root` / `turing` may log in and change it, and
    /// nothing else. See [`crate::authentication::factory_password`].
    factory: Arc<FactoryPassword>,
}

impl<S> AuthenticationService<S> {
    pub fn new(
        service: Rc<S>,
        context: Arc<Mutex<AuthenticationContext<UnixValidator>>>,
        authentication_path: &'static str,
        realm: &'static str,
        identity_header: Arc<str>,
        factory: Arc<FactoryPassword>,
    ) -> Self {
        AuthenticationService {
            service,
            context,
            authentication_path,
            realm,
            identity_header,
            factory,
        }
    }
}

/// What a factory board answers to everything but logging in and changing it.
///
/// `application/problem+json`, like every other refusal on the path form, so
/// a client that reads one reads this.
fn factory_password_response<B>(request: &HttpRequest) -> ServiceResponse<EitherBody<B>> {
    let response = HttpResponse::Forbidden()
        .insert_header((header::CONTENT_TYPE, "application/problem+json"))
        .json(serde_json::json!({
            "type": "about:blank",
            "title": "This board is still using its factory password",
            "status": 403,
            "detail": REFUSAL,
        }));
    ServiceResponse::new(request.clone(), response).map_into_right_body()
}

/// The human a trusted proxy says it authenticated, if this connection has
/// earned the right to say so.
///
/// Returns `None` unless BOTH hold: the connection presented a client
/// certificate this daemon verified against its configured CA, and the
/// request carries a non-empty identity header. Either alone is nothing --
/// a certificate with no header names nobody, and a header with no
/// certificate is a string an attacker on the management LAN can set.
fn proxied_identity(request: &ServiceRequest, header: &str) -> Option<(String, String)> {
    let proxy = request.request().conn_data::<VerifiedProxy>();
    let claimed = request
        .headers()
        .get(header)
        .and_then(|value| value.to_str().ok());
    decide_proxied_identity(proxy, claimed)
}

/// The decision itself, with both inputs handed to it.
///
/// Split out from the request so it can be tested: actix offers no way to put
/// connection data on a `TestRequest`, and this is the one piece of logic in
/// the daemon where being wrong means an authentication bypass. Testing it
/// through a live TLS handshake would test OpenSSL; this tests the rule.
fn decide_proxied_identity(
    proxy: Option<&VerifiedProxy>,
    claimed: Option<&str>,
) -> Option<(String, String)> {
    let proxy = proxy?;
    let name = claimed.map(str::trim).filter(|name| !name.is_empty())?;
    Some((name.to_string(), proxy.subject.clone()))
}

impl<S, B> Service<ServiceRequest> for AuthenticationService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, mut request: ServiceRequest) -> Self::Future {
        let service = self.service.clone();

        // Decided before any credential is read, for every request under
        // this middleware, so a route cannot opt out (see `request_policy`).
        if let Some(refusal) = policy_refusal(&request, self.realm) {
            return Box::pin(std::future::ready(Ok(refusal)));
        }

        // A proxy holding a certificate from our CA, naming the human it
        // already authenticated. This is the only way in that is neither a
        // password nor a session token, and it is what lets the fleet
        // interface be exposed while a board is not (SQU-136).
        //
        // Note what is NOT happening: the certificate does not authorise
        // anything by itself, and a trusted proxy that names nobody falls
        // through to ordinary token authentication below. The certificate
        // only makes the header worth reading.
        if let Some((name, subject)) = proxied_identity(&request, &self.identity_header) {
            tracing::debug!("request authorised by {subject} on behalf of {name}");
            // A factory board cannot be driven from the fleet either. The
            // gateway vouching for a person says nothing about whether this
            // board has been set up.
            if !self
                .factory
                .permits(request.request().method(), request.request().uri().path())
            {
                tracing::warn!(
                    "refused {} for {}: this board is still on its factory password",
                    request.request().uri().path(),
                    name
                );
                return Box::pin(std::future::ready(Ok(factory_password_response(
                    request.request(),
                ))));
            }
            request.extensions_mut().insert(Actor::User {
                name,
                scheme: "mtls",
            });
            return Box::pin(async move {
                service
                    .call(request)
                    .await
                    .map(ServiceResponse::map_into_left_body)
            });
        }

        let context = self.context.clone();
        let auth_path = self.authentication_path;
        let realm = self.realm;
        let factory = self.factory.clone();

        Box::pin(async move {
            let peer = request
                .connection_info()
                .peer_addr()
                .unwrap_or_default()
                .to_string();
            // The on-board local token, from the board itself only. Not
            // gated by the factory password: whoever can read the token is
            // already root on the board. The lock is taken just for the
            // comparison.
            let loopback = request
                .head()
                .peer_addr
                .is_some_and(|addr| addr.ip().to_canonical().is_loopback());
            let candidate = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(request_policy::bearer_token)
                .map(str::to_owned);
            let local = match candidate {
                Some(token) if loopback => context.lock().await.is_local_token(&token),
                _ => false,
            };
            if local {
                request.extensions_mut().insert(Actor::Loopback);
                return service
                    .call(request)
                    .await
                    .map(ServiceResponse::map_into_left_body);
            }

            // The login body is read before the mutex. It is the one
            // unauthenticated request that carries a body, and holding the
            // only authentication lock across that read stalls every other
            // request for as long as the sender likes.
            if request.request().uri().path() == auth_path {
                return authentication_request(&mut request, &peer, &context).await;
            }

            // Bearer is decided under the lock. Basic only looks up the hash
            // there: the crypt runs on the blocking pool with the lock
            // released, and the lock is taken again to record the result.
            let pending = match credential_line(&request) {
                Ok(auth) => context.lock().await.begin_authorization(&peer, &auth).await,
                Err(e) => Err(e.into_basic_error()),
            };
            let authorized = match pending {
                Ok(PendingAuthorization::Decided(actor)) => Ok(actor),
                Ok(PendingAuthorization::Password {
                    username,
                    password,
                    hash,
                }) => {
                    let outcome =
                        AuthenticationContext::<UnixValidator>::check_password(hash, password)
                            .await;
                    context.lock().await.finish_basic(&peer, username, outcome)
                }
                Err(e) => Err(e),
            };

            match authorized {
                Err(e) => match e.retry_after() {
                    Some(seconds) => too_many_requests_response(request.request(), seconds, e),
                    None => unauthorized_response(request.request(), e, realm),
                },
                Ok(actor) => {
                    // The credential was right; the board is not set up. Only
                    // now, so that a wrong password is still a wrong password
                    // rather than this message -- which would otherwise tell
                    // a stranger the account name is right.
                    if !factory.permits(request.request().method(), request.request().uri().path())
                    {
                        tracing::warn!(
                            "refused {}: this board is still on its factory password",
                            request.request().uri().path()
                        );
                        return Ok(factory_password_response(request.request()));
                    }
                    request.extensions_mut().insert(actor);
                    service
                        .call(request)
                        .await
                        .map(ServiceResponse::map_into_left_body)
                }
            }
        })
    }
}

/// About 4 KiB. A login is a username and a password; anything larger is not one.
const LOGIN_BODY_MAX: usize = 4096;
/// A few seconds. A sender that stops mid-body does not hold the request open.
const LOGIN_BODY_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug)]
enum LoginBodyError {
    TooLarge,
    TimedOut,
    Unreadable,
}

async fn read_login_payload<S>(payload: &mut S, wait: Duration) -> Result<Vec<u8>, LoginBodyError>
where
    S: futures::Stream<Item = Result<bytes::Bytes, actix_web::error::PayloadError>> + Unpin,
{
    let read = async {
        let mut buffer = Vec::new();
        while let Some(chunk) = payload.next().await {
            let bytes = chunk.map_err(|_| LoginBodyError::Unreadable)?;
            if buffer.len().saturating_add(bytes.len()) > LOGIN_BODY_MAX {
                return Err(LoginBodyError::TooLarge);
            }
            buffer.extend_from_slice(&bytes);
        }
        Ok(buffer)
    };
    match tokio::time::timeout(wait, read).await {
        Ok(result) => result,
        Err(_) => Err(LoginBodyError::TimedOut),
    }
}

async fn authentication_request<B>(
    request: &mut ServiceRequest,
    peer: &str,
    context: &Arc<Mutex<AuthenticationContext<UnixValidator>>>,
) -> Result<ServiceResponse<EitherBody<B>>, Error> {
    tracing::debug!("authentication request");
    let buffer = {
        let payload = &mut request.parts_mut().1;
        read_login_payload(payload, LOGIN_BODY_WAIT).await
    };
    let buffer = match buffer {
        Ok(buffer) => buffer,
        Err(LoginBodyError::TooLarge) => {
            return Ok(ServiceResponse::new(
                request.request().clone(),
                HttpResponse::PayloadTooLarge()
                    .body("login body is larger than 4 KiB")
                    .map_into_right_body(),
            ));
        }
        Err(LoginBodyError::TimedOut) => {
            return Ok(ServiceResponse::new(
                request.request().clone(),
                HttpResponse::RequestTimeout()
                    .body("login body was not received in time")
                    .map_into_right_body(),
            ));
        }
        Err(LoginBodyError::Unreadable) => {
            return forbidden_response(request.request(), "login body could not be read");
        }
    };

    let (username, password) =
        match AuthenticationContext::<UnixValidator>::credentials_from_body(&buffer) {
            Ok(credentials) => credentials,
            Err(error) => return forbidden_response(request.request(), error),
        };

    // Hash lookup and the ban check are the map. The crypt is not.
    let hash = {
        let mut context = context.lock().await;
        match context.password_hash(peer, &username) {
            Ok(hash) => hash,
            Err(error) => {
                return match error.retry_after() {
                    Some(seconds) => too_many_requests_response(request.request(), seconds, error),
                    None => forbidden_response(request.request(), error),
                };
            }
        }
    };
    let outcome = AuthenticationContext::<UnixValidator>::check_password(hash, password).await;

    let response = {
        let mut context = context.lock().await;
        match context.establish_session(peer, username, outcome).await {
            Ok(session) => authenticated_response(request.request(), session.id.clone(), session),
            Err(error) => match error.retry_after() {
                Some(seconds) => too_many_requests_response(request.request(), seconds, error),
                None => forbidden_response(request.request(), error),
            },
        }
    };

    response
}

/// The credential this request offers, written as an HTTP authorization line
/// for [`AuthenticationContext::begin_authorization`] to take apart.
///
/// `Authorization` is where it lives for every client that can set a header.
/// A browser opening a websocket cannot: the `WebSocket` constructor takes a
/// URL and a list of subprotocols and nothing else, which left the serial
/// console reachable from `curl` and unreachable from a page. Such a request
/// may name its bearer token as a subprotocol instead, and this turns that
/// back into the `Bearer <token>` line the rest of the stack already
/// understands -- so the token store, the expiry and the ban patrol all treat
/// it exactly as they treat a header. See
/// [`super::websocket_subprotocol`] for the format.
///
/// The fallback is deliberately narrow. It applies only when `Authorization`
/// is absent -- a header that is there but unreadable is still that header's
/// error -- and only to a request that is a complete websocket handshake, so
/// it cannot become a second way to authenticate an ordinary REST call.
fn credential_line(request: &ServiceRequest) -> Result<Cow<'_, str>, AuthenticationError> {
    match parse_authorization_header(request) {
        Ok(line) => Ok(Cow::Borrowed(line)),
        Err(AuthenticationError::Empty) if is_websocket_handshake(request.head()) => {
            match bearer_token(request.head()) {
                Some(token) => Ok(Cow::Owned(format!("Bearer {}", token))),
                None => Err(AuthenticationError::Empty),
            }
        }
        Err(e) => Err(e),
    }
}

fn parse_authorization_header(request: &ServiceRequest) -> Result<&str, AuthenticationError> {
    tracing::debug!("authorize request");
    request
        .headers()
        .get(header::AUTHORIZATION)
        .ok_or(AuthenticationError::Empty)
        .and_then(|auth| {
            auth.to_str()
                .map_err(|e| AuthenticationError::HttpParseError(e.to_string()))
        })
}

fn forbidden_response<B, E: ToString>(
    request: &HttpRequest,
    response_text: E,
) -> Result<ServiceResponse<EitherBody<B>>, Error> {
    Ok(ServiceResponse::new(
        request.clone(),
        HttpResponse::Forbidden()
            .body(response_text.to_string())
            .map_into_right_body(),
    ))
}

/// The answer a banned caller gets, from the login endpoint and from every
/// other path alike.
///
/// A ban is not a statement about the credential offered -- `patrole_ban` runs
/// before anything is read -- and reporting it as one is what made this worth
/// fixing: the web interface showed "Invalid username or password" to someone
/// whose password was correct, so they went on trying, and the first attempt
/// after each ban lapses doubles the next one.
///
/// So it gets its own status, and `Retry-After` in delta-seconds so a client
/// can wait rather than guess. Deliberately no `WWW-Authenticate`: a challenge
/// here is an instruction to a browser to prompt for the credential again,
/// which is the loop this is trying to break. A 429 is not an authentication
/// challenge and does not carry one.
fn too_many_requests_response<B, E: ToString>(
    request: &HttpRequest,
    retry_after: u64,
    response_text: E,
) -> Result<ServiceResponse<EitherBody<B>>, Error> {
    Ok(ServiceResponse::new(
        request.clone(),
        HttpResponse::TooManyRequests()
            .insert_header((header::RETRY_AFTER, retry_after.to_string()))
            .body(response_text.to_string())
            .map_into_right_body(),
    ))
}

fn authenticated_response<B>(
    request: &HttpRequest,
    token: String,
    body: impl Serialize,
) -> Result<ServiceResponse<EitherBody<B>>, Error> {
    let text = serde_json::to_string(&body)?;
    Ok(ServiceResponse::new(
        request.clone(),
        HttpResponse::Ok()
            .insert_header(("X-Auth-Token", token))
            .body(text)
            .map_into_right_body(),
    ))
}

/// A browser call: see [`request_policy::is_browser_request`].
fn is_browser_call(request: &HttpRequest) -> bool {
    request_policy::is_browser_request(request.headers())
}

/// The refusals that do not depend on who is asking (contract C2, C3).
///
/// * A state change that `Sec-Fetch-Site` says came from another site or
///   another origin of this one: 403. `same-origin`, `none` or no header pass.
/// * A state change on `GET` (`?opt=set`, an upload cancel, a backup) without
///   an `Authorization: Bearer` header: 405, use POST. Older clients that
///   send a Bearer token keep working for this release.
/// * Basic authentication on a browser request: 401 with a Bearer challenge,
///   so the browser never caches a password it would attach to an `<img>`.
fn policy_refusal<B>(
    request: &ServiceRequest,
    realm: &str,
) -> Option<ServiceResponse<EitherBody<B>>> {
    let http = request.request();
    let headers = http.headers();
    let method = http.method();
    let path = http.uri().path();
    let query = http.uri().query().unwrap_or("");
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let refuse = |response: HttpResponse| {
        Some(ServiceResponse::new(http.clone(), response).map_into_right_body())
    };

    let state_change = request_policy::is_state_change(method, path, query);
    if state_change && request_policy::is_cross_site(headers) {
        tracing::warn!("refused a cross-site {method} {path}");
        return refuse(
            HttpResponse::Forbidden().body("a state change from another site is refused"),
        );
    }
    if state_change
        && matches!(*method, Method::GET | Method::HEAD)
        && authorization
            .and_then(request_policy::bearer_token)
            .is_none()
    {
        return refuse(
            HttpResponse::MethodNotAllowed()
                .insert_header((header::ALLOW, "POST"))
                .body(
                    "this request changes state: send it as POST. GET is served only with \
                     an Authorization: Bearer token, for older clients",
                ),
        );
    }
    if authorization.is_some_and(request_policy::is_basic)
        && request_policy::is_browser_request(headers)
    {
        return refuse(
            HttpResponse::Unauthorized()
                .insert_header((
                    header::WWW_AUTHENTICATE,
                    format!(r#"Bearer realm="{realm}""#),
                ))
                .body("Basic authentication is not accepted from a browser; log in for a token"),
        );
    }
    None
}

fn unauthorized_response<B>(
    request: &HttpRequest,
    error: SchemedAuthError,
    realm: &str,
) -> Result<ServiceResponse<EitherBody<B>>, Error> {
    let response = HttpResponse::Unauthorized()
        .insert_header((
            header::WWW_AUTHENTICATE,
            error.challenge_for(realm, is_browser_call(request)),
        ))
        .body(error.to_string());
    Ok(ServiceResponse::map_into_right_body(ServiceResponse::new(
        request.clone(),
        response,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authentication::authentication_context::tests::build_test_context;
    use crate::utils::format_duration;
    use actix_web::body::BoxBody;
    use actix_web::dev::fn_service;
    use actix_web::http::StatusCode;
    use actix_web::test::TestRequest;
    use base64::{engine::general_purpose, Engine as _};
    use std::time::Duration;
    use tokio::time::Instant;

    /// A token the store knows, and an account whose shadow hash matches
    /// `PASSWORD`. Both are built fresh for every call, so no test can leave a
    /// ban or an aged token behind for the next one.
    const TOKEN: &str = "9RmQ0OGpXwYbLJ1t";
    /// The on-board local token the test context holds.
    const LOCAL_TOKEN: &str = "LocalTokenForTheTestsOnly0123456789abcdefghijklmnopqrstuvwxyzAB";
    const PASSWORD: &str = "hunter2";

    /// The address these tests are refused from, and one they are not. The ban
    /// is keyed on the peer, so a second address is how a test can show a
    /// credential is good while the first address is locked out -- which is
    /// how the board's owner established it, with a 200 from elsewhere during
    /// a lockout the web interface was calling a wrong password.
    const PEER: &str = "192.168.1.10:41234";
    const OTHER_PEER: &str = "192.168.1.11:41234";

    /// How many failures [`build_test_context`] allows before it bans. The
    /// board's default is five; the number is not what these tests are about,
    /// only what the answer is once it is reached.
    const ATTEMPTS: usize = 10;

    /// The middleware in front of a route that answers 200 to anything that
    /// reaches it, so the status alone says whether the request got through.
    ///
    /// One instance owns one ban patrol. `call` below builds a fresh one per
    /// request on purpose, so that no test can leave a ban behind for the
    /// next; a test *about* a ban therefore has to hold this itself and drive
    /// several requests through the one instance.
    fn service(
    ) -> impl Service<ServiceRequest, Response = ServiceResponse<EitherBody<BoxBody>>, Error = Error>
           + Clone {
        service_with_shadow("/nonexistent/shadow")
    }

    /// The same, with a shadow file of the caller's choosing, for the tests
    /// about a board still on its factory password. The default above points
    /// at nothing, which reads as "not a factory board" -- so every other
    /// test in this module is unaffected by the gate.
    fn service_with_shadow(
        shadow: &str,
    ) -> impl Service<ServiceRequest, Response = ServiceResponse<EitherBody<BoxBody>>, Error = Error>
           + Clone {
        let mut context = build_test_context(
            [(TOKEN.to_string(), Instant::now())],
            [(
                "root".to_string(),
                pwhash::sha512_crypt::hash(PASSWORD).expect("a shadow hash"),
            )],
        );
        context.set_local_token(LOCAL_TOKEN.to_string());

        let route = Rc::new(fn_service(|request: ServiceRequest| async move {
            Ok::<_, Error>(request.into_response(HttpResponse::Ok().body("reached the route")))
        }));

        AuthenticationService::new(
            route,
            Arc::new(Mutex::new(context)),
            "/api/bmc/authenticate",
            "test realm",
            IDENTITY_HEADER.into(),
            Arc::new(FactoryPassword::new(shadow)),
        )
    }

    async fn call(request: ServiceRequest) -> ServiceResponse<EitherBody<BoxBody>> {
        service()
            .call(request)
            .await
            .expect("the middleware always answers")
    }

    fn basic(user: &str, password: &str) -> String {
        let credentials = general_purpose::STANDARD.encode(format!("{user}:{password}"));
        format!("Basic {credentials}")
    }

    /// A request from somewhere other than the board itself, so the
    /// authentication middleware actually runs.
    fn rest_call() -> TestRequest {
        rest_call_from(PEER)
    }

    fn rest_call_from(peer: &str) -> TestRequest {
        TestRequest::get()
            .uri("/api/bmc?opt=get&type=power")
            .peer_addr(peer.parse().expect("a peer address"))
    }

    /// One wrong password, offered the way a browser offers it.
    fn wrong_password(peer: &str) -> ServiceRequest {
        rest_call_from(peer)
            .insert_header((header::AUTHORIZATION, basic("root", "not the password")))
            .to_srv_request()
    }

    #[tokio::test]
    async fn a_login_body_is_capped_at_four_kibibytes() {
        let mut ok = futures::stream::iter([Ok(bytes::Bytes::from(vec![b'a'; LOGIN_BODY_MAX]))]);
        let body = read_login_payload(&mut ok, LOGIN_BODY_WAIT).await.unwrap();
        assert_eq!(body.len(), LOGIN_BODY_MAX);

        let mut too_big =
            futures::stream::iter([Ok(bytes::Bytes::from(vec![b'a'; LOGIN_BODY_MAX + 1]))]);
        assert!(matches!(
            read_login_payload(&mut too_big, LOGIN_BODY_WAIT).await,
            Err(LoginBodyError::TooLarge)
        ));
    }

    #[tokio::test]
    async fn a_login_body_that_never_arrives_stops_after_its_deadline() {
        let mut pending =
            futures::stream::pending::<Result<bytes::Bytes, actix_web::error::PayloadError>>();
        let wait = Duration::from_millis(30);
        let started = std::time::Instant::now();
        let error = read_login_payload(&mut pending, wait).await.unwrap_err();
        assert!(matches!(error, LoginBodyError::TimedOut));
        assert!(started.elapsed() >= wait);
        assert_eq!(LOGIN_BODY_WAIT, Duration::from_secs(5));
    }

    #[actix_web::test]
    async fn a_login_body_over_the_cap_is_refused() {
        let response = call(
            TestRequest::post()
                .uri("/api/bmc/authenticate")
                .peer_addr(PEER.parse().expect("a peer address"))
                .set_payload(vec![b'a'; LOGIN_BODY_MAX + 1])
                .to_srv_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// The body is still unread. A token request on the same service must
    /// answer anyway, which it cannot do while the login holds the mutex.
    #[actix_web::test]
    async fn a_slow_login_body_does_not_block_a_token_request() {
        let auth = service();
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let body = format!(r#"{{"username":"root","password":"{PASSWORD}"}}"#);
        let stream = futures::stream::once(async move {
            let _ = gate.await;
            Ok::<_, actix_web::error::PayloadError>(bytes::Bytes::from(body))
        });
        let (req, _) = TestRequest::post()
            .uri("/api/bmc/authenticate")
            .peer_addr(PEER.parse().expect("a peer address"))
            .to_srv_request()
            .into_parts();
        let login_request = ServiceRequest::from_parts(
            req,
            actix_web::dev::Payload::Stream {
                payload: Box::pin(stream),
            },
        );
        let login = auth.clone().call(login_request);
        tokio::pin!(login);

        let token = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                _ = &mut login => panic!("the login finished before its body was sent"),
                response = auth.call(
                    rest_call()
                        .insert_header((header::AUTHORIZATION, format!("Bearer {TOKEN}")))
                        .to_srv_request(),
                ) => response,
            }
        })
        .await
        .expect("a token request waited on the login body");
        assert_eq!(token.expect("middleware answers").status(), StatusCode::OK);

        release.send(()).unwrap();
        let login = login.await.expect("middleware answers");
        assert_eq!(login.status(), StatusCode::OK);
    }

    /// A Basic password check runs with the authentication mutex released.
    ///
    /// The hash is made slow on purpose (200 000 sha512 rounds). While the
    /// Basic request is still inside the check, the mutex must be free and the
    /// runtime thread must still run other futures; with the crypt inline
    /// under the lock, neither holds and the request finishes first.
    #[actix_web::test]
    async fn a_basic_password_check_does_not_hold_the_lock() {
        let slow = pwhash::sha512_crypt::hash_with("$6$rounds=200000$saltsalt$", PASSWORD)
            .expect("a shadow hash");
        let context = Arc::new(Mutex::new(build_test_context(
            [(TOKEN.to_string(), Instant::now())],
            [("root".to_string(), slow)],
        )));
        let route = Rc::new(fn_service(|request: ServiceRequest| async move {
            Ok::<_, Error>(request.into_response(HttpResponse::Ok().body("reached the route")))
        }));
        let auth = AuthenticationService::new(
            route,
            context.clone(),
            "/api/bmc/authenticate",
            "test realm",
            IDENTITY_HEADER.into(),
            Arc::new(FactoryPassword::new("/nonexistent/shadow")),
        );

        let basic = auth.call(
            rest_call()
                .insert_header((header::AUTHORIZATION, basic("root", PASSWORD)))
                .to_srv_request(),
        );
        tokio::pin!(basic);
        tokio::select! {
            biased;
            _ = &mut basic => panic!("the password check finished before it could be observed"),
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        assert!(
            context.try_lock().is_ok(),
            "the authentication mutex is held across the password check"
        );
        let response = basic.await.expect("middleware answers");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// A login request: the path the web interface actually uses, and the one
    /// whose answer it turns into "Invalid username or password".
    fn login(peer: &str, password: &str) -> ServiceRequest {
        TestRequest::post()
            .uri("/api/bmc/authenticate")
            .peer_addr(peer.parse().expect("a peer address"))
            .set_payload(format!(r#"{{"username":"root","password":"{password}"}}"#))
            .to_srv_request()
    }

    /// The four headers a browser puts on the wire for `new WebSocket(...)`.
    fn handshake() -> TestRequest {
        TestRequest::get()
            .uri("/api/bmc/serial/ws?node=0")
            .peer_addr("192.168.1.10:41234".parse().expect("a peer address"))
            .insert_header((header::CONNECTION, "Upgrade"))
            .insert_header((header::UPGRADE, "websocket"))
            .insert_header((header::SEC_WEBSOCKET_VERSION, "13"))
            .insert_header((header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ=="))
    }

    fn subprotocols() -> String {
        format!("bmcd.serial.v1, bmcd.bearer.{TOKEN}")
    }

    /// Both header schemes, exactly as before. `tpi` and `curl` reach the
    /// daemon this way and nothing about that has moved.
    #[actix_web::test]
    async fn an_authorization_header_still_authenticates() {
        let bearer = call(
            rest_call()
                .insert_header((header::AUTHORIZATION, format!("Bearer {TOKEN}")))
                .to_srv_request(),
        )
        .await;
        assert_eq!(bearer.status(), StatusCode::OK);

        let basic = call(
            rest_call()
                .insert_header((header::AUTHORIZATION, basic("root", PASSWORD)))
                .to_srv_request(),
        )
        .await;
        assert_eq!(basic.status(), StatusCode::OK);
    }

    /// And still refuses what it always refused, with the same challenge.
    #[actix_web::test]
    async fn a_wrong_authorization_header_is_still_rejected() {
        let bearer = call(
            rest_call()
                .insert_header((header::AUTHORIZATION, "Bearer not-a-token"))
                .to_srv_request(),
        )
        .await;
        assert_eq!(bearer.status(), StatusCode::UNAUTHORIZED);
        assert!(challenge(&bearer).starts_with("Bearer "));

        let basic = call(
            rest_call()
                .insert_header((header::AUTHORIZATION, basic("root", "not the password")))
                .to_srv_request(),
        )
        .await;
        assert_eq!(basic.status(), StatusCode::UNAUTHORIZED);
        assert!(challenge(&basic).starts_with("Basic "));

        let nothing = call(rest_call().to_srv_request()).await;
        assert_eq!(nothing.status(), StatusCode::UNAUTHORIZED);
    }

    /// Loopback is an address, not a credential. The health probe is
    /// `https://127.0.0.1/`, the static interface, and does not come through
    /// this middleware.
    #[actix_web::test]
    async fn a_request_from_loopback_needs_a_token() {
        for peer in ["127.0.0.1:41234", "[::1]:41234", "[::ffff:127.0.0.1]:41234"] {
            let response = call(
                TestRequest::get()
                    .uri("/api/bmc?opt=get&type=power")
                    .peer_addr(peer.parse().expect("a peer address"))
                    .to_srv_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "from {peer}");
        }

        let response = call(
            TestRequest::get()
                .uri("/api/bmc?opt=get&type=power")
                .peer_addr("127.0.0.1:41234".parse().expect("a peer address"))
                .insert_header((header::AUTHORIZATION, format!("Bearer {TOKEN}")))
                .to_srv_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// A page sends `Sec-Fetch-*`. Offering it `Basic` is what makes the
    /// browser cache the password and attach it to a later `<img>`.
    #[actix_web::test]
    async fn a_browser_call_is_not_offered_basic() {
        let response = call(
            rest_call()
                .insert_header(("Sec-Fetch-Mode", "no-cors"))
                .insert_header(("Sec-Fetch-Site", "cross-site"))
                .to_srv_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let challenge = challenge(&response);
        assert!(challenge.starts_with("Bearer "));
        assert!(!challenge.contains("Basic"));
    }

    /// The new path: a browser's handshake, with the token where the
    /// `WebSocket` constructor can put it.
    #[actix_web::test]
    async fn a_subprotocol_token_authenticates_an_upgrade() {
        let response = call(
            handshake()
                .insert_header((header::SEC_WEBSOCKET_PROTOCOL, subprotocols()))
                .to_srv_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// A token the store does not know is refused as a bearer token, because
    /// that is all it ever was.
    #[actix_web::test]
    async fn a_subprotocol_token_the_store_does_not_know_is_rejected() {
        let response = call(
            handshake()
                .insert_header((
                    header::SEC_WEBSOCKET_PROTOCOL,
                    "bmcd.serial.v1, bmcd.bearer.not-a-token",
                ))
                .to_srv_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(challenge(&response).starts_with("Bearer "));
    }

    /// The reason the fallback is gated on the whole handshake and not on a
    /// header or two: the identical `Sec-WebSocket-Protocol` on an ordinary
    /// REST call buys nothing at all.
    #[actix_web::test]
    async fn a_rest_call_carrying_the_same_header_is_not_authenticated() {
        let response = call(
            rest_call()
                .insert_header((header::SEC_WEBSOCKET_PROTOCOL, subprotocols()))
                .to_srv_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// The fallback fills in for an absent `Authorization` header and does
    /// nothing else. A handshake with no credential anywhere is still a 401,
    /// and a handshake whose header is wrong is not rescued by a good token in
    /// its subprotocols -- the header that is there is the one that counts.
    #[actix_web::test]
    async fn the_fallback_only_fills_in_for_an_absent_header() {
        let bare = call(handshake().to_srv_request()).await;
        assert_eq!(bare.status(), StatusCode::UNAUTHORIZED);

        let contradicted = call(
            handshake()
                .insert_header((header::AUTHORIZATION, "Bearer not-a-token"))
                .insert_header((header::SEC_WEBSOCKET_PROTOCOL, subprotocols()))
                .to_srv_request(),
        )
        .await;
        assert_eq!(contradicted.status(), StatusCode::UNAUTHORIZED);
    }

    /// A ban answers as a ban. Up to the threshold every wrong password is
    /// still a 401 with the `Basic` challenge, exactly as before; the attempt
    /// that trips the ban, and everything after it, is a 429 that says how
    /// long is left, in a header a client can act on and in a body a person
    /// can read.
    ///
    /// The 401s in the loop are the half of this that must not move. Were the
    /// dispatch ever to fire on something other than a ban, they would turn
    /// into 429s here.
    #[actix_web::test]
    async fn a_ban_is_answered_as_a_ban() {
        let service = service();

        for attempt in 1..ATTEMPTS {
            let refused = service
                .call(wrong_password(PEER))
                .await
                .expect("the middleware always answers");
            assert_eq!(
                refused.status(),
                StatusCode::UNAUTHORIZED,
                "attempt {attempt} of {ATTEMPTS}"
            );
            assert!(challenge(&refused).starts_with("Basic "));
            assert!(
                refused.headers().get(header::RETRY_AFTER).is_none(),
                "attempt {attempt} is not a ban and must not tell anyone to wait"
            );
        }

        let banned = service
            .call(wrong_password(PEER))
            .await
            .expect("the middleware always answers");
        assert_eq!(banned.status(), StatusCode::TOO_MANY_REQUESTS);

        // Never zero, never negative, and no longer than the first ban level.
        let seconds = retry_after(&banned);
        assert!((1..=60).contains(&seconds), "Retry-After: {seconds}");

        // No challenge. A 429 carrying one is an instruction to a browser to
        // ask for the password again, which is the loop this fixes.
        assert!(banned.headers().get(header::WWW_AUTHENTICATE).is_none());

        // And the body says the same thing the header does.
        let body = body(banned).await;
        assert!(
            body.contains("Exceeded allowed authentication attempts"),
            "body: {body}"
        );
        assert!(
            body.contains(&format_duration(Duration::from_secs(seconds))),
            "body {body:?} does not agree with Retry-After: {seconds}"
        );
    }

    /// The ban is unchanged in strength: a correct password does not lift it.
    /// The proof that the password really is correct is that the same one is
    /// accepted from another address while this one is locked out -- which is
    /// the observation this change came from.
    #[actix_web::test]
    async fn a_ban_still_refuses_a_correct_password() {
        let service = service();

        for _ in 0..ATTEMPTS {
            let _ = service.call(wrong_password(PEER)).await;
        }

        let banned = service
            .call(
                rest_call_from(PEER)
                    .insert_header((header::AUTHORIZATION, basic("root", PASSWORD)))
                    .to_srv_request(),
            )
            .await
            .expect("the middleware always answers");
        assert_eq!(banned.status(), StatusCode::TOO_MANY_REQUESTS);

        let elsewhere = service
            .call(
                rest_call_from(OTHER_PEER)
                    .insert_header((header::AUTHORIZATION, basic("root", PASSWORD)))
                    .to_srv_request(),
            )
            .await
            .expect("the middleware always answers");
        assert_eq!(elsewhere.status(), StatusCode::OK);
    }

    /// The login endpoint is where the web interface reads its message from,
    /// so it is where the wrong answer did its damage. A wrong password there
    /// is still the 403 it always was; a ban is a 429.
    #[actix_web::test]
    async fn the_login_endpoint_reports_a_ban_too() {
        let service = service();

        for attempt in 1..ATTEMPTS {
            let refused = service
                .call(login(PEER, "not the password"))
                .await
                .expect("the middleware always answers");
            assert_eq!(
                refused.status(),
                StatusCode::FORBIDDEN,
                "attempt {attempt} of {ATTEMPTS}"
            );
        }

        let banned = service
            .call(login(PEER, "not the password"))
            .await
            .expect("the middleware always answers");
        assert_eq!(banned.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!((1..=60).contains(&retry_after(&banned)));

        // And the ban does not become an oracle: the right password gets the
        // same answer, so nothing here tells a guesser it guessed right.
        let correct = service
            .call(login(PEER, PASSWORD))
            .await
            .expect("the middleware always answers");
        assert_eq!(correct.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    /// A ban is per peer. Loopback still needs a credential, and a ban on
    /// another address does not answer for it.
    #[actix_web::test]
    async fn loopback_with_a_password_is_not_banned_with_another_peer() {
        let service = service();

        for _ in 0..ATTEMPTS {
            let _ = service.call(wrong_password(PEER)).await;
        }

        let bare = service
            .call(rest_call_from("127.0.0.1:41234").to_srv_request())
            .await
            .expect("the middleware always answers");
        assert_eq!(bare.status(), StatusCode::UNAUTHORIZED);

        let authed = service
            .call(
                rest_call_from("127.0.0.1:41234")
                    .insert_header((header::AUTHORIZATION, basic("root", PASSWORD)))
                    .to_srv_request(),
            )
            .await
            .expect("the middleware always answers");
        assert_eq!(authed.status(), StatusCode::OK);
    }

    /// A token this daemon no longer knows is not a guessed password. The
    /// store is in memory, so every restart -- every firmware upgrade, every
    /// power cut -- forgets every session, and an expired token is dropped
    /// from it too. The browser tab or script still holding one sends it on
    /// every request until it is told otherwise, and a page fires several of
    /// those at once.
    ///
    /// Those were counted against the peer like wrong passwords, so the tab
    /// alone could walk its owner up the ban ladder, and the correct password
    /// typed from the same machine was then refused with a 429. A token is 64
    /// random alphanumerics, which no one is going to guess, so counting them
    /// protected nothing.
    #[actix_web::test]
    async fn a_token_the_daemon_forgot_does_not_ban_its_owner() {
        let service = service();

        for attempt in 1..=ATTEMPTS * 2 {
            let refused = service
                .call(
                    rest_call_from(PEER)
                        .insert_header((
                            header::AUTHORIZATION,
                            "Bearer issued-before-the-daemon-restarted",
                        ))
                        .to_srv_request(),
                )
                .await
                .expect("the middleware always answers");
            assert_eq!(
                refused.status(),
                StatusCode::UNAUTHORIZED,
                "stale token, attempt {attempt}"
            );
            assert!(challenge(&refused).starts_with("Bearer "));
        }

        let correct = service
            .call(
                rest_call_from(PEER)
                    .insert_header((header::AUTHORIZATION, basic("root", PASSWORD)))
                    .to_srv_request(),
            )
            .await
            .expect("the middleware always answers");
        assert_eq!(correct.status(), StatusCode::OK);
    }

    /// The ban itself is not loosened by that: a peer already banned for wrong
    /// passwords is refused with a 429 whatever it offers, a token included.
    #[actix_web::test]
    async fn a_banned_peer_is_still_refused_with_a_token() {
        let service = service();

        for _ in 0..ATTEMPTS {
            let _ = service.call(wrong_password(PEER)).await;
        }

        let banned = service
            .call(
                rest_call_from(PEER)
                    .insert_header((header::AUTHORIZATION, format!("Bearer {TOKEN}")))
                    .to_srv_request(),
            )
            .await
            .expect("the middleware always answers");
        assert_eq!(banned.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    fn retry_after<B>(response: &ServiceResponse<B>) -> u64 {
        response
            .headers()
            .get(header::RETRY_AFTER)
            .expect("a 429 always says when to come back")
            .to_str()
            .expect("the delay is text")
            .parse()
            .expect("the delay is a whole number of seconds")
    }

    async fn body<B: MessageBody>(response: ServiceResponse<B>) -> String {
        let Ok(bytes) = actix_web::body::to_bytes(response.into_body()).await else {
            panic!("the body reads");
        };
        String::from_utf8(bytes.to_vec()).expect("the body is text")
    }

    fn challenge<B>(response: &ServiceResponse<B>) -> &str {
        response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .expect("a 401 always carries a challenge")
            .to_str()
            .expect("the challenge is text")
    }

    // ---- the proxy identity path (SQU-136) --------------------------------
    //
    // These four are the security contract of the whole feature, and the
    // first is the one that matters: an identity header on its own must buy
    // nothing at all. Anyone who can reach the board on the management LAN
    // can set a header; only the client certificate makes it mean something,
    // and only the TLS handshake can produce one.

    /// A board still on `root` / `turing`, as `/etc/shadow` would have it.
    fn factory_shadow(dir: &std::path::Path) -> String {
        let path = dir.join("shadow");
        let hash =
            pwhash::sha512_crypt::hash(crate::authentication::factory_password::FACTORY_PASSWORD)
                .expect("a shadow hash");
        std::fs::write(&path, format!("root:{hash}:19000:0:99999:7:::\n")).expect("write");
        path.to_string_lossy().into_owned()
    }

    async fn call_on_factory_board(request: ServiceRequest, shadow: &str) -> StatusCode {
        service_with_shadow(shadow)
            .call(request)
            .await
            .expect("the middleware always answers")
            .status()
    }

    /// The whole point: a correct credential, and the board still says no.
    ///
    /// The credential IS correct here -- `PASSWORD`, which the test context
    /// accepts -- so a 403 is the factory gate and not a rejected login.
    #[actix_web::test]
    async fn a_factory_board_refuses_everything_but_login_and_the_password_change() {
        let dir = tempfile::TempDir::with_prefix("gate").expect("temp dir");
        let shadow = factory_shadow(dir.path());

        let refused = rest_call()
            .insert_header((header::AUTHORIZATION, basic("root", PASSWORD)))
            .to_srv_request();
        assert_eq!(
            call_on_factory_board(refused, &shadow).await,
            StatusCode::FORBIDDEN
        );
    }

    #[actix_web::test]
    async fn a_factory_board_still_lets_you_log_in_and_change_the_password() {
        let dir = tempfile::TempDir::with_prefix("gate-open").expect("temp dir");
        let shadow = factory_shadow(dir.path());

        // Logging in has to work, or there is no way to reach the change.
        assert_eq!(
            call_on_factory_board(login(PEER, PASSWORD), &shadow).await,
            StatusCode::OK
        );

        for path in ["/api/bmc/access", "/api/bmc/access/password"] {
            let allowed = TestRequest::post()
                .uri(path)
                .peer_addr(PEER.parse().expect("a peer address"))
                .insert_header((header::AUTHORIZATION, basic("root", PASSWORD)))
                .to_srv_request();
            assert_eq!(
                call_on_factory_board(allowed, &shadow).await,
                StatusCode::OK,
                "{path} must stay open on a factory board"
            );
        }
    }

    /// A board in safe mode is always on the factory password, and the page
    /// that says so needs the storage report and the repair. Those two, each
    /// for its own method, are reachable with a correct credential; still
    /// authenticated; and nothing else that changes the board was opened.
    #[actix_web::test]
    async fn a_factory_board_still_answers_the_storage_report_and_the_repair() {
        let dir = tempfile::TempDir::with_prefix("gate-storage").expect("temp dir");
        let shadow = factory_shadow(dir.path());
        let call_as = |request: TestRequest, credential: Option<String>| {
            let request = request.peer_addr(PEER.parse().expect("a peer address"));
            match credential {
                Some(c) => request.insert_header((header::AUTHORIZATION, c)),
                None => request,
            }
            .to_srv_request()
        };
        let root = || Some(basic("root", PASSWORD));

        // Open, with a credential.
        assert_eq!(
            call_on_factory_board(
                call_as(TestRequest::get().uri("/api/bmc/storage"), root()),
                &shadow
            )
            .await,
            StatusCode::OK,
            "GET /api/bmc/storage must be reachable on a factory board"
        );
        assert_eq!(
            call_on_factory_board(
                call_as(TestRequest::post().uri("/api/bmc/storage/repair"), root()),
                &shadow
            )
            .await,
            StatusCode::OK,
            "POST /api/bmc/storage/repair must be reachable on a factory board"
        );

        // Without a credential it is a 401, not a way round authentication;
        // with a wrong one, the same.
        assert_eq!(
            call_on_factory_board(
                call_as(TestRequest::get().uri("/api/bmc/storage"), None),
                &shadow
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call_on_factory_board(
                call_as(
                    TestRequest::post().uri("/api/bmc/storage/repair"),
                    Some(basic("root", "not the password"))
                ),
                &shadow
            )
            .await,
            StatusCode::UNAUTHORIZED
        );

        // Closed: the other verb on each, and the other mutating endpoints.
        let closed = [
            TestRequest::post().uri("/api/bmc/storage"),
            TestRequest::get().uri("/api/bmc/storage/repair"),
            TestRequest::put().uri("/api/bmc/network/switch"),
            TestRequest::post().uri("/api/bmc/network/switch/confirm"),
            TestRequest::put().uri("/api/bmc/network/address"),
            TestRequest::post().uri("/api/bmc/network/address/confirm"),
            TestRequest::put().uri("/api/bmc/tls/certificate"),
            TestRequest::delete().uri("/api/bmc/tls/certificate"),
            TestRequest::put().uri("/api/bmc/access/client-ca"),
            TestRequest::post().uri("/api/bmc/firmware/install"),
            TestRequest::post().uri("/api/bmc/hostname"),
            TestRequest::post().uri("/api/bmc?opt=set&type=reboot"),
        ];
        for request in closed {
            let request = request.peer_addr(PEER.parse().expect("a peer address"));
            let request = request
                .insert_header((header::AUTHORIZATION, basic("root", PASSWORD)))
                .to_srv_request();
            let (method, uri) = (request.method().clone(), request.uri().clone());
            assert_eq!(
                call_on_factory_board(request, &shadow).await,
                StatusCode::FORBIDDEN,
                "{method} {uri} must stay closed on a factory board"
            );
        }
        // The GET spelling with Basic is refused before the gate is asked;
        // with a token the gate still closes it.
        let get = "/api/bmc?opt=set&type=reboot";
        assert_eq!(
            call_on_factory_board(
                call_as(TestRequest::get().uri(get), Some(basic("root", PASSWORD))),
                &shadow
            )
            .await,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            call_on_factory_board(
                call_as(TestRequest::get().uri(get), Some(format!("Bearer {TOKEN}"))),
                &shadow
            )
            .await,
            StatusCode::FORBIDDEN
        );
    }

    /// A wrong password on a factory board is a wrong password, not a lecture
    /// about the factory password. Answering the lecture would confirm the
    /// account name to somebody guessing.
    #[actix_web::test]
    async fn a_wrong_password_on_a_factory_board_is_still_unauthorized() {
        let dir = tempfile::TempDir::with_prefix("gate-wrong").expect("temp dir");
        let shadow = factory_shadow(dir.path());
        assert_eq!(
            call_on_factory_board(wrong_password(PEER), &shadow).await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// Loopback without a token is a 401 on a factory board too. Promotion
    /// reads `/metrics` and `https://127.0.0.1/`, neither of which is this
    /// middleware.
    #[actix_web::test]
    async fn loopback_on_a_factory_board_still_needs_a_token() {
        let dir = tempfile::TempDir::with_prefix("gate-loopback").expect("temp dir");
        let shadow = factory_shadow(dir.path());
        let local = rest_call_from("127.0.0.1:41234").to_srv_request();
        assert_eq!(
            call_on_factory_board(local, &shadow).await,
            StatusCode::UNAUTHORIZED
        );
        // The local token is the board's own root acting; it is not gated.
        let with_token = rest_call_from("127.0.0.1:41234")
            .insert_header((header::AUTHORIZATION, format!("Bearer {LOCAL_TOKEN}")))
            .to_srv_request();
        assert_eq!(
            call_on_factory_board(with_token, &shadow).await,
            StatusCode::OK
        );
    }

    /// Contract C1: the local token from loopback is the board itself.
    #[actix_web::test]
    async fn loopback_with_the_local_token_is_the_board() {
        for peer in ["127.0.0.1:41234", "[::1]:41234", "[::ffff:127.0.0.1]:41234"] {
            for scheme in ["Bearer", "bearer"] {
                let response = call(
                    rest_call_from(peer)
                        .insert_header((header::AUTHORIZATION, format!("{scheme} {LOCAL_TOKEN}")))
                        .to_srv_request(),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK, "{scheme} from {peer}");
            }
        }
    }

    /// And from anywhere else it is just an unknown token.
    #[actix_web::test]
    async fn the_local_token_from_the_network_is_refused() {
        let response = call(
            rest_call()
                .insert_header((header::AUTHORIZATION, format!("Bearer {LOCAL_TOKEN}")))
                .to_srv_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    fn set_request(method: actix_web::http::Method, uri: &str) -> TestRequest {
        TestRequest::default()
            .method(method)
            .uri(uri)
            .peer_addr(PEER.parse().expect("a peer address"))
    }

    /// Contract C2: the transfer route was the one GET that skipped the
    /// refusal. Without a Bearer header it is a 405 now, for every credential
    /// that is not one.
    #[actix_web::test]
    async fn firmware_on_get_without_bearer_is_refused() {
        let uri = "/api/bmc?opt=set&type=firmware&file=tp2.tpu&length=1";
        let none = call(set_request(actix_web::http::Method::GET, uri).to_srv_request()).await;
        assert_eq!(none.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            none.headers()
                .get(header::ALLOW)
                .and_then(|v| v.to_str().ok()),
            Some("POST")
        );
        let basic = call(
            set_request(actix_web::http::Method::GET, uri)
                .insert_header((header::AUTHORIZATION, basic("root", PASSWORD)))
                .to_srv_request(),
        )
        .await;
        assert_eq!(basic.status(), StatusCode::METHOD_NOT_ALLOWED);
        for uri in ["/api/bmc/upload/3/cancel", "/api/bmc/backup"] {
            let response =
                call(set_request(actix_web::http::Method::GET, uri).to_srv_request()).await;
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "{uri}");
        }
    }

    /// The transition: an older client with a token still works on GET.
    #[actix_web::test]
    async fn get_opt_set_with_bearer_is_still_served() {
        for uri in [
            "/api/bmc?opt=set&type=power&node1=1",
            "/api/bmc?opt=set&type=firmware&file=tp2.tpu&length=1",
            "/api/bmc/upload/3/cancel",
            "/api/bmc/backup",
        ] {
            let response = call(
                set_request(actix_web::http::Method::GET, uri)
                    .insert_header((header::AUTHORIZATION, format!("Bearer {TOKEN}")))
                    .to_srv_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
        }
    }

    #[actix_web::test]
    async fn post_opt_set_is_served() {
        for credential in [format!("Bearer {TOKEN}"), basic("root", PASSWORD)] {
            let response = call(
                set_request(
                    actix_web::http::Method::POST,
                    "/api/bmc?opt=set&type=power&node1=1",
                )
                .insert_header((header::AUTHORIZATION, credential.clone()))
                .to_srv_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{credential}");
        }
    }

    /// Contract C3: a state change from another site is refused whatever the
    /// credential; from the same origin it is served.
    #[actix_web::test]
    async fn a_cross_site_post_is_refused() {
        for site in ["cross-site", "same-site"] {
            let response = call(
                set_request(
                    actix_web::http::Method::POST,
                    "/api/bmc?opt=set&type=power&node1=1",
                )
                .insert_header((header::AUTHORIZATION, format!("Bearer {TOKEN}")))
                .insert_header(("Sec-Fetch-Site", site))
                .to_srv_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{site}");
        }
        let same = call(
            set_request(
                actix_web::http::Method::POST,
                "/api/bmc?opt=set&type=power&node1=1",
            )
            .insert_header((header::AUTHORIZATION, format!("Bearer {TOKEN}")))
            .insert_header(("Sec-Fetch-Site", "same-origin"))
            .to_srv_request(),
        )
        .await;
        assert_eq!(same.status(), StatusCode::OK);
        // A read from another site is not a state change.
        let read = call(
            rest_call()
                .insert_header((header::AUTHORIZATION, format!("Bearer {TOKEN}")))
                .insert_header(("Sec-Fetch-Site", "cross-site"))
                .to_srv_request(),
        )
        .await;
        assert_eq!(read.status(), StatusCode::OK);
    }

    /// Contract C3: Basic is refused on any browser request, even with the
    /// right password, and the challenge is not Basic.
    #[actix_web::test]
    async fn basic_from_a_browser_is_refused() {
        for (name, value) in [
            ("Origin", "https://bmc.local"),
            ("Referer", "https://bmc.local/"),
            ("Sec-Fetch-Mode", "cors"),
        ] {
            let response = call(
                rest_call()
                    .insert_header((header::AUTHORIZATION, basic("root", PASSWORD)))
                    .insert_header((name, value))
                    .to_srv_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{name}");
            assert!(challenge(&response).starts_with("Bearer "), "{name}");
        }
    }

    /// And the control: the same request on a board whose password was
    /// changed goes straight through.
    #[actix_web::test]
    async fn a_board_with_a_changed_password_is_not_gated() {
        let dir = tempfile::TempDir::with_prefix("gate-changed").expect("temp dir");
        let path = dir.path().join("shadow");
        let hash = pwhash::sha512_crypt::hash("a much better password").expect("a shadow hash");
        std::fs::write(&path, format!("root:{hash}:19000:0:99999:7:::\n")).expect("write");

        let request = rest_call()
            .insert_header((header::AUTHORIZATION, basic("root", PASSWORD)))
            .to_srv_request();
        assert_eq!(
            call_on_factory_board(request, &path.to_string_lossy()).await,
            StatusCode::OK
        );
    }

    const IDENTITY_HEADER: &str = "x-forwarded-email";

    /// A connection that presented a certificate our CA signed.
    fn verified() -> VerifiedProxy {
        VerifiedProxy::new("CN=envoy.hive")
    }

    #[actix_web::test]
    async fn identity_header_alone_authorises_nothing() {
        // No certificate on the connection: the header is a string a stranger
        // typed. If this ever passes, every board on the LAN is wide open to
        // anyone who can spell a header name.
        let request = rest_call()
            .insert_header((IDENTITY_HEADER, "attacker@example.com"))
            .to_srv_request();

        let response = call(request).await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn verified_proxy_naming_a_user_is_a_user() {
        let who = decide_proxied_identity(Some(&verified()), Some("oleg@tsarev.id"));

        assert_eq!(
            who,
            Some(("oleg@tsarev.id".to_string(), "CN=envoy.hive".to_string()))
        );
    }

    #[test]
    fn a_name_without_a_certificate_is_nobody() {
        // The same header that works above, on a connection that proved
        // nothing. This is the bypass the feature must not have.
        assert_eq!(
            decide_proxied_identity(None, Some("attacker@example.com")),
            None
        );
    }

    #[test]
    fn a_certificate_naming_nobody_is_nobody() {
        // A certificate authorises nothing by itself: it only makes the
        // header worth reading. With no header the request falls through to
        // ordinary token authentication.
        assert_eq!(decide_proxied_identity(Some(&verified()), None), None);
    }

    #[test]
    fn a_blank_name_is_a_misconfiguration_not_an_anonymous_login() {
        assert_eq!(
            decide_proxied_identity(Some(&verified()), Some("   ")),
            None
        );
        assert_eq!(decide_proxied_identity(Some(&verified()), Some("")), None);
    }

    #[test]
    fn the_name_is_trimmed_not_rejected() {
        let who = decide_proxied_identity(Some(&verified()), Some("  oleg@tsarev.id  "));

        assert_eq!(
            who.map(|(name, _)| name),
            Some("oleg@tsarev.id".to_string())
        );
    }
}
