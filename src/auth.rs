//! Optional bearer-token authentication for the OSS `evald serve` surface.
//!
//! evald's default posture is a **local, single-tenant tool**: `serve` binds loopback and
//! ships **no** authentication (the intended threat model is untrusted *input* on a
//! *trusted* network — a laptop or a locked-down CI runner). That default is unchanged.
//!
//! This module adds the *opt-in* gate the docs used to punt entirely to a reverse proxy:
//! the case where evald is exposed on a shared or public network. When one or more tokens
//! are configured (`--auth-token` / `EVALD_AUTH_TOKEN` / `--auth-token-file`), **every**
//! request to the HTTP surface (OTLP ingest, the `/v1/*` API, and the embedded SPA) and to
//! the OTLP/gRPC receiver must present a matching `Authorization: Bearer <token>`; anything
//! else is `401` (HTTP) / `UNAUTHENTICATED` (gRPC). With no token configured the gate is
//! [`Auth::disabled`] and every check passes — byte-for-byte the pre-auth behavior.
//!
//! Scope, stated honestly — this is a **shared-secret bearer gate**, not a full security
//! stack:
//!
//! - It is **not TLS.** A bearer token sent over plaintext HTTP is only as private as the
//!   transport. If the network between client and evald is untrusted, terminate TLS at a
//!   reverse proxy (nginx / Caddy / an mTLS mesh, or the sibling `edgeguard` front door)
//!   in front of evald — evald itself does no TLS.
//! - It is **not per-user identity or roles.** Every valid token grants the same full
//!   access (read, write, SQL). Multiple tokens exist only to make rotation and per-client
//!   revocation possible, not to model roles. Per-tenant identity / OIDC live in the
//!   separately-licensed `ee` fleet layer, not here.
//! - It is **not rate limiting.** The bounded ingest channel still sheds with `429`, but a
//!   valid token is not throttled.
//!
//! Tokens are matched by SHA-256 digest — a fixed-length comparison that does not early-exit
//! on the secret's own bytes — and are **never logged** (only the token *count* is, so an
//! operator can confirm the gate is armed).

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};

/// Minimum accepted token length. A short shared secret fails at boot with a clear message
/// rather than silently standing up a guessable gate at 3 a.m. Mirrors the ee fleet
/// registry's `MIN_TOKEN_LEN`.
pub const MIN_TOKEN_LEN: usize = 16;

/// A configuration error building the [`Auth`] gate — surfaced at `serve` startup so a
/// misconfigured deployment fails loudly instead of coming up wide open, or all-401.
#[derive(Debug)]
pub enum AuthError {
    /// A configured token is shorter than [`MIN_TOKEN_LEN`] chars.
    TokenTooShort { len: usize },
    /// A configured token has a byte an `Authorization: Bearer <token>` header (or ASCII gRPC
    /// metadata) cannot carry — anything outside printable ASCII (`0x21..=0x7E`), e.g. a
    /// non-ASCII char, a space, or a control byte. Such a token would pass a length check but
    /// could never be *sent*, so every request with it would be a silent 401. Caught at boot.
    NonAsciiToken,
    /// Auth was requested (a flag/env/file was set) but no usable token resulted — every
    /// request would then be a 401, which is never what the operator meant.
    NoTokens,
    /// The `--auth-token-file` could not be read.
    File {
        path: String,
        source: std::io::Error,
    },
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::TokenTooShort { len } => write!(
                f,
                "auth token is too short ({len} chars) — use at least {MIN_TOKEN_LEN}"
            ),
            AuthError::NonAsciiToken => write!(
                f,
                "auth token has a character that cannot be sent in an Authorization header \
                 — use printable ASCII only (no spaces, control, or non-ASCII characters)"
            ),
            AuthError::NoTokens => write!(
                f,
                "authentication was requested but no usable token was provided \
                 (every request would be rejected with 401)"
            ),
            AuthError::File { path, source } => {
                write!(f, "could not read auth token file {path}: {source}")
            }
        }
    }
}

impl std::error::Error for AuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AuthError::File { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The bearer-token gate. Cheap to clone (an `Arc` around the digest set), so it can be
/// handed to the axum middleware state and moved into the gRPC interceptor closure.
/// [`Auth::disabled`] / `Default` is the no-auth posture — every [`Auth::check`] passes.
#[derive(Clone, Default)]
pub struct Auth {
    /// `None` = disabled (no gate). `Some(set)` = every request needs a bearer token whose
    /// SHA-256 digest is in the set.
    tokens: Option<Arc<HashSet<[u8; 32]>>>,
}

impl Auth {
    /// The no-auth gate — every check passes. This is the zero-config default.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Is the gate armed (at least one token configured)?
    pub fn is_enabled(&self) -> bool {
        self.tokens.is_some()
    }

    /// Number of distinct configured tokens (0 when disabled). Logged at startup — the
    /// *count* only, never the tokens themselves.
    pub fn token_count(&self) -> usize {
        self.tokens.as_ref().map_or(0, |s| s.len())
    }

