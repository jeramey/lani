//! Authentication and browser-security checks (docs/design.md §13.2).

use askama::Template;
use axum::Extension;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use super::AppState;
use super::views::PageError;
use crate::daemon::Endpoint;

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

/// The port the browser addressed, from the `Host` header. Through SSH port forwarding it can
/// differ from the port this server listens on (§13.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrowserPort(pub u16);

async fn check(state: &AppState, mut request: Request, next: Next) -> Response {
    let instance = state.instance();
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let (Some(host), Some(port)) = (host.as_deref(), browser_port(host.as_deref())) else {
        // A foreign Host means DNS rebinding or a misdirected request.
        return PageError::new(
            StatusCode::MISDIRECTED_REQUEST,
            "Wrong address",
            "LANI only answers requests addressed to 127.0.0.1 or localhost.",
        )
        .into_response();
    };
    request.extensions_mut().insert(BrowserPort(port));
    if request.uri().path() == "/login" {
        return next.run(request).await;
    }
    let Some(credential) = credential(request.headers(), &instance.token, port) else {
        let message = match &instance.endpoint {
            Endpoint::Tcp { .. } => {
                "Open LANI with `lani open`, or ask Claude to open it, to get a fresh link."
                    .to_string()
            }
            socket @ Endpoint::Socket { .. } => sign_in_on(socket),
        };
        return PageError::new(StatusCode::UNAUTHORIZED, "Not signed in", &message).into_response();
    };
    let changes_state = !matches!(*request.method(), Method::GET | Method::HEAD);
    if credential == Credential::Cookie && changes_state && !same_origin(request.headers(), host) {
        return PageError::new(
            StatusCode::FORBIDDEN,
            "Request refused",
            "This request did not come from a LANI page, so it was refused.",
        )
        .into_response();
    }
    if counts_as_activity(request.uri().path()) {
        state.record_activity();
    }
    next.run(request).await
}

/// Whether a request to `path` keeps the server from stopping when idle (§13.1). Health checks
/// don't, and neither do live-update streams, so a forgotten browser tab cannot keep the server
/// running forever.
fn counts_as_activity(path: &str) -> bool {
    let live_updates = path
        .strip_prefix("/reviews/")
        .and_then(|rest| rest.strip_suffix("/events"))
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()));
    path != "/api/health" && !live_updates
}

/// The port the browser addressed, if the `Host` header names the loopback interface:
/// `127.0.0.1:<port>` or `localhost:<port>`. `None` for any other `Host`.
///
/// Any port is accepted, not only the server's own, so that LANI works through SSH port
/// forwarding to a different local port (§13.2). The host name is what blocks DNS rebinding: a
/// rebinding page's requests carry its own domain name.
#[must_use]
pub fn browser_port(host: Option<&str>) -> Option<u16> {
    let (name, port) = host?.rsplit_once(':')?;
    let digits = !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit());
    if !(matches!(name, "127.0.0.1" | "localhost") && digits) {
        return None;
    }
    port.parse().ok().filter(|port| *port != 0)
}

/// Whether a state-changing request came from one of this server's own pages, as the browser
/// addressed it: `host` is the request's `Host` header, already checked by [`browser_port`].
///
/// A matching `Origin` header is enough. `Origin: null` (which browsers send in some contexts,
/// for example under a `no-referrer` policy) carries no information, so it is treated like a
/// missing header: then the browser-set `Sec-Fetch-Site: same-origin`, which pages cannot forge,
/// is required.
#[must_use]
pub fn same_origin(headers: &HeaderMap, host: &str) -> bool {
    let origin = headers
        .get(header::ORIGIN)
        .filter(|origin| origin.as_bytes() != b"null");
    if let Some(origin) = origin {
        return origin
            .to_str()
            .is_ok_and(|origin| origin.strip_prefix("http://") == Some(host));
    }
    headers
        .get("sec-fetch-site")
        .is_some_and(|site| site.as_bytes() == b"same-origin")
}

/// Whether `next` is a local path that is safe to continue to after login. It is embedded in
/// the sign-in page's meta refresh, so only a small character set is allowed: letters, digits
/// and `-._~/?=&#%+`. That covers every path LANI links to.
#[must_use]
pub fn is_safe_next(next: &str) -> bool {
    next.starts_with('/')
        && !next.starts_with("//")
        && next.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'-' | b'.' | b'_' | b'~' | b'/' | b'?' | b'=' | b'&' | b'#' | b'%' | b'+'
                )
        })
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

/// The session cookie's name, with the port the browser addressed. Browsers share 127.0.0.1
/// cookies across ports, so two servers would otherwise overwrite each other's cookie: two
/// users' servers, or your own and a remote one forwarded to another local port (§13.2).
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
        // `same-origin`, not `no-referrer`: under `no-referrer` browsers send `Origin: null` on
        // form posts, which the same-origin check cannot use. `same-origin` still sends no
        // referrer to other sites, so LANI URLs do not leak.
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

#[derive(Deserialize)]
pub struct LoginQuery {
    token: String,
    next: Option<String>,
}

