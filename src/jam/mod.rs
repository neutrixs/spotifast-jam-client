//! Jams: listening together through a jam server of the listener's choice
//! (spotifast-jam-server), since Spotify's own Jam has no API.
//!
//! Everyone plays on their own account. The host's player decides what
//! plays and publishes it; the others follow it, and their controls and
//! queued songs travel to the host as requests. The rules live in
//! `crate::app::jam`; this module only talks to the server.
//!
//! Two threads per jam keep that talk off the interface: one sends what the
//! app asks, in order, and one holds a long poll open so a change reaches
//! the app the moment the server has it. Both hand their results back
//! through a channel and wake the app.

pub mod protocol;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use protocol::*;

/// How long a long poll asks the server to wait for a change.
const POLL_WAIT_SECS: u64 = 25;
/// The client's own deadline for any request, past the long poll's wait.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(POLL_WAIT_SECS + 15);
/// Longest wait between attempts while the server cannot be reached.
const MAX_BACKOFF: Duration = Duration::from_secs(15);
/// The scheme invite pages hand to the app.
pub const SCHEME: &str = "spotifast-jam";
/// Characters a session code is made of, as the server makes them.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 6;

/// How a jam is entered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    Create { name: String },
    Join { code: String, name: String },
}

/// What the app asks the server once in a jam.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Publish(PublishPlayback),
    /// Songs for the jam's queue, at a position or at its end.
    Add(Vec<Track>, Option<usize>),
    Remove(Vec<String>),
    Move {
        id: String,
        to: usize,
    },
    Command(Command),
    Leave,
}

/// What the server answered, for the app.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Joined(Box<JoinResponse>),
    /// The jam as it stands, and this computer's clock, in Unix
    /// milliseconds, when the answer arrived.
    View(Box<SessionView>, u64),
    /// The server numbered a command this computer sent.
    CommandSent(u64),
    /// The server cannot be reached or refused one request; the jam goes on.
    Trouble(String),
    /// Starting or joining failed; there is no jam.
    Failed(String),
    /// The jam ended or forgot this listener.
    Gone,
    /// Leaving finished.
    Left,
}

/// One jam's connection. Dropping it stops both threads; [`Request::Leave`]
/// first tells the server.
pub struct Connection {
    requests: mpsc::Sender<Request>,
    events: mpsc::Receiver<Event>,
    stop: Arc<AtomicBool>,
}

struct Credentials {
    code: String,
    token: String,
}

