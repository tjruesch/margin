//! OAuth 2.0 (PKCE) flow runner + per-call refresh helper for cloud
//! connectors (#60).
//!
//! Two public entry points:
//!   - `run_authorization_flow(app, kind)` — opens the system browser,
//!     listens on a localhost loopback for the redirect, exchanges the
//!     code for tokens, persists them to the keychain. Returns the
//!     user's email so the caller can construct a stable `connector_id`.
//!   - `with_valid_token(app, connector_id, kind, f)` — reads tokens,
//!     refreshes if expired or near-expired, hands a valid access token
//!     to the closure, retries once on 401. The main API surface for
//!     a `Connector::sync` implementation.
//!
//! PKCE replaces the client secret. Public clients (desktop apps)
//! generate a verifier, send only the SHA-256 challenge to the auth
//! server, and prove possession at code-exchange time. Means we ship
//! the client_id in the binary safely.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use oauth2::basic::{BasicClient, BasicErrorResponseType, BasicTokenType};
use oauth2::{
    AuthUrl, AuthorizationCode, Client, ClientId, CsrfToken, EmptyExtraTokenFields,
    EndpointNotSet, EndpointSet, PkceCodeChallenge, RedirectUrl, RefreshToken,
    RequestTokenError, RevocationErrorResponseType, Scope, StandardErrorResponse,
    StandardRevocableToken, StandardTokenIntrospectionResponse, StandardTokenResponse,
    TokenResponse, TokenUrl,
};
use serde::Deserialize;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::providers::{self, OAuthProvider};
use super::ConnectorError;
use crate::keychain::{
    delete_connector_tokens, read_connector_tokens, write_connector_tokens, ConnectorTokens,
};

/// First port we try for the OAuth redirect listener; we walk forward
/// to the end of the range looking for one that's free. Both Google
/// and Microsoft accept any localhost port for desktop apps as long
/// as the URI is registered exactly. Register `8765..8784` once in
/// each console.
const REDIRECT_PORT_START: u16 = 8765;
const REDIRECT_PORT_END: u16 = 8784;
/// Hard timeout on the entire flow — opens browser, waits for the
/// user's grant + redirect. 120s gives time for password / 2FA / app
/// switcher confusion.
const FLOW_TIMEOUT: Duration = Duration::from_secs(120);
/// Refresh tokens proactively if they expire within this window.
/// Avoids the case where a sync starts at T-30s and fails 5s in
/// when the token expires mid-flight.
const REFRESH_LEEWAY: Duration = Duration::from_secs(60);

/// Ergonomic alias for the type-state-encoded fully-configured client.
type ConfiguredClient = Client<
    StandardErrorResponse<BasicErrorResponseType>,
    StandardTokenResponse<EmptyExtraTokenFields, BasicTokenType>,
    StandardTokenIntrospectionResponse<EmptyExtraTokenFields, BasicTokenType>,
    StandardRevocableToken,
    StandardErrorResponse<RevocationErrorResponseType>,
    EndpointSet,    // auth_url
    EndpointNotSet, // device_auth_url
    EndpointNotSet, // introspection_url
    EndpointNotSet, // revocation_url
    EndpointSet,    // token_url
>;

#[derive(Debug, Clone)]
pub struct AuthorizationResult {
    /// User's email / UPN, extracted from the id_token. Used to
    /// construct connector_id (`<kind>:<email>`).
    pub email: String,
    pub tokens: ConnectorTokens,
}

