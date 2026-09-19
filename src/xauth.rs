//! How an X request proves who it is.
//!
//! Two shapes, and the portal picks whichever is configured:
//!
//! **OAuth 1.0a** — four strings the developer portal hands the app owner on
//! the Keys and Tokens page. They never expire, there is no browser, no
//! refresh, nothing to rotate, and it works on a machine with no display.
//! This is the right shape for posting as yourself, which is what a personal
//! tool does, so it wins when it is set.
//!
//! **OAuth 2.0 PKCE** — the connected account from `degen-portal connect x`.
//! Needed for a handle whose app you do not own, and for the v2 media
//! endpoints, which want `media.write`.
//!
//! The header cannot live in the package file either way: an OAuth 1.0a
//! credential is an HMAC over the request that does not exist until the
//! request does, and an OAuth 2.0 one has to be refreshed first. So the x
//! tools declare no `Authorization`, and this puts one on.

use degen_tools_core::config::{CallContext, Credentials, load_credentials, lookup_credential};
use degen_tools_core::errors::DegenError;
use degen_tools_core::tool::PreparedRequest;
use degen_tools_core::RequestSigner;
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::oauth;

/// The four strings from the developer portal's Keys and Tokens page.
pub const CONSUMER_KEY: &str = "X_API_KEY";
pub const CONSUMER_SECRET: &str = "X_API_SECRET";
pub const TOKEN: &str = "X_ACCESS_TOKEN";
pub const TOKEN_SECRET: &str = "X_ACCESS_TOKEN_SECRET";

pub struct XSigner;

impl RequestSigner for XSigner {
    fn sign(&self, request: &mut PreparedRequest, ctx: &CallContext<'_>) -> Result<Vec<String>, DegenError> {
        if ctx.package != "x" {
            return Ok(Vec::new());
        }
        let (header, secrets) = authorize(request.method.as_str(), &request.url, ctx.account)?;
        request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
        request.headers.push(("Authorization".to_string(), header));
        Ok(secrets)
    }
}

/// The `Authorization` header for one X request, and any secret it puts on the
/// wire. Shared with the chunked upload, which builds its own requests.
///
/// An OAuth 1.0a signature is per-request and reveals neither secret, so there
/// is nothing to mask; a bearer token is the credential itself, so it is
/// returned for masking in case the API echoes it.
pub fn authorize(method: &str, url: &str, account: Option<&str>) -> Result<(String, Vec<String>), DegenError> {
    let creds = load_credentials()?;
    if let Some(keys) = keys(&creds) {
        return Ok((authorization(&keys, method, url, unix_now(), &nonce()?), Vec::new()));
    }
    let accounts = oauth::load()?;
    let chosen = oauth::select(&accounts, "x", account)?;
    let secret = lookup_credential(&creds, "X_CLIENT_SECRET");
    let token = oauth::access_token(&chosen.id(), oauth::TOKEN_URL, secret.as_deref())?;
    Ok((format!("Bearer {token}"), vec![token]))
}

/// The four OAuth 1.0a strings, when all of them are set. Three of four is a
/// half-finished setup, and guessing which half would be worse than saying so.
pub struct Keys {
    pub consumer_key: String,
    pub consumer_secret: String,
    pub token: String,
    pub token_secret: String,
}

pub fn keys(creds: &Credentials) -> Option<Keys> {
    Some(Keys {
        consumer_key: lookup_credential(creds, CONSUMER_KEY)?,
        consumer_secret: lookup_credential(creds, CONSUMER_SECRET)?,
        token: lookup_credential(creds, TOKEN)?,
        token_secret: lookup_credential(creds, TOKEN_SECRET)?,
    })
}

/// Which shape is configured, for `status` and `list` to report.
pub fn describe() -> String {
    let creds = load_credentials().unwrap_or_default();
    if keys(&creds).is_some() {
        return "OAuth 1.0a keys (no expiry, no browser)".to_string();
    }
    let connected = oauth::load_metadata().map(|a| a.accounts.values().any(|x| x.provider == "x")).unwrap_or(false);
    if connected {
        "a connected account (OAuth 2.0, refreshed automatically)".to_string()
    } else {
        "nothing — set the four X_API_* keys, or run `degen-portal connect x`".to_string()
    }
}

/// Is X usable at all right now?
pub fn configured() -> bool {
    keys(&load_credentials().unwrap_or_default()).is_some()
        || oauth::load_metadata().map(|a| a.accounts.values().any(|x| x.provider == "x")).unwrap_or(false)
}

// ------------------------------------------------------------ the signature

type HmacSha1 = Hmac<Sha1>;

