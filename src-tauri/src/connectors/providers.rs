//! Static metadata for the OAuth providers Margin supports.
//!
//! Each entry hardcodes the public auth/token URLs and the scope set
//! the connector needs. Client IDs are read from environment variables
//! at build time (`option_env!`) so no secrets land in the source tree
//! — see README's "OAuth client ID setup" section.
//!
//! Adding a new provider is one entry in `PROVIDERS`. The actual
//! `Connector` factory and API client live in the connector's own
//! module (e.g. `connectors/google_calendar.rs`, landing in #61).
//!
//! PKCE is the protection model: desktop apps register the client ID
//! as a "public client" in the provider's developer console, which
//! means no client secret. The flow is secured by the PKCE
//! verifier/challenge pair instead.

#[derive(Debug, Clone, Copy)]
pub struct OAuthProvider {
    pub kind: &'static str,
    pub display_name: &'static str,
    pub auth_url: &'static str,
    pub token_url: &'static str,
    pub scopes: &'static [&'static str],
    /// `None` when the build doesn't have a client ID for this
    /// provider — surfaced to the frontend's "Add connector" picker
    /// so users don't see a button that can't actually work.
    pub client_id: Option<&'static str>,
}

pub const PROVIDERS: &[OAuthProvider] = &[
    OAuthProvider {
        kind: "google",
        display_name: "Google",
        auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
        token_url: "https://oauth2.googleapis.com/token",
        scopes: &[
            "https://www.googleapis.com/auth/calendar.readonly",
            "https://www.googleapis.com/auth/gmail.readonly",
            "https://www.googleapis.com/auth/userinfo.email",
            "openid",
        ],
        client_id: option_env!("MARGIN_GOOGLE_CLIENT_ID"),
    },
    OAuthProvider {
        kind: "microsoft_graph",
        display_name: "Microsoft",
        auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
        token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        scopes: &[
            "Calendars.Read",
            "Mail.Read",
            "Chat.Read",
            "User.Read",
            "offline_access",
        ],
        client_id: option_env!("MARGIN_MICROSOFT_CLIENT_ID"),
    },
];

pub fn lookup(kind: &str) -> Option<&'static OAuthProvider> {
    PROVIDERS.iter().find(|p| p.kind == kind)
}

/// All providers whose client ID is set at build time. Drives the
/// "Add connector" picker — providers without a configured client ID
/// don't appear in the UI at all (they'd just fail at flow start).
pub fn list_configured() -> Vec<&'static OAuthProvider> {
    PROVIDERS
        .iter()
        .filter(|p| p.client_id.is_some())
        .collect()
}
