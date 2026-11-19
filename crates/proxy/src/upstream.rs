//! Where requests go: upstreams fixed when the session starts, reached over
//! verified TLS only (ADR 0011, security points 3 and 5).

use std::sync::Arc;
use std::time::Duration;

use axel_store::ModelProvider;
use bytes::Bytes;
use http::uri::{Authority, PathAndQuery, Scheme};
use http::{HeaderValue, Uri};
use http_body_util::Full;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioTimer};

use crate::route::{Endpoint, Side};
use crate::throttle::Paced;

/// Which OpenAI backend Codex's login talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenAiUpstream {
    /// `https://api.openai.com/v1`, with an API key.
    ApiKey,
    /// `https://chatgpt.com/backend-api/codex`, with a ChatGPT login.
    ChatGpt,
}

impl OpenAiUpstream {
    fn url(self) -> &'static str {
        match self {
            OpenAiUpstream::ApiKey => "https://api.openai.com/v1",
            OpenAiUpstream::ChatGpt => "https://chatgpt.com/backend-api/codex",
        }
    }

    fn provider(self) -> ModelProvider {
        match self {
            OpenAiUpstream::ApiKey => ModelProvider::OpenAi,
            OpenAiUpstream::ChatGpt => ModelProvider::ChatGpt,
        }
    }
}

const ANTHROPIC_URL: &str = "https://api.anthropic.com";
/// Longest upstream URL accepted: far past any real base URL.
const UPSTREAM_URL_BYTES_MAX: usize = 2048;
/// A provider that doesn't accept a connection in this long is down.
const CONNECT_TIMEOUT_MS: u64 = 10_000;
/// Pooled connections to the provider close after this long unused; a
/// harness between turns reconnects, which costs one TLS handshake.
const POOL_IDLE_TIMEOUT_MS: u64 = 90_000;
/// Idle pooled connections kept per provider host.
const POOL_IDLE_PER_HOST_MAX: usize = 8;

/// One provider base URL: scheme, host and a path prefix, nothing else.
#[derive(Clone, Debug)]
pub(crate) struct Upstream {
    scheme: Scheme,
    authority: Authority,
    /// No trailing `/`; empty for the root.
    base_path: String,
}

impl Upstream {
    /// An `https` base URL, refused otherwise: the proxy never sends
    /// credentials in the clear.
    pub(crate) fn https(url: &str) -> anyhow::Result<Upstream> {
        let upstream = Upstream::parse(url)?;
        anyhow::ensure!(upstream.scheme == Scheme::HTTPS, "the upstream must be https, not {}", upstream.scheme);
        Ok(upstream)
    }

    /// Any scheme: only tests, against a fake provider on loopback.
    #[cfg(test)]
    pub(crate) fn plain_for_test(url: &str) -> Upstream {
        Upstream::parse(url).expect("a test upstream parses")
    }

    fn parse(url: &str) -> anyhow::Result<Upstream> {
        anyhow::ensure!(url.len() <= UPSTREAM_URL_BYTES_MAX, "the upstream URL is too long");
        let uri: Uri = url.trim().parse().map_err(|_| anyhow::anyhow!("the upstream is not a URL"))?;
        let (Some(scheme), Some(authority)) = (uri.scheme().cloned(), uri.authority().cloned()) else {
            anyhow::bail!("the upstream URL needs a scheme and a host");
        };
        anyhow::ensure!(!authority.as_str().contains('@'), "the upstream URL must not carry credentials");
        anyhow::ensure!(uri.query().is_none(), "the upstream URL must not have a query");
        let base_path = uri.path().trim_end_matches('/').to_string();
        Ok(Upstream { scheme, authority, base_path })
    }

    /// The URL for `endpoint` with the harness's own query.
    pub(crate) fn uri(&self, endpoint: Endpoint, query: Option<&str>) -> Uri {
        let path = match query {
            Some(q) => format!("{}{}?{q}", self.base_path, endpoint.path()),
            None => format!("{}{}", self.base_path, endpoint.path()),
        };
        let path_and_query: PathAndQuery = path.parse().expect("a base path and an allowlisted path make a path");
        Uri::builder()
            .scheme(self.scheme.clone())
            .authority(self.authority.clone())
            .path_and_query(path_and_query)
            .build()
            .expect("scheme, authority and path make a URI")
    }

    /// `Host` for requests to this upstream.
    pub(crate) fn host(&self) -> HeaderValue {
        HeaderValue::from_str(self.authority.as_str()).expect("an authority is a valid header value")
    }
}

/// Both upstreams of a session, chosen once.
#[derive(Clone, Debug)]
pub(crate) struct Upstreams {
    pub(crate) anthropic: Upstream,
    pub(crate) openai: Upstream,
    pub(crate) openai_provider: ModelProvider,
}

impl Upstreams {
    pub(crate) fn new(anthropic: Option<&str>, openai: OpenAiUpstream) -> anyhow::Result<Upstreams> {
        Ok(Upstreams {
            anthropic: Upstream::https(anthropic.unwrap_or(ANTHROPIC_URL))?,
            openai: Upstream::https(openai.url())?,
            openai_provider: openai.provider(),
        })
    }

    pub(crate) fn for_side(&self, side: Side) -> (&Upstream, ModelProvider) {
        match side {
            Side::Anthropic => (&self.anthropic, ModelProvider::Anthropic),
            Side::OpenAi => (&self.openai, self.openai_provider),
        }
    }
}

/// Request bodies go up paced, when the session is throttled.
pub(crate) type Client = HyperClient<HttpsConnector<HttpConnector>, Paced<Full<Bytes>>>;

/// Which schemes the client may use.  Only tests can name the plain one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transport {
    HttpsOnly,
    #[cfg(test)]
    PlainAllowed,
}

/// The client to the providers: rustls with ring and the webpki roots,
/// verifying the provider's name; HTTP/1.1.
pub(crate) fn client(transport: Transport) -> anyhow::Result<Client> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut http = HttpConnector::new();
    // The TLS layer above decides the scheme; this one only dials.
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(Duration::from_millis(CONNECT_TIMEOUT_MS)));
    let schemes = hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(tls);
    let connector = match transport {
        Transport::HttpsOnly => schemes.https_only().enable_http1().wrap_connector(http),
        #[cfg(test)]
        Transport::PlainAllowed => schemes.https_or_http().enable_http1().wrap_connector(http),
    };
    Ok(HyperClient::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_millis(POOL_IDLE_TIMEOUT_MS))
        .pool_max_idle_per_host(POOL_IDLE_PER_HOST_MAX)
        .pool_timer(TokioTimer::new())
        .build(connector))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstreams_are_https_only() {
        for bad in ["http://api.anthropic.com", "ftp://x", "api.anthropic.com", "https://user:pw@api.anthropic.com", "https://x/?q=1", ""] {
            assert!(Upstream::https(bad).is_err(), "{bad}");
        }
        assert!(Upstreams::new(Some("http://127.0.0.1:8080"), OpenAiUpstream::ApiKey).is_err());
        let u = Upstream::https("https://gateway.example/anthropic/").unwrap();
        assert_eq!(u.uri(Endpoint::AnthropicMessages, Some("beta=true")).to_string(), "https://gateway.example/anthropic/v1/messages?beta=true");
        let chatgpt = Upstreams::new(None, OpenAiUpstream::ChatGpt).unwrap();
        assert_eq!(chatgpt.openai.uri(Endpoint::OpenAiResponses, None).to_string(), "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(chatgpt.anthropic.host(), "api.anthropic.com");
    }
}