/// `GET /login?token=…&next=…`: exchange the token for a session cookie, then redirect.
pub async fn login(
    State(state): State<AppState>,
    Extension(BrowserPort(port)): Extension<BrowserPort>,
    Query(query): Query<LoginQuery>,
) -> Response {
    let instance = state.instance();
    if !token_matches(&query.token, &instance.token) {
        let fresh_link = match &instance.endpoint {
            Endpoint::Tcp { .. } => {
                "Run `lani open`, or ask Claude to open LANI, to get a fresh link.".to_string()
            }
            socket @ Endpoint::Socket { .. } => sign_in_on(socket),
        };
        return PageError::new(
            StatusCode::UNAUTHORIZED,
            "Link expired",
            &format!(
                "This sign-in link is not valid for the running LANI server (it may have \
                 restarted). {fresh_link}"
            ),
        )
        .into_response();
    }
    state.record_activity();
    let next = query
        .next
        .filter(|next| is_safe_next(next))
        .unwrap_or_else(|| "/".to_string());
    // SameSite=Strict, because browsers send 127.0.0.1 cookies to every port on the host: a Lax
    // cookie would travel with any website's first navigation to another local port, handing the
    // token to whatever listens there. Strict reduces that exposure but does not close it.
    // SameSite compares sites, not ports, so once a page from another loopback port is showing,
    // its own requests (a reload, an image, the favicon) carry the cookie. That remaining
    // exposure is accepted and documented (design §13.2, §15).
    //
    // `lani open` reaches /login from a local redirect file, which browsers treat as cross-site,
    // and they withhold Strict cookies for the rest of that navigation, including any HTTP
    // redirect. So instead of a 303 this answers with a page that moves on to `next` itself; that
    // navigation starts from a 127.0.0.1 page, so it is same-site and carries the cookie
    // (design §13.2).
    let cookie = format!(
        "{}={}; HttpOnly; SameSite=Strict; Path=/",
        cookie_name(port),
        instance.token
    );
    let page = SignInPage { next: &next };
    match (HeaderValue::from_str(&cookie), page.render()) {
        (Ok(cookie), Ok(html)) => {
            (StatusCode::OK, [(header::SET_COOKIE, cookie)], Html(html)).into_response()
        }
        (Err(_), _) => PageError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong",
            "could not build the session cookie",
        )
        .into_response(),
        (_, Err(error)) => PageError::from(error).into_response(),
    }
}

/// How to get a sign-in link for a socket-mode server: `lani socket`, run on its own machine.
/// Nothing opens a browser for such a server, and Claude never passes the token on (§13.2).
fn sign_in_on(endpoint: &Endpoint) -> String {
    format!(
        "Run `lani socket` on {} and open the sign-in link it prints.",
        endpoint.machine()
    )
}

/// Sent by `/login` with the session cookie; continues to `next` with a same-site navigation.
#[derive(Template)]
#[template(path = "signin.html")]
struct SignInPage<'a> {
    next: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_checks_and_live_updates_are_not_activity() {
        assert!(!counts_as_activity("/api/health"));
        assert!(!counts_as_activity("/reviews/12/events"));
        assert!(counts_as_activity("/reviews/12"));
        assert!(
            counts_as_activity("/reviews/12/files/events"),
            "a file named events"
        );
        assert!(counts_as_activity("/reviews//events"));
    }

    #[test]
    fn hosts() {
        assert_eq!(browser_port(Some("127.0.0.1:4242")), Some(4242));
        assert_eq!(browser_port(Some("localhost:4242")), Some(4242));
        assert_eq!(
            browser_port(Some("127.0.0.1:4243")),
            Some(4243),
            "a forwarded port"
        );
        for refused in [
            "evil.example:4242",
            "127.0.0.1.evil.example:4242",
            "127.0.0.1",
            "127.0.0.1:",
            "127.0.0.1:0",
            "127.0.0.1:+80",
            "127.0.0.1:70000",
            "localhost:4242:4242",
        ] {
            assert_eq!(browser_port(Some(refused)), None, "{refused}");
        }
        assert_eq!(browser_port(None), None);
    }

    #[test]
    fn origins() {
        let host = "localhost:4242";
        let mut headers = HeaderMap::new();
        assert!(!same_origin(&headers, host), "no evidence means refuse");
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(same_origin(&headers, host));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://evil.example"),
        );
        assert!(
            !same_origin(&headers, host),
            "a foreign Origin wins over Sec-Fetch-Site"
        );
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:4242"),
        );
        assert!(same_origin(&headers, host));
        for other in [
            "http://127.0.0.1:4242",
            "http://localhost:4243",
            "https://localhost:4242",
        ] {
            headers.insert(header::ORIGIN, HeaderValue::from_static(other));
            assert!(
                !same_origin(&headers, host),
                "{other} is another origin than the page's"
            );
        }

        // `Origin: null` is no evidence either way; Sec-Fetch-Site decides.
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, HeaderValue::from_static("null"));
        assert!(!same_origin(&headers, host));
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!same_origin(&headers, host));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(same_origin(&headers, host));
    }

    #[test]
    fn next_paths() {
        assert!(is_safe_next("/reviews/3"));
        assert!(is_safe_next("/"));
        assert!(is_safe_next("/reviews/3/issues/7?status=all#c2"));
        for unsafe_next in [
            "https://evil.example",
            "//evil.example",
            "/\\evil.example",
            "reviews",
            "/a\r\nSet-Cookie: x",
            "/x\"><script>",
            "/x;url=https://evil.example",
            "/x y",
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