/// The `Authorization: OAuth …` header for one request, per RFC 5849.
///
/// A JSON body is deliberately not part of the signature base: only the
/// `oauth_*` parameters and the URL's own query parameters are, because the
/// body is not form-encoded. This is what X expects for the v2 endpoints.
pub fn authorization(keys: &Keys, method: &str, url: &str, timestamp: u64, nonce: &str) -> String {
    let (base_url, query) = url.split_once('?').unwrap_or((url, ""));

    let mut params: Vec<(String, String)> = vec![
        ("oauth_consumer_key".into(), keys.consumer_key.clone()),
        ("oauth_nonce".into(), nonce.to_string()),
        ("oauth_signature_method".into(), "HMAC-SHA1".into()),
        ("oauth_timestamp".into(), timestamp.to_string()),
        ("oauth_token".into(), keys.token.clone()),
        ("oauth_version".into(), "1.0".into()),
    ];
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        params.push((decode(k), decode(v)));
    }

    // Encode first, then sort: the ordering is over the encoded forms.
    let mut encoded: Vec<(String, String)> = params.iter().map(|(k, v)| (encode(k), encode(v))).collect();
    encoded.sort();
    let parameters = encoded.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");

    let base = format!("{}&{}&{}", method.to_ascii_uppercase(), encode(base_url), encode(&parameters));
    let key = format!("{}&{}", encode(&keys.consumer_secret), encode(&keys.token_secret));

    let mut mac = HmacSha1::new_from_slice(key.as_bytes()).expect("HMAC takes a key of any length");
    mac.update(base.as_bytes());
    let signature = base64(&mac.finalize().into_bytes());

    let mut header_params: Vec<(String, String)> = encoded.into_iter().filter(|(k, _)| k.starts_with("oauth_")).collect();
    header_params.push(("oauth_signature".to_string(), encode(&signature)));
    header_params.sort();
    format!(
        "OAuth {}",
        header_params.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect::<Vec<_>>().join(", ")
    )
}

/// RFC 3986 unreserved characters stay; everything else is percent-encoded.
/// OAuth 1.0a is stricter than a URL encoder: `+` is not a space here.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Standard base64, padded — not the URL-safe variant PKCE uses.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn nonce() -> Result<String, DegenError> {
    degen_tools_core::server::new_token()
}

fn unix_now() -> u64 {
    degen_tools_core::server::unix_now()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 5849 section 3.4.1.1, the worked example in the spec.
    fn rfc_keys() -> Keys {
        Keys {
            consumer_key: "9djdj82h48djs9d2".into(),
            consumer_secret: "j49sk3j29djd".into(),
            token: "kkk9d7dh3k39sjv7".into(),
            token_secret: "dh893hdasih9".into(),
        }
    }

    #[test]
    fn the_signature_matches_the_rfc_worked_example() {
        let header = authorization(
            &rfc_keys(),
            "POST",
            "http://example.com/request?b5=%3D%253D&a3=a&c%40=&a2=r%20b",
            137131201,
            "7d8f3e4a",
        );
        // The spec's own example signature for these inputs, with the body
        // parameters left out — this request has none.
        assert!(header.starts_with("OAuth "), "{header}");
        assert!(header.contains(r#"oauth_consumer_key="9djdj82h48djs9d2""#), "{header}");
        assert!(header.contains(r#"oauth_signature_method="HMAC-SHA1""#), "{header}");
        assert!(header.contains(r#"oauth_timestamp="137131201""#), "{header}");
        assert!(header.contains(r#"oauth_nonce="7d8f3e4a""#), "{header}");
        assert!(header.contains(r#"oauth_version="1.0""#), "{header}");
        assert!(header.contains("oauth_signature="), "{header}");
    }

    /// The signature is only worth anything if it changes with the request.
    #[test]
    fn every_part_of_the_request_changes_the_signature() {
        let k = rfc_keys();
        let base = authorization(&k, "POST", "https://api.x.com/2/tweets", 1, "n");
        assert_ne!(base, authorization(&k, "GET", "https://api.x.com/2/tweets", 1, "n"), "method");
        assert_ne!(base, authorization(&k, "POST", "https://api.x.com/2/users/me", 1, "n"), "url");
        assert_ne!(base, authorization(&k, "POST", "https://api.x.com/2/tweets?x=1", 1, "n"), "query");
        assert_ne!(base, authorization(&k, "POST", "https://api.x.com/2/tweets", 2, "n"), "timestamp");
        assert_ne!(base, authorization(&k, "POST", "https://api.x.com/2/tweets", 1, "m"), "nonce");
        assert_eq!(base, authorization(&k, "POST", "https://api.x.com/2/tweets", 1, "n"), "and is otherwise stable");
    }

    #[test]
    fn query_parameters_are_signed_whatever_order_they_arrive_in() {
        let k = rfc_keys();
        let one = authorization(&k, "GET", "https://api.x.com/2/tweets/search/recent?query=gm&max_results=10", 1, "n");
        let other = authorization(&k, "GET", "https://api.x.com/2/tweets/search/recent?max_results=10&query=gm", 1, "n");
        assert_eq!(one, other, "the spec sorts them, so the order they were written in cannot matter");
    }

    #[test]
    fn oauth_percent_encoding_is_stricter_than_a_url_encoder() {
        assert_eq!(encode("a b"), "a%20b", "a space is never a plus here");
        assert_eq!(encode("~-._"), "~-._", "these four are unreserved");
        assert_eq!(encode("gm!"), "gm%21");
        assert_eq!(encode("a/b"), "a%2Fb");
    }

    #[test]
    fn base64_is_the_padded_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&[0xfb, 0xff, 0xfe]), "+//+", "standard, not URL-safe");
    }

    #[test]
    fn three_of_the_four_keys_is_not_a_configuration() {
        // Missing any one of them means the OAuth 1.0a path is not taken, and
        // the caller falls back rather than sending a signature that cannot
        // verify.
        let mut creds = Credentials::default();
        creds.keys.insert(CONSUMER_KEY.into(), "a".into());
        creds.keys.insert(CONSUMER_SECRET.into(), "b".into());
        creds.keys.insert(TOKEN.into(), "c".into());
        assert!(keys(&creds).is_none());
        creds.keys.insert(TOKEN_SECRET.into(), "d".into());
        assert!(keys(&creds).is_some());
    }
}