    /// Build a gate from already-collected raw tokens: validate each token's length, dedup
    /// by digest, and error if the result is empty. Callers that assemble tokens from the
    /// CLI/env/file should use [`Auth::from_sources`].
    pub fn from_tokens<I, S>(tokens: I) -> Result<Self, AuthError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut set = HashSet::new();
        for tok in tokens {
            let tok = tok.as_ref().trim();
            if tok.is_empty() {
                continue;
            }
            // Only printable-ASCII tokens can round-trip through an `Authorization: Bearer`
            // header / ASCII gRPC metadata value, so reject anything else at boot — otherwise a
            // non-ASCII token would pass the length check but 401 on every request (the client
            // can't send it). This also makes the length check unambiguous: for ASCII, chars ==
            // bytes, so `len()` is the character count.
            if !tok.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(AuthError::NonAsciiToken);
            }
            if tok.len() < MIN_TOKEN_LEN {
                return Err(AuthError::TokenTooShort { len: tok.len() });
            }
            set.insert(sha256(tok));
        }
        if set.is_empty() {
            return Err(AuthError::NoTokens);
        }
        Ok(Self {
            tokens: Some(Arc::new(set)),
        })
    }

    /// Assemble the gate from the OSS `serve` sources — the **union** of the inline tokens
    /// (the `--auth-token` flags plus any comma-separated `EVALD_AUTH_TOKEN`, already flattened
    /// into `inline` by the caller) and every non-comment line of the `--auth-token-file`.
    ///
    /// "No source set" is judged on the *real* tokens, so a present-but-empty placeholder
    /// (`EVALD_AUTH_TOKEN=` → `[""]`, or `EVALD_AUTH_TOKEN_FILE=` → an empty path) leaves auth
    /// **disabled** rather than failing to boot — matching the documented "unset ⇒ off". Once a
    /// real source is present, an empty result (a token file of only comments) is an
    /// [`AuthError::NoTokens`] — the operator asked for auth but gave nothing usable.
    pub fn from_sources(inline: &[String], file: Option<&Path>) -> Result<Self, AuthError> {
        // Keep only real tokens up front, so blank/whitespace placeholders don't count as
        // "auth requested" (and don't reach the length/ASCII checks as spurious errors).
        let mut tokens: Vec<String> = inline
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        // An empty/whitespace file path (an `EVALD_AUTH_TOKEN_FILE=` placeholder) is "no file".
        let file = file.filter(|p| !p.as_os_str().is_empty());
        let any_source = !tokens.is_empty() || file.is_some();
        if !any_source {
            return Ok(Self::disabled());
        }
        if let Some(path) = file {
            let text = std::fs::read_to_string(path).map_err(|e| AuthError::File {
                path: path.display().to_string(),
                source: e,
            })?;
            tokens.extend(parse_token_file(&text));
        }
        Self::from_tokens(tokens)
    }

    /// Verify one request's `Authorization` header value. A [`disabled`](Auth::disabled)
    /// gate always passes. Otherwise the value must be `Bearer <token>` (scheme
    /// case-insensitive) whose token digest is registered; a missing header, a malformed
    /// value, a non-`Bearer` scheme, and a wrong token all fail **closed**.
    pub fn check(&self, authorization: Option<&str>) -> bool {
        let set = match &self.tokens {
            None => return true,
            Some(s) => s,
        };
        match bearer_token(authorization) {
            Some(tok) => set.contains(&sha256(tok)),
            None => false,
        }
    }
}

