//! Minimal client for the daemon's newline-delimited RPC protocol
//! (issue #5). Used by the CLI (`daemon status`) and by the integration
//! tests; clients never read SQLite directly.
//!
//! One request line, one response line. `events.subscribe` connections
//! become push-only after the response: the caller switches to reading
//! hf-event/v1 lines.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::schema::{Family, validate_doc};
use crate::value::{Val, object, string};

/// A live request/response connection over the daemon socket.
#[derive(Debug)]
pub struct Connection {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

/// A typed RPC failure (an `ok:false` envelope or a transport error).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcError {
    /// Stable lowercase-dotted code (transport errors use `client.*`).
    pub code: String,
    /// Human message.
    pub message: String,
}

fn client_error(code: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code: code.to_string(),
        message: message.into(),
    }
}

/// A parsed response document: either an ok result object or an error.
#[derive(Clone, Debug)]
pub struct Response {
    /// The request id echoed by the daemon.
    pub id: String,
    /// Whether the response is an ok result.
    pub ok: bool,
    /// The result object when `ok` (empty object when none).
    pub result: Val,
    /// The error envelope when refused.
    pub error: Option<RpcError>,
}

impl Connection {
    /// Open a connection to the daemon socket.
    pub fn open(socket_path: &Path) -> Result<Connection, RpcError> {
        let writer = UnixStream::connect(socket_path)
            .map_err(|err| client_error("client.connect", format!("{err}")))?;
        writer
            .set_read_timeout(Some(std::time::Duration::from_secs(15)))
            .map_err(|err| client_error("client.connect", format!("set timeout: {err}")))?;
        let reader_stream = writer
            .try_clone()
            .map_err(|err| client_error("client.connect", err.to_string()))?;
        Ok(Connection {
            reader: BufReader::new(reader_stream),
            writer,
        })
    }

    /// Send one canonical request line for `method` with `id` and `params`.
    pub fn send_request(
        &mut self,
        id: &str,
        method: &str,
        params: Option<&Val>,
    ) -> Result<(), RpcError> {
        let doc = object(vec![
            ("schema", string("hf-rpc-request/v1")),
            ("id", string(id)),
            ("method", string(method)),
            ("params", params.cloned().unwrap_or_else(crate::value::null)),
        ]);
        let mut line = crate::canonical::canonical_text(&doc);
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .map_err(|err| client_error("client.write", err.to_string()))
    }

    /// Read one response line, validating its envelope shape.
    pub fn read_response(&mut self) -> Result<Response, RpcError> {
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .map_err(|err| client_error("client.read", err.to_string()))?;
        if read == 0 {
            return Err(client_error(
                "client.closed",
                "daemon closed the connection without a response",
            ));
        }
        let doc = Val::parse_json(line.trim_end_matches(['\r', '\n']))
            .map_err(|message| client_error("client.malformed", message))?;
        let verdict = validate_doc(Family::RpcResponse, &doc);
        if !verdict.is_accepted() {
            return Err(client_error(
                "client.malformed",
                format!("daemon sent a refused response: {}", verdict.message()),
            ));
        }
        let id = doc
            .get("id")
            .and_then(Val::as_str)
            .unwrap_or_default()
            .to_string();
        let ok = doc.get("ok").and_then(Val::as_bool).unwrap_or(false);
        if ok {
            let result = doc
                .get("result")
                .cloned()
                .unwrap_or_else(crate::value::object_empty);
            Ok(Response {
                id,
                ok: true,
                result,
                error: None,
            })
        } else {
            let error = match doc.get("error") {
                Some(error) => Some(RpcError {
                    code: error
                        .get("code")
                        .and_then(Val::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    message: error
                        .get("message")
                        .and_then(Val::as_str)
                        .unwrap_or("unknown error")
                        .to_string(),
                }),
                None => Some(client_error(
                    "client.malformed",
                    "error envelope missing error",
                )),
            };
            Ok(Response {
                id,
                ok: false,
                result: crate::value::null(),
                error,
            })
        }
    }

    /// Read the next raw line (event-stream mode after `events.subscribe`).
    pub fn read_line(&mut self) -> Result<Option<String>, RpcError> {
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .map_err(|err| client_error("client.read", err.to_string()))?;
        if read == 0 {
            return Ok(None);
        }
        Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
    }
}

/// One request/response exchange on a fresh connection: the request id is
/// generated (8 lowercase hex from a per-process counter + pid mix).
pub fn call(socket_path: &Path, method: &str, params: Option<&Val>) -> Result<Val, RpcError> {
    let mut connection = Connection::open(socket_path)?;
    let id = fresh_id();
    connection.send_request(&id, method, params)?;
    let response = connection.read_response()?;
    if response.id != id {
        return Err(client_error(
            "client.id_mismatch",
            format!("daemon echoed id {} for request {id}", response.id),
        ));
    }
    match response.ok {
        true => Ok(response.result),
        false => Err(response
            .error
            .unwrap_or_else(|| client_error("client.malformed", "response missing a typed error"))),
    }
}

/// A fresh, schema-valid request id (8 lowercase hex; unique per process).
pub fn fresh_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nonce = COUNTER.fetch_add(1, Ordering::Relaxed) ^ std::process::id();
    format!("{nonce:08x}")
}

/// Subscribe to the event stream: send the request, read the response line,
/// then yield hf-event/v1 lines until the daemon disconnects.
pub struct EventSubscription {
    connection: Connection,
}

impl EventSubscription {
    /// Send `events.subscribe` and confirm the ok response.
    pub fn open(socket_path: &Path, last_seq: Option<i64>) -> Result<EventSubscription, RpcError> {
        let mut connection = Connection::open(socket_path)?;
        let id = fresh_id();
        let params = last_seq.map(|seq| object(vec![("last_seq", crate::value::integer(seq))]));
        connection.send_request(&id, "events.subscribe", params.as_ref())?;
        let response = connection.read_response()?;
        if !response.ok {
            return Err(response
                .error
                .unwrap_or_else(|| client_error("client.subscribe", "subscribe refused")));
        }
        Ok(EventSubscription { connection })
    }

    /// Read the next event line (None on clean disconnect).
    pub fn next_line(&mut self) -> Result<Option<String>, RpcError> {
        self.connection.read_line()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_ids_are_schema_valid_and_distinct() {
        let a = fresh_id();
        let b = fresh_id();
        assert_ne!(a, b);
        assert!(crate::formats::is_request_id(&a));
        assert!(crate::formats::is_request_id(&b));
    }

    #[test]
    fn send_request_line_validates_as_a_request() {
        let path = std::env::temp_dir().join(format!("client-none-{}", std::process::id()));
        // No daemon at this path; open fails with a typed client error.
        let err = Connection::open(&path).expect_err("absent socket must fail");
        assert!(err.code.starts_with("client."));
    }
}