impl Connection {
    pub fn spawn(
        server: String,
        start: Start,
        proxy: crate::settings::ProxyConfig,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let (requests, request_receiver) = mpsc::channel();
        let (event_sender, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let sender_stop = Arc::clone(&stop);
        let spawned = std::thread::Builder::new()
            .name("jam".into())
            .spawn(move || {
                run(
                    server,
                    start,
                    proxy,
                    request_receiver,
                    event_sender,
                    wake,
                    sender_stop,
                )
            });
        let connection = Self {
            requests,
            events,
            stop,
        };
        if let Err(error) = spawned {
            log::warn!("jam thread did not start: {error}");
        }
        connection
    }

    /// A connection with no server behind it: the test reads what the app
    /// asks and answers for the server.
    #[cfg(test)]
    pub fn detached() -> (Self, mpsc::Receiver<Request>, mpsc::Sender<Event>) {
        let (requests, request_receiver) = mpsc::channel();
        let (event_sender, events) = mpsc::channel();
        (
            Self {
                requests,
                events,
                stop: Arc::new(AtomicBool::new(false)),
            },
            request_receiver,
            event_sender,
        )
    }

    pub fn send(&self, request: Request) {
        let _ = self.requests.send(request);
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

struct Link {
    server: String,
    http: reqwest::blocking::Client,
    events: mpsc::Sender<Event>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Link {
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
        (self.wake)();
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.server)
    }
}

fn run(
    server: String,
    start: Start,
    proxy: crate::settings::ProxyConfig,
    requests: mpsc::Receiver<Request>,
    events: mpsc::Sender<Event>,
    wake: Arc<dyn Fn() + Send + Sync>,
    stop: Arc<AtomicBool>,
) {
    let http = match crate::http::build_blocking(&proxy, REQUEST_TIMEOUT) {
        Ok(http) => http,
        Err(error) => {
            let _ = events.send(Event::Failed(error));
            wake();
            return;
        }
    };
    let link = Arc::new(Link {
        server,
        http,
        events,
        wake,
    });
    let joined = match check_server(&link).and_then(|()| enter(&link, &start)) {
        Ok(joined) => joined,
        Err(error) => {
            link.emit(Event::Failed(error));
            return;
        }
    };
    let credentials = Arc::new(OnceLock::new());
    let _ = credentials.set(Credentials {
        code: joined.session.id.clone(),
        token: joined.token.clone(),
    });
    let since = joined.session.version;
    link.emit(Event::Joined(Box::new(joined)));

    let poller = {
        let link = Arc::clone(&link);
        let credentials = Arc::clone(&credentials);
        let stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("jam-poll".into())
            .spawn(move || poll(&link, credentials.get(), since, &stop))
    };
    if let Err(error) = poller {
        log::warn!("jam poll thread did not start: {error}");
    }
    let Some(credentials) = credentials.get() else {
        return;
    };
    while let Ok(first) = requests.recv() {
        // Only the newest playback matters: older ones still waiting are
        // already out of date.
        let mut batch = vec![first];
        batch.extend(requests.try_iter());
        let newest_publish = batch
            .iter()
            .rposition(|request| matches!(request, Request::Publish(_)));
        for (index, request) in batch.into_iter().enumerate() {
            if matches!(request, Request::Publish(_)) && Some(index) != newest_publish {
                continue;
            }
            if stop.load(Ordering::SeqCst) && request != Request::Leave {
                continue;
            }
            let leaving = request == Request::Leave;
            send(&link, credentials, request);
            if leaving {
                stop.store(true, Ordering::SeqCst);
                link.emit(Event::Left);
                return;
            }
        }
    }
    // The app dropped the connection without leaving, as it does when it
    // quits: tell the server, so the others see this listener go at once.
    stop.store(true, Ordering::SeqCst);
    let _ = authorized(
        link.http
            .post(link.url(&format!("/sessions/{}/leave", credentials.code))),
        credentials,
    )
    .timeout(Duration::from_secs(3))
    .send();
}

fn check_server(link: &Link) -> Result<(), String> {
    let info: ServerInfo = read(link.http.get(link.url("/info")).send())?;
    if info.protocol != PROTOCOL {
        return Err(format!(
            "The jam server speaks version {} and this Spotifast speaks version {PROTOCOL}.",
            info.protocol
        ));
    }
    Ok(())
}

fn enter(link: &Link, start: &Start) -> Result<JoinResponse, String> {
    match start {
        Start::Create { name } => read(
            link.http
                .post(link.url("/sessions"))
                .json(&JoinRequest { name: name.clone() })
                .send(),
        ),
        Start::Join { code, name } => read(
            link.http
                .post(link.url(&format!("/sessions/{code}/join")))
                .json(&JoinRequest { name: name.clone() })
                .send(),
        ),
    }
}

fn authorized(
    builder: reqwest::blocking::RequestBuilder,
    credentials: &Credentials,
) -> reqwest::blocking::RequestBuilder {
    builder.bearer_auth(&credentials.token)
}

fn send(link: &Link, credentials: &Credentials, request: Request) {
    let session = format!("/sessions/{}", credentials.code);
    let http = &link.http;
    let result: Result<Option<SessionView>, Answer> = match request {
        Request::Publish(playback) => answer(
            authorized(
                http.put(link.url(&format!("{session}/playback"))),
                credentials,
            )
            .json(&playback)
            .send(),
        )
        .map(Some),
        Request::Add(tracks, position) => {
            let mut last = Ok(None);
            let mut position = position;
            for chunk in tracks.chunks(MAX_ADD) {
                last = answer(
                    authorized(
                        http.post(link.url(&format!("{session}/queue"))),
                        credentials,
                    )
                    .json(&AddToQueue {
                        tracks: chunk.to_vec(),
                        position,
                    })
                    .send(),
                )
                .map(Some);
                if last.is_err() {
                    break;
                }
                // The next chunk follows this one.
                position = position.map(|at| at + chunk.len());
            }
            last
        }
        Request::Move { id, to } => answer(
            authorized(
                http.post(link.url(&format!("{session}/queue/move"))),
                credentials,
            )
            .json(&MoveInQueue { id, to })
            .send(),
        )
        .map(Some),
        Request::Remove(ids) => answer(
            authorized(
                http.post(link.url(&format!("{session}/queue/remove"))),
                credentials,
            )
            .json(&RemoveFromQueue { ids })
            .send(),
        )
        .map(Some),
        Request::Command(command) => answer::<CommandAccepted>(
            authorized(
                http.post(link.url(&format!("{session}/commands"))),
                credentials,
            )
            .json(&SendCommand { command })
            .send(),
        )
        .map(|accepted| {
            link.emit(Event::CommandSent(accepted.seq));
            Some(accepted.session)
        }),
        Request::Leave => authorized(
            http.post(link.url(&format!("{session}/leave"))),
            credentials,
        )
        .send()
        .map(|_| None)
        .map_err(|error| Answer::Trouble(error.without_url().to_string())),
    };
    match result {
        Ok(Some(view)) => link.emit(Event::View(Box::new(view), unix_ms())),
        Ok(_) => {}
        Err(Answer::Gone) => link.emit(Event::Gone),
        Err(Answer::Trouble(message)) => link.emit(Event::Trouble(message)),
    }
}

fn poll(link: &Link, credentials: Option<&Credentials>, mut since: u64, stop: &AtomicBool) {
    let Some(credentials) = credentials else {
        return;
    };
    let mut backoff = Duration::from_secs(1);
    // The first poll answers at once: the join's own view may already be
    // older than what the others did meanwhile.
    let mut wait = 0;
    while !stop.load(Ordering::SeqCst) {
        let request = authorized(
            link.http.get(link.url(&format!(
                "/sessions/{}?since={since}&wait={wait}",
                credentials.code
            ))),
            credentials,
        );
        match answer::<SessionView>(request.send()) {
            Ok(view) => {
                backoff = Duration::from_secs(1);
                wait = POLL_WAIT_SECS.min(MAX_WAIT_SECS);
                since = since.max(view.version);
                let ended = view.ended;
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                link.emit(Event::View(Box::new(view), unix_ms()));
                if ended {
                    return;
                }
            }
            Err(Answer::Gone) => {
                if !stop.load(Ordering::SeqCst) {
                    link.emit(Event::Gone);
                }
                return;
            }
            Err(Answer::Trouble(message)) => {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                link.emit(Event::Trouble(message));
                // Sleep in short steps so leaving is not held up.
                let until = std::time::Instant::now() + backoff;
                while std::time::Instant::now() < until && !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(200));
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                wait = 0;
            }
        }
    }
}

enum Answer {
    /// The jam ended, or the server no longer knows this listener.
    Gone,
    Trouble(String),
}

fn answer<T: serde::de::DeserializeOwned>(
    response: reqwest::Result<reqwest::blocking::Response>,
) -> Result<T, Answer> {
    let response = response.map_err(|error| Answer::Trouble(error.without_url().to_string()))?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 404 | 410) {
        return Err(Answer::Gone);
    }
    if !status.is_success() {
        let message = response
            .json::<ErrorBody>()
            .map(|body| body.error)
            .unwrap_or_else(|_| status.to_string());
        return Err(Answer::Trouble(message));
    }
    response
        .json::<T>()
        .map_err(|error| Answer::Trouble(error.without_url().to_string()))
}

