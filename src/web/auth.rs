//! Authentication and browser-security checks (docs/design.md §13.2).

use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use super::AppState;
use super::views::PageError;

const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
     img-src 'self' data:; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// How a request proved it knows the token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Credential {
    /// `Authorization: Bearer` — sent by the `lani` CLI; cannot be forged cross-site.
    Bearer,
    /// The session cookie set by `/login` — sent by the browser.
    Cookie,
}

/// The middleware in front of every route: Host check, authentication, same-origin check for
/// state changes, activity tracking for the idle timer, and security headers on every response.
pub async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let mut response = check(&state, request, next).await;
    add_security_headers(response.headers_mut());
    response
}

async fn check(state: &AppState, request: Request, next: Next) -> Response {
    let instance = state.instance();
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    if !allowed_host(host, instance.port) {
        // A foreign Host means DNS rebinding or a misdirected request.
        return PageError::new(
            StatusCode::MISDIRECTED_REQUEST,
            "Wrong address",
            "LANI only answers requests addressed to 127.0.0.1 or localhost.",
        )
        .into_response();
    }
    if request.uri().path() == "/login" {
        return next.run(request).await;
    }
    let Some(credential) = credential(request.headers(), &instance.token, instance.port) else {
        return PageError::new(
            StatusCode::UNAUTHORIZED,
            "Not signed in",
            "Open LANI with `lani open`, or ask Claude to open it, to get a fresh link.",
        )
        .into_response();
    };
    let changes_state = !matches!(*request.method(), Method::GET | Method::HEAD);
    if credential == Credential::Cookie
        && changes_state
        && !same_origin(request.headers(), instance.port)
    {
        return PageError::new(
            StatusCode::FORBIDDEN,
            "Request refused",
            "This request did not come from a LANI page, so it was refused.",
        )
        .into_response();
    }
    if request.uri().path() != "/api/health" {
        state.record_activity();
    }
    next.run(request).await
}

/// Whether the `Host` header names this server on the loopback interface.
#[must_use]
pub fn allowed_host(host: Option<&str>, port: u16) -> bool {
    host.is_some_and(|host| {
        host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
    })
}

/// Whether a state-changing request came from one of this server's own pages.
#[must_use]
pub fn same_origin(headers: &HeaderMap, port: u16) -> bool {
    if let Some(origin) = headers.get(header::ORIGIN) {
        return origin.to_str().is_ok_and(|origin| {
            origin == format!("http://127.0.0.1:{port}")
                || origin == format!("http://localhost:{port}")
        });
    }
    headers
        .get("sec-fetch-site")
        .is_some_and(|site| site.as_bytes() == b"same-origin")
}

/// Whether `next` is a local path that is safe to redirect to after login.
#[must_use]
pub fn is_safe_next(next: &str) -> bool {
    next.starts_with('/')
        && !next.starts_with("//")
        && !next.starts_with("/\\")
        && !next.chars().any(char::is_control)
}

fn credential(headers: &HeaderMap, token: &str, port: u16) -> Option<Credential> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if bearer.is_some_and(|candidate| token_matches(candidate, token)) {
        return Some(Credential::Bearer);
    }
    let cookie_name = cookie_name(port);
    let from_cookie = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .any(|(name, value)| name == cookie_name && token_matches(value, token));
    from_cookie.then_some(Credential::Cookie)
}

fn token_matches(candidate: &str, token: &str) -> bool {
    candidate.as_bytes().ct_eq(token.as_bytes()).into()
}

/// The session cookie's name. It includes the port because browsers share 127.0.0.1 cookies
/// across ports, so two users' servers would otherwise overwrite each other's cookie.
fn cookie_name(port: u16) -> String {
    format!("lani_{port}")
}

fn add_security_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

#[derive(Deserialize)]
pub struct LoginQuery {
    token: String,
    next: Option<String>,
}

/// `GET /login?token=…&next=…`: exchange the token for a session cookie, then redirect.
pub async fn login(State(state): State<AppState>, Query(query): Query<LoginQuery>) -> Response {
    let instance = state.instance();
    if !token_matches(&query.token, &instance.token) {
        return PageError::new(
            StatusCode::UNAUTHORIZED,
            "Link expired",
            "This sign-in link is not valid for the running LANI server (it may have restarted). \
             Run `lani open`, or ask Claude to open LANI, to get a fresh link.",
        )
        .into_response();
    }
    state.record_activity();
    let next = query
        .next
        .filter(|next| is_safe_next(next))
        .unwrap_or_else(|| "/".to_string());
    let cookie = format!(
        "{}={}; HttpOnly; SameSite=Strict; Path=/",
        cookie_name(instance.port),
        instance.token
    );
    match (HeaderValue::from_str(&next), HeaderValue::from_str(&cookie)) {
        (Ok(location), Ok(cookie)) => (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, location), (header::SET_COOKIE, cookie)],
        )
            .into_response(),
        _ => PageError::new(
            StatusCode::BAD_REQUEST,
            "Bad link",
            "The link's target is not valid.",
        )
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert!(allowed_host(Some("127.0.0.1:4242"), 4242));
        assert!(allowed_host(Some("localhost:4242"), 4242));
        assert!(!allowed_host(Some("127.0.0.1:4243"), 4242));
        assert!(!allowed_host(Some("evil.example:4242"), 4242));
        assert!(!allowed_host(None, 4242));
    }

    #[test]
    fn origins() {
        let mut headers = HeaderMap::new();
        assert!(!same_origin(&headers, 4242), "no evidence means refuse");
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(same_origin(&headers, 4242));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://evil.example"),
        );
        assert!(
            !same_origin(&headers, 4242),
            "a foreign Origin wins over Sec-Fetch-Site"
        );
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:4242"),
        );
        assert!(same_origin(&headers, 4242));
    }

    #[test]
    fn next_paths() {
        assert!(is_safe_next("/reviews/3"));
        assert!(is_safe_next("/"));
        for unsafe_next in [
            "https://evil.example",
            "//evil.example",
            "/\\evil.example",
            "reviews",
            "/a\r\nSet-Cookie: x",
        ] {
            assert!(!is_safe_next(unsafe_next), "{unsafe_next:?}");
        }
    }

    #[test]
    fn credentials() {
        let token = "abc123";
        let mut headers = HeaderMap::new();
        assert_eq!(credential(&headers, token, 1), None);
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=1; lani_1=abc123"),
        );
        assert_eq!(credential(&headers, token, 1), Some(Credential::Cookie));
        assert_eq!(
            credential(&headers, token, 2),
            None,
            "cookie for another port"
        );
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc123"),
        );
        assert_eq!(credential(&headers, token, 1), Some(Credential::Bearer));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        headers.insert(header::COOKIE, HeaderValue::from_static("lani_1=abc12"));
        assert_eq!(credential(&headers, token, 1), None);
    }
}
