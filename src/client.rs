//! herdr's socket API: one request per connection, or one long-lived
//! `events.subscribe` connection that streams event lines.

use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::transport::{self, ReadLine, Stream};

#[derive(Debug)]
pub enum HerdrError {
    Io(io::Error),
    Timeout,
    /// herdr closed the connection without answering.
    NoResponse,
    Api {
        code: String,
        message: String,
    },
    BadResponse(String),
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HerdrError::Io(e) => write!(f, "{e}"),
            HerdrError::Timeout => write!(f, "timed out waiting for herdr"),
            HerdrError::NoResponse => write!(f, "herdr closed the connection without a response"),
            HerdrError::Api { code, message } => write!(f, "{code}: {message}"),
            HerdrError::BadResponse(line) => write!(f, "unreadable response from herdr: {line}"),
        }
    }
}

impl From<io::Error> for HerdrError {
    fn from(e: io::Error) -> Self {
        HerdrError::Io(e)
    }
}

/// The subscription was closed, by herdr or by an error on the connection.
#[derive(Debug)]
pub struct Closed;

pub trait EventSource {
    /// The next pushed line, or None if none arrives before `deadline`.
    fn next_event(&mut self, deadline: Instant) -> Result<Option<Value>, Closed>;
}

pub trait Api {
    type Events: EventSource;
    fn request(&mut self, method: &str, params: Value) -> Result<Value, HerdrError>;
    fn subscribe(&mut self, subscriptions: Value) -> Result<Self::Events, HerdrError>;
}

pub struct HerdrClient {
    path: String,
    timeout: Duration,
    next_id: u64,
}

impl HerdrClient {
    pub fn new(path: String, timeout: Duration) -> Self {
        HerdrClient {
            path,
            timeout,
            next_id: 0,
        }
    }

    fn id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}_{}", self.next_id)
    }

    /// Connect, send one request line, and read the first line back.
    fn open(
        &mut self,
        id: String,
        method: &str,
        params: Value,
    ) -> Result<(Box<dyn Stream>, Value), HerdrError> {
        let deadline = Instant::now() + self.timeout;
        let mut stream = transport::connect(&self.path, deadline)?;
        let mut line = serde_json::to_vec(&json!({"id": id, "method": method, "params": params}))
            .expect("a JSON value always serializes");
        line.push(b'\n');
        stream.send(&line)?;
        let reply = match stream.read_line(deadline)? {
            ReadLine::Line(reply) => reply,
            ReadLine::Timeout => return Err(HerdrError::Timeout),
            ReadLine::Eof => return Err(HerdrError::NoResponse),
        };
        let reply: Value = serde_json::from_slice(&reply)
            .map_err(|_| HerdrError::BadResponse(String::from_utf8_lossy(&reply).into_owned()))?;
        if let Some(error) = reply.get("error") {
            return Err(api_error(error));
        }
        Ok((stream, reply))
    }
}

fn api_error(error: &Value) -> HerdrError {
    let text = |key: &str, default: &str| {
        error
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or(default)
            .to_string()
    };
    HerdrError::Api {
        code: text("code", "unknown_error"),
        message: text("message", ""),
    }
}

impl Api for HerdrClient {
    type Events = Subscription;

    fn request(&mut self, method: &str, params: Value) -> Result<Value, HerdrError> {
        let id = self.id("req");
        let (_stream, reply) = self.open(id, method, params)?;
        Ok(reply.get("result").cloned().unwrap_or_else(|| json!({})))
    }

    fn subscribe(&mut self, subscriptions: Value) -> Result<Subscription, HerdrError> {
        let id = self.id("sub");
        let (stream, _ack) = self.open(
            id,
            "events.subscribe",
            json!({"subscriptions": subscriptions}),
        )?;
        Ok(Subscription {
            stream: Some(stream),
        })
    }
}

pub struct Subscription {
    stream: Option<Box<dyn Stream>>,
}

impl EventSource for Subscription {
    fn next_event(&mut self, deadline: Instant) -> Result<Option<Value>, Closed> {
        let stream = self.stream.as_mut().ok_or(Closed)?;
        match stream.read_line(deadline) {
            Ok(ReadLine::Line(line)) => Ok(serde_json::from_slice(&line).ok()),
            Ok(ReadLine::Timeout) => Ok(None),
            Ok(ReadLine::Eof) | Err(_) => {
                self.stream = None;
                Err(Closed)
            }
        }
    }
}
