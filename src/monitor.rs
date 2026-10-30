//! Loopback HTTP monitor with read-only messages and narrow board responses.
//!
//! The HTTP parser is Hyper. API credentials travel in an Authorization header;
//! a bootstrap URL fragment is consumed and cleared by the browser. Same-user local code and
//! administrators remain inside the endpoint's existing trust boundary.
use crate::broker::Broker;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::{StreamExt, stream};
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Request, Response, StatusCode,
    body::{Bytes, Frame, Incoming},
    header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HOST, ORIGIN, TRANSFER_ENCODING},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};

type Body = UnsyncBoxBody<Bytes, Infallible>;
const MAX_CONNECTIONS: usize = 32;
const MAX_EVENTS: usize = 8;
const MAX_REQUESTS_PER_SECOND: usize = 100;
const MAX_QUERY: usize = 2048;

/// Each enable generation has a fresh credential and owns its HTTP connections.
/// Deliberately has no Debug implementation: status/logging must not expose keys.
#[derive(Default)]
pub struct Control {
    enabled: bool,
    port: u16,
    generation: u64,
    token: Option<String>,
    url: Option<String>,
    last_error: Option<String>,
}
impl Control {
    pub fn enable(&mut self, port: u16) -> Value {
        if !self.enabled || self.port != port || self.last_error.is_some() {
            self.generation = self.generation.wrapping_add(1);
            self.enabled = true;
            self.port = port;
            self.token = Some(URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()));
            self.url = None;
            self.last_error = None;
        }
        self.credentials()
    }
    pub fn disable(&mut self) -> Value {
        self.generation = self.generation.wrapping_add(1);
        self.enabled = false;
        self.token = None;
        self.url = None;
        self.last_error = None;
        self.status()
    }
    pub fn status(&self) -> Value {
        json!({"enabled":self.enabled,"url":self.url,"requested_port":self.port,"last_error":self.last_error})
    }
    /// Only the private local administration socket may return this value.
    pub fn credentials(&self) -> Value {
        let mut value = self.status();
        value["access_token"] = json!(self.token);
        value["access_url"] = json!(
            self.url
                .as_ref()
                .zip(self.token.as_ref())
                .map(|(url, token)| format!("{url}/#access_token={token}"))
        );
        value
    }
    pub(crate) fn current(&self, generation: u64) -> bool {
        self.enabled && self.generation == generation
    }
}
fn default_limit() -> usize {
    50
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorQuery {
    pub machine: Option<String>,
    pub agent: Option<String>,
    pub state: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub before: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}
impl Default for MonitorQuery {
    fn default() -> Self {
        Self {
            machine: None,
            agent: None,
            state: None,
            since: None,
            until: None,
            before: None,
            limit: default_limit(),
        }
    }
}
struct Rate {
    started: Instant,
    count: usize,
}
impl Rate {
    fn allow(&mut self) -> bool {
        self.allow_limit(MAX_REQUESTS_PER_SECOND)
    }
    fn allow_limit(&mut self, limit: usize) -> bool {
        if self.started.elapsed() >= Duration::from_secs(1) {
            self.started = Instant::now();
            self.count = 0;
        }
        self.count += 1;
        self.count <= limit
    }
}
#[derive(Clone)]
struct App {
    broker: Broker,
    generation: u64,
    host: String,
    origin: String,
    token: String,
    events: Arc<Semaphore>,
    rate: Arc<Mutex<Rate>>,
    write_rate: Arc<Mutex<Rate>>,
    writes: Arc<Semaphore>,
    reads: Arc<Semaphore>,
}
impl App {
    fn current(&self) -> bool {
        self.broker
            .monitor_control
            .lock()
            .unwrap()
            .current(self.generation)
    }
}
fn body(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into()).boxed_unsync()
}
fn response(
    status: StatusCode,
    content_type: &'static str,
    content: impl Into<Bytes>,
) -> Response<Body> {
    let mut response = Response::new(body(content));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, content_type.parse().unwrap());
    headers.insert("cache-control", "no-store".parse().unwrap());
    headers.insert("pragma", "no-cache".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("x-frame-options", "DENY".parse().unwrap());
    headers.insert("x-dns-prefetch-control", "off".parse().unwrap());
    headers.insert(
        "cross-origin-resource-policy",
        "same-origin".parse().unwrap(),
    );
    headers.insert("content-security-policy", "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'".parse().unwrap());
    response
}
fn error(status: StatusCode, message: &'static str) -> Response<Body> {
    response(
        status,
        "application/json; charset=utf-8",
        json!({"error":message,"code":match status {
            StatusCode::UNAUTHORIZED=>"unauthorized",StatusCode::FORBIDDEN=>"forbidden",
            StatusCode::TOO_MANY_REQUESTS=>"rate_limited",StatusCode::PAYLOAD_TOO_LARGE=>"too_large",
            StatusCode::REQUEST_TIMEOUT=>"timeout",StatusCode::NOT_FOUND=>"not_found",
            StatusCode::METHOD_NOT_ALLOWED=>"method_not_allowed",StatusCode::UNSUPPORTED_MEDIA_TYPE=>"unsupported_media_type",
            StatusCode::BAD_REQUEST=>"invalid_request",_=>"unavailable"
        }}).to_string(),
    )
}
fn unique_header<'a>(request: &'a Request<Incoming>, name: &str) -> Result<Option<&'a str>, ()> {
    let mut values = request.headers().get_all(name).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }
    first.map(|v| v.to_str().map_err(|_| ())).transpose()
}
fn safe_context(request: &Request<Incoming>, app: &App) -> bool {
    // Absolute-form proxy targets are not an additional origin authority.
    if request.uri().scheme().is_some() || request.uri().authority().is_some() {
        return false;
    }
    if unique_header(request, HOST.as_str()) != Ok(Some(app.host.as_str())) {
        return false;
    }
    match unique_header(request, ORIGIN.as_str()) {
        Ok(None) => {}
        Ok(Some(origin)) if origin == app.origin => {}
        _ => return false,
    }
    matches!(
        unique_header(request, "sec-fetch-site"),
        Ok(None) | Ok(Some("none" | "same-origin"))
    )
}
fn authorized(request: &Request<Incoming>, expected: &str) -> bool {
    let Ok(Some(value)) = unique_header(request, AUTHORIZATION.as_str()) else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    // Fixed-size constant work after rejecting an invalid credential length.
    if token.len() != expected.len() {
        return false;
    }
    token.as_bytes().ct_eq(expected.as_bytes()).into()
}
async fn route(request: Request<Incoming>, app: App) -> Result<Response<Body>, Infallible> {
    if !app.current() {
        return Ok(error(StatusCode::UNAUTHORIZED, "Monitor access expired"));
    }
    if !safe_context(&request, &app) {
        return Ok(error(
            StatusCode::FORBIDDEN,
            "Untrusted browser origin or host",
        ));
    }
    if !app.rate.lock().unwrap().allow() {
        return Ok(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Monitor request limit reached",
        ));
    }
    let path = request.uri().path().to_owned();
    let query = request.uri().query().map(str::to_owned);
    if request.uri().to_string().len() > MAX_QUERY {
        return Ok(error(StatusCode::BAD_REQUEST, "Oversized query"));
    }
    if path.starts_with("/api/") && !authorized(&request, &app.token) {
        return Ok(error(StatusCode::UNAUTHORIZED, "Access token required"));
    }
    if request.method() == hyper::Method::POST
        && path.starts_with("/api/boards/")
        && (path.ends_with("/responses") || path.ends_with("/messages"))
    {
        return Ok(board_response(request, app).await);
    }
    if request.method() != hyper::Method::GET {
        return Ok(error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Only reads and existing board control submissions are supported",
        ));
    }
    if request.headers().contains_key(TRANSFER_ENCODING)
        || request
            .headers()
            .get(CONTENT_LENGTH)
            .is_some_and(|v| v.as_bytes() != b"0")
    {
        return Ok(error(
            StatusCode::BAD_REQUEST,
            "Request bodies are not accepted",
        ));
    }
    let board_read = path
        .strip_prefix("/api/boards/")
        .is_some_and(|name| !name.contains('/'));
    if path != "/api/messages" && !board_read && query.is_some() {
        return Ok(error(StatusCode::BAD_REQUEST, "Unexpected query"));
    }
    let response = match path.as_str() {
        "/" => response(
            StatusCode::OK,
            "text/html; charset=utf-8",
            include_str!("../assets/monitor/index.html"),
        ),
        "/style.css" => response(
            StatusCode::OK,
            "text/css; charset=utf-8",
            include_str!("../assets/monitor/style.css"),
        ),
        "/boards.js" => response(
            StatusCode::OK,
            "text/javascript; charset=utf-8",
            include_str!("../assets/monitor/boards.js"),
        ),
        "/app.js" => response(
            StatusCode::OK,
            "text/javascript; charset=utf-8",
            include_str!("../assets/monitor/app.js"),
        ),
        "/api/status" => match app.broker.monitor_metadata() {
            Ok(value) => json_response(value),
            Err(_) => error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Monitor data unavailable",
            ),
        },
        "/api/messages" => {
            match serde_urlencoded::from_str::<MonitorQuery>(query.as_deref().unwrap_or("")) {
                Ok(query) => match app.broker.monitor_messages(query) {
                    Ok(value) => json_response(value),
                    Err(_) => error(StatusCode::BAD_REQUEST, "Invalid message filters"),
                },
                Err(_) => error(StatusCode::BAD_REQUEST, "Invalid message filters"),
            }
        }
        "/api/boards" => {
            let Ok(permit) = app.reads.clone().try_acquire_owned() else {
                return Ok(error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Board read limit reached",
                ));
            };
            let broker = app.broker.clone();
            let generation = app.generation;
            match tokio::task::spawn_blocking(move || {
                let _permit = permit;
                broker.monitor_boards(generation)
            })
            .await
            {
                Ok(reply) => board_reply(reply),
                Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Boards unavailable"),
            }
        }
        _ if path.starts_with("/api/boards/") && path.ends_with("/events") => {
            let name = path
                .strip_prefix("/api/boards/")
                .and_then(|name| name.strip_suffix("/events"))
                .unwrap_or("")
                .parse::<crate::board::Name>();
            match name {
                Ok(name) => board_events(app.clone(), name).await,
                Err(_) => error(StatusCode::BAD_REQUEST, "Invalid board name"),
            }
        }
        _ if board_read => {
            let Ok(permit) = app.reads.clone().try_acquire_owned() else {
                return Ok(error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Board read limit reached",
                ));
            };
            let name = path
                .trim_start_matches("/api/boards/")
                .parse::<crate::board::Name>();
            let conditional =
                serde_urlencoded::from_str::<BoardQuery>(query.as_deref().unwrap_or(""));
            match (name, conditional) {
                (Ok(name), Ok(query))
                    if query
                        .if_revision
                        .as_deref()
                        .is_none_or(crate::monitor_board::hash) =>
                {
                    let broker = app.broker.clone();
                    let generation = app.generation;
                    match tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        broker.monitor_board(generation, &name, query.if_revision.as_deref())
                    })
                    .await
                    {
                        Ok(reply) => board_reply(reply),
                        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Board unavailable"),
                    }
                }
                _ => error(
                    StatusCode::BAD_REQUEST,
                    "Invalid board name or conditional revision",
                ),
            }
        }
        "/api/events" => events(app.clone()),
        _ if path.starts_with("/api/messages/") => {
            let id = path.trim_start_matches("/api/messages/");
            if id.is_empty() || id.len() > 100 || id.contains('/') {
                error(StatusCode::BAD_REQUEST, "Invalid message ID")
            } else {
                match app.broker.monitor_message(id) {
                    Ok(value) => json_response(value),
                    Err(_) => error(StatusCode::NOT_FOUND, "Message unavailable"),
                }
            }
        }
        _ => error(StatusCode::NOT_FOUND, "Not found"),
    };
    if path.starts_with("/api/") && !app.current() {
        return Ok(error(StatusCode::UNAUTHORIZED, "Monitor access expired"));
    }
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BoardQuery {
    if_revision: Option<String>,
}
fn board_reply(reply: crate::monitor_board::Reply) -> Response<Body> {
    response(
        StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        "application/json; charset=utf-8",
        reply.body.to_string(),
    )
}
async fn board_response(request: Request<Incoming>, app: App) -> Response<Body> {
    const MAX_BODY: usize = 512 * 1024;
    let message = request.uri().path().ends_with("/messages");
    if request.uri().query().is_some() {
        return error(StatusCode::BAD_REQUEST, "Unexpected query");
    }
    let Some(name) = request
        .uri()
        .path()
        .strip_prefix("/api/boards/")
        .and_then(|s| s.strip_suffix(if message { "/messages" } else { "/responses" }))
        .and_then(|s| s.parse::<crate::board::Name>().ok())
    else {
        return error(StatusCode::BAD_REQUEST, "Invalid board name");
    };
    if unique_header(&request, ORIGIN.as_str()) != Ok(Some(app.origin.as_str())) {
        return error(
            StatusCode::FORBIDDEN,
            "Board responses require the exact local Origin",
        );
    }
    let content_type = unique_header(&request, CONTENT_TYPE.as_str())
        .ok()
        .flatten()
        .map(str::to_ascii_lowercase);
    if !matches!(
        content_type.as_deref(),
        Some(
            "application/json"
                | "application/json; charset=utf-8"
                | "application/json;charset=utf-8"
        )
    ) || request.headers().contains_key("content-encoding")
    {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Board responses require unencoded JSON",
        );
    }
    if let Some(value) = request.headers().get(CONTENT_LENGTH) {
        match value.to_str().ok().and_then(|v| v.parse::<usize>().ok()) {
            Some(length) if length <= MAX_BODY => (),
            _ => {
                return error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Board response exceeds the request limit",
                );
            }
        }
    }
    let Ok(permit) = app.writes.clone().try_acquire_owned() else {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "Board response concurrency limit reached",
        );
    };
    if !app.write_rate.lock().unwrap().allow_limit(10) {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "Board response rate limit reached",
        );
    }
    let mut incoming = request.into_body();
    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        while let Some(frame) = incoming.frame().await {
            let frame = frame.map_err(|_| StatusCode::BAD_REQUEST)?;
            if let Some(data) = frame.data_ref() {
                if bytes.len().saturating_add(data.len()) > MAX_BODY {
                    return Err(StatusCode::PAYLOAD_TOO_LARGE);
                }
                bytes.extend_from_slice(data);
            }
        }
        Ok(bytes)
    })
    .await;
    let bytes = match collected {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(status)) => return error(status, "Invalid or oversized board response body"),
        Err(_) => return error(StatusCode::REQUEST_TIMEOUT, "Board response body timed out"),
    };
    enum Write {
        Response(crate::monitor_board::ResponseRequest),
        Message(crate::board_messages::Submission),
    }
    let request = if message {
        let request = match serde_json::from_slice::<crate::board_messages::Submission>(&bytes) {
            Ok(value) => value,
            Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid board message JSON"),
        };
        if request.board != name || !request.valid() {
            return error(StatusCode::BAD_REQUEST, "Invalid board message submission");
        }
        Write::Message(request)
    } else {
        let request = match serde_json::from_slice::<crate::monitor_board::ResponseRequest>(&bytes)
        {
            Ok(value) => value,
            Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid board response JSON"),
        };
        if matches!(&request.value, crate::monitor_board::Desired::Text { text } if text.len() > crate::board::MAX_BOARD_BYTES)
        {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Decoded text response exceeds 64 KiB",
            );
        }
        if !request.valid(&name) {
            return error(
                StatusCode::BAD_REQUEST,
                "Invalid board response or decoded text exceeds 64 KiB",
            );
        }
        Write::Response(request)
    };
    let broker = app.broker.clone();
    let generation = app.generation;
    match tokio::task::spawn_blocking(move || {
        // HTTP cancellation cannot release capacity while queued/running work
        // continues. The commit guard checks revocation after the DB queue.
        let _permit = permit;
        match request {
            Write::Response(request) => broker.monitor_board_response(generation, &name, request),
            Write::Message(request) => broker.monitor_board_message(generation, &name, request),
        }
    }).await {
        Ok(reply) if app.current()=>board_reply(reply),
        Ok(_)=>error(StatusCode::UNAUTHORIZED, "Monitor access expired"),
        Err(_)=>response(StatusCode::INTERNAL_SERVER_ERROR,"application/json; charset=utf-8",json!({"code":"uncertain","uncertain":true,"error":"Submission outcome is uncertain; retain the exact message ID and payload for retry"}).to_string()),
    }
}
fn json_response(value: Value) -> Response<Body> {
    response(
        StatusCode::OK,
        "application/json; charset=utf-8",
        value.to_string(),
    )
}
struct EventState {
    app: App,
    _permit: OwnedSemaphorePermit,
    revision: Option<(u64, u64)>,
    heartbeat: Instant,
}
async fn read_board_revision(
    app: &App,
    name: &crate::board::Name,
    permit: OwnedSemaphorePermit,
) -> Result<crate::monitor_board::BoardIdentity, Box<Response<Body>>> {
    let broker = app.broker.clone();
    let generation = app.generation;
    let name = name.clone();
    match tokio::task::spawn_blocking(move || {
        // Keep capacity occupied even if the HTTP future is cancelled while
        // this job waits for the shared broker lock.
        let _permit = permit;
        broker.monitor_board_revision(generation, &name)
    })
    .await
    {
        Ok(Ok(revision)) if app.current() => Ok(revision),
        Ok(Ok(_)) => Err(Box::new(error(
            StatusCode::UNAUTHORIZED,
            "Monitor access expired",
        ))),
        Ok(Err(reply)) => Err(Box::new(board_reply(reply))),
        Err(_) => Err(Box::new(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Board unavailable",
        ))),
    }
}
struct BoardEventState {
    app: App,
    name: crate::board::Name,
    _permit: OwnedSemaphorePermit,
    revision: crate::monitor_board::BoardIdentity,
    initial: bool,
    heartbeat: Instant,
}
async fn board_events(app: App, name: crate::board::Name) -> Response<Body> {
    let Ok(permit) = app.events.clone().try_acquire_owned() else {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "Live monitor connection limit reached",
        );
    };
    let Ok(read_permit) = app.reads.clone().try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "Board read limit reached");
    };
    let revision = match read_board_revision(&app, &name, read_permit).await {
        Ok(revision) => revision,
        Err(response) => return *response,
    };
    let state = BoardEventState {
        app,
        name,
        _permit: permit,
        revision,
        initial: true,
        heartbeat: Instant::now(),
    };
    let stream = stream::unfold(state, |mut state| async move {
        loop {
            if !state.app.current() {
                return None;
            }
            if state.initial {
                state.initial = false;
                state.heartbeat = Instant::now();
                let bytes = Bytes::from(format!(
                    "event: board-revision\ndata: {}\n\n",
                    json!({"name":state.name,"revision":state.revision.revision,"exists":state.revision.exists,"last_updated":state.revision.last_updated,"last_updated_source":state.revision.last_updated_source})
                ));
                return Some((Ok::<_, Infallible>(Frame::data(bytes)), state));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            if !state.app.current() {
                return None;
            }
            let Ok(permit) = state.app.reads.clone().try_acquire_owned() else {
                continue;
            };
            let revision = match read_board_revision(&state.app, &state.name, permit).await {
                Ok(revision) => revision,
                Err(_) => return None,
            };
            if state.revision != revision {
                state.revision = revision;
                state.initial = true;
                continue;
            }
            if state.heartbeat.elapsed() >= Duration::from_secs(15) {
                state.heartbeat = Instant::now();
                return Some((
                    Ok(Frame::data(Bytes::from_static(b": heartbeat\n\n"))),
                    state,
                ));
            }
        }
    })
    .boxed();
    let mut response = response(
        StatusCode::OK,
        "text/event-stream; charset=utf-8",
        Bytes::new(),
    );
    *response.body_mut() = StreamBody::new(stream).boxed_unsync();
    response
}
fn events(app: App) -> Response<Body> {
    let Ok(permit) = app.events.clone().try_acquire_owned() else {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "Live monitor connection limit reached",
        );
    };
    let state = EventState {
        app,
        _permit: permit,
        revision: None,
        heartbeat: Instant::now(),
    };
    let stream = stream::unfold(state, |mut state| async move {
        loop {
            if !state.app.current() {
                return None;
            }
            let revision = match state.app.broker.monitor_revision() {
                Ok(v) => v,
                Err(_) => return None,
            };
            if state.revision != Some(revision) {
                state.revision = Some(revision);
                state.heartbeat = Instant::now();
                let bytes = Bytes::from(format!(
                    "event: revision\ndata: {}\n\n",
                    json!({"revision":revision})
                ));
                return Some((Ok::<_, Infallible>(Frame::data(bytes)), state));
            }
            if state.heartbeat.elapsed() >= Duration::from_secs(15) {
                state.heartbeat = Instant::now();
                return Some((
                    Ok(Frame::data(Bytes::from_static(b": heartbeat\n\n"))),
                    state,
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .boxed();
    let mut response = response(
        StatusCode::OK,
        "text/event-stream; charset=utf-8",
        Bytes::new(),
    );
    *response.body_mut() = StreamBody::new(stream).boxed_unsync();
    response
}

/// Supervisor is polled alongside the broker; no separate executable is launched.
/// Returning/dropping this future aborts all of its connection tasks.
pub async fn run(broker: Broker) {
    loop {
        let desired = {
            let control = broker.monitor_control.lock().unwrap();
            if control.enabled {
                Some((
                    control.generation,
                    control.port,
                    control.token.clone().unwrap(),
                ))
            } else {
                None
            }
        };
        let Some((generation, port, token)) = desired else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => listener,
            Err(_) => {
                {
                    let mut control = broker.monitor_control.lock().unwrap();
                    if control.current(generation) {
                        control.url = None;
                        control.last_error = Some("Could not bind local monitor port".into());
                    }
                }
                loop {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if !broker.monitor_control.lock().unwrap().current(generation) {
                        break;
                    }
                }
                continue;
            }
        };
        let address = match listener.local_addr() {
            Ok(address) => address,
            Err(_) => continue,
        };
        {
            let mut control = broker.monitor_control.lock().unwrap();
            if !control.current(generation) {
                continue;
            }
            control.url = Some(format!("http://{address}"));
        }
        serve_generation(broker.clone(), generation, token, listener, address).await;
    }
}
async fn serve_generation(
    broker: Broker,
    generation: u64,
    token: String,
    listener: TcpListener,
    address: SocketAddr,
) {
    let app = App {
        broker,
        generation,
        host: address.to_string(),
        origin: format!("http://{address}"),
        token,
        events: Arc::new(Semaphore::new(MAX_EVENTS)),
        writes: Arc::new(Semaphore::new(4)),
        reads: Arc::new(Semaphore::new(8)),
        write_rate: Arc::new(Mutex::new(Rate {
            started: Instant::now(),
            count: 0,
        })),
        rate: Arc::new(Mutex::new(Rate {
            started: Instant::now(),
            count: 0,
        })),
    };
    let mut tasks = JoinSet::new();
    let mut check = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _=check.tick()=>{if !app.current(){break;}},
            accepted=listener.accept()=>{
                let Ok((socket,_))=accepted else{break;};
                if tasks.len()>=MAX_CONNECTIONS{drop(socket);continue;}
                let app=app.clone();
                tasks.spawn(async move {
                    let event_stream=Arc::new(AtomicBool::new(false));
                    let flag=event_stream.clone();
                    let service=service_fn(move|request|{
                        let app=app.clone();let flag=flag.clone();
                        async move {
                            let response=route(request,app).await?;
                            flag.store(response.status()==StatusCode::OK && response.headers().get(CONTENT_TYPE).is_some_and(|v|v.as_bytes()==b"text/event-stream; charset=utf-8"),Ordering::Relaxed);
                            Ok::<_,Infallible>(response)
                        }
                    });
                    let mut builder=http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(5)).max_headers(32).max_buf_size(16*1024).keep_alive(false);
                    let connection=builder.serve_connection(TokioIo::new(socket),service);
                    tokio::pin!(connection);
                    // Ordinary slow readers cannot occupy slots for an SSE lifetime.
                    if tokio::time::timeout(Duration::from_secs(10), &mut connection).await.is_err() && event_stream.load(Ordering::Relaxed) {
                        let _=tokio::time::timeout(Duration::from_secs(1790),connection).await;
                    }
                });
            },
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

#[cfg(test)]
mod control_tests {
    use super::*;

    #[test]
    fn credentials_include_fragment_link_but_status_never_exposes_secrets() {
        let mut control = Control::default();
        let pending = control.enable(0);
        assert!(pending["access_url"].is_null());
        let token = pending["access_token"].as_str().unwrap();
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );

        control.url = Some("http://127.0.0.1:44002".into());
        let credentials = control.credentials();
        assert_eq!(credentials["url"], "http://127.0.0.1:44002");
        assert_eq!(credentials["access_token"], token);
        assert_eq!(
            credentials["access_url"],
            format!("http://127.0.0.1:44002/#access_token={token}")
        );
        let status = control.status();
        assert!(status.get("access_token").is_none());
        assert!(status.get("access_url").is_none());
        assert!(!status.to_string().contains(token));

        control.disable();
        let disabled = control.credentials();
        assert!(disabled["access_token"].is_null());
        assert!(disabled["access_url"].is_null());
    }
}

#[cfg(test)]
mod board_limits_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn cancelled_http_reads_retain_capacity_until_blocking_work_finishes() {
        let root = tempfile::tempdir().unwrap();
        let bound = Broker::bind(root.path().into(), "/missing/codex".into()).unwrap();
        let access = bound.broker.monitor_control.lock().unwrap().enable(0);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = App {
            broker: bound.broker.clone(),
            generation: 1,
            host: address.to_string(),
            origin: format!("http://{address}"),
            token: access["access_token"].as_str().unwrap().into(),
            events: Arc::new(Semaphore::new(8)),
            rate: Arc::new(Mutex::new(Rate {
                started: Instant::now(),
                count: 0,
            })),
            write_rate: Arc::new(Mutex::new(Rate {
                started: Instant::now(),
                count: 0,
            })),
            writes: Arc::new(Semaphore::new(4)),
            reads: Arc::new(Semaphore::new(8)),
        };
        let server_app = app.clone();
        let server = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let app = server_app.clone();
                connections.spawn(async move {
                    let service = service_fn(move |request| route(request, app.clone()));
                    let _ = http1::Builder::new()
                        .keep_alive(false)
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        // Freeze the already-full write-rate window for deterministic HTTP
        // coverage without depending on ten network round trips fitting 1s.
        *app.write_rate.lock().unwrap() = Rate {
            started: Instant::now() + Duration::from_secs(60),
            count: 10,
        };
        let mut limited = tokio::net::TcpStream::connect(address).await.unwrap();
        limited.write_all(format!("POST /api/boards/main/responses HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}", app.token).as_bytes()).await.unwrap();
        let mut denied = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), limited.read_to_end(&mut denied))
            .await
            .unwrap()
            .unwrap();
        assert!(denied.starts_with(b"HTTP/1.1 429"));
        assert!(String::from_utf8_lossy(&denied).contains("response rate limit"));
        *app.write_rate.lock().unwrap() = Rate {
            started: Instant::now(),
            count: 0,
        };
        let broker = bound.broker.clone();
        let (ready, waiting) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _db = broker.db.lock().unwrap();
            ready.send(()).unwrap();
            held.recv().unwrap();
        });
        waiting.recv().unwrap();
        let mut sockets = Vec::new();
        for _ in 0..8 {
            let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
            socket.write_all(format!("GET /api/boards/main HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\n\r\n", app.token).as_bytes()).await.unwrap();
            sockets.push(socket);
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while app.reads.available_permits() != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut overflow = tokio::net::TcpStream::connect(address).await.unwrap();
        overflow.write_all(format!("GET /api/boards/main HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\n\r\n", app.token).as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), overflow.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(bytes.starts_with(b"HTTP/1.1 429"));
        // Supervisor cancellation drops HTTP futures but not queued blocking jobs.
        server.abort();
        let _ = server.await;
        drop(sockets);
        assert_eq!(app.reads.available_permits(), 0);
        release.send(()).unwrap();
        holder.join().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while app.reads.available_permits() != 8 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
}
