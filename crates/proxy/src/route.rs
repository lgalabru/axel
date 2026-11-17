//! What a request may ask for: the checks every request passes before any
//! byte of it reaches a provider (ADR 0011, security points 2 to 4).

use http::{HeaderMap, Method, StatusCode, Uri, header};

/// The per-session secret: [`SECRET_BYTES`] random bytes, as lowercase hex.
pub(crate) struct Secret {
    hex: String,
}

/// 32 bytes: 256 bits, so guessing it is not an attack worth naming.
const SECRET_BYTES: usize = 32;

impl Secret {
    pub(crate) fn generate() -> anyhow::Result<Secret> {
        let mut bytes = [0u8; SECRET_BYTES];
        getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness for the proxy secret: {e}"))?;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex.len(), SECRET_BYTES * 2);
        Ok(Secret { hex })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.hex
    }

    /// Whether `candidate` is the secret, in time that does not depend on
    /// where the two first differ.  The length is no secret: it is fixed.
    pub(crate) fn matches(&self, candidate: &str) -> bool {
        let (a, b) = (self.hex.as_bytes(), candidate.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        let difference = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
        difference == 0
    }
}

/// Which upstream a route goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Anthropic,
    OpenAi,
}

/// Every endpoint the proxy forwards: anything else is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Endpoint {
    AnthropicMessages,
    AnthropicCountTokens,
    AnthropicModels,
    OpenAiResponses,
    OpenAiResponsesCompact,
    OpenAiModels,
}

impl Endpoint {
    pub(crate) const ALL: [Endpoint; 6] = [
        Endpoint::AnthropicMessages,
        Endpoint::AnthropicCountTokens,
        Endpoint::AnthropicModels,
        Endpoint::OpenAiResponses,
        Endpoint::OpenAiResponsesCompact,
        Endpoint::OpenAiModels,
    ];

    pub(crate) fn side(self) -> Side {
        match self {
            Endpoint::AnthropicMessages | Endpoint::AnthropicCountTokens | Endpoint::AnthropicModels => Side::Anthropic,
            Endpoint::OpenAiResponses | Endpoint::OpenAiResponsesCompact | Endpoint::OpenAiModels => Side::OpenAi,
        }
    }

    /// The path below the upstream's base, as the harness asks for it.
    pub(crate) fn path(self) -> &'static str {
        match self {
            Endpoint::AnthropicMessages => "/v1/messages",
            Endpoint::AnthropicCountTokens => "/v1/messages/count_tokens",
            Endpoint::AnthropicModels => "/v1/models",
            Endpoint::OpenAiResponses => "/responses",
            Endpoint::OpenAiResponsesCompact => "/responses/compact",
            Endpoint::OpenAiModels => "/models",
        }
    }

    pub(crate) fn method(self) -> Method {
        match self {
            Endpoint::AnthropicModels | Endpoint::OpenAiModels => Method::GET,
            _ => Method::POST,
        }
    }

    /// Whether the body is a model request whose context is worth a breakdown
    /// and whose response carries token usage.
    pub(crate) fn is_inference(self) -> bool {
        matches!(self, Endpoint::AnthropicMessages | Endpoint::OpenAiResponses | Endpoint::OpenAiResponsesCompact)
    }

    /// Whether the body is a context to break down (a token count asks about
    /// one without spending it).
    pub(crate) fn has_context(self) -> bool {
        self.is_inference() || self == Endpoint::AnthropicCountTokens
    }

    fn side_prefix(side: Side) -> &'static str {
        match side {
            Side::Anthropic => "anthropic",
            Side::OpenAi => "openai",
        }
    }
}

/// Why a request was refused; each one is a bare status, with no body that
/// could tell a prober anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// An absolute-form URI (`GET http://evil/ HTTP/1.1`): not an open proxy.
    AbsoluteUri,
    /// `CONNECT`: no tunnels.
    Connect,
    /// A browser sent it (`Origin`).
    Origin,
    /// A `Host` that isn't this listener: DNS rebinding.
    Host,
    /// Wrong secret, or a path outside the allowlist.
    NotFound,
    /// An allowed path, with a method it doesn't take.
    Method,
    /// A body over [`crate::server::REQUEST_BODY_BYTES_MAX`].
    TooLarge,
    /// The body could not be read (broken, or too slow).
    BadBody,
}

impl Refusal {
    pub(crate) fn status(self) -> StatusCode {
        match self {
            Refusal::AbsoluteUri | Refusal::BadBody => StatusCode::BAD_REQUEST,
            Refusal::Connect | Refusal::Method => StatusCode::METHOD_NOT_ALLOWED,
            Refusal::Origin | Refusal::Host => StatusCode::FORBIDDEN,
            Refusal::NotFound => StatusCode::NOT_FOUND,
            Refusal::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        }
    }
}