/// [`answer`] for the requests before a jam exists, where every refusal
/// ends the attempt.
fn read<T: serde::de::DeserializeOwned>(
    response: reqwest::Result<reqwest::blocking::Response>,
) -> Result<T, String> {
    let response = response.map_err(|error| error.without_url().to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(response
            .json::<ErrorBody>()
            .map(|body| body.error)
            .unwrap_or_else(|_| {
                if status.as_u16() == 404 {
                    "This address is not a jam server.".to_owned()
                } else {
                    status.to_string()
                }
            }));
    }
    response
        .json::<T>()
        .map_err(|_| "This address is not a jam server.".to_owned())
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

// Invites.

/// A jam to join: its code, and the server it is on when the invite named
/// one. A bare code uses the server in Settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    pub server: Option<String>,
    pub code: String,
}

/// Codes are typed by people: any case, stray spaces and dashes.
pub fn normalize_code(code: &str) -> Option<String> {
    let code: String = code
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-'))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(code)
}

/// The server address as requests use it: with a scheme, `https` when none
/// was typed, and no trailing slash. `None` when it cannot be one.
pub fn normalize_server(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty()
        || text.len() > 300
        || text.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let (scheme, rest) = match text.split_once("://") {
        Some((scheme @ ("https" | "http"), rest)) => (scheme, rest),
        Some(_) => return None,
        None => ("https", text),
    };
    let rest = rest.trim_end_matches('/');
    // A host before any path, with no credentials smuggled into it.
    let authority = rest.split('/').next()?;
    if authority.is_empty() || authority.contains('@') || rest.contains(['?', '#']) {
        return None;
    }
    Some(format!("{scheme}://{rest}"))
}

