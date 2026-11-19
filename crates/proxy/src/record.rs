//! One request's row, as it goes: recorded when the request starts and
//! again when it ends, however it ends.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axel_store::{ModelProvider, ModelRequest, RequestOutcome};
use bytes::Bytes;
use http::StatusCode;
use http_body::{Body, Frame, SizeHint};
use hyper::body::Incoming;

use crate::RequestSink;
use crate::analyze::Analysis;
use crate::observe::{Observer, Shape};
use crate::route::Endpoint;
use crate::throttle::Paced;

/// Why a request failed, as the short fixed text the row keeps: never a
/// body, a header, or an error's own text (which can quote either).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// No connection to the provider (DNS, TCP, TLS).
    Connect,
    /// The provider's connection broke before it answered.
    Request,
    /// The response stream broke partway.
    Stream,
    /// The network throttle lost it on purpose (`AXEL_THROTTLE`): the
    /// connection broke before the provider saw it.
    Lost,
}

impl Failure {
    fn as_str(self) -> &'static str {
        match self {
            Failure::Connect => "could not connect to the provider",
            Failure::Request => "the provider connection failed before it answered",
            Failure::Stream => "the response stream broke",
            Failure::Lost => "lost by the network throttle",
        }
    }
}

/// Who a request belongs to: the session's identity and its turn.
pub(crate) struct Owner {
    pub(crate) session_id: String,
    pub(crate) thread_id: String,
    pub(crate) agent_pubkey: String,
    pub(crate) turn: Option<u32>,
}

/// A request being recorded.  Dropped before it finished means the harness
/// hung up: it is recorded as `Cancelled`.
pub(crate) struct Recording {
    sink: Arc<dyn RequestSink>,
    request: ModelRequest,
    started: Instant,
    is_finished: bool,
}

impl Recording {
    /// The row for a request that is about to go upstream, recorded `InFlight`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        sink: Arc<dyn RequestSink>,
        owner: Owner,
        provider: ModelProvider,
        endpoint: Endpoint,
        analysis: Analysis,
        request_bytes: u64,
        started: Instant,
        is_throttled: bool,
    ) -> Recording {
        let request = ModelRequest {
            id: ulid::Ulid::new().to_string(),
            session_id: owner.session_id,
            thread_id: owner.thread_id,
            agent_pubkey: owner.agent_pubkey,
            turn: owner.turn,
            provider,
            endpoint: endpoint.path().to_string(),
            model: analysis.model,
            started_at_ms: now_ms(),
            first_byte_ms: None,
            duration_ms: None,
            http_status: None,
            streamed: false,
            outcome: RequestOutcome::InFlight,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            request_bytes,
            response_bytes: 0,
            stop_reason: None,
            error: None,
            breakdown: analysis.breakdown,
            throttled: is_throttled,
        };
        sink.record(&request);
        Recording { sink, request, started, is_finished: false }
    }

    /// The provider answered with `status`; the body follows.
    fn answered(&mut self, status: StatusCode) {
        self.request.http_status = Some(status.as_u16());
    }

    /// The request never got an answer.
    pub(crate) fn fail(mut self, failure: Failure) {
        self.finish(RequestOutcome::Failed, Some(failure.as_str().to_string()));
    }

    fn first_byte(&mut self) {
        if self.request.first_byte_ms.is_none() {
            self.request.first_byte_ms = Some(self.elapsed_ms());
        }
    }

    fn observed(&mut self, observer: &Observer, response_bytes: u64) {
        let usage = &observer.usage;
        let r = &mut self.request;
        r.model = usage.model.clone().or(r.model.take());
        r.input_tokens = usage.input_tokens;
        r.output_tokens = usage.output_tokens;
        r.cache_read_tokens = usage.cache_read_tokens;
        r.cache_write_tokens = usage.cache_write_tokens;
        r.reasoning_tokens = usage.reasoning_tokens;
        r.stop_reason = usage.stop_reason.clone();
        r.response_bytes = response_bytes;
        r.streamed = observer.shape() == Shape::Stream;
    }

    /// The final row; once.
    fn finish(&mut self, outcome: RequestOutcome, error: Option<String>) {
        assert!(!self.is_finished, "a request is finished once");
        assert_ne!(outcome, RequestOutcome::InFlight);
        self.is_finished = true;
        self.request.outcome = outcome;
        self.request.error = error;
        self.request.duration_ms = Some(self.elapsed_ms());
        self.sink.record(&self.request);
    }

    fn elapsed_ms(&self) -> u32 {
        u32::try_from(self.started.elapsed().as_millis()).unwrap_or(u32::MAX)
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if !self.is_finished {
            self.finish(RequestOutcome::Cancelled, Some("the harness hung up before the response ended".into()));
        }
    }
}

