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
//! Carrying a bearer token through a websocket handshake.
//!
//! The browser `WebSocket` constructor takes a URL and a list of subprotocols,
//! and nothing else. A page cannot set `Authorization` on it, so the whole
//! `/api/bmc` scope -- which the serial console at `/api/bmc/serial/ws` sits
//! in -- was reachable from `curl` and unreachable from a browser. A token in
//! the query string is not the answer: this daemon traces request paths, so a
//! `?token=` would write the credential into the log. `Sec-WebSocket-Protocol`
//! is the one handshake header the constructor does let a page choose, which
//! is why the Kubernetes API server puts the credential for `exec` and
//! `attach` there too.
//!
//! # The format
//!
//! A client offers two subprotocol names: the plain one that says what it
//! wants to speak, and one that carries the token.
//!
//! ```text
//! Sec-WebSocket-Protocol: bmcd.serial.v1, bmcd.bearer.<token>
//! ```
//!
//! `<token>` is the session token verbatim -- the `id` from
//! `POST /api/bmc/authenticate`, which is also its `X-Auth-Token` header. It
//! is 64 characters of `[A-Za-z0-9]`, so it needs no encoding to be a legal
//! subprotocol name. This is `Authorization: Bearer <token>` and nothing more:
//! the token is looked up in the same store, ages the same way and is banned
//! the same way.
//!
//! The server answers with the plain name, never with the credential:
//!
//! ```text
//! Sec-WebSocket-Protocol: bmcd.serial.v1
//! ```
//!
//! Browsers fail the connection if the server names nothing, and RFC 6455 only
//! lets it name something the client offered -- so a client that offers *only*
//! `bmcd.bearer.<token>` gets a handshake the browser will reject. Always
//! offer a plain name, and put it first. `bmcd.serial.v1` is the one to use
//! for the serial console; the daemon does not care which name it is, it
//! echoes back the first offered entry that is not a credential.
//!
//! In JavaScript that is:
//!
//! ```text
//! new WebSocket(`wss://${host}/api/bmc/serial/ws?node=0`,
//!               ["bmcd.serial.v1", `bmcd.bearer.${token}`]);
//! ```
//!
//! Both halves of the rule live in this one module so that the middleware that
//! reads the token and the route that echoes the selection cannot drift apart.
use actix_web::{
    dev::RequestHead,
    http::{
        header::{self, HeaderValue},
        Method,
    },
    HttpResponse,
};

/// Marks a subprotocol entry as a credential rather than a protocol.
/// Everything after it is the bearer token.
pub const BEARER_PROTOCOL_PREFIX: &str = "bmcd.bearer.";

/// Whether this request is a complete websocket handshake.
///
/// This is the same test `actix_http::ws::verify_handshake` applies at the
/// route, restated here because `actix-http` is not a direct dependency of
/// this crate. It is deliberately the *whole* test and not just the two
/// upgrade headers: the subprotocol credential must not become a second way to
/// authenticate an ordinary REST call, and a request that fails any of these
/// could never have completed a handshake anyway.
pub fn is_websocket_handshake(head: &RequestHead) -> bool {
    head.method == Method::GET
        && head.upgrade()
        && header_contains(head, header::UPGRADE, "websocket")
        && matches!(
            head.headers()
                .get(header::SEC_WEBSOCKET_VERSION)
                .and_then(|version| version.to_str().ok()),
            Some("13" | "8" | "7")
        )
        && head.headers().contains_key(header::SEC_WEBSOCKET_KEY)
}

/// The bearer token this handshake offers, if it offers one.
///
/// The first credential entry wins. An entry with nothing after the prefix is
/// not a token and is treated as if it were not there.
pub fn bearer_token(head: &RequestHead) -> Option<&str> {
    offered_protocols(head)
        .find_map(|protocol| protocol.strip_prefix(BEARER_PROTOCOL_PREFIX))
        .filter(|token| !token.is_empty())
}

/// The subprotocol the server selects: the first offered entry that is not a
/// credential. `None` when the client offered none, or offered nothing but
/// credentials -- echoing a token back would put it in the response headers,
/// which is the mistake this whole scheme exists to avoid.
pub fn selected_protocol(head: &RequestHead) -> Option<&str> {
    offered_protocols(head).find(|protocol| !protocol.starts_with(BEARER_PROTOCOL_PREFIX))
}

/// Names the selected subprotocol on a handshake response. Without this a
/// browser closes the connection the moment it opens, whatever the status
/// line said. Called unconditionally by the route, because a client may offer
/// a subprotocol whether it authenticated with a header, with a subprotocol,
/// or -- over the loopback interface -- not at all.
pub fn echo_selected_protocol<B>(head: &RequestHead, response: &mut HttpResponse<B>) {
    let Some(protocol) = selected_protocol(head) else {
        return;
    };

    // The name came out of a header, so it is already header-safe; the
    // conversion cannot fail, and a response without the echo is better than
    // a panic if it ever did.
    if let Ok(value) = HeaderValue::from_str(protocol) {
        response
            .headers_mut()
            .insert(header::SEC_WEBSOCKET_PROTOCOL, value);
    }
}

/// Every subprotocol the client offered, in the order it offered them.
/// `Sec-WebSocket-Protocol` is a comma-separated list, and a client may send
/// the header more than once instead, so both are flattened here.
fn offered_protocols(head: &RequestHead) -> impl Iterator<Item = &str> {
    head.headers()
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|protocol| !protocol.is_empty())
}

