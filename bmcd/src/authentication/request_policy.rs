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

//! What the authentication middleware decides about a request before it
//! looks at a credential: is it a state change, is it from a browser, is it
//! cross-site. One place, read by the middleware, so a route cannot opt out.

use actix_web::http::{header::HeaderMap, Method};
use actix_web::web;
use std::collections::HashMap;

/// The scope the middleware wraps.
const API: &str = "/api/bmc";

/// A request that changes state on the board.
///
/// Every method other than `GET`, `HEAD` and `OPTIONS` is one. So are the
/// three `GET` forms older clients send ([`is_get_state_change`]), which the
/// middleware refuses.
///
/// `path` is the path the router matched, not the raw request target: see
/// [`is_percent_encoded`] and `routed_path` in the middleware.
pub fn is_state_change(method: &Method, path: &str, query: &str) -> bool {
    if is_safe_method(method) {
        is_get_state_change(path, query)
    } else {
        true
    }
}

/// `GET`, `HEAD` and `OPTIONS`: the methods that must not change state.
pub fn is_safe_method(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// The raw path of a request carries percent-encoding.
///
/// The router matches the percent-decoded path (`/api/b%6dc` is `/api/bmc`),
/// so a policy that reads the raw path and one that reads the routed path can
/// disagree about the same request (BMC-Firmware#132). Nothing that talks to
/// this daemon encodes a path: the API paths are fixed words and a numeric
/// handle, and the web interface and `tpi` send the same bytes the OpenAPI
/// document lists. The middleware therefore answers 400 to any request under
/// it whose path has a `%`, and decides everything else on the routed path as
/// well, so a spelling that slips past one is still caught by the other.
/// Query strings are not paths and may be encoded.
pub fn is_percent_encoded(raw_path: &str) -> bool {
    raw_path.contains('%')
}

/// The state changes older clients send on `GET`, refused with 405:
/// `?opt=set` on `/api/bmc` (every type, including the flash and firmware
/// transfer), `/api/bmc/upload/{handle}/cancel` and `/api/bmc/backup`.
///
/// The query is decoded the way the handlers decode it, so `opt=%73et` is
/// `set` here too.
pub fn is_get_state_change(path: &str, query: &str) -> bool {
    let Some(rest) = path.strip_prefix(API) else {
        return false;
    };
    match rest {
        "" | "/" => web::Query::<HashMap<String, String>>::from_query(query)
            .is_ok_and(|q| q.get("opt").map(String::as_str) == Some("set")),
        "/backup" => true,
        _ => rest
            .strip_prefix("/upload/")
            .and_then(|r| r.strip_suffix("/cancel"))
            .is_some_and(|handle| !handle.is_empty() && !handle.contains('/')),
    }
}

/// A request a browser made: it carries a `Sec-Fetch-*`, `Origin` or
/// `Referer` header. `curl` and `tpi` send none of them.
pub fn is_browser_request(headers: &HeaderMap) -> bool {
    headers.contains_key("origin")
        || headers.contains_key("referer")
        || headers
            .keys()
            .any(|name| name.as_str().starts_with("sec-fetch-"))
}

/// `Sec-Fetch-Site` says another site, or another origin of this site, made
/// the request. `same-origin`, `none` (typed in the address bar) and an
/// absent header are not.
pub fn is_cross_site(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|site| {
            site.eq_ignore_ascii_case("cross-site") || site.eq_ignore_ascii_case("same-site")
        })
}