/// Drive the OAuth Authorization Code + PKCE flow end-to-end. After
/// this returns, tokens have been persisted to the keychain at
/// `connector::<kind>:<email>` — caller's responsibility is just to
/// upsert the `connectors` table row.
pub async fn run_authorization_flow(
    app: &AppHandle,
    kind: &str,
) -> Result<AuthorizationResult, ConnectorError> {
    let provider = providers::lookup(kind).ok_or_else(|| {
        ConnectorError::Other(format!("unknown OAuth kind: {kind}"))
    })?;
    let client_id = provider.client_id.ok_or_else(|| {
        ConnectorError::Other(format!(
            "{} client ID not configured (set MARGIN_{}_CLIENT_ID at build time)",
            provider.display_name,
            provider.kind.to_uppercase().replace('_', "_")
        ))
    })?;

    // Bind the loopback FIRST — we need the chosen port to build the
    // redirect_uri before opening the browser.
    let (listener, port) = bind_loopback().await?;
    // Match the redirect URI registered in the provider console
    // (no path suffix — keeps Azure / Google registration as just
    // `http://127.0.0.1:<port>`).
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let oauth_client = build_client(provider, client_id, &redirect_uri)?;
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let mut auth_request = oauth_client.authorize_url(CsrfToken::new_random);
    for scope in provider.scopes {
        auth_request = auth_request.add_scope(Scope::new((*scope).to_string()));
    }
    let (auth_url, csrf_token) = auth_request
        .set_pkce_challenge(pkce_challenge)
        .url();

    // Open in the system browser. Failure is recoverable — surface a
    // message asking the user to copy the URL manually. Most users
    // never see this path.
    if let Err(e) = app.opener().open_url(auth_url.as_str(), None::<&str>) {
        eprintln!("[oauth] open_url failed: {e}; flow continues — user can copy URL manually");
    }

    // Wait for the callback. The browser hits `/oauth/callback?code=...&state=...`
    // (or `?error=...`). We accept exactly one connection and parse it.
    let (code, returned_state) = tokio::time::timeout(FLOW_TIMEOUT, accept_callback(listener))
        .await
        .map_err(|_| ConnectorError::Other("OAuth flow timed out (120s)".to_string()))??;

    if returned_state != *csrf_token.secret() {
        return Err(ConnectorError::Other(
            "OAuth state mismatch — possible CSRF; flow aborted".to_string(),
        ));
    }

    // Exchange code for tokens.
    let http_client = reqwest::Client::builder()
        // Block redirects — token endpoint should never redirect.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ConnectorError::Network(format!("http client init: {e}")))?;

    let token_result = oauth_client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http_client)
        .await
        .map_err(map_token_error)?;

    let tokens = token_response_to_stored(&token_result);

    // Extract user identity from the id_token (or fall back to a
    // /userinfo call if absent). For our two supported providers,
    // id_token is always present when openid/User.Read scope is
    // requested.
    let email = extract_email(&token_result, provider, &tokens.access_token, &http_client)
        .await
        .ok_or_else(|| {
            ConnectorError::Other(
                "couldn't determine user email from OAuth response".to_string(),
            )
        })?;

    let connector_id = format!("{}:{}", kind, email);
    write_connector_tokens(&connector_id, &tokens).map_err(ConnectorError::Other)?;

    Ok(AuthorizationResult { email, tokens })
}

/// Run an authenticated request with a guaranteed-fresh access token.
///
/// Refresh logic:
/// - If `expires_at_ms - REFRESH_LEEWAY <= now`, refresh before
///   calling the closure.
/// - If the closure returns an error, we don't auto-retry — the
///   connector decides retry semantics based on its own status code
///   handling. (Adding 401-detection here would require a uniform
///   error type; not worth the abstraction yet.)
///
/// On refresh failure with an `invalid_grant` response, surfaces
/// `ConnectorError::ReauthNeeded` so the runner records the error
/// and the UI can offer a "Reconnect" button.
pub async fn with_valid_token<F, Fut, T>(
    app: &AppHandle,
    connector_id: &str,
    kind: &str,
    f: F,
) -> Result<T, ConnectorError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<T, ConnectorError>>,
{
    let _ = app; // kept on signature for future use (e.g. emit refresh-progress)
    let mut tokens = read_connector_tokens(connector_id)
        .map_err(ConnectorError::Other)?
        .ok_or_else(|| {
            ConnectorError::ReauthNeeded(format!(
                "no stored tokens for connector {connector_id}"
            ))
        })?;

    let now = current_unix_ms();
    let expires_soon =
        tokens.expires_at_ms - (REFRESH_LEEWAY.as_millis() as i64) <= now;
    if expires_soon {
        tokens = refresh_tokens(connector_id, kind, &tokens).await?;
    }

    f(tokens.access_token.clone()).await
}

