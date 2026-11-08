use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

/// Build a ureq Agent that uses the platform's native TLS library.
///
/// Uses OpenSSL on Linux, Secure Transport on macOS, and SChannel on
/// Windows — the same TLS implementations that curl uses. This ensures
/// certificates trusted by the OS (including custom CA certs added to
/// the system trust store) are handled identically to curl and browsers.
pub fn build_agent(timeout_secs: Option<u64>) -> ureq::Agent {
    let tls = native_tls::TlsConnector::new().expect("failed to create TLS connector");
    let mut builder = ureq::AgentBuilder::new().tls_connector(Arc::new(tls));

    if let Some(secs) = timeout_secs {
        builder = builder.timeout(Duration::from_secs(secs));
    }

    builder.build()
}

/// HTTP response wrapper that normalizes ureq's error handling.
/// ureq treats non-2xx responses as errors; this wrapper treats them as normal
/// responses (matching minreq's previous behavior and what callers expect).
pub struct Response {
    pub status_code: u16,
    body: Vec<u8>,
}

impl Response {
    pub fn as_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.body)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.body
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.body
    }
}

fn read_ureq_response(response: ureq::Response) -> Result<Response, String> {
    let status_code = response.status();
    let mut body = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut body)
        .map_err(|e| format!("Failed to read response body: {}", e))?;
    Ok(Response { status_code, body })
}

/// Execute a ureq request, normalizing errors so that HTTP error status codes
/// are returned as Ok(Response) rather than Err.
pub fn send(request: ureq::Request) -> Result<Response, String> {
    match request.call() {
        Ok(response) => read_ureq_response(response),
        Err(ureq::Error::Status(_code, response)) => read_ureq_response(response),
        Err(ureq::Error::Transport(err)) => Err(err.to_string()),
    }
}

/// Execute a ureq request with a string body.
pub fn send_with_body(request: ureq::Request, body: &str) -> Result<Response, String> {
    send_with_body_classified(request, body).map_err(|failure| failure.message)
}

/// A request that never produced an HTTP response.
#[derive(Debug, Clone)]
pub struct TransportFailure {
    pub message: String,
    /// The request hit the agent timeout (connect, write or read).
    pub timed_out: bool,
}

/// Like [`send_with_body`], but says whether a transport failure was a
/// timeout, so callers can treat "the server took too long" differently from
/// "the server could not be reached".
pub fn send_with_body_classified(
    request: ureq::Request,
    body: &str,
) -> Result<Response, TransportFailure> {
    match request.send_string(body) {
        Ok(response) => read_ureq_response(response).map_err(|message| TransportFailure {
            timed_out: message_says_timed_out(&message),
            message,
        }),
        Err(ureq::Error::Status(_code, response)) => {
            read_ureq_response(response).map_err(|message| TransportFailure {
                timed_out: message_says_timed_out(&message),
                message,
            })
        }
        Err(ureq::Error::Transport(err)) => Err(TransportFailure {
            timed_out: transport_timed_out(&err),
            message: err.to_string(),
        }),
    }
}

fn transport_timed_out(err: &ureq::Transport) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(err);
    while let Some(error) = source {
        if let Some(io) = error.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
        {
            return true;
        }
        source = error.source();
    }
    message_says_timed_out(&err.to_string())
}

fn message_says_timed_out(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("timed out") || message.contains("timeout")
}
