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
    authentication_context::AuthenticationContext,
    authentication_errors::{AuthenticationError, SchemedAuthError},
    passwd_validator::UnixValidator,
    websocket_subprotocol::{bearer_token, is_websocket_handshake},
};
use actix_web::{
    body::{EitherBody, MessageBody},
    dev::{Service, ServiceRequest, ServiceResponse},
    http::header::{self},
    Error, HttpRequest, HttpResponse,
};
use futures::future::LocalBoxFuture;
use futures::StreamExt;
use serde::Serialize;
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
}

impl<S> AuthenticationService<S> {
    pub fn new(
        service: Rc<S>,
        context: Arc<Mutex<AuthenticationContext<UnixValidator>>>,
        authentication_path: &'static str,
        realm: &'static str,
    ) -> Self {
        AuthenticationService {
            service,
            context,
            authentication_path,
            realm,
        }
    }
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

        // drop authentication for requests on loopback interface
        if request
            .head()
            .peer_addr
            .is_some_and(|addr| addr.ip().to_canonical().is_loopback())
        {
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

        Box::pin(async move {
            let peer = request
                .connection_info()
                .peer_addr()
                .unwrap_or_default()
                .to_string();
            let mut context = context.lock().await;

            // handle authentication requests and return
            if request.request().uri().path() == auth_path {
                return authentication_request(&mut request, &peer, &mut context).await;
            }

            let authorized = match credential_line(&request) {
                Ok(auth) => context.authorize_request(&peer, &auth).await,
                Err(e) => Err(e.into_basic_error()),
            };

            if let Err(e) = authorized {
                match e.retry_after() {
                    Some(seconds) => too_many_requests_response(request.request(), seconds, e),
                    None => unauthorized_response(request.request(), e, realm),
                }
            } else {
                service
                    .call(request)
                    .await
                    .map(ServiceResponse::map_into_left_body)
            }
        })
    }
}

async fn authentication_request<B>(
    request: &mut ServiceRequest,
    peer: &str,
    context: &mut AuthenticationContext<UnixValidator>,
) -> Result<ServiceResponse<EitherBody<B>>, Error> {
    tracing::debug!("authentication request");
    let mut buffer = Vec::new();
    while let Some(Ok(bytes)) = request.parts_mut().1.next().await {
        buffer.extend_from_slice(&bytes);
    }

    let response = match context.authenticate_request(peer, &buffer).await {
        Ok(session) => authenticated_response(request.request(), session.id.clone(), session),
        Err(error) => match error.retry_after() {
            Some(seconds) => too_many_requests_response(request.request(), seconds, error),
            None => forbidden_response(request.request(), error),
        },
    };

    response
}

/// The credential this request offers, written as an HTTP authorization line
/// for [`AuthenticationContext::authorize_request`] to take apart.
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

fn unauthorized_response<B>(
    request: &HttpRequest,
    error: SchemedAuthError,
    realm: &str,
) -> Result<ServiceResponse<EitherBody<B>>, Error> {
    let response = HttpResponse::Unauthorized()
        .insert_header((header::WWW_AUTHENTICATE, error.challenge(realm)))
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
    use actix_web::body::BoxBody;
    use actix_web::dev::fn_service;
    use actix_web::http::StatusCode;
    use actix_web::test::TestRequest;
    use base64::{engine::general_purpose, Engine as _};
    use humantime::format_duration;
    use std::time::Duration;
    use tokio::time::Instant;

    /// A token the store knows, and an account whose shadow hash matches
    /// `PASSWORD`. Both are built fresh for every call, so no test can leave a
    /// ban or an aged token behind for the next one.
    const TOKEN: &str = "9RmQ0OGpXwYbLJ1t";
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
    {
        let context = build_test_context(
            [(TOKEN.to_string(), Instant::now())],
            [(
                "root".to_string(),
                pwhash::sha512_crypt::hash(PASSWORD).expect("a shadow hash"),
            )],
        );

        let route = Rc::new(fn_service(|request: ServiceRequest| async move {
            Ok::<_, Error>(request.into_response(HttpResponse::Ok().body("reached the route")))
        }));

        AuthenticationService::new(
            route,
            Arc::new(Mutex::new(context)),
            "/api/bmc/authenticate",
            "test realm",
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

    /// The loopback exemption. The firmware's boot-time health gate probes
    /// `https://127.0.0.1/` while a new image is on trial, so a 401 here would
    /// not be an inconvenience -- it would roll the firmware back.
    #[actix_web::test]
    async fn a_request_from_loopback_needs_no_credentials() {
        for peer in ["127.0.0.1:41234", "[::1]:41234", "[::ffff:127.0.0.1]:41234"] {
            let response = call(
                TestRequest::get()
                    .uri("/api/bmc?opt=get&type=power")
                    .peer_addr(peer.parse().expect("a peer address"))
                    .to_srv_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "from {peer}");
        }
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
            body.contains(&format_duration(Duration::from_secs(seconds)).to_string()),
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

    /// The loopback exemption, under the one condition this change could have
    /// broken it: with a ban already standing. The firmware's boot-time health
    /// gate probes `https://127.0.0.1/` while a new image is on trial, and a
    /// 429 there would roll the firmware back just as surely as a 401 would.
    /// Nothing on the board shares an address with the peer being banned, but
    /// nothing in the code said so until this test did.
    #[actix_web::test]
    async fn loopback_is_exempt_even_while_another_peer_is_banned() {
        let service = service();

        for _ in 0..ATTEMPTS {
            let _ = service.call(wrong_password(PEER)).await;
        }

        for peer in ["127.0.0.1:41234", "[::1]:41234", "[::ffff:127.0.0.1]:41234"] {
            let response = service
                .call(rest_call_from(peer).to_srv_request())
                .await
                .expect("the middleware always answers");
            assert_eq!(response.status(), StatusCode::OK, "from {peer}");
        }
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
}