/// What follows `spotifast-jam:` in `text`, in any case.
fn strip_scheme(text: &str) -> Option<&str> {
    let (scheme, rest) = text.split_once(':')?;
    scheme.eq_ignore_ascii_case(SCHEME).then_some(rest)
}

/// An invite in any shape it travels in: the app link an invite page
/// opens, the invite page's own address, or the bare code.
pub fn parse_invite(text: &str) -> Option<Invite> {
    let text = text.trim();
    if let Some(rest) = strip_scheme(text) {
        // Windows hands a link over tidied: `join/?…` rather than `join?…`,
        // sometimes a slash at the very end too. Take every such shape.
        let rest = rest.trim_start_matches('/');
        let rest = &rest[..rest.find('#').unwrap_or(rest.len())];
        let (verb, query) = rest.split_once('?')?;
        if !verb.trim_end_matches('/').eq_ignore_ascii_case("join") {
            return None;
        }
        let query = query.trim_end_matches('/');
        let mut server = None;
        let mut code = None;
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=')?;
            let value = percent_encoding::percent_decode_str(value)
                .decode_utf8()
                .ok()?;
            match key {
                "server" => server = Some(normalize_server(&value)?),
                "session" => code = Some(normalize_code(&value)?),
                _ => {}
            }
        }
        return Some(Invite {
            server: Some(server?),
            code: code?,
        });
    }
    if text.starts_with("https://") || text.starts_with("http://") {
        let path_end = text.find(['?', '#']).unwrap_or(text.len());
        let (base, code) = text[..path_end]
            .trim_end_matches('/')
            .rsplit_once("/jam/")?;
        return Some(Invite {
            server: Some(normalize_server(base)?),
            code: normalize_code(code)?,
        });
    }
    Some(Invite {
        server: None,
        code: normalize_code(text)?,
    })
}

/// What an invite page's `encodeURIComponent` leaves alone among the
/// characters a server address holds, so both sides write the same link.
const SERVER_ENCODING: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'.')
    .remove(b'-')
    .remove(b'_')
    .remove(b'~');

/// The link an invite page hands the app, which the desktop delivers.
pub fn app_link(server: &str, code: &str) -> String {
    format!(
        "{SCHEME}://join?server={}&session={code}",
        percent_encoding::utf8_percent_encode(server, SERVER_ENCODING)
    )
}

