//! HTTP consoles (ADR-056). A statement is `METHOD /path?key=value`, followed by an
//! optional JSON body. Only a path on the connection's server can become a request.

use serde_json::{Map, Value, json};
use spelaion_driver_api::{
    Dialect, ObjectInfo, ObjectKind, RequestClass, RequestError, StatementSpan,
};

#[derive(Debug, Clone, Copy)]
pub struct HttpDialect {
    engine: &'static str,
}

impl HttpDialect {
    pub const fn named(engine: &'static str) -> Self {
        Self { engine }
    }
}

impl Dialect for HttpDialect {
    fn name(&self) -> &'static str {
        self.engine
    }
    fn quote_identifier(&self, name: &str) -> String {
        Value::String(name.into()).to_string()
    }
    fn reads_requests(&self) -> bool {
        true
    }
    // SQL parameter substitution has no meaning in an HTTP console. In particular a colon
    // in JSON or a path is never a named SQL parameter.
    fn code_ranges(&self, _script: &str) -> Vec<std::ops::Range<usize>> {
        Vec::new()
    }
    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }
    fn to_request(&self, text: &str, _database: &str) -> Option<Result<Value, RequestError>> {
        Some(to_request(text))
    }
    fn request_class(&self, request: &Value) -> RequestClass {
        request_class(self.engine, request)
    }
    fn browse_statement(&self, object: &ObjectInfo) -> Option<String> {
        if object.kind != ObjectKind::Collection {
            return None;
        }
        let name = &object.name;
        // A catalogue name is data too: it must not introduce another path or query.
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            || matches!(name.as_str(), "." | "..")
        {
            return None;
        }
        Some(match self.engine {
            "solr" => format!("GET /solr/{name}/select?q=*%3A*&rows=200&wt=json"),
            "meilisearch" => format!("POST /indexes/{name}/search\n{{\"limit\":200}}"),
            _ => format!("GET /{name}/_search\n{{\"size\":200}}"),
        })
    }
}

fn error(position: usize, message: impl Into<String>) -> RequestError {
    RequestError {
        position,
        message: message.into(),
    }
}

fn method(text: &str) -> bool {
    matches!(text, "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE")
}

/// Read the whole statement, never a prefix. No headers, hosts or arbitrary URLs are accepted.
pub fn to_request(text: &str) -> Result<Value, RequestError> {
    let text = text.trim();
    let (header, body) = text.split_once('\n').unwrap_or((text, ""));
    let parts: Vec<_> = header.split_whitespace().collect();
    if parts.len() != 2 || !method(parts[0]) {
        return Err(error(
            0,
            "expected METHOD /path followed by an optional JSON body",
        ));
    }
    let (path, raw_query) = parts[1].split_once('?').unwrap_or((parts[1], ""));
    validate_path(path)?;
    let mut query = Map::new();
    if !raw_query.is_empty() {
        for pair in raw_query.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let key = decode_query(key)?;
            let value = decode_query(value)?;
            if key.is_empty() || query.insert(key, Value::String(value)).is_some() {
                return Err(error(
                    0,
                    "query parameter names must be nonempty and unique",
                ));
            }
        }
    }
    let body = if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body)
            .map_err(|e| error(header.len() + 1, format!("invalid JSON body: {e}")))?
    };
    Ok(json!({"method":parts[0], "path":path, "query":query, "body":body}))
}

/// A path is deliberately plain. Percent escapes in paths are refused rather than letting
/// the client, proxy and server disagree about escaped slashes, dots or a second decoding.
pub fn validate_path(path: &str) -> Result<(), RequestError> {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains(['\\', '%', '#', '?'])
        || path.bytes().any(|b| b <= b' ' || b >= 127)
        || path.split('/').any(|segment| matches!(segment, "." | ".."))
    {
        return Err(error(
            0,
            "expected a plain path from / on this connection's server (no host, escapes or dot segments)",
        ));
    }
    Ok(())
}