/// Extract the `<token>` from a `Bearer <token>` header value (scheme case-insensitive,
/// surrounding whitespace trimmed). `None` for a missing header, an empty token, or any
/// other scheme (`Basic`, …).
fn bearer_token(authorization: Option<&str>) -> Option<&str> {
    let value = authorization?.trim();
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Parse a token file: one token per line, blank lines and `#` comments ignored. Keeps the
/// on-disk format trivial (no JSON, no principal metadata) — the OSS gate is a flat set of
/// shared secrets, so rotation is "add a line, later remove a line".
fn parse_token_file(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}

fn sha256(s: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOK_A: &str = "alice-token-0123456789";
    const TOK_B: &str = "bob-token-abcdefghijkl";

    #[test]
    fn disabled_gate_passes_everything() {
        let auth = Auth::disabled();
        assert!(!auth.is_enabled());
        assert_eq!(auth.token_count(), 0);
        assert!(auth.check(None));
        assert!(auth.check(Some("Bearer whatever")));
        assert!(auth.check(Some("garbage")));
    }

    #[test]
    fn enabled_gate_accepts_only_valid_bearer_tokens() {
        let auth = Auth::from_tokens([TOK_A, TOK_B]).unwrap();
        assert!(auth.is_enabled());
        assert_eq!(auth.token_count(), 2);

        // Correct token, either casing of the scheme.
        assert!(auth.check(Some(&format!("Bearer {TOK_A}"))));
        assert!(auth.check(Some(&format!("bearer {TOK_B}"))));
        assert!(auth.check(Some(&format!("BEARER {TOK_A}"))));
        // Extra whitespace around the token is tolerated.
        assert!(auth.check(Some(&format!("Bearer   {TOK_A}  "))));

        // Everything else fails closed.
        assert!(!auth.check(None));
        assert!(!auth.check(Some("")));
        assert!(!auth.check(Some(TOK_A))); // no scheme
        assert!(!auth.check(Some("Bearer"))); // scheme, no token
        assert!(!auth.check(Some("Bearer "))); // scheme, empty token
        assert!(!auth.check(Some("Bearer wrong-token-0123456789")));
        assert!(!auth.check(Some(&format!("Basic {TOK_A}")))); // wrong scheme
    }

    #[test]
    fn short_tokens_are_rejected_at_build() {
        assert!(matches!(
            Auth::from_tokens(["short"]),
            Err(AuthError::TokenTooShort { .. })
        ));
        // A 15-char token is one short of the floor.
        assert!(matches!(
            Auth::from_tokens(["123456789012345"]),
            Err(AuthError::TokenTooShort { len: 15 })
        ));
        // Exactly 16 is accepted.
        assert!(Auth::from_tokens(["1234567890123456"]).is_ok());
    }

    #[test]
    fn duplicate_tokens_collapse() {
        let auth = Auth::from_tokens([TOK_A, TOK_A, TOK_A]).unwrap();
        assert_eq!(auth.token_count(), 1);
    }

    #[test]
    fn empty_or_blank_only_tokens_are_an_error() {
        assert!(matches!(
            Auth::from_tokens(Vec::<&str>::new()),
            Err(AuthError::NoTokens)
        ));
        assert!(matches!(
            Auth::from_tokens(["", "   ", "\t"]),
            Err(AuthError::NoTokens)
        ));
    }

    #[test]
    fn from_sources_with_nothing_is_disabled() {
        let auth = Auth::from_sources(&[], None).unwrap();
        assert!(!auth.is_enabled());
    }

    #[test]
    fn from_sources_unions_inline_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.txt");
        std::fs::write(
            &path,
            format!("# a comment\n\n{TOK_B}\n   # indented comment\n{TOK_B}\n"),
        )
        .unwrap();

        let auth = Auth::from_sources(&[TOK_A.to_string()], Some(&path)).unwrap();
        // TOK_A (inline) + TOK_B (file, deduped from two lines) = 2 distinct tokens.
        assert_eq!(auth.token_count(), 2);
        assert!(auth.check(Some(&format!("Bearer {TOK_A}"))));
        assert!(auth.check(Some(&format!("Bearer {TOK_B}"))));
    }

    #[test]
    fn from_sources_with_a_source_but_no_usable_token_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.txt");
        std::fs::write(&path, "# only comments\n\n").unwrap();
        assert!(matches!(
            Auth::from_sources(&[], Some(&path)),
            Err(AuthError::NoTokens)
        ));
    }

    #[test]
    fn from_sources_missing_file_is_an_error() {
        let missing = std::path::Path::new("/nonexistent/evald-tokens.txt");
        assert!(matches!(
            Auth::from_sources(&[], Some(missing)),
            Err(AuthError::File { .. })
        ));
    }

    #[test]
    fn non_ascii_or_spaced_tokens_are_rejected_at_build() {
        // A 16+ char token with a non-ASCII char passes the length floor but can't be sent in
        // an Authorization header — reject it at boot instead of a silent all-401.
        assert!(matches!(
            Auth::from_tokens(["café-supersecret-2024"]),
            Err(AuthError::NonAsciiToken)
        ));
        // A space inside the token is also rejected (it would break `Bearer <token>` parsing).
        assert!(matches!(
            Auth::from_tokens(["has a space in it 123"]),
            Err(AuthError::NonAsciiToken)
        ));
        // A comma IS printable ASCII, so a comma-bearing passphrase is one valid whole token
        // (the flag no longer splits on commas — see main.rs).
        let auth = Auth::from_tokens(["swift,otter,brass,cedar"]).unwrap();
        assert!(auth.check(Some("Bearer swift,otter,brass,cedar")));
    }

    #[test]
    fn from_sources_present_but_empty_placeholders_leave_auth_disabled() {
        // `EVALD_AUTH_TOKEN=` expands to [""]; a blank placeholder must leave auth OFF, not
        // fail to boot (a very common orchestration env pattern).
        assert!(!Auth::from_sources(&[String::new()], None)
            .unwrap()
            .is_enabled());
        assert!(!Auth::from_sources(&["   ".to_string()], None)
            .unwrap()
            .is_enabled());
        // `EVALD_AUTH_TOKEN_FILE=` expands to an empty path — treat it as "no file", not a
        // read error.
        assert!(!Auth::from_sources(&[], Some(std::path::Path::new("")))
            .unwrap()
            .is_enabled());
    }
}