fn now_ms() -> i64 {
    let since_epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}

/// What the proxy answers with: nothing (a refusal), or the provider's body
/// as it streams, observed on the way.
pub(crate) enum ProxyBody {
    Empty,
    Observed(Box<ObservedBody>),
}

pub(crate) struct ObservedBody {
    /// The provider's body, slowed to the throttle's down rate if any: what
    /// is observed is what the harness receives, when it receives it.
    inner: Paced<Incoming>,
    observer: Observer,
    recording: Option<Recording>,
    status: StatusCode,
    response_bytes: u64,
}

impl ObservedBody {
    pub(crate) fn new(inner: Paced<Incoming>, observer: Observer, mut recording: Recording, status: StatusCode) -> ObservedBody {
        recording.answered(status);
        let mut body = ObservedBody { inner, observer, recording: Some(recording), status, response_bytes: 0 };
        // An empty body is never polled: it ends here.
        if body.inner.is_end_stream() {
            body.end(None);
        }
        body
    }

    fn end(&mut self, failure: Option<Failure>) {
        let Some(mut recording) = self.recording.take() else { return };
        self.observer.finish();
        recording.observed(&self.observer, self.response_bytes);
        let (outcome, error) = match failure {
            Some(f) => (RequestOutcome::Failed, Some(f.as_str().to_string())),
            None if self.status.is_success() => (RequestOutcome::Completed, None),
            None => {
                let code = self.observer.usage.error_type.as_deref().map(|t| format!(" ({t})")).unwrap_or_default();
                (RequestOutcome::HttpError, Some(format!("the provider answered {}{code}", self.status.as_u16())))
            }
        };
        recording.finish(outcome, error);
    }
}

impl Drop for ObservedBody {
    /// The harness hung up before the body ended.  After the provider's
    /// final event it had the whole answer (Codex hangs up right then), so
    /// the request ended as it would have; before, keep what was seen, and
    /// the recording's own drop marks it `Cancelled`.
    fn drop(&mut self) {
        if self.observer.has_final_event() {
            self.end(None);
            return;
        }
        if let Some(recording) = self.recording.as_mut() {
            recording.observed(&self.observer, self.response_bytes);
        }
    }
}

impl Body for ProxyBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let body = match self.get_mut() {
            ProxyBody::Empty => return Poll::Ready(None),
            ProxyBody::Observed(body) => body,
        };
        let frame = match Pin::new(&mut body.inner).poll_frame(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(frame) => frame,
        };
        match frame {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    if let Some(recording) = body.recording.as_mut() {
                        recording.first_byte();
                    }
                    body.response_bytes = body.response_bytes.saturating_add(data.len() as u64);
                    body.observer.feed(data);
                }
                if body.inner.is_end_stream() {
                    body.end(None);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Some(Err(e)) => {
                body.end(Some(Failure::Stream));
                Poll::Ready(Some(Err(e)))
            }
            None => {
                body.end(None);
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            ProxyBody::Empty => true,
            ProxyBody::Observed(body) => body.inner.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            ProxyBody::Empty => SizeHint::with_exact(0),
            ProxyBody::Observed(body) => body.inner.size_hint(),
        }
    }
}
