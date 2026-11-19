//! Model requests the inspector proxy saw: one row per HTTP request a harness
//! sent to its model provider, local-only and metadata only. No body, header
//! or credential is ever stored: sizes, token counts, timings, and a
//! breakdown of what the request's context was made of.

use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::{Error, Store, get_integer, sql_integer, sql_limit};

/// Most rows one read returns, whatever the caller asks for.
pub const MODEL_REQUESTS_LIMIT_MAX: usize = 5000;
/// Most parts a context breakdown keeps (the largest; the rest fold into `Other`).
pub const CONTEXT_PARTS_MAX: usize = 64;
/// Longest endpoint path stored, in bytes.
const ENDPOINT_BYTES_MAX: usize = 200;
/// Longest error summary stored, in bytes.
const ERROR_BYTES_MAX: usize = 500;
/// Longest part label (a tool's name) stored, in bytes.
const LABEL_BYTES_MAX: usize = 120;

/// Who served the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProvider {
    /// `api.anthropic.com` (or the user's own `ANTHROPIC_BASE_URL`).
    Anthropic,
    /// `api.openai.com`, with an API key.
    OpenAi,
    /// ChatGPT's Codex backend, with a ChatGPT login.
    ChatGpt,
}

impl ModelProvider {
    pub const ALL: [ModelProvider; 3] = [ModelProvider::Anthropic, ModelProvider::OpenAi, ModelProvider::ChatGpt];

    pub fn as_str(self) -> &'static str {
        match self {
            ModelProvider::Anthropic => "anthropic",
            ModelProvider::OpenAi => "openai",
            ModelProvider::ChatGpt => "chatgpt",
        }
    }

    pub fn parse(s: &str) -> Option<ModelProvider> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

/// How a request ended, or that it hasn't yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOutcome {
    InFlight,
    /// A 2xx response, read to its end.
    Completed,
    /// The provider answered with a non-2xx status.
    HttpError,
    /// The connection or the stream broke.
    Failed,
    /// The harness hung up before the response ended.
    Cancelled,
}

impl RequestOutcome {
    pub const ALL: [RequestOutcome; 5] =
        [RequestOutcome::InFlight, RequestOutcome::Completed, RequestOutcome::HttpError, RequestOutcome::Failed, RequestOutcome::Cancelled];

    pub fn as_str(self) -> &'static str {
        match self {
            RequestOutcome::InFlight => "in_flight",
            RequestOutcome::Completed => "completed",
            RequestOutcome::HttpError => "http_error",
            RequestOutcome::Failed => "failed",
            RequestOutcome::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<RequestOutcome> {
        Self::ALL.into_iter().find(|o| o.as_str() == s)
    }
}

/// What a slice of a request's context is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    /// The system prompt (Anthropic `system`, OpenAI `instructions`).
    System,
    /// One tool's definition; `label` is its name.
    ToolDefinition,
    /// Text the user (or the harness on its behalf) wrote.
    UserText,
    /// Text the model wrote in earlier turns.
    AssistantText,
    /// Tool calls the model made; `label` is the tool.
    ToolCall,
    /// What tools returned; `label` is the tool.
    ToolResult,
    Image,
    /// Thinking or reasoning blocks carried back in.
    Reasoning,
    Other,
}

impl ContextKind {
    pub const ALL: [ContextKind; 9] = [
        ContextKind::System,
        ContextKind::ToolDefinition,
        ContextKind::UserText,
        ContextKind::AssistantText,
        ContextKind::ToolCall,
        ContextKind::ToolResult,
        ContextKind::Image,
        ContextKind::Reasoning,
        ContextKind::Other,
    ];
}

/// One slice of a request's context: every block of one kind (and, for
/// tools, one tool) added up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPart {
    pub kind: ContextKind,
    /// A tool's name for tool parts; empty otherwise.
    pub label: String,
    /// Bytes of JSON this slice takes in the request body.
    pub bytes: u64,
    /// How many blocks it is.
    pub count: u32,
}

/// What a request's context was made of, by size.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBreakdown {
    /// Largest first, at most [`CONTEXT_PARTS_MAX`].
    pub parts: Vec<ContextPart>,
    /// Messages (Anthropic) or input items (OpenAI) in the request.
    pub messages: u32,
    /// Tool definitions in the request.
    pub tools: u32,
}