fn decode_query(text: &str) -> Result<String, RequestError> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .ok_or_else(|| error(i, "incomplete query escape"))?;
                let hex = std::str::from_utf8(hex).map_err(|_| error(i, "invalid query escape"))?;
                out.push(
                    u8::from_str_radix(hex, 16).map_err(|_| error(i, "invalid query escape"))?,
                );
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    let decoded = String::from_utf8(out).map_err(|_| error(0, "query is not UTF-8"))?;
    if decoded.chars().any(char::is_control) {
        return Err(error(0, "control character in query"));
    }
    Ok(decoded)
}

fn split(script: &str) -> Vec<StatementSpan> {
    let mut spans = Vec::new();
    let mut start = None;
    let mut offset = 0;
    let mut depth: i64 = 0;
    let mut quoted = false;
    let mut escape = false;
    for line in script.split_inclusive('\n') {
        let trim = line.trim();
        if depth == 0 && !quoted && trim.starts_with('#') {
            if let Some(begin) = start.take() {
                push(script, begin, offset, &mut spans);
            }
            offset += line.len();
            continue;
        }
        if depth == 0 && !quoted && !trim.is_empty() && !trim.starts_with('#') {
            let header = trim.split_whitespace().next().is_some_and(method);
            if header && let Some(begin) = start.take() {
                push(script, begin, offset, &mut spans);
            }
            if start.is_none() {
                start = Some(offset);
            }
        }
        if start.is_some() {
            for b in line.bytes() {
                if quoted {
                    if escape {
                        escape = false;
                    } else if b == b'\\' {
                        escape = true;
                    } else if b == b'"' {
                        quoted = false;
                    }
                } else {
                    match b {
                        b'"' => quoted = true,
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => depth -= 1,
                        _ => {}
                    }
                }
            }
        }
        offset += line.len();
    }
    if let Some(begin) = start {
        push(script, begin, script.len(), &mut spans);
    }
    spans
}

fn push(script: &str, start: usize, end: usize, spans: &mut Vec<StatementSpan>) {
    let text = &script[start..end];
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        spans.push(StatementSpan {
            start: start + text.len() - text.trim_start().len(),
            end: end - (text.len() - text.trim_end().len()),
        });
    }
}

/// A small closed read list, used by the guard only. These engines have no read barrier:
/// neither agents nor read-only connections may use it as permission to run a request.
pub fn request_class(engine: &str, request: &Value) -> RequestClass {
    let Some(raw_path) = request["path"].as_str() else {
        return RequestClass::Unknown;
    };
    if validate_path(raw_path).is_err() {
        return RequestClass::Unknown;
    }
    let path = if raw_path == "/" {
        "/"
    } else {
        raw_path.trim_end_matches('/')
    };
    let Some(method) = request["method"].as_str() else {
        return RequestClass::Unknown;
    };
    if method == "DELETE" {
        return RequestClass::Drop;
    }
    if matches!(engine, "elasticsearch" | "opensearch")
        && contains_key(&request["body"], "remove_index")
    {
        return RequestClass::Drop;
    }
    if engine == "solr" && path.contains("/admin/") {
        let action = request["query"]["action"]
            .as_str()
            .unwrap_or("")
            .to_ascii_uppercase();
        return match action.as_str() {
            "DELETE" | "UNLOAD" | "DELETESHARD" | "DELETEREPLICA" | "DELETEALIAS" => {
                RequestClass::Drop
            }
            "LIST" | "STATUS" | "CLUSTERSTATUS" => RequestClass::Read,
            "" if path == "/solr/admin/info/system" => RequestClass::Read,
            _ => RequestClass::Unknown,
        };
    }
    if path.ends_with("/_delete_by_query") {
        return RequestClass::DeleteAll;
    }
    if path.ends_with("/_update_by_query") {
        return RequestClass::UpdateAll;
    }
    if engine == "solr"
        && path.contains("/update")
        && (contains_key(&request["body"], "delete")
            || !request["query"]["stream.body"].is_null()
            || method == "GET")
    {
        return RequestClass::DeleteAll;
    }
    if contains_key(&request["body"], "script") || contains_key(&request["body"], "script_fields") {
        return RequestClass::Unknown;
    }
    if method == "GET" || method == "HEAD" {
        let read = match engine {
            "solr" => {
                path.ends_with("/select")
                    || path == "/solr/admin/collections"
                    || path == "/solr/admin/info/system"
            }
            "meilisearch" => {
                path == "/health"
                    || path == "/version"
                    || path == "/indexes"
                    || path.ends_with("/documents")
            }
            _ => {
                path == "/"
                    || path == "/_cat/indices"
                    || path.ends_with("/_search")
                    || path.ends_with("/_mapping")
                    || path.ends_with("/_count")
            }
        };
        if read {
            return RequestClass::Read;
        }
    }
    if method == "POST"
        && ((engine == "meilisearch" && path.ends_with("/search"))
            || (matches!(engine, "elasticsearch" | "opensearch")
                && (path.ends_with("/_search") || path.ends_with("/_count"))))
    {
        return RequestClass::Read;
    }
    if matches!(method, "POST" | "PUT" | "PATCH") {
        RequestClass::Write
    } else {
        RequestClass::Unknown
    }
}

