//! Serving the web interface, and what the browser may remember of it.
//!
//! `index.html` names the built chunks (`/assets/index-<hash>.js`), and the
//! chunks are replaced on every firmware upgrade. A browser that keeps an old
//! `index.html` asks for chunks that no longer exist, so `index.html` is sent
//! `no-cache` (always revalidate, cheaply, with the ETag) and the hashed
//! chunks are sent `immutable`, since a new build never reuses a name.
//!
//! Without a `Cache-Control` at all, browsers apply heuristic freshness off
//! `Last-Modified` and can run the old interface for days.

use actix_files::{Files, NamedFile};
use actix_service::fn_service;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::header::{
    HeaderMap, HeaderValue, CACHE_CONTROL, CONTENT_SECURITY_POLICY, X_CONTENT_TYPE_OPTIONS,
    X_FRAME_OPTIONS,
};
use actix_web::{web, Error, HttpRequest, HttpResponse};
use std::path::{Path, PathBuf};

const REVALIDATE: &str = "no-cache";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const ASSETS: &str = "/assets/";

/// What the built interface (BMC-UI 4.8.0) needs, and nothing else. It was
/// read off the release bundle: one module script and its chunks from the same
/// origin, no inline script, no `eval`, no worker, fonts and images served as
/// files, and the API, the upload and the serial console (a WebSocket) on the
/// same host.
///
/// * `script-src 'self'` is the point of the header: an injected `<script>`
///   or an `onclick=` handler does not run.
/// * `style-src` keeps `'unsafe-inline'` because the terminal and the theme
///   switch add `<style>` elements and the components set `style` attributes.
///   Styles cannot run script, so this costs little.
/// * `connect-src 'self'` covers `fetch`, the upload and the serial console
///   WebSocket: browsers match `'self'` against `ws:` and `wss:` of the same
///   host (CSP level 3; Safari before 15.4 did not).
/// * `form-action 'self'`, `base-uri 'none'` and `object-src 'none'` close the
///   ways an injected element could send a form elsewhere, rebase every
///   relative URL or load a plugin.
///
/// It applies to the documents bmcd serves, so to the board's own interface.
/// When the interface is served by a fleet pod, that pod's policy governs the
/// page and the board's header is not involved: the board only answers the
/// pod's API calls, and a CSP on an API response (JSON, the backup archive)
/// restricts nothing. `frame-ancestors 'none'` stays the same as before.
const CONTENT_SECURITY: &str = "default-src 'self'; \
script-src 'self'; \
style-src 'self' 'unsafe-inline'; \
img-src 'self' data:; \
font-src 'self'; \
connect-src 'self'; \
object-src 'none'; \
base-uri 'none'; \
form-action 'self'; \
frame-ancestors 'none'";

fn browser_guards(headers: &mut HeaderMap) {
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY),
    );
}

/// The policy for a path that was found on disk.
fn cache_control(path: &str) -> &'static str {
    if path.starts_with(ASSETS) {
        IMMUTABLE
    } else {
        REVALIDATE
    }
}

/// Any other path is a client-side route (`/power-control`), which must return
/// `index.html` on a hard load. A missing chunk is not a route: answering it
/// with HTML is what makes a stale `index.html` fail on a MIME type.
async fn fallback(req: HttpRequest, www: PathBuf) -> Result<HttpResponse, Error> {
    if req.path().starts_with(ASSETS) {
        let mut response = HttpResponse::NotFound().finish();
        browser_guards(response.headers_mut());
        return Ok(response);
    }
    let index = NamedFile::open_async(www.join("index.html")).await?;
    let mut response = index.into_response(&req);
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(REVALIDATE));
    browser_guards(headers);
    Ok(response)
}