/// The endpoint a request may go to, or why it may not.  Checks run from
/// the cheapest to the one that needs the secret; none reads the body.
pub(crate) fn check(method: &Method, uri: &Uri, headers: &HeaderMap, secret: &Secret, port: u16) -> Result<Endpoint, Refusal> {
    if method == Method::CONNECT {
        return Err(Refusal::Connect);
    }
    if uri.scheme().is_some() || uri.authority().is_some() {
        return Err(Refusal::AbsoluteUri);
    }
    if headers.contains_key(header::ORIGIN) {
        return Err(Refusal::Origin);
    }
    if !is_own_host(headers, port) {
        return Err(Refusal::Host);
    }
    let endpoint = endpoint(uri.path(), secret)?;
    if *method != endpoint.method() {
        return Err(Refusal::Method);
    }
    Ok(endpoint)
}

/// Exactly one `Host`, naming this listener as a harness would reach it.
fn is_own_host(headers: &HeaderMap, port: u16) -> bool {
    let mut hosts = headers.get_all(header::HOST).iter();
    let (Some(host), None) = (hosts.next(), hosts.next()) else { return false };
    let Ok(host) = host.to_str() else { return false };
    let Some((name, host_port)) = host.rsplit_once(':') else { return false };
    let is_loopback_name = name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost");
    is_loopback_name && host_port == port.to_string()
}

/// `/<secret>/<side>/<path>` to the endpoint it names.
fn endpoint(path: &str, secret: &Secret) -> Result<Endpoint, Refusal> {
    let Some(rest) = path.strip_prefix('/') else { return Err(Refusal::NotFound) };
    let Some((candidate, rest)) = rest.split_once('/') else { return Err(Refusal::NotFound) };
    if !secret.matches(candidate) {
        return Err(Refusal::NotFound);
    }
    let Some((side, rest)) = rest.split_once('/') else { return Err(Refusal::NotFound) };
    let wanted = format!("/{rest}");
    // An exact match: no prefixes, so `..`, encodings and trailing parts never pass.
    Endpoint::ALL
        .into_iter()
        .find(|e| Endpoint::side_prefix(e.side()) == side && e.path() == wanted)
        .ok_or(Refusal::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(host: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, host.parse().unwrap());
        h
    }

    #[test]
    fn the_secret_is_random_hex_and_compared_whole() {
        let (a, b) = (Secret::generate().unwrap(), Secret::generate().unwrap());
        assert_eq!(a.as_str().len(), 64);
        assert!(a.as_str().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.as_str(), b.as_str());
        assert!(a.matches(a.as_str()));
        assert!(!a.matches(b.as_str()));
        assert!(!a.matches(&a.as_str()[..63]));
        assert!(!a.matches(""));
    }

    #[test]
    fn only_allowlisted_paths_under_the_secret_route() {
        let s = Secret::generate().unwrap();
        let p = |rest: &str| endpoint(&format!("/{}/{rest}", s.as_str()), &s);
        assert_eq!(p("anthropic/v1/messages"), Ok(Endpoint::AnthropicMessages));
        assert_eq!(p("anthropic/v1/messages/count_tokens"), Ok(Endpoint::AnthropicCountTokens));
        assert_eq!(p("openai/responses"), Ok(Endpoint::OpenAiResponses));
        assert_eq!(p("openai/models"), Ok(Endpoint::OpenAiModels));
        for bad in ["anthropic/v1/messages/", "anthropic/v1/messages/../x", "openai/v1/messages", "anthropic/responses", "anthropic/", "anthropic", "", "other/v1/messages", "anthropic/v1/complete"] {
            assert_eq!(p(bad), Err(Refusal::NotFound), "{bad}");
        }
        assert_eq!(endpoint("/v1/messages", &s), Err(Refusal::NotFound));
        assert_eq!(endpoint(&format!("/{}x/anthropic/v1/messages", s.as_str()), &s), Err(Refusal::NotFound));
    }

    #[test]
    fn only_this_listener_is_a_good_host() {
        assert!(is_own_host(&headers("127.0.0.1:4000"), 4000));
        assert!(is_own_host(&headers("LOCALHOST:4000"), 4000));
        for bad in ["127.0.0.1:4001", "127.0.0.1", "evil.example:4000", "127.0.0.1.evil:4000", "[::1]:4000", "localhost:04000"] {
            assert!(!is_own_host(&headers(bad), 4000), "{bad}");
        }
        assert!(!is_own_host(&HeaderMap::new(), 4000));
        let mut two = headers("127.0.0.1:4000");
        two.append(header::HOST, "127.0.0.1:4000".parse().unwrap());
        assert!(!is_own_host(&two, 4000));
    }

    #[test]
    fn methods_must_match_the_endpoint() {
        let s = Secret::generate().unwrap();
        let uri: Uri = format!("/{}/anthropic/v1/messages", s.as_str()).parse().unwrap();
        let h = headers("127.0.0.1:9");
        assert_eq!(check(&Method::POST, &uri, &h, &s, 9), Ok(Endpoint::AnthropicMessages));
        assert_eq!(check(&Method::GET, &uri, &h, &s, 9), Err(Refusal::Method));
        assert_eq!(check(&Method::CONNECT, &uri, &h, &s, 9), Err(Refusal::Connect));
    }
}