async fn refresh_tokens(
    connector_id: &str,
    kind: &str,
    current: &ConnectorTokens,
) -> Result<ConnectorTokens, ConnectorError> {
    let provider = providers::lookup(kind).ok_or_else(|| {
        ConnectorError::Other(format!("unknown kind during refresh: {kind}"))
    })?;
    let client_id = provider.client_id.ok_or_else(|| {
        ConnectorError::Other(format!("{} client ID not configured", provider.display_name))
    })?;
    let refresh_token_str = current.refresh_token.as_ref().ok_or_else(|| {
        ConnectorError::ReauthNeeded(
            "no refresh token stored — initial flow didn't return one".to_string(),
        )
    })?;

    // Build a client without a redirect_uri — refresh doesn't use one.
    // We still need it typed correctly; pass a placeholder that the
    // request never references.
    let oauth_client = build_client(provider, client_id, "http://127.0.0.1:0/unused")?;

    let http_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ConnectorError::Network(format!("http client init: {e}")))?;

    let result = oauth_client
        .exchange_refresh_token(&RefreshToken::new(refresh_token_str.clone()))
        .request_async(&http_client)
        .await
        .map_err(map_token_error)?;

    let mut next = token_response_to_stored(&result);
    // Some providers (Google) only return a refresh token on the
    // initial exchange. Preserve the existing one if the refresh
    // response didn't include a new one.
    if next.refresh_token.is_none() {
        next.refresh_token = current.refresh_token.clone();
    }

    write_connector_tokens(connector_id, &next).map_err(ConnectorError::Other)?;
    Ok(next)
}

/// Remove tokens from the keychain. Called by `delete_connector`
/// alongside the DB row deletion.
pub fn forget_tokens(connector_id: &str) -> Result<(), String> {
    delete_connector_tokens(connector_id)
}

// ----- Internal helpers --------------------------------------------------

fn build_client(
    provider: &OAuthProvider,
    client_id: &str,
    redirect_uri: &str,
) -> Result<ConfiguredClient, ConnectorError> {
    let auth_url = AuthUrl::new(provider.auth_url.to_string())
        .map_err(|e| ConnectorError::Other(format!("bad auth_url for {}: {e}", provider.kind)))?;
    let token_url = TokenUrl::new(provider.token_url.to_string())
        .map_err(|e| ConnectorError::Other(format!("bad token_url for {}: {e}", provider.kind)))?;
    let redirect_url = RedirectUrl::new(redirect_uri.to_string())
        .map_err(|e| ConnectorError::Other(format!("bad redirect_uri: {e}")))?;

    let client = BasicClient::new(ClientId::new(client_id.to_string()))
        .set_auth_uri(auth_url)
        .set_token_uri(token_url)
        .set_redirect_uri(redirect_url);
    Ok(client)
}

async fn bind_loopback() -> Result<(TcpListener, u16), ConnectorError> {
    for port in REDIRECT_PORT_START..=REDIRECT_PORT_END {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => return Ok((l, port)),
            Err(_) => continue,
        }
    }
    Err(ConnectorError::Other(format!(
        "no free port in {}..={}",
        REDIRECT_PORT_START, REDIRECT_PORT_END
    )))
}