/// Must be the last item of the app: it answers every path left over.
pub fn config(www: &Path) -> impl FnOnce(&mut web::ServiceConfig) {
    let www = www.to_path_buf();
    move |cfg| {
        let for_files = www.clone();
        cfg.service(
            web::scope("")
                .wrap_fn(|req, srv| {
                    let path = req.path().to_owned();
                    let call = actix_service::Service::call(srv, req);
                    async move {
                        let mut response = call.await?;
                        let status = response.status();
                        if status.is_success() || status == 304 {
                            let headers = response.headers_mut();
                            if !headers.contains_key(CACHE_CONTROL) {
                                headers.insert(
                                    CACHE_CONTROL,
                                    HeaderValue::from_static(cache_control(&path)),
                                );
                            }
                            browser_guards(headers);
                        }
                        Ok(response)
                    }
                })
                .service(
                    Files::new("/", &www)
                        .index_file("index.html")
                        .default_handler(fn_service(move |req: ServiceRequest| {
                            let www = for_files.clone();
                            async move {
                                let (req, _) = req.into_parts();
                                let response = fallback(req.clone(), www).await?;
                                Ok(ServiceResponse::new(req, response))
                            }
                        })),
                ),
        )
        .default_service(web::to(move |req: HttpRequest| fallback(req, www.clone())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{http::StatusCode, test, App};
    use tempfile::TempDir;

    fn www() -> TempDir {
        let dir = TempDir::with_prefix("www").unwrap();
        std::fs::create_dir(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("index.html"), "<html>ui</html>").unwrap();
        std::fs::write(dir.path().join("favicon.ico"), "icon").unwrap();
        std::fs::write(dir.path().join("assets/app-abc123.js"), "export{}").unwrap();
        dir
    }

    async fn get(path: &str) -> (StatusCode, Option<String>, String) {
        let dir = www();
        let app = test::init_service(
            App::new()
                .service(web::scope("/api/bmc").route("/ping", web::get().to(|| async { "pong" })))
                .configure(config(dir.path())),
        )
        .await;
        let res = test::call_service(&app, test::TestRequest::get().uri(path).to_request()).await;
        let status = res.status();
        let headers = res.headers();
        if !path.starts_with("/api/") {
            assert_eq!(
                headers
                    .get(X_CONTENT_TYPE_OPTIONS)
                    .and_then(|v| v.to_str().ok()),
                Some("nosniff")
            );
            assert_eq!(
                headers.get(X_FRAME_OPTIONS).and_then(|v| v.to_str().ok()),
                Some("DENY")
            );
            assert_eq!(
                headers
                    .get(CONTENT_SECURITY_POLICY)
                    .and_then(|v| v.to_str().ok()),
                Some(CONTENT_SECURITY)
            );
        }
        let cc = headers
            .get(CACHE_CONTROL)
            .map(|v| v.to_str().unwrap().to_owned());
        let body = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
        (status, cc, body)
    }

    /// The policy that is sent, as a set of directives, so that the tests say
    /// what it forbids and not only that it is a particular string.
    fn directives() -> std::collections::HashMap<String, Vec<String>> {
        CONTENT_SECURITY
            .split(';')
            .map(|d| {
                let mut parts = d.split_whitespace().map(str::to_owned);
                (parts.next().unwrap(), parts.collect())
            })
            .collect()
    }

    #[actix_web::test]
    async fn policy_keeps_script_to_own_origin() {
        let d = directives();
        assert_eq!(d["script-src"], ["'self'"]);
        assert_eq!(d["default-src"], ["'self'"]);
        assert!(!CONTENT_SECURITY.contains("'unsafe-eval'"));
        assert!(!d["script-src"].iter().any(|v| v.contains("unsafe")));
    }

    #[actix_web::test]
    async fn policy_closes_plugins_base_and_framing() {
        let d = directives();
        assert_eq!(d["object-src"], ["'none'"]);
        assert_eq!(d["base-uri"], ["'none'"]);
        assert_eq!(d["frame-ancestors"], ["'none'"]);
        assert_eq!(d["form-action"], ["'self'"]);
    }

    #[actix_web::test]
    async fn policy_lets_the_interface_reach_its_own_board_only() {
        let d = directives();
        assert_eq!(d["connect-src"], ["'self'"]);
        assert_eq!(d["font-src"], ["'self'"]);
        assert_eq!(d["img-src"], ["'self'", "data:"]);
    }

    #[actix_web::test]
    async fn every_ui_response_carries_the_policy() {
        // `get` asserts the headers on each of these, found or not.
        for path in ["/", "/index.html", "/power-control", "/favicon.ico"] {
            get(path).await;
        }
        let (status, _, _) = get("/assets/missing.js").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn api_responses_carry_no_policy() {
        let dir = www();
        let app = test::init_service(
            App::new()
                .service(web::scope("/api/bmc").route("/ping", web::get().to(|| async { "pong" })))
                .configure(config(dir.path())),
        )
        .await;
        let res = test::call_service(
            &app,
            test::TestRequest::get().uri("/api/bmc/ping").to_request(),
        )
        .await;
        assert!(res.headers().get(CONTENT_SECURITY_POLICY).is_none());
    }

    #[actix_web::test]
    async fn index_at_root_must_revalidate() {
        let (status, cc, body) = get("/").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cc.as_deref(), Some("no-cache"));
        assert!(body.contains("ui"));
    }

    #[actix_web::test]
    async fn index_by_name_must_revalidate() {
        let (_, cc, _) = get("/index.html").await;
        assert_eq!(cc.as_deref(), Some("no-cache"));
    }

    #[actix_web::test]
    async fn client_route_falls_back_to_index_and_must_revalidate() {
        let (status, cc, body) = get("/power-control").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cc.as_deref(), Some("no-cache"));
        assert!(body.contains("ui"));
    }

    #[actix_web::test]
    async fn hashed_asset_is_immutable() {
        let (status, cc, _) = get("/assets/app-abc123.js").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cc.as_deref(), Some("public, max-age=31536000, immutable"));
    }

    #[actix_web::test]
    async fn other_root_file_must_revalidate() {
        let (status, cc, _) = get("/favicon.ico").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cc.as_deref(), Some("no-cache"));
    }

    #[actix_web::test]
    async fn missing_asset_is_404_not_index() {
        let (status, _, body) = get("/assets/missing.js").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!body.contains("ui"));
    }

    #[actix_web::test]
    async fn unrouted_api_path_still_answers_index() {
        // Documented in the README: kept as it was.
        let (status, cc, body) = get("/api/bmc/nothing-here").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("ui"));
        assert_eq!(cc.as_deref(), Some("no-cache"));
    }

    #[actix_web::test]
    async fn api_routes_get_no_cache_header() {
        let (status, cc, body) = get("/api/bmc/ping").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "pong");
        assert_eq!(cc, None);
    }

    /// A repeat load: the browser sends the ETag back and must get a 304 that
    /// still says how long it may be kept.
    async fn revalidate(path: &str) -> (StatusCode, Option<String>) {
        let dir = www();
        let app = test::init_service(App::new().configure(config(dir.path()))).await;
        let first = test::call_service(&app, test::TestRequest::get().uri(path).to_request()).await;
        let etag = first.headers().get("etag").expect("etag").clone();
        let req = test::TestRequest::get()
            .uri(path)
            .insert_header(("if-none-match", etag))
            .to_request();
        let res = test::call_service(&app, req).await;
        let cc = res
            .headers()
            .get(CACHE_CONTROL)
            .map(|v| v.to_str().unwrap().to_owned());
        (res.status(), cc)
    }

    #[actix_web::test]
    async fn index_304_keeps_no_cache() {
        let (status, cc) = revalidate("/").await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert_eq!(cc.as_deref(), Some("no-cache"));
    }

    #[actix_web::test]
    async fn hashed_asset_304_keeps_immutable() {
        let (status, cc) = revalidate("/assets/app-abc123.js").await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert_eq!(cc.as_deref(), Some("public, max-age=31536000, immutable"));
    }
}
