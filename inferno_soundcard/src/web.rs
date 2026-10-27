//! The settings page, on `127.0.0.1`: `GET /` (the page), `GET /api/status`, `GET /api/config` and
//! `PUT /api/config` (saves and applies it).
//!
//! Other web sites the user visits must not change the settings: requests must be addressed to this host
//! by name (no DNS rebinding), and changes need a JSON body and a custom header, which browsers don't send
//! cross-origin without a CORS preflight this server never allows.

use std::{
  io::{BufRead, BufReader, Read, Write},
  net::{Ipv4Addr, TcpListener, TcpStream},
  sync::mpsc,
  time::Duration,
};

use log::{info, warn};

use crate::{
  app::{Command, SharedStatus},
  config::Config,
};

const PAGE: &str = include_str!("ui.html");
const MAX_BODY: usize = 64 * 1024;
pub const CHANGE_HEADER: &str = "x-inferno-soundcard";

pub struct Request {
  pub method: String,
  pub path: String,
  pub headers: Vec<(String, String)>,
  pub body: Vec<u8>,
}

impl Request {
  fn header(&self, name: &str) -> Option<&str> {
    self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
  }
}

pub struct Response {
  pub status: u16,
  pub content_type: &'static str,
  pub body: Vec<u8>,
}

fn json(status: u16, value: impl serde::Serialize) -> Response {
  Response { status, content_type: "application/json", body: serde_json::to_vec(&value).unwrap() }
}

fn error(status: u16, message: impl Into<String>) -> Response {
  json(status, serde_json::json!({ "error": message.into() }))
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Request> {
  let mut reader = BufReader::new(stream.try_clone()?);
  let mut line = String::new();
  reader.read_line(&mut line)?;
  let mut parts = line.split_whitespace();
  let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
  let mut headers = vec![];
  loop {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
      break;
    }
    if let Some((name, value)) = line.split_once(':') {
      headers.push((name.trim().to_lowercase(), value.trim().to_string()));
    }
    if headers.len() > 100 {
      return Err(std::io::ErrorKind::InvalidData.into());
    }
  }
  let length: usize = headers.iter().find(|(n, _)| n == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
  if length > MAX_BODY {
    return Err(std::io::ErrorKind::InvalidData.into());
  }
  let mut body = vec![0; length];
  reader.read_exact(&mut body)?;
  Ok(Request { method, path, headers, body })
}

/// Handles a request; `port` is the page's, `commands` applies a new configuration.
pub fn handle(request: &Request, port: u16, status: &SharedStatus, commands: &mpsc::Sender<Command>) -> Response {
  let host_ok = matches!(request.header("host"), Some(host) if host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}"));
  if !host_ok {
    return error(421, "unexpected Host");
  }
  match (request.method.as_str(), request.path.as_str()) {
    ("GET", "/") => Response { status: 200, content_type: "text/html; charset=utf-8", body: PAGE.as_bytes().to_vec() },
    ("GET", "/api/status") => json(200, &*status.lock().unwrap()),
    ("GET", "/api/config") => match Config::load() {
      Ok(config) => json(200, config),
      Err(e) => error(500, e),
    },
    ("PUT", "/api/config") => {
      let is_json = request.header("content-type").is_some_and(|t| t.starts_with("application/json"));
      if request.header(CHANGE_HEADER).is_none() || !is_json {
        return error(403, "changes need a JSON body and the settings page's header");
      }
      let config: Config = match serde_json::from_slice(&request.body) {
        Ok(config) => config,
        Err(e) => return error(400, e.to_string()),
      };
      if let Err(e) = config.validate() {
        return error(400, e);
      }
      if let Err(e) = config.save() {
        return error(500, e);
      }
      info!("configuration saved");
      let _ = commands.send(Command::Reload);
      json(200, config)
    }
    _ => error(404, "not found"),
  }
}

fn respond(stream: &mut TcpStream, response: Response) -> std::io::Result<()> {
  let reason = match response.status {
    200 => "OK",
    400 => "Bad Request",
    403 => "Forbidden",
    404 => "Not Found",
    421 => "Misdirected Request",
    _ => "Error",
  };
  write!(
    stream,
    "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
     X-Frame-Options: DENY\r\nConnection: close\r\n\r\n",
    response.status,
    response.content_type,
    response.body.len()
  )?;
  stream.write_all(&response.body)
}

/// Serves the settings page on `127.0.0.1:port` (in this thread, forever).
pub fn serve(listener: TcpListener, status: SharedStatus, commands: mpsc::Sender<Command>) {
  let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
  for stream in listener.incoming() {
    let Ok(mut stream) = stream else { continue };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    match read_request(&mut stream) {
      Ok(request) => {
        let response = handle(&request, port, &status, &commands);
        let _ = respond(&mut stream, response);
      }
      Err(e) => warn!("settings page: bad request: {e}"),
    }
  }
}

pub fn bind(port: u16) -> std::io::Result<TcpListener> {
  TcpListener::bind((Ipv4Addr::LOCALHOST, port))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::app::Status;
  use std::sync::{Arc, Mutex};

  fn request(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Request {
    Request {
      method: method.into(),
      path: path.into(),
      headers: headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect(),
      body: body.as_bytes().to_vec(),
    }
  }

  #[test]
  fn requests_must_be_addressed_to_this_host() {
    let status = Arc::new(Mutex::new(Status::default()));
    let (commands, _rx) = mpsc::channel();
    let ok = handle(&request("GET", "/", &[("host", "127.0.0.1:47800")], ""), 47800, &status, &commands);
    assert_eq!(ok.status, 200);
    let rebound = handle(&request("GET", "/api/status", &[("host", "evil.example:47800")], ""), 47800, &status, &commands);
    assert_eq!(rebound.status, 421);
  }

  #[test]
  fn changes_need_the_header_and_valid_json() {
    let status = Arc::new(Mutex::new(Status::default()));
    let (commands, rx) = mpsc::channel();
    let host = ("host", "localhost:47800");
    // what a cross-origin form can send
    let form = request("PUT", "/api/config", &[host, ("content-type", "text/plain")], "{}");
    assert_eq!(handle(&form, 47800, &status, &commands).status, 403);
    let headers = [host, ("content-type", "application/json"), (CHANGE_HEADER, "1")];
    let invalid = request("PUT", "/api/config", &headers, r#"{"cards": []}"#);
    assert_eq!(handle(&invalid, 47800, &status, &commands).status, 400);
    assert!(rx.try_recv().is_err(), "nothing applied");
  }
}