fn contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => map.contains_key(key) || map.values().any(|v| contains_key(v, key)),
        Value::Array(values) => values.iter().any(|v| contains_key(v, key)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_request_preserves_the_body_and_query() {
        assert_eq!(
            to_request("GET /orders/_search?size=2&q=a%3Ab+c\n{\"query\":{\"match_all\":{}}}")
                .unwrap(),
            json!({"method":"GET","path":"/orders/_search","query":{"size":"2","q":"a:b c"},"body":{"query":{"match_all":{}}}})
        );
    }
    #[test]
    fn cannot_leave_the_connection_or_smuggle_a_request() {
        for target in [
            "https://elsewhere/x",
            "//elsewhere/x",
            "/../x",
            "/a/./b",
            "/%2felsewhere",
            "/%252felsewhere",
            "/a\\b",
            "/a#x",
            "/a?x=%0d%0aHost:elsewhere",
            "/a?x=1&x=2",
        ] {
            assert!(to_request(&format!("GET {target}")).is_err(), "{target}");
        }
        for text in [
            "GET / HTTP/1.1",
            "GET /\nHost: elsewhere",
            "GET /\n{} trailing",
            "GET /\n{}\nDELETE /x",
        ] {
            assert!(to_request(text).is_err(), "{text}");
        }
    }
    #[test]
    fn splits_json_bodies_and_keeps_positions() {
        let script = "# Requests\nGET /a/_search\n{\n\"query\": {\"match\": {\"text\": \"DELETE /x; \\\"x\\\"\"}}\n}\n\nDELETE /b\n";
        let spans = split(script);
        assert_eq!(spans.len(), 2);
        assert!(script[spans[0].start..spans[0].end].starts_with("GET /a"));
        assert_eq!(&script[spans[1].start..spans[1].end], "DELETE /b");
        let script = "# first\nGET /one\n# a comment with { and \"\nGET /two\n# end";
        let spans = split(script);
        assert_eq!(
            spans
                .iter()
                .map(|span| span.text(script))
                .collect::<Vec<_>>(),
            ["GET /one", "GET /two"]
        );
    }
    #[test]
    fn destructive_routes_cannot_look_like_reads() {
        for text in [
            "DELETE /orders",
            "POST /orders/_delete_by_query\n{\"query\":{\"match_all\":{}}}",
            "GET /orders/_delete_by_query",
        ] {
            let class = request_class("elasticsearch", &to_request(text).unwrap());
            assert!(matches!(
                class,
                RequestClass::Drop | RequestClass::DeleteAll
            ));
        }
        assert!(matches!(
            request_class("elasticsearch", &to_request("GET /_unknown").unwrap()),
            RequestClass::Unknown
        ));
        assert_eq!(
            request_class(
                "solr",
                &to_request("GET /solr/admin/collections?action=DELETE&name=orders").unwrap()
            ),
            RequestClass::Drop
        );
        assert_eq!(
            request_class(
                "solr",
                &to_request("POST /solr/orders/update\n[{\"delete\":{\"query\":\"*:*\"}}]")
                    .unwrap()
            ),
            RequestClass::DeleteAll
        );
    }
}