/// Whether a header holds `needle` among its comma-separated tokens, ignoring
/// case. `Upgrade` is a list header, so `websocket` may not be alone in it.
fn header_contains(head: &RequestHead, name: header::HeaderName, needle: &str) -> bool {
    head.headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case(needle))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test::TestRequest;

    /// The four headers that make a request a websocket handshake.
    fn handshake_headers() -> [(header::HeaderName, &'static str); 4] {
        [
            (header::CONNECTION, "Upgrade"),
            (header::UPGRADE, "websocket"),
            (header::SEC_WEBSOCKET_VERSION, "13"),
            (header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ=="),
        ]
    }

    /// A handshake as a browser sends it, minus one header when asked.
    fn handshake_without(missing: Option<&header::HeaderName>) -> TestRequest {
        let mut request = TestRequest::get().uri("/api/bmc/serial/ws?node=0");
        for (name, value) in handshake_headers() {
            if Some(&name) != missing {
                request = request.insert_header((name, value));
            }
        }
        request
    }

    fn handshake() -> TestRequest {
        handshake_without(None)
    }

    #[test]
    fn a_handshake_is_recognised_and_a_rest_call_is_not() {
        assert!(is_websocket_handshake(handshake().to_http_request().head()));

        // Every one of the four headers is load-bearing. Drop any of them and
        // this is not a handshake, so the middleware will not read a token off
        // it -- which is what keeps the subprotocol from becoming a second way
        // to authenticate an ordinary REST call.
        for (name, _) in handshake_headers() {
            let request = handshake_without(Some(&name)).to_http_request();
            assert!(
                !is_websocket_handshake(request.head()),
                "{name} is part of the handshake"
            );
        }

        // A plain GET carrying the same subprotocol header. This is the
        // request the fallback must never authenticate.
        let rest_call = TestRequest::get()
            .uri("/api/bmc?opt=get&type=power")
            .insert_header((header::SEC_WEBSOCKET_PROTOCOL, "bmcd.bearer.abc123"))
            .to_http_request();
        assert!(!is_websocket_handshake(rest_call.head()));

        // A POST cannot be a handshake either, even though the route accepts
        // POST: `actix_ws::handle` answers that with 405.
        let post = handshake().method(Method::POST).to_http_request();
        assert!(!is_websocket_handshake(post.head()));

        // A version this server cannot speak is not a handshake it can finish.
        let old = handshake()
            .insert_header((header::SEC_WEBSOCKET_VERSION, "6"))
            .to_http_request();
        assert!(!is_websocket_handshake(old.head()));
    }

    #[test]
    fn the_token_comes_out_of_the_credential_entry() {
        let request = handshake()
            .insert_header((
                header::SEC_WEBSOCKET_PROTOCOL,
                "bmcd.serial.v1, bmcd.bearer.abc123",
            ))
            .to_http_request();

        assert_eq!(bearer_token(request.head()), Some("abc123"));
        assert_eq!(selected_protocol(request.head()), Some("bmcd.serial.v1"));
    }

    /// A client may send the header repeated rather than comma-separated, and
    /// may space it however it likes. Both are one list.
    #[test]
    fn a_repeated_header_is_one_list() {
        let request = handshake()
            .append_header((header::SEC_WEBSOCKET_PROTOCOL, "bmcd.serial.v1"))
            .append_header((header::SEC_WEBSOCKET_PROTOCOL, "  bmcd.bearer.abc123  "))
            .to_http_request();

        assert_eq!(bearer_token(request.head()), Some("abc123"));
        assert_eq!(selected_protocol(request.head()), Some("bmcd.serial.v1"));
    }

    #[test]
    fn no_subprotocol_is_no_token_and_no_selection() {
        let request = handshake().to_http_request();

        assert_eq!(bearer_token(request.head()), None);
        assert_eq!(selected_protocol(request.head()), None);
    }

    /// The prefix on its own carries no token, and must not be read as one --
    /// an empty bearer token would go on to be looked up in the token store.
    #[test]
    fn the_bare_prefix_is_not_a_token() {
        let request = handshake()
            .insert_header((header::SEC_WEBSOCKET_PROTOCOL, "bmcd.bearer."))
            .to_http_request();

        assert_eq!(bearer_token(request.head()), None);
    }

    /// The token is never echoed. A client that offers nothing but its
    /// credential gets no selection at all, which its browser will reject --
    /// that is the documented way to get this wrong, and it fails loudly
    /// rather than putting the token in a response header.
    #[test]
    fn a_credential_is_never_selected() {
        let request = handshake()
            .insert_header((header::SEC_WEBSOCKET_PROTOCOL, "bmcd.bearer.abc123"))
            .to_http_request();

        assert_eq!(selected_protocol(request.head()), None);

        let mut response = HttpResponse::SwitchingProtocols().finish();
        echo_selected_protocol(request.head(), &mut response);
        assert_eq!(response.headers().get(header::SEC_WEBSOCKET_PROTOCOL), None);
    }

    #[test]
    fn the_handshake_response_names_the_selected_subprotocol() {
        let request = handshake()
            .insert_header((
                header::SEC_WEBSOCKET_PROTOCOL,
                "bmcd.serial.v1, bmcd.bearer.abc123",
            ))
            .to_http_request();

        let mut response = HttpResponse::SwitchingProtocols().finish();
        echo_selected_protocol(request.head(), &mut response);

        assert_eq!(
            response
                .headers()
                .get(header::SEC_WEBSOCKET_PROTOCOL)
                .expect("the response names a subprotocol"),
            "bmcd.serial.v1"
        );
    }
}