/// One request a harness sent through the inspector proxy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Unique per request, chosen by the proxy.
    pub id: String,
    pub session_id: String,
    pub thread_id: String,
    pub agent_pubkey: String,
    /// The session's turn this request served, when the host knew it.
    pub turn: Option<u32>,
    pub provider: ModelProvider,
    /// The path asked for, without the proxy's secret prefix or the query
    /// (`/v1/messages`, `/responses`).
    pub endpoint: String,
    pub model: Option<String>,
    /// Unix milliseconds.
    pub started_at_ms: i64,
    /// From the request to the first byte of the response body.
    pub first_byte_ms: Option<u32>,
    /// From the request to the end of the response.
    pub duration_ms: Option<u32>,
    pub http_status: Option<u16>,
    pub streamed: bool,
    pub outcome: RequestOutcome,
    /// Uncached input tokens, as the provider reported them.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    /// Part of `output_tokens`, where the provider says so.
    pub reasoning_tokens: Option<u64>,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub stop_reason: Option<String>,
    /// A short reason when the request failed: never a body or a header.
    pub error: Option<String>,
    pub breakdown: Option<ContextBreakdown>,
    /// The proxy slowed this request on purpose (`AXEL_THROTTLE`): its
    /// timings, and a loss, are the throttle's doing.
    #[serde(default)]
    pub throttled: bool,
}

impl ModelRequest {
    /// Every input token the model read: uncached, cache reads and writes.
    pub fn context_tokens(&self) -> u64 {
        [self.input_tokens, self.cache_read_tokens, self.cache_write_tokens].into_iter().flatten().fold(0u64, u64::saturating_add)
    }

    /// Estimated cost in USD, from the per-model price table.
    pub fn cost_usd_est(&self) -> f64 {
        let signed = |v: Option<u64>| i64::try_from(v.unwrap_or(0)).unwrap_or(i64::MAX);
        crate::pricing::estimate(
            self.model.as_deref(),
            signed(self.input_tokens),
            signed(self.output_tokens),
            signed(self.cache_read_tokens),
            signed(self.cache_write_tokens),
        )
    }
}

/// Which requests to read.  Every set field must match.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelRequestFilter {
    pub thread_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_pubkey: Option<String>,
    /// Unix milliseconds; requests started before it are left out.
    pub since_ms: i64,
    /// At most this many (capped at [`MODEL_REQUESTS_LIMIT_MAX`]).
    pub limit: usize,
}

fn check(request: &ModelRequest) -> Result<(), Error> {
    let invalid = |what: &str| Err(Error::Invalid(format!("model request {}: {what}", request.id)));
    if request.id.is_empty() || request.session_id.is_empty() {
        return invalid("empty id or session");
    }
    if request.endpoint.len() > ENDPOINT_BYTES_MAX || request.error.as_ref().is_some_and(|e| e.len() > ERROR_BYTES_MAX) {
        return invalid("endpoint or error too long");
    }
    if let Some(breakdown) = &request.breakdown
        && (breakdown.parts.len() > CONTEXT_PARTS_MAX || breakdown.parts.iter().any(|p| p.label.len() > LABEL_BYTES_MAX))
    {
        return invalid("breakdown too large");
    }
    Ok(())
}

fn optional(value: Option<u64>, what: &str) -> Result<Option<i64>, Error> {
    value.map(|v| sql_integer(v, what)).transpose()
}