/// Accept exactly one TCP connection, parse the HTTP request line for
/// `?code=...&state=...` (or `?error=...`), respond with a friendly
/// HTML page, return the code + state.
async fn accept_callback(
    listener: TcpListener,
) -> Result<(String, String), ConnectorError> {
    let (mut stream, _peer) = listener
        .accept()
        .await
        .map_err(|e| ConnectorError::Network(format!("accept callback: {e}")))?;

    // Read until we have at least the request line + first newline.
    // OAuth callbacks don't have request bodies, so we don't need to
    // parse Content-Length.
    let mut buf = vec![0u8; 4096];
    let mut total = 0usize;
    let request_line = loop {
        let n = stream
            .read(&mut buf[total..])
            .await
            .map_err(|e| ConnectorError::Network(format!("read callback: {e}")))?;
        if n == 0 {
            return Err(ConnectorError::Other(
                "callback connection closed before request line".to_string(),
            ));
        }
        total += n;
        if let Some(idx) = buf[..total].iter().position(|&b| b == b'\n') {
            let line = std::str::from_utf8(&buf[..idx])
                .map_err(|e| ConnectorError::Other(format!("callback utf8: {e}")))?
                .trim_end_matches('\r')
                .to_string();
            break line;
        }
        if total >= buf.len() {
            return Err(ConnectorError::Other(
                "callback request line too long".to_string(),
            ));
        }
    };

    // Send the success page back synchronously so the user's tab
    // gets a friendly close-message before our task ends.
    let body = "<html><body style=\"font-family:-apple-system,sans-serif;padding:32px;text-align:center;color:#333\">\
        <h2>Connected to Margin</h2>\
        <p>You can close this tab and return to the app.</p>\
        </body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;

    parse_callback_request_line(&request_line)
}

/// Parse the HTTP request line "GET /oauth/callback?code=...&state=... HTTP/1.1".
fn parse_callback_request_line(line: &str) -> Result<(String, String), ConnectorError> {
    let mut parts = line.split_whitespace();
    let _method = parts
        .next()
        .ok_or_else(|| ConnectorError::Other("malformed callback request line".to_string()))?;
    let path = parts
        .next()
        .ok_or_else(|| ConnectorError::Other("missing path on callback".to_string()))?;
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");

    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    let mut error: Option<String> = None;
    for pair in query.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        let v_decoded = url_decode(v);
        match k {
            "code" => code = Some(v_decoded),
            "state" => state = Some(v_decoded),
            "error" => error = Some(v_decoded),
            _ => {}
        }
    }
    if let Some(err) = error {
        return Err(ConnectorError::Other(format!("OAuth provider returned error: {err}")));
    }
    let code = code.ok_or_else(|| {
        ConnectorError::Other("no `code` in OAuth callback".to_string())
    })?;
    let state = state.ok_or_else(|| {
        ConnectorError::Other("no `state` in OAuth callback".to_string())
    })?;
    Ok((code, state))
}

fn url_decode(s: &str) -> String {
    // Minimal — just %XX and `+` → space. Sufficient for OAuth tokens
    // and state strings, which are URL-safe base64 in practice anyway.
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'+' {
            out.push(' ');
            i += 1;
        } else if b == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            match (hi, lo) {
                (Some(h), Some(l)) => {
                    out.push(((h * 16 + l) as u8) as char);
                    i += 3;
                }
                _ => {
                    out.push(b as char);
                    i += 1;
                }
            }
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    out
}

