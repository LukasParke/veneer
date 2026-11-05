//! OpenTelemetry traces (standards proposal §5.7): one span per run with a child span per
//! step, exported once at the end as OTLP/HTTP JSON. Enabled only by the standard
//! environment (`OTEL_EXPORTER_OTLP_ENDPOINT` or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`;
//! `OTEL_SDK_DISABLED=true` turns it off). A W3C `TRACEPARENT` makes the run a child of the
//! caller's trace, and every step's commands receive their own `TRACEPARENT`. Export
//! failures never fail the run.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Random-enough ids without an RNG dependency: hash of time, pid, a counter, and an address.
fn random_bytes<const N: usize>() -> [u8; N] {
    let local = 0u8;
    let mut h = blake3::Hasher::new();
    h.update(&now_ns().to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    h.update(&COUNTER.fetch_add(1, Ordering::SeqCst).to_le_bytes());
    h.update(&(&local as *const u8 as usize).to_le_bytes());
    let mut out = [0u8; N];
    out.copy_from_slice(&h.finalize().as_bytes()[..N]);
    out
}

/// Parses a W3C `traceparent` (`00-<32 hex>-<16 hex>-<2 hex>`): (trace id, parent span id).
pub fn parse_traceparent(s: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = s.trim().split('-').collect();
    let hex = |x: &str, n: usize| x.len() == n && x.chars().all(|c| c.is_ascii_hexdigit());
    if parts.len() < 4
        || !hex(parts[0], 2)
        || parts[0] == "ff"
        || !hex(parts[1], 32)
        || !hex(parts[2], 16)
    {
        return None;
    }
    let (t, p) = (parts[1].to_lowercase(), parts[2].to_lowercase());
    if t.chars().all(|c| c == '0') || p.chars().all(|c| c == '0') {
        return None;
    }
    Some((t, p))
}

#[derive(Debug, Clone)]
struct SpanRec {
    span_id: String,
    parent: Option<String>,
    name: String,
    start: u64,
    end: u64,
    attrs: Vec<(String, Value)>,
    error: Option<String>,
}

pub struct Tracer {
    endpoint: String,
    headers: Vec<(String, String)>,
    pub trace_id: String,
    /// The caller's span (from `TRACEPARENT`), parent of the run span.
    remote_parent: Option<String>,
    pub run_span: String,
    run_start: u64,
    spans: Mutex<Vec<SpanRec>>,
}

/// An in-flight step span.
pub struct StepSpan {
    pub span_id: String,
    start: u64,
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

/// `k=v,k2=v2` with percent-decoded values (the OTEL_*_HEADERS / RESOURCE_ATTRIBUTES format).
fn kv_list(s: &str) -> Vec<(String, String)> {
    s.split(',')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), percent_decode(v.trim())))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Tracer {
    /// A tracer when the standard OTLP environment asks for one.
    pub fn from_env() -> Option<Self> {
        if env("OTEL_SDK_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
            return None;
        }
        if env("OTEL_TRACES_EXPORTER").is_some_and(|v| v == "none") {
            return None;
        }
        let endpoint = env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").or_else(|| {
            env("OTEL_EXPORTER_OTLP_ENDPOINT")
                .map(|b| format!("{}/v1/traces", b.trim_end_matches('/')))
        })?;
        let mut headers = env("OTEL_EXPORTER_OTLP_HEADERS")
            .map(|h| kv_list(&h))
            .unwrap_or_default();
        headers.extend(
            env("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
                .map(|h| kv_list(&h))
                .unwrap_or_default(),
        );
        let parent = env("TRACEPARENT").and_then(|t| parse_traceparent(&t));
        let trace_id = parent
            .as_ref()
            .map(|(t, _)| t.clone())
            .unwrap_or_else(|| hex::encode(random_bytes::<16>()));
        Some(Self {
            endpoint,
            headers,
            trace_id,
            remote_parent: parent.map(|(_, p)| p),
            run_span: hex::encode(random_bytes::<8>()),
            run_start: now_ns(),
            spans: Mutex::new(Vec::new()),
        })
    }

    pub fn start_step(&self) -> StepSpan {
        StepSpan {
            span_id: hex::encode(random_bytes::<8>()),
            start: now_ns(),
        }
    }

    /// `traceparent` for commands run inside `span` (sampled).
    pub fn traceparent(&self, span: &StepSpan) -> String {
        format!("00-{}-{}-01", self.trace_id, span.span_id)
    }

    pub fn end_step(
        &self,
        span: StepSpan,
        name: &str,
        attrs: Vec<(String, Value)>,
        error: Option<String>,
    ) {
        self.spans.lock().unwrap().push(SpanRec {
            span_id: span.span_id,
            parent: Some(self.run_span.clone()),
            name: name.to_string(),
            start: span.start,
            end: now_ns(),
            attrs,
            error,
        });
    }

    /// The OTLP/JSON `ExportTraceServiceRequest`, with the run span closed now.
    pub fn request(
        &self,
        run_name: &str,
        run_attrs: Vec<(String, Value)>,
        error: Option<String>,
    ) -> Value {
        let mut spans = self.spans.lock().unwrap().clone();
        spans.insert(
            0,
            SpanRec {
                span_id: self.run_span.clone(),
                parent: self.remote_parent.clone(),
                name: run_name.to_string(),
                start: self.run_start,
                end: now_ns(),
                attrs: run_attrs,
                error,
            },
        );
        let attr = |(k, v): &(String, Value)| {
            let value = match v {
                Value::Bool(b) => json!({"boolValue": b}),
                Value::Number(n) if n.is_i64() || n.is_u64() => json!({"intValue": n.to_string()}),
                Value::Number(n) => json!({"doubleValue": n.as_f64()}),
                Value::String(s) => json!({"stringValue": s}),
                other => json!({"stringValue": other.to_string()}),
            };
            json!({"key": k, "value": value})
        };
        let mut resource = vec![
            (
                "service.name".to_string(),
                json!(env("OTEL_SERVICE_NAME").unwrap_or_else(|| "veneer".into())),
            ),
            (
                "service.version".to_string(),
                json!(env!("CARGO_PKG_VERSION")),
            ),
        ];
        for (k, v) in env("OTEL_RESOURCE_ATTRIBUTES")
            .map(|s| kv_list(&s))
            .unwrap_or_default()
        {
            if k == "service.name" && env("OTEL_SERVICE_NAME").is_some() {
                continue;
            }
            resource.retain(|(rk, _)| rk != &k);
            resource.push((k, json!(v)));
        }
        let spans: Vec<Value> = spans
            .iter()
            .map(|s| {
                let mut o = json!({
                    "traceId": self.trace_id,
                    "spanId": s.span_id,
                    "name": s.name,
                    "kind": 1,
                    "startTimeUnixNano": s.start.to_string(),
                    "endTimeUnixNano": s.end.max(s.start).to_string(),
                    "attributes": s.attrs.iter().map(attr).collect::<Vec<_>>(),
                    "status": match &s.error {
                        Some(m) => json!({"code": 2, "message": m}),
                        None => json!({"code": 1}),
                    },
                });
                if let Some(p) = &s.parent {
                    o["parentSpanId"] = json!(p);
                }
                o
            })
            .collect();
        json!({"resourceSpans": [{
            "resource": {"attributes": resource.iter().map(attr).collect::<Vec<_>>()},
            "scopeSpans": [{"scope": {"name": "veneer", "version": env!("CARGO_PKG_VERSION")}, "spans": spans}],
        }]})
    }

    /// POSTs the request. Returns an error description instead of failing the run.
    pub fn export(&self, body: &Value) -> Result<(), String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(5)))
            .http_status_as_error(true)
            .build()
            .into();
        let mut req = agent
            .post(&self.endpoint)
            .header("Content-Type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req.send(body.to_string())
            .map(|_| ())
            .map_err(|e| format!("OTLP export to {} failed: {e}", self.endpoint))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_parsing() {
        assert_eq!(
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            Some((
                "4bf92f3577b34da6a3ce929d0e0e4736".into(),
                "00f067aa0ba902b7".into()
            ))
        );
        assert_eq!(
            parse_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01"),
            None
        );
        assert_eq!(parse_traceparent("garbage"), None);
    }

    #[test]
    fn headers_decode() {
        assert_eq!(
            kv_list("api-key=a%20b,x=1"),
            vec![("api-key".into(), "a b".into()), ("x".into(), "1".into())]
        );
    }
}