impl Store {
    /// Insert a request, or update it as it progresses (same `id`): the proxy
    /// records it when it starts and again when it ends.
    pub fn record_model_request(&self, request: &ModelRequest) -> Result<(), Error> {
        check(request)?;
        let breakdown = request.breakdown.as_ref().map(serde_json::to_string).transpose()?;
        self.conn.execute(
            "INSERT INTO model_requests (id, session_id, thread_id, agent_pubkey, turn, provider, endpoint, model,
                 started_at_ms, first_byte_ms, duration_ms, http_status, streamed, outcome,
                 input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                 request_bytes, response_bytes, stop_reason, error, breakdown_json, throttled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)
             ON CONFLICT(id) DO UPDATE SET
                 turn = excluded.turn, model = excluded.model, first_byte_ms = excluded.first_byte_ms,
                 duration_ms = excluded.duration_ms, http_status = excluded.http_status, streamed = excluded.streamed,
                 outcome = excluded.outcome, input_tokens = excluded.input_tokens, output_tokens = excluded.output_tokens,
                 cache_read_tokens = excluded.cache_read_tokens, cache_write_tokens = excluded.cache_write_tokens,
                 reasoning_tokens = excluded.reasoning_tokens, request_bytes = excluded.request_bytes,
                 response_bytes = excluded.response_bytes, stop_reason = excluded.stop_reason, error = excluded.error,
                 breakdown_json = excluded.breakdown_json, throttled = excluded.throttled",
            params![
                request.id,
                request.session_id,
                request.thread_id,
                request.agent_pubkey,
                request.turn,
                request.provider.as_str(),
                request.endpoint,
                request.model,
                request.started_at_ms,
                request.first_byte_ms,
                request.duration_ms,
                request.http_status,
                request.streamed,
                request.outcome.as_str(),
                optional(request.input_tokens, "input tokens")?,
                optional(request.output_tokens, "output tokens")?,
                optional(request.cache_read_tokens, "cache read tokens")?,
                optional(request.cache_write_tokens, "cache write tokens")?,
                optional(request.reasoning_tokens, "reasoning tokens")?,
                sql_integer(request.request_bytes, "request bytes")?,
                sql_integer(request.response_bytes, "response bytes")?,
                request.stop_reason,
                request.error,
                breakdown,
                request.throttled,
            ],
        )?;
        Ok(())
    }

    /// One request by id.
    pub fn model_request(&self, id: &str) -> Result<Option<ModelRequest>, Error> {
        let mut st = self.conn.prepare("SELECT * FROM model_requests WHERE id = ?1")?;
        Ok(st.query_row([id], row_model_request).optional()?)
    }

    /// Requests matching `filter`, newest first.
    pub fn model_requests(&self, filter: &ModelRequestFilter) -> Result<Vec<ModelRequest>, Error> {
        let limit = filter.limit.min(MODEL_REQUESTS_LIMIT_MAX);
        let mut st = self.conn.prepare(
            "SELECT * FROM model_requests
             WHERE started_at_ms >= ?1
               AND (?2 IS NULL OR thread_id = ?2)
               AND (?3 IS NULL OR session_id = ?3)
               AND (?4 IS NULL OR agent_pubkey = ?4)
             ORDER BY started_at_ms DESC, id DESC
             LIMIT ?5",
        )?;
        let rows = st.query_map(
            params![filter.since_ms, filter.thread_id, filter.session_id, filter.agent_pubkey, sql_limit(limit)],
            row_model_request,
        )?;
        let out: Vec<ModelRequest> = rows.collect::<Result<_, _>>()?;
        debug_assert!(out.len() <= limit);
        Ok(out)
    }

    /// Drop requests that started before `before_ms`; how many went.
    pub fn prune_model_requests(&self, before_ms: i64) -> Result<usize, Error> {
        Ok(self.conn.execute("DELETE FROM model_requests WHERE started_at_ms < ?1", [before_ms])?)
    }
}