/// The token of an `Authorization: Bearer <token>` line. The scheme is
/// case-insensitive (RFC 9110).
pub fn bearer_token(line: &str) -> Option<&str> {
    let (scheme, token) = line.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// Whether an `Authorization` line is Basic. Case-insensitive, as above.
pub fn is_basic(line: &str) -> bool {
    line.split_once(' ')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("basic"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::http::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        map
    }

    #[test]
    fn every_set_on_the_legacy_path_is_a_state_change() {
        for query in [
            "opt=set&type=power&node1=1",
            "opt=set&type=firmware&file=a.tpu&length=1",
            "opt=set&type=flash&node=0&file=x",
            "type=power&opt=%73et",
        ] {
            assert!(is_state_change(&Method::GET, "/api/bmc", query), "{query}");
            assert!(is_state_change(&Method::POST, "/api/bmc", query), "{query}");
        }
        assert!(!is_state_change(
            &Method::GET,
            "/api/bmc",
            "opt=get&type=power"
        ));
        assert!(!is_state_change(&Method::GET, "/api/bmc/about", ""));
    }

    #[test]
    fn cancel_and_backup_on_get_are_state_changes() {
        assert!(is_get_state_change("/api/bmc/upload/7/cancel", ""));
        assert!(is_get_state_change("/api/bmc/backup", ""));
        assert!(!is_get_state_change("/api/bmc/upload/7", ""));
        assert!(!is_get_state_change("/api/bmc/upload//cancel", ""));
    }

    /// The path the router matches, as actix hands it to a middleware.
    fn routed(uri: &str) -> String {
        actix_web::test::TestRequest::get()
            .uri(uri)
            .to_srv_request()
            .match_info()
            .as_str()
            .to_string()
    }

    /// BMC-Firmware#132: the router matched `/api/bmc` for all of these, and
    /// the policy read the raw path and saw something else.
    #[test]
    fn every_spelling_of_the_api_path_is_decoded_the_way_the_router_decodes_it() {
        let query = "opt=set&type=power&node1=1";
        for spelling in [
            "/api/b%6dc",
            "/api/b%6Dc",
            "/api/bm%63",
            "/api/%62mc",
            "/%61pi/bmc",
            "/%61%70%69/%62%6d%63",
        ] {
            let path = routed(&format!("{spelling}?{query}"));
            assert_eq!(path, "/api/bmc", "{spelling}");
            assert!(is_percent_encoded(spelling), "{spelling}");
            for method in [Method::GET, Method::HEAD] {
                assert!(
                    is_state_change(&method, &path, query),
                    "{method} {spelling}"
                );
            }
            // What the policy used to be given. It is not `/api/bmc`, so it
            // classified a state change as a read.
            assert!(
                !is_state_change(&Method::GET, spelling, query),
                "{spelling}"
            );
        }
    }

    /// `%25`, `%2F` and `%2B` are not decoded by the router, so a double
    /// encoding stays encoded and is no spelling of `/api/bmc`.
    #[test]
    fn a_double_encoding_is_not_decoded_a_second_time() {
        for spelling in [
            "/api/b%256dc",
            "/api/bmc%2Fbackup",
            "/api/bmc%2fbackup",
            "/api/bmc/upload/7%2Fcancel",
        ] {
            let path = routed(spelling);
            assert_eq!(path, spelling);
            assert!(is_percent_encoded(&path), "{spelling}");
            assert!(
                !is_get_state_change(&path, "opt=set&type=power"),
                "{spelling}"
            );
        }
    }

    /// Nothing normalises these: the router matches them as written, so none
    /// of them is `/api/bmc` and none reaches its handlers.
    #[test]
    fn dot_and_slash_spellings_are_not_the_api_path() {
        for spelling in [
            "//api/bmc",
            "/./api/bmc",
            "/api/../api/bmc",
            "/api//bmc",
            "/api/bmc/../bmc",
            "/API/bmc",
        ] {
            assert_eq!(routed(spelling), spelling);
            assert!(
                !is_get_state_change(spelling, "opt=set&type=power"),
                "{spelling}"
            );
        }
    }

    #[test]
    fn a_trailing_slash_is_the_same_resource() {
        assert!(is_get_state_change("/api/bmc/", "opt=set&type=power"));
        assert!(!is_get_state_change("/api/bmc/", "opt=get&type=power"));
    }

    #[test]
    fn only_a_path_without_a_percent_is_plain() {
        for plain in [
            "/",
            "/api/bmc",
            "/api/bmc/upload/7/cancel",
            "/api/bmc/backup",
        ] {
            assert!(!is_percent_encoded(plain), "{plain}");
        }
        for encoded in ["/api/b%6dc", "/%61pi", "/api/bmc/%62ackup", "/100%"] {
            assert!(is_percent_encoded(encoded), "{encoded}");
        }
    }

    #[test]
    fn get_head_and_options_are_the_safe_methods() {
        for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(is_safe_method(&method), "{method}");
        }
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            assert!(!is_safe_method(&method), "{method}");
        }
    }

    #[test]
    fn any_other_method_is_a_state_change() {
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            assert!(is_state_change(&method, "/api/bmc/network/switch", ""));
        }
    }

    #[test]
    fn browser_and_cross_site() {
        assert!(!is_browser_request(&headers(&[])));
        assert!(is_browser_request(&headers(&[("origin", "https://x")])));
        assert!(is_browser_request(&headers(&[("referer", "https://x/")])));
        assert!(is_browser_request(&headers(&[("sec-fetch-dest", "image")])));

        assert!(is_cross_site(&headers(&[("sec-fetch-site", "cross-site")])));
        assert!(is_cross_site(&headers(&[("sec-fetch-site", "same-site")])));
        assert!(!is_cross_site(&headers(&[(
            "sec-fetch-site",
            "same-origin"
        )])));
        assert!(!is_cross_site(&headers(&[("sec-fetch-site", "none")])));
        assert!(!is_cross_site(&headers(&[])));
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer abc"), Some("abc"));
        assert_eq!(bearer_token("BEARER abc"), Some("abc"));
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("Basic abc"), None);
        assert!(is_basic("basic abc"));
        assert!(!is_basic("Bearer abc"));
    }
}