/// The address to share: the server's invite page for the jam.
pub fn invite_page(server: &str, code: &str) -> String {
    format!("{server}/jam/{code}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invites_arrive_in_every_shape() {
        let expected = Invite {
            server: Some("https://jam.example.com".into()),
            code: "ABC234".into(),
        };
        for text in [
            "spotifast-jam://join?server=https%3A%2F%2Fjam.example.com&session=ABC234",
            "spotifast-jam://join?session=abc234&server=https%3A%2F%2Fjam.example.com%2F",
            "https://jam.example.com/jam/ABC234",
            " https://jam.example.com/jam/abc234/?utm=1 ",
        ] {
            assert_eq!(parse_invite(text).as_ref(), Some(&expected), "{text}");
        }
        assert_eq!(
            parse_invite("http://10.0.0.2:8787/sub/jam/ABC234"),
            Some(Invite {
                server: Some("http://10.0.0.2:8787/sub".into()),
                code: "ABC234".into()
            })
        );
        assert_eq!(
            parse_invite("abc-234"),
            Some(Invite {
                server: None,
                code: "ABC234".into()
            })
        );
    }

    /// Windows passes a link on tidied, and browsers vary: every shape
    /// still joins the same jam.
    #[test]
    fn app_links_survive_the_way_windows_and_browsers_hand_them_over() {
        let expected = Some(Invite {
            server: Some("https://jam.example.com".into()),
            code: "ABC234".into(),
        });
        for text in [
            "spotifast-jam://join/?server=https%3A%2F%2Fjam.example.com&session=ABC234",
            "spotifast-jam://join?server=https%3A%2F%2Fjam.example.com&session=ABC234/",
            "spotifast-jam://join/?server=https%3A%2F%2Fjam.example.com&session=ABC234/",
            "SPOTIFAST-JAM://Join/?server=https%3A%2F%2Fjam.example.com&session=ABC234",
            "spotifast-jam:join?server=https%3A%2F%2Fjam.example.com&session=ABC234",
            "spotifast-jam://join?server=https%3A%2F%2Fjam.example.com&&session=ABC234#x",
        ] {
            assert_eq!(parse_invite(text), expected, "{text}");
        }
        assert_eq!(
            parse_invite("spotifast-jam://leave/?server=https%3A%2F%2Fa&session=ABC234"),
            None
        );
    }

    #[test]
    fn what_is_not_an_invite_is_refused() {
        for text in [
            "",
            "ABC23",
            "ABC2340",
            "ABCD0O",
            "spotifast-jam://join?session=ABC234",
            "spotifast-jam://leave?server=https%3A%2F%2Fa&session=ABC234",
            "spotifast-jam://join?server=ftp%3A%2F%2Fa&session=ABC234",
            "https://jam.example.com/ABC234",
            "https://user@evil.example/jam/ABC234",
            "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
        ] {
            assert_eq!(parse_invite(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_app_link_round_trips() {
        let link = app_link("https://jam.example.com/x", "ABC234");
        assert!(link.is_ascii() && !link.contains(' '));
        assert_eq!(
            parse_invite(&link),
            Some(Invite {
                server: Some("https://jam.example.com/x".into()),
                code: "ABC234".into()
            })
        );
    }

    #[test]
    fn server_addresses_get_a_scheme_and_lose_the_trailing_slash() {
        assert_eq!(
            normalize_server(" jam.example.com/ ").as_deref(),
            Some("https://jam.example.com")
        );
        assert_eq!(
            normalize_server("http://localhost:8787").as_deref(),
            Some("http://localhost:8787")
        );
        for bad in [
            "",
            "ftp://x",
            "https://",
            "https://a b",
            "https://u@h",
            "https://h/?q",
        ] {
            assert_eq!(normalize_server(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_wire_format_matches_the_server() {
        let command: Command = serde_json::from_str(r#"{"type":"seek","position_ms":5}"#).unwrap();
        assert_eq!(command, Command::Seek { position_ms: 5 });
        let view: SessionView = serde_json::from_str(
            r#"{"id":"ABC234","version":3,"server_time_ms":9,"host_id":"m1",
                "members":[{"id":"m1","name":"H","host":true}],
                "playback":{"track":{"uri":"spotify:track:a"},"position_ms":1,"playing":true,"updated_at_ms":8},
                "queue":[],"commands":[{"seq":1,"from":"m2","from_name":"G","command":{"type":"next"}}],
                "ended":false}"#,
        )
        .unwrap();
        assert_eq!(view.playback.applied_seq, 0);
        assert_eq!(view.commands[0].command, Command::Next);
    }
}