fn row_model_request(row: &Row<'_>) -> rusqlite::Result<ModelRequest> {
    let text_enum = |column: &str, value: String| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, format!("unknown {column} {value:?}").into())
    };
    let provider: String = row.get("provider")?;
    let outcome: String = row.get("outcome")?;
    let breakdown: Option<String> = row.get("breakdown_json")?;
    let optional_u64 = |column: &str| -> rusqlite::Result<Option<u64>> {
        let value: Option<i64> = row.get(column)?;
        value.map(|v| u64::try_from(v).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Integer, Box::new(e)))).transpose()
    };
    Ok(ModelRequest {
        id: row.get("id")?,
        session_id: row.get("session_id")?,
        thread_id: row.get("thread_id")?,
        agent_pubkey: row.get("agent_pubkey")?,
        turn: row.get("turn")?,
        provider: ModelProvider::parse(&provider).ok_or_else(|| text_enum("provider", provider.clone()))?,
        endpoint: row.get("endpoint")?,
        model: row.get("model")?,
        started_at_ms: row.get("started_at_ms")?,
        first_byte_ms: row.get("first_byte_ms")?,
        duration_ms: row.get("duration_ms")?,
        http_status: row.get("http_status")?,
        streamed: row.get("streamed")?,
        outcome: RequestOutcome::parse(&outcome).ok_or_else(|| text_enum("outcome", outcome.clone()))?,
        input_tokens: optional_u64("input_tokens")?,
        output_tokens: optional_u64("output_tokens")?,
        cache_read_tokens: optional_u64("cache_read_tokens")?,
        cache_write_tokens: optional_u64("cache_write_tokens")?,
        reasoning_tokens: optional_u64("reasoning_tokens")?,
        request_bytes: get_integer(row, "request_bytes")?,
        response_bytes: get_integer(row, "response_bytes")?,
        stop_reason: row.get("stop_reason")?,
        error: row.get("error")?,
        // A breakdown that no longer parses (an older build's shape) reads as none.
        breakdown: breakdown.and_then(|json| serde_json::from_str(&json).ok()),
        throttled: row.get("throttled")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: &str, started_at_ms: i64) -> ModelRequest {
        ModelRequest {
            id: id.into(),
            session_id: "S1".into(),
            thread_id: "T1".into(),
            agent_pubkey: "PK".into(),
            turn: Some(3),
            provider: ModelProvider::Anthropic,
            endpoint: "/v1/messages".into(),
            model: Some("claude-sonnet-5-5".into()),
            started_at_ms,
            first_byte_ms: None,
            duration_ms: None,
            http_status: None,
            streamed: true,
            outcome: RequestOutcome::InFlight,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            request_bytes: 107_926,
            response_bytes: 0,
            stop_reason: None,
            error: None,
            breakdown: Some(ContextBreakdown {
                parts: vec![ContextPart { kind: ContextKind::ToolDefinition, label: "Bash".into(), bytes: 4000, count: 1 }],
                messages: 1,
                tools: 1,
            }),
            throttled: false,
        }
    }

    #[test]
    fn a_request_is_recorded_when_it_starts_and_updated_when_it_ends() {
        let store = Store::open_memory().unwrap();
        let mut r = request("R1", 1_000);
        store.record_model_request(&r).unwrap();
        assert_eq!(store.model_request("R1").unwrap(), Some(r.clone()));
        r.outcome = RequestOutcome::Completed;
        r.http_status = Some(200);
        r.input_tokens = Some(12);
        r.cache_read_tokens = Some(26_000);
        r.output_tokens = Some(40);
        r.duration_ms = Some(2_100);
        store.record_model_request(&r).unwrap();
        let read = store.model_request("R1").unwrap().unwrap();
        assert_eq!(read, r);
        assert_eq!(read.context_tokens(), 26_012);
    }

    #[test]
    fn reads_filter_order_newest_first_and_prune() {
        let store = Store::open_memory().unwrap();
        for (id, at) in [("A", 1_000), ("B", 3_000), ("C", 2_000)] {
            store.record_model_request(&request(id, at)).unwrap();
        }
        let mut other = request("D", 4_000);
        other.thread_id = "T2".into();
        store.record_model_request(&other).unwrap();
        let ids = |f: &ModelRequestFilter| store.model_requests(f).unwrap().into_iter().map(|r| r.id).collect::<Vec<_>>();
        let t1 = ModelRequestFilter { thread_id: Some("T1".into()), limit: 10, ..Default::default() };
        assert_eq!(ids(&t1), ["B", "C", "A"]);
        assert_eq!(ids(&ModelRequestFilter { limit: 2, ..Default::default() }), ["D", "B"]);
        assert_eq!(ids(&ModelRequestFilter { since_ms: 2_500, limit: 10, ..Default::default() }), ["D", "B"]);
        assert_eq!(store.prune_model_requests(2_500).unwrap(), 2);
        assert_eq!(ids(&t1), ["B"]);
    }

    #[test]
    fn oversized_metadata_is_refused() {
        let store = Store::open_memory().unwrap();
        let mut r = request("R1", 1);
        r.error = Some("x".repeat(ERROR_BYTES_MAX + 1));
        assert!(matches!(store.record_model_request(&r), Err(Error::Invalid(_))));
        let mut r = request("R2", 1);
        r.breakdown.as_mut().unwrap().parts = vec![r.breakdown.as_ref().unwrap().parts[0].clone(); CONTEXT_PARTS_MAX + 1];
        assert!(matches!(store.record_model_request(&r), Err(Error::Invalid(_))));
    }
}