fn token_response_to_stored(
    resp: &StandardTokenResponse<EmptyExtraTokenFields, BasicTokenType>,
) -> ConnectorTokens {
    let access_token = resp.access_token().secret().clone();
    let refresh_token = resp.refresh_token().map(|r| r.secret().clone());
    let expires_at_ms = current_unix_ms()
        + resp
            .expires_in()
            .map(|d| d.as_millis() as i64)
            // Conservative default if the provider didn't tell us.
            .unwrap_or(60 * 60 * 1000);
    let scope = resp
        .scopes()
        .map(|scopes| {
            scopes
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    ConnectorTokens {
        access_token,
        refresh_token,
        expires_at_ms,
        scope,
    }
}

/// Try to extract the user's email from the id_token first, falling
/// back to a userinfo call. Both Google and Microsoft put a usable
/// identifier somewhere — we don't care about the exact form, just
/// that it's stable per user account.
async fn extract_email(
    resp: &StandardTokenResponse<EmptyExtraTokenFields, BasicTokenType>,
    provider: &OAuthProvider,
    access_token: &str,
    http_client: &reqwest::Client,
) -> Option<String> {
    // The basic token response doesn't expose id_token directly, but
    // it's serializable with extra fields. We re-deserialize the raw
    // JSON to grab it. Quick + avoids a custom ExtraTokenFields type
    // for a one-shot.
    if let Ok(extra) = serde_json::to_value(resp) {
        if let Some(id_token) = extra.get("id_token").and_then(|v| v.as_str()) {
            if let Some(email) = email_from_id_token(id_token) {
                return Some(email);
            }
        }
    }

    // Fallback: provider-specific userinfo endpoint.
    let url = match provider.kind {
        "google" => "https://openidconnect.googleapis.com/v1/userinfo",
        "microsoft_graph" => "https://graph.microsoft.com/v1.0/me",
        _ => return None,
    };
    let resp = http_client
        .get(url)
        .bearer_auth(access_token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("email")
        .or_else(|| body.get("mail"))
        .or_else(|| body.get("userPrincipalName"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn email_from_id_token(id_token: &str) -> Option<String> {
    // JWT: header.payload.signature, all base64url. We don't verify
    // the signature — TLS to the provider's well-known token endpoint
    // is the trust anchor here.
    let payload_b64 = id_token.split('.').nth(1)?;
    let payload_bytes = base64_url_decode(payload_b64)?;
    let claims: IdTokenClaims = serde_json::from_slice(&payload_bytes).ok()?;
    claims
        .email
        .or(claims.preferred_username)
        .or(claims.upn)
}

#[derive(Deserialize)]
struct IdTokenClaims {
    email: Option<String>,
    preferred_username: Option<String>,
    upn: Option<String>,
}

fn base64_url_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .ok()
        .or_else(|| {
            // Some providers use padded URL-safe encoding.
            base64::engine::general_purpose::URL_SAFE.decode(s).ok()
        })
}

fn map_token_error<E>(
    e: RequestTokenError<E, StandardErrorResponse<BasicErrorResponseType>>,
) -> ConnectorError
where
    E: std::error::Error + 'static,
{
    match e {
        RequestTokenError::ServerResponse(resp) => match resp.error() {
            BasicErrorResponseType::InvalidGrant => {
                ConnectorError::ReauthNeeded("invalid_grant — token revoked".to_string())
            }
            other => ConnectorError::Other(format!(
                "token endpoint returned {other:?}: {}",
                resp.error_description().map(String::as_str).unwrap_or("")
            )),
        },
        RequestTokenError::Request(err) => ConnectorError::Network(err.to_string()),
        RequestTokenError::Parse(err, _) => {
            ConnectorError::Other(format!("token response parse: {err}"))
        }
        RequestTokenError::Other(s) => ConnectorError::Other(s),
    }
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ----- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_callback_extracts_code_and_state() {
        let line = "GET /oauth/callback?code=4%2F0AeanS&state=abc123 HTTP/1.1";
        let (code, state) = parse_callback_request_line(line).unwrap();
        assert_eq!(code, "4/0AeanS");
        assert_eq!(state, "abc123");
    }

    #[test]
    fn parse_callback_propagates_provider_error() {
        let line = "GET /oauth/callback?error=access_denied&state=xyz HTTP/1.1";
        let err = parse_callback_request_line(line).unwrap_err();
        assert!(matches!(err, ConnectorError::Other(ref m) if m.contains("access_denied")));
    }

    #[test]
    fn url_decode_handles_percent_encoding() {
        assert_eq!(url_decode("hello%20world"), "hello world");
        assert_eq!(url_decode("a%2Fb"), "a/b");
        assert_eq!(url_decode("plain"), "plain");
    }

    #[test]
    fn email_from_id_token_extracts_email_claim() {
        // Hand-crafted JWT with `email` claim. Header is irrelevant.
        let header = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            r#"{"email":"alice@example.com","sub":"123"}"#,
        );
        let token = format!("{header}.{payload}.sig");
        assert_eq!(
            email_from_id_token(&token),
            Some("alice@example.com".to_string())
        );
    }

    #[test]
    fn email_from_id_token_falls_back_to_preferred_username() {
        let header = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            r#"{"preferred_username":"bob@contoso.com"}"#,
        );
        let token = format!("{header}.{payload}.sig");
        assert_eq!(
            email_from_id_token(&token),
            Some("bob@contoso.com".to_string())
        );
    }
}

// re-export base64 Engine trait so tests can use encode without a separate use clause.
#[cfg(test)]
use base64::Engine as _;
