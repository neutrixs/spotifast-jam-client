//! Jams, as the app plays them.
//!
//! Everyone in a jam plays on their own account; `crate::jam` carries what
//! they say to each other through the jam server. The host's player is the
//! one that decides: it publishes what it plays and carries out what the
//! others ask. Everyone has the same controls as the host, and anyone adds,
//! reorders and removes the jam's songs.
//!
//! The jam's queue lives on the server, where anyone can rearrange it. The
//! host's player is handed only its next song, through its own queue, so
//! the queue rules (`docs/_reference/queue.md`) play it as any song; the
//! next one follows once it starts. A song handed over and then moved or
//! removed is taken back when this computer plays (rule 11); on another
//! device it still plays, and the jam carries on after it.
//!
//! The others follow. Their player starts the host's song at the host's
//! place and keeps within a few seconds of it, with the host's shuffle and
//! repeat, and their controls become requests to the host. Pause, seek,
//! shuffle and repeat still act at once on their own player, the way every
//! control does, and while the host has not yet carried the request out,
//! the jam's older answers are not allowed to undo it.

use super::*;
use crate::jam::protocol::{
    Command as JamCommand, Member, Playback as JamPlayback, PublishPlayback, QueueItem,
    Repeat as JamRepeat, SessionView, Track as JamTrack,
};
use crate::jam::{self as wire, Event as JamEvent, Request as JamRequest};

/// A listener further than this from the host's place seeks to it. Loose on
/// purpose: a second or two apart is still listening together, while a
/// correction is heard, and network delay blurs every estimate of the
/// host's place.
const DRIFT_MS: u32 = 5_000;
/// How long a listener must stay that far off before it seeks.
const DRIFT_CONFIRM: Duration = Duration::from_secs(2);
/// The host's place jumping by more than this is a seek worth publishing.
const SEEK_JUMP_MS: i64 = 2_500;
/// The host publishes at least this often while playing, so a listener's
/// estimate of its place never wanders far.
const HEARTBEAT: Duration = Duration::from_secs(15);
/// The host publishes at most this often.
const PUBLISH_GAP: Duration = Duration::from_millis(400);
/// After carrying out a request, the host waits this long for its player
/// to show the result before publishing it.
const SETTLE: Duration = Duration::from_millis(350);
/// A listener asks for the host's song again only after this long.
const FOLLOW_RETRY: Duration = Duration::from_secs(8);
/// A listener corrects play or pause at most this often.
const TOGGLE_GAP: Duration = Duration::from_millis(2_500);
/// A listener corrects its place at most this often.
const SEEK_GAP: Duration = Duration::from_secs(15);
/// The longest a listener's own pause or seek holds off the jam's answers.
const HOLD: Duration = Duration::from_secs(5);
/// How far ahead a listener starts a song, for the time it takes to load.
const LOAD_LEAD_MS: u32 = 400;
/// A listener matches the host's shuffle and repeat at most this often.
const MODES_GAP: Duration = Duration::from_secs(3);
/// How long the host waits for a song asked for from the jam's queue to
/// reach its own queue before giving up on playing it.
const PLAY_NOW_WAIT: Duration = Duration::from_secs(5);
/// How long the host's player may show no song before the jam pauses on
/// its last one. A seek, a skip or a device's answer catching up leaves a
/// moment with no song; the jam must not stutter for it.
const LOST_GRACE: Duration = Duration::from_secs(4);
/// How long a difference from the host must last before anything acts on
/// it. A player reports "paused" or the last song for a moment while a song
/// loads, a seek lands or Spotify's answer catches up; acting on that made
/// listeners stutter.
const CONFIRM: Duration = Duration::from_secs(1);
/// Near the end of the host's song, a listener on another song has most
/// likely moved on first: it started a little ahead, and its player goes on
/// to the next song by itself. The host's next song is on its way, so the
/// listener waits rather than going back for the last seconds.
const BOUNDARY_MS: u32 = 5_000;
/// How often the app looks at the jam while nothing else wakes it.
const TICK: Duration = Duration::from_secs(1);

/// The jam this computer is in.
pub struct Jam {
    connection: wire::Connection,
    pub server: String,
    /// This listener, once the server has let it in.
    pub member_id: Option<String>,
    pub host: bool,
    pub view: Option<SessionView>,
    /// Why the server cannot be reached right now, while it cannot.
    pub trouble: Option<String>,
    /// The server's clock minus this computer's, in milliseconds, from the
    /// quickest of the recent answers: see [`Jam::note_clock`].
    clock_offset_ms: i64,
    /// The recent answers' readings of that offset, newest last.
    clock_samples: std::collections::VecDeque<i64>,
    // Host.
    published: Option<Published>,
    /// The newest request the host has carried out.
    handled_seq: u64,
    /// When the host may publish what it just carried out.
    settle_until: Option<Instant>,
    /// The jam's songs handed to the host's queue, by id and URI, in order:
    /// the next one, and any handed over earlier that another device's
    /// queue could not take back.
    handed: Vec<(String, String)>,
    /// Songs the host asked the server to take out, until they are gone.
    removing: HashSet<String>,
    /// A song from the jam's queue to play as soon as it is handed over.
    play_now: Option<(String, Instant)>,
    /// The song the host played when it last looked.
    host_track: Option<String>,
    /// Songs whose details were asked of Spotify, because the jam brought
    /// only their ids.
    details_asked: HashSet<String>,
    /// Since when the host's player has shown no song.
    lost_since: Option<Instant>,
    // Listeners.
    hold: Option<Hold>,
    followed: Option<(String, Instant)>,
    toggled: Option<Instant>,
    sought: Option<Instant>,
    modes_matched: Option<Instant>,
    /// Since when this listener's play or pause has differed from the host's.
    toggle_since: Option<Instant>,
    /// Since when this listener has been too far from the host's place.
    drift_since: Option<Instant>,
    /// Since when this listener has played another song than the host's.
    track_since: Option<(String, Instant)>,
    /// Since when the host's player has shown "paused" (the host).
    paused_since: Option<Instant>,
}

struct Published {
    track: Option<JamTrack>,
    uri: Option<String>,
    context: Option<String>,
    playing: bool,
    position_ms: u32,
    applied_seq: u64,
    shuffle: bool,
    repeat: JamRepeat,
    next_up: Vec<JamTrack>,
    at: Instant,
}

/// A listener's own pause or seek, until the host has carried it out.
struct Hold {
    /// The server's number for the request, once it answered.
    seq: Option<u64>,
    at: Instant,
}

impl Jam {
    fn new(connection: wire::Connection, server: String, host: bool) -> Self {
        Self {
            connection,
            server,
            member_id: None,
            host,
            view: None,
            trouble: None,
            clock_offset_ms: 0,
            clock_samples: std::collections::VecDeque::new(),
            published: None,
            handled_seq: 0,
            settle_until: None,
            handed: Vec::new(),
            removing: HashSet::new(),
            play_now: None,
            host_track: None,
            details_asked: HashSet::new(),
            lost_since: None,
            hold: None,
            followed: None,
            toggled: None,
            sought: None,
            modes_matched: None,
            toggle_since: None,
            drift_since: None,
            track_since: None,
            paused_since: None,
        }
    }

    /// In the jam, not still being let in.
    pub fn joined(&self) -> bool {
        self.member_id.is_some() && self.view.is_some()
    }

    pub fn code(&self) -> Option<&str> {
        self.view.as_ref().map(|view| view.id.as_str())
    }

    /// The address to share: the server's invite page.
    pub fn invite(&self) -> Option<String> {
        Some(wire::invite_page(&self.server, self.code()?))
    }

    pub fn members(&self) -> &[Member] {
        self.view
            .as_ref()
            .map_or(&[], |view| view.members.as_slice())
    }

    pub fn queue(&self) -> &[QueueItem] {
        self.view.as_ref().map_or(&[], |view| view.queue.as_slice())
    }

    pub fn host_name(&self) -> Option<&str> {
        let view = self.view.as_ref()?;
        view.members
            .iter()
            .find(|member| member.id == view.host_id)
            .map(|member| member.name.as_str())
    }

    fn send(&self, request: JamRequest) {
        self.connection.send(request);
    }

    /// Notes one answer's reading of the server's clock. An answer reads
    /// the offset short by however long it travelled, and that changes from
    /// one answer to the next. The highest recent reading is the quickest
    /// answer's, the closest to true, and does not wobble with the network:
    /// a wobbling clock made a listener in step look ahead or behind.
    fn note_clock(&mut self, server_time_ms: u64, received_ms: u64) {
        const KEPT: usize = 32;
        self.clock_samples
            .push_back(server_time_ms as i64 - received_ms as i64);
        while self.clock_samples.len() > KEPT {
            self.clock_samples.pop_front();
        }
        self.clock_offset_ms = self.clock_samples.iter().copied().max().unwrap_or(0);
    }

    /// The server's clock now, once an answer has told it; until then, none.
    fn server_clock(&self) -> Option<u64> {
        (!self.clock_samples.is_empty()).then(|| self.server_now())
    }

    fn server_now(&self) -> u64 {
        (wire::unix_ms() as i64)
            .saturating_add(self.clock_offset_ms)
            .max(0) as u64
    }

    /// Where the host's song is now, by the server's clock.
    fn expected_position(&self, playback: &JamPlayback) -> u32 {
        let mut position = u64::from(playback.position_ms);
        if playback.playing {
            position += self.server_now().saturating_sub(playback.updated_at_ms);
        }
        let duration = playback
            .track
            .as_ref()
            .map_or(0, |track| u64::from(track.duration_ms));
        if duration > 0 {
            position = position.min(duration);
        }
        position.min(u64::from(u32::MAX)) as u32
    }

    /// Whether a listener's own pause or seek still holds the jam off.
    fn held(&mut self) -> bool {
        let applied = self
            .view
            .as_ref()
            .map_or(0, |view| view.playback.applied_seq);
        let Some(hold) = &self.hold else {
            return false;
        };
        let done = hold.at.elapsed() >= HOLD || hold.seq.is_some_and(|seq| applied >= seq);
        if done {
            self.hold = None;
        }
        !done
    }
}

impl App {
    fn jam_name(&self) -> String {
        self.user
            .as_ref()
            .map(|user| user.name().trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| gettext(self.locale, "Listener").into_owned())
    }

    /// Starts a jam, as its host, on the server typed into the devices
    /// popup, which is remembered for the next one.
    pub(crate) fn start_jam(&mut self) {
        let Some(server) = wire::normalize_server(&self.jam_server_text) else {
            self.jam_problem =
                Some(gettext(self.locale, "Enter the address of a jam server.").into_owned());
            return;
        };
        if self.settings.jam_server != server {
            self.settings.jam_server.clone_from(&server);
            self.mark_settings_dirty();
        }
        self.jam_server_text.clone_from(&server);
        self.jam_starting = false;
        self.leave_jam(false);
        let name = self.jam_name();
        self.open_jam(server, wire::Start::Create { name }, true);
    }

    /// Joins the jam an invite link names: the link carries its server.
    pub(crate) fn join_jam(&mut self, text: &str) {
        let Some(wire::Invite {
            server: Some(server),
            code,
        }) = wire::parse_invite(text)
        else {
            self.jam_problem = Some(
                gettext(
                    self.locale,
                    "That isn't a jam invite link. Paste the whole link.",
                )
                .into_owned(),
            );
            return;
        };
        if self
            .jam
            .as_ref()
            .is_some_and(|jam| jam.server == server && jam.code() == Some(code.as_str()))
        {
            return;
        }
        self.jam_join_text.clear();
        self.leave_jam(false);
        let name = self.jam_name();
        self.open_jam(server, wire::Start::Join { code, name }, false);
    }

    fn open_jam(&mut self, server: String, start: wire::Start, host: bool) {
        let wake = self.jam_waker.clone();
        // Tests talk to no server.
        #[cfg(test)]
        let connection = {
            let _ = (start, wake);
            wire::Connection::detached().0
        };
        #[cfg(not(test))]
        let connection = wire::Connection::spawn(
            server.clone(),
            start,
            self.applied_proxy.clone(),
            move || wake.wake(),
        );
        self.jam_problem = None;
        self.jam = Some(Jam::new(connection, server, host));
    }

    /// Leaves the jam; the host leaving ends it. The server is told in the
    /// background, so leaving never waits for it.
    pub(crate) fn leave_jam(&mut self, announce: bool) {
        let Some(jam) = self.jam.take() else {
            return;
        };
        jam.send(JamRequest::Leave);
        if announce && jam.joined() {
            self.toast(if jam.host {
                gettext(self.locale, "You ended the jam")
            } else {
                gettext(self.locale, "You left the jam")
            });
        }
    }

    pub(crate) fn remove_from_jam(&mut self, ids: Vec<String>) {
        if let Some(jam) = &self.jam {
            let ids: Vec<String> = ids
                .into_iter()
                .filter(|id| jam.queue().iter().any(|item| item.id == *id))
                .collect();
            if !ids.is_empty() {
                jam.send(JamRequest::Remove(ids));
            }
        }
    }

    pub(crate) fn clear_jam(&mut self) {
        let ids = self.jam.as_ref().map_or_else(Vec::new, |jam| {
            jam.queue().iter().map(|item| item.id.clone()).collect()
        });
        self.remove_from_jam(ids);
    }

    pub(crate) fn move_in_jam(&mut self, id: String, to: usize) {
        if let Some(jam) = &self.jam {
            jam.send(JamRequest::Move { id, to });
        }
    }

    /// Plays a song from the jam's queue now: the host's player skips to
    /// it, and a listener asks the host to.
    pub(crate) fn play_from_jam(&mut self, id: String) {
        let Some(jam) = &self.jam else {
            return;
        };
        if jam.host {
            self.play_from_jam_here(&id);
        } else {
            jam.send(JamRequest::Command(JamCommand::PlayQueued { id }));
        }
    }

    /// The host plays a jam song now: the songs before it are skipped, as a
    /// queue row is played (queue rule 5), and it plays once handed over.
    fn play_from_jam_here(&mut self, id: &str) {
        let Some(jam) = &mut self.jam else {
            return;
        };
        let Some(at) = jam.queue().iter().position(|item| item.id == id) else {
            return;
        };
        let skipped: Vec<String> = jam.queue()[..at]
            .iter()
            .map(|item| item.id.clone())
            .filter(|id| !jam.removing.contains(id))
            .collect();
        if !skipped.is_empty() {
            jam.removing.extend(skipped.iter().cloned());
            jam.send(JamRequest::Remove(skipped));
        }
        jam.play_now = Some((id.to_owned(), Instant::now()));
    }

    /// Reads what the jam server said and plays the part of host or
    /// listener. Runs every pass, before the actions it may queue apply.
    pub(crate) fn poll_jam(&mut self, ctx: &egui::Context) {
        let Some(jam) = &self.jam else {
            return;
        };
        for event in jam.connection.events() {
            self.jam_event(event, ctx);
            if self.jam.is_none() {
                return;
            }
        }
        let Some(jam) = &self.jam else {
            return;
        };
        if !jam.joined() {
            return;
        }
        if jam.host {
            self.host_jam();
        } else {
            self.follow_jam();
        }
        self.ask_jam_details();
        ctx.request_repaint_after(TICK);
    }

    fn jam_event(&mut self, event: JamEvent, ctx: &egui::Context) {
        let locale = self.locale;
        match event {
            JamEvent::Joined(joined) => {
                let Some(jam) = &mut self.jam else {
                    return;
                };
                jam.host = joined.session.host_id == joined.member_id;
                jam.member_id = Some(joined.member_id);
                // Requests from before this listener arrived are not its to
                // carry out.
                jam.handled_seq = joined
                    .session
                    .commands
                    .iter()
                    .map(|entry| entry.seq)
                    .max()
                    .unwrap_or(0);
                let host = jam.host;
                self.jam_event(
                    JamEvent::View(Box::new(joined.session), wire::unix_ms()),
                    ctx,
                );
                let Some(jam) = &self.jam else {
                    return;
                };
                if host {
                    if let Some(invite) = jam.invite() {
                        ctx.copy_text(invite);
                    }
                    self.toast(gettext(locale, "Jam started. Invite link copied"));
                } else {
                    let name = jam.host_name().unwrap_or_default().to_owned();
                    // Translators: {name} is the person hosting the jam.
                    self.toast(gettext(locale, "You joined {name}'s jam").replace("{name}", &name));
                }
            }
            JamEvent::View(view, received_ms) => {
                let Some(jam) = &mut self.jam else {
                    return;
                };
                // The sender's and the poller's answers can cross; a
                // smaller version tells the story from before.
                if jam
                    .view
                    .as_ref()
                    .is_some_and(|current| current.version > view.version)
                {
                    return;
                }
                jam.note_clock(view.server_time_ms, received_ms);
                jam.trouble = None;
                let gone = view.ended
                    || jam
                        .member_id
                        .as_ref()
                        .is_some_and(|me| !view.members.iter().any(|member| &member.id == me));
                let was_host = jam.host;
                jam.view = Some(*view);
                if gone {
                    self.jam = None;
                    if !was_host {
                        self.toast(gettext(locale, "The jam ended"));
                    }
                }
            }
            JamEvent::CommandSent(seq) => {
                if let Some(hold) = self.jam.as_mut().and_then(|jam| jam.hold.as_mut())
                    && hold.seq.is_none()
                {
                    hold.seq = Some(seq);
                }
            }
            JamEvent::Trouble(message) => {
                log::warn!("jam: server trouble: {message}");
                if let Some(jam) = &mut self.jam {
                    jam.trouble = Some(message);
                }
            }
            JamEvent::Failed(message) => {
                log::warn!("jam: couldn't start or join: {message}");
                self.jam = None;
                self.jam_problem = Some(message.clone());
                self.toast_error(
                    gettext(locale, "Couldn't reach the jam: {error}").replace("{error}", &message),
                );
            }
            JamEvent::Gone => {
                log::info!("jam: the jam ended or forgot this listener");
                self.jam = None;
                self.toast(gettext(locale, "The jam ended"));
            }
            JamEvent::Left => {}
        }
    }

    /// The host: carry out the others' requests, hand the jam's next song
    /// to this player's queue, and publish what plays.
    fn host_jam(&mut self) {
        let Some(jam) = &mut self.jam else {
            return;
        };
        let Some(view) = jam.view.clone() else {
            return;
        };
        let me = jam.member_id.clone().unwrap_or_default();

        // Requests, oldest first.
        let pending: Vec<JamCommand> = view
            .commands
            .iter()
            .filter(|entry| entry.seq > jam.handled_seq && entry.from != me)
            .map(|entry| entry.command.clone())
            .collect();
        if let Some(newest) = view.commands.iter().map(|entry| entry.seq).max() {
            jam.handled_seq = jam.handled_seq.max(newest);
        }
        if !pending.is_empty() {
            jam.settle_until = Some(Instant::now() + SETTLE);
        }
        for command in pending {
            self.carry_out(command);
        }

        self.feed_jam_queue(&view);
        let now = self.now_playing().filter(|now| !now.resuming);
        let next_up = self.host_next_up();
        self.publish_jam(now, next_up);
    }

    /// What this player's playlist or album plays after the queued songs:
    /// its Next up, for the others to see.
    fn host_next_up(&self) -> Vec<JamTrack> {
        let queued = self.queued_rows_len();
        self.queue.get().map_or_else(Vec::new, |queue| {
            queue
                .queue
                .iter()
                .skip(queued)
                .take(crate::jam::protocol::MAX_NEXT_UP)
                .map(Self::jam_item)
                .collect()
        })
    }

    /// Keeps the jam's next song, and only that one, in this player's
    /// queue, and takes songs off the jam's list as they start.
    fn feed_jam_queue(&mut self, view: &SessionView) {
        let now_uri = self
            .now_playing()
            .filter(|now| !now.resuming)
            .map(|now| now.uri);
        let reorderable = self.queue_locally_reorderable();
        let Some(jam) = &mut self.jam else {
            return;
        };
        let queued: HashSet<&str> = view.queue.iter().map(|item| item.id.as_str()).collect();
        jam.removing.retain(|id| queued.contains(id.as_str()));

        // A handed-over song that started has played, and any handed over
        // before it were skipped.
        let mut played = Vec::new();
        // A moment with no song is not a new one.
        if now_uri.is_some() && now_uri != jam.host_track {
            jam.host_track.clone_from(&now_uri);
            if let Some(uri) = &now_uri
                && let Some(at) = jam.handed.iter().position(|(_, held)| held == uri)
            {
                played.extend(jam.handed.drain(..=at).map(|(id, _)| id));
            }
        }
        played.retain(|id| queued.contains(id.as_str()) && !jam.removing.contains(id));
        if !played.is_empty() {
            jam.removing.extend(played.iter().cloned());
            jam.send(JamRequest::Remove(played));
        }

        let head = view
            .queue
            .iter()
            .find(|item| !jam.removing.contains(&item.id))
            .cloned();
        // Handed over but no longer next: moved, removed, or overtaken.
        // This computer's queue gives them back; another device's cannot,
        // so they stay to play there and leave the jam's list on starting.
        let stale: Vec<(String, String)> = if reorderable {
            let head_id = head.as_ref().map(|item| item.id.as_str());
            let (stale, kept) = std::mem::take(&mut jam.handed)
                .into_iter()
                .partition(|(id, _)| Some(id.as_str()) != head_id);
            jam.handed = kept;
            stale
        } else {
            Vec::new()
        };
        for (_, uri) in stale {
            if let Some(index) = self.queued_row_index(&uri) {
                self.remove_from_queue(vec![(index, uri)]);
            }
        }

        let Some(jam) = &mut self.jam else {
            return;
        };
        if let Some(item) = head.filter(|item| !jam.handed.iter().any(|(id, _)| *id == item.id)) {
            let label = if item.track.title.is_empty() {
                item.track.uri.clone()
            } else {
                item.track.title.clone()
            };
            if util::uri_kind(&item.track.uri) == Some("album") {
                // An album queues as its songs, which the jam cannot follow
                // one by one: it leaves the jam's list once handed over.
                jam.removing.insert(item.id.clone());
                jam.send(JamRequest::Remove(vec![item.id]));
                self.add_to_queue(item.track.uri, label);
            } else {
                jam.handed.push((item.id, item.track.uri.clone()));
                self.queue_one(item.track.uri, label, false);
            }
        }

        // A song asked for from the jam's queue plays once it is here.
        let Some(jam) = &mut self.jam else {
            return;
        };
        let Some((id, asked)) = jam.play_now.clone() else {
            return;
        };
        if asked.elapsed() >= PLAY_NOW_WAIT {
            jam.play_now = None;
            return;
        }
        let Some(uri) = jam
            .handed
            .iter()
            .find(|(handed, _)| *handed == id)
            .map(|(_, uri)| uri.clone())
        else {
            return;
        };
        if let Some(index) = self.queued_row_index(&uri) {
            if let Some(jam) = &mut self.jam {
                jam.play_now = None;
            }
            self.play_queue_item(index, uri);
        }
    }

    /// The row a song the user queued holds in the shown queue.
    fn queued_row_index(&self, uri: &str) -> Option<usize> {
        let queued = self.queued_rows_len();
        self.queue
            .get()?
            .queue
            .iter()
            .take(queued)
            .position(|item| item.uri() == uri)
    }

    /// Asks Spotify for the songs the jam shows without their names, as a
    /// host that cannot look them up sends them, once each. A real host
    /// sends names, covers and artists, so this rarely asks anything.
    fn ask_jam_details(&mut self) {
        /// The shown Next up songs looked up at most, so a long list never
        /// turns into a burst of requests.
        const NEXT_UP_LOOKED_UP: usize = 15;
        let Some(jam) = &self.jam else {
            return;
        };
        let Some(view) = &jam.view else {
            return;
        };
        let wanted: Vec<String> = view
            .playback
            .track
            .iter()
            .chain(view.queue.iter().map(|item| &item.track))
            .chain(view.playback.next_up.iter().take(NEXT_UP_LOOKED_UP))
            .filter(|track| track_unnamed(track))
            .filter_map(|track| {
                (util::uri_kind(&track.uri) == Some("track"))
                    .then(|| util::uri_id(&track.uri))
                    .flatten()
            })
            .filter(|id| !self.track_cache.contains_key(*id) && !jam.details_asked.contains(*id))
            .map(str::to_owned)
            .collect();
        for id in wanted {
            if let Some(jam) = &mut self.jam {
                jam.details_asked.insert(id.clone());
            }
            self.backend.api(ApiRequest::Track { id });
        }
    }

    fn carry_out(&mut self, command: JamCommand) {
        log::info!("jam: carrying out a listener's {command:?}");
        let playing = self.now_playing().is_some_and(|now| now.playing);
        match command {
            JamCommand::Play if !playing => self.toggle_play(),
            JamCommand::Pause if playing => self.toggle_play(),
            JamCommand::Play | JamCommand::Pause => {}
            JamCommand::Seek { position_ms } => self.seek(position_ms),
            JamCommand::Next => self.actions.push(Action::Next),
            JamCommand::Previous => self.actions.push(Action::Previous),
            JamCommand::PlayContext {
                context_uri,
                offset_uri,
                offset_index,
            } => self.actions.push(Action::PlayContext {
                uri: context_uri,
                offset_uri,
                offset_index,
            }),
            JamCommand::PlayUris { uris, index } => {
                self.actions.push(Action::PlayUris { uris, index });
            }
            JamCommand::PlayQueued { id } => self.play_from_jam_here(&id),
            JamCommand::Shuffle { on } => self.set_shuffle(on),
            JamCommand::Repeat { mode } => self.set_repeat(repeat_from_jam(mode)),
        }
    }

    fn publish_jam(&mut self, now: Option<NowPlaying>, next_up: Vec<JamTrack>) {
        let context = self.playing_context_uri();
        let shuffle = now.as_ref().map_or(self.shuffle_wanted, |now| now.shuffle);
        let repeat = repeat_to_jam(now.as_ref().map(|now| now.repeat).unwrap_or_default());
        let Some(jam) = &mut self.jam else {
            return;
        };
        if jam.settle_until.is_some_and(|until| Instant::now() < until) {
            return;
        }
        jam.settle_until = None;
        // No song for a moment is no news: wait for it to last.
        if now.is_some() {
            jam.lost_since = None;
        } else if jam
            .published
            .as_ref()
            .is_some_and(|published| published.track.is_some())
        {
            let since = *jam.lost_since.get_or_insert_with(Instant::now);
            if since.elapsed() < LOST_GRACE {
                return;
            }
        }
        let track = match &now {
            Some(now) => Some(JamTrack {
                uri: now.uri.clone(),
                title: now.title.clone(),
                artists: now.subtitle.clone(),
                duration_ms: now.duration_ms,
                image_url: now.art_small.clone().or_else(|| now.art_url.clone()),
            }),
            // This player lost its song, as it does while its connection
            // to Spotify drops: the jam keeps the song, paused where it
            // was, rather than telling everyone nothing plays.
            None => jam
                .published
                .as_ref()
                .and_then(|published| published.track.clone()),
        };
        let uri = track.as_ref().map(|track| track.uri.clone());
        let reported = now.as_ref().is_some_and(|now| now.playing);
        // A pause reaches the jam once it lasts: the player reports one for
        // a moment while a song loads or a seek lands. A pause a listener
        // asked for has already happened on their player.
        if reported {
            jam.paused_since = None;
        }
        let was_playing = jam
            .published
            .as_ref()
            .is_some_and(|published| published.playing);
        let playing = reported
            || (now.is_some()
                && was_playing
                && jam.paused_since.get_or_insert_with(Instant::now).elapsed() < CONFIRM);
        let position_ms = match (&now, &jam.published) {
            (Some(now), _) => now.position_ms,
            (None, Some(published)) if published.playing => {
                let elapsed = published.at.elapsed().as_millis();
                let at = u128::from(published.position_ms) + elapsed;
                let duration = track.as_ref().map_or(0, |track| track.duration_ms);
                let at = if duration > 0 {
                    at.min(u128::from(duration))
                } else {
                    at
                };
                u32::try_from(at).unwrap_or(u32::MAX)
            }
            (None, Some(published)) => published.position_ms,
            (None, None) => 0,
        };
        // A player that lost its song keeps the jam's Next up too.
        let next_up = if now.is_none() && next_up.is_empty() {
            jam.published
                .as_ref()
                .map_or_else(Vec::new, |published| published.next_up.clone())
        } else {
            next_up
        };
        let due = match &jam.published {
            None => true,
            Some(published) if published.at.elapsed() < PUBLISH_GAP => false,
            Some(published) => {
                let expected = if published.playing {
                    i64::from(published.position_ms) + published.at.elapsed().as_millis() as i64
                } else {
                    i64::from(published.position_ms)
                };
                published.uri != uri
                    || published.context != context
                    || published.playing != playing
                    || published.shuffle != shuffle
                    || published.repeat != repeat
                    || published.next_up != next_up
                    || published.applied_seq != jam.handled_seq
                    || (i64::from(position_ms) - expected).abs() > SEEK_JUMP_MS
                    || (playing && published.at.elapsed() >= HEARTBEAT)
            }
        };
        if !due {
            return;
        }
        log::info!(
            "jam: publishing {} {} at {} ms",
            if playing { "playing" } else { "paused" },
            uri.as_deref().unwrap_or("nothing"),
            position_ms
        );
        jam.send(JamRequest::Publish(PublishPlayback {
            // When the position was read, by the server's clock: stamped on
            // arrival instead, it would be late by the upload and any wait
            // behind another request, and the host would look behind.
            measured_at_ms: jam.server_clock(),
            next_up: next_up.clone(),
            track: track.clone(),
            context_uri: context.clone(),
            position_ms,
            playing,
            applied_seq: jam.handled_seq,
            shuffle,
            repeat,
        }));
        jam.published = Some(Published {
            track,
            uri,
            context,
            playing,
            position_ms,
            applied_seq: jam.handled_seq,
            shuffle,
            repeat,
            next_up,
            at: Instant::now(),
        });
    }

    /// A listener: play what the host plays, where the host plays it.
    fn follow_jam(&mut self) {
        let Some(jam) = &mut self.jam else {
            return;
        };
        if jam.held() {
            return;
        }
        let Some(playback) = jam.view.as_ref().map(|view| view.playback.clone()) else {
            return;
        };
        let expected = jam.expected_position(&playback);
        let toggle_due = jam.toggled.is_none_or(|at| at.elapsed() >= TOGGLE_GAP);
        let seek_due = jam.sought.is_none_or(|at| at.elapsed() >= SEEK_GAP);
        let modes_due = jam.modes_matched.is_none_or(|at| at.elapsed() >= MODES_GAP);
        let now = self.now_playing().filter(|now| !now.resuming);
        let local_playing = now.as_ref().is_some_and(|now| now.playing);

        // The host's shuffle and repeat, so this player's next song is the
        // host's next song too.
        if modes_due {
            let shuffle = now.as_ref().map_or(self.shuffle_wanted, |now| now.shuffle);
            let repeat = repeat_to_jam(now.as_ref().map(|now| now.repeat).unwrap_or_default());
            if shuffle != playback.shuffle || repeat != playback.repeat {
                if let Some(jam) = &mut self.jam {
                    jam.modes_matched = Some(Instant::now());
                }
                if shuffle != playback.shuffle {
                    self.set_shuffle(playback.shuffle);
                }
                if repeat != playback.repeat {
                    self.set_repeat(repeat_from_jam(playback.repeat));
                }
            }
        }

        let Some(track) = playback.track.clone() else {
            // The host plays nothing; neither should this listener.
            if local_playing && toggle_due && self.jam_confirm_toggle() {
                self.jam_toggle();
            }
            return;
        };
        match now {
            Some(now) if now.uri == track.uri => {
                if let Some(jam) = &mut self.jam {
                    jam.track_since = None;
                }
                if now.loading {
                    return;
                }
                if now.playing != playback.playing {
                    if toggle_due && self.jam_confirm_toggle() {
                        self.jam_toggle();
                    }
                    return;
                }
                if let Some(jam) = &mut self.jam {
                    jam.toggle_since = None;
                }
                let drifted = playback.playing && now.position_ms.abs_diff(expected) > DRIFT_MS;
                let Some(jam) = &mut self.jam else {
                    return;
                };
                if !drifted {
                    jam.drift_since = None;
                    return;
                }
                let since = *jam.drift_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= DRIFT_CONFIRM && seek_due {
                    jam.sought = Some(Instant::now());
                    jam.drift_since = None;
                    let target = expected.saturating_add(LOAD_LEAD_MS / 2);
                    log::info!(
                        "jam: {} ms from the host, seeking to {target} ms",
                        now.position_ms.abs_diff(expected)
                    );
                    self.seek(if track.duration_ms > 0 {
                        target.min(track.duration_ms)
                    } else {
                        target
                    });
                }
            }
            _ => {
                if !playback.playing {
                    // A paused host is waited for rather than started
                    // here only to stop: this listener's own song pauses.
                    if local_playing && toggle_due && self.jam_confirm_toggle() {
                        self.jam_toggle();
                    }
                    return;
                }
                if let Some(jam) = &mut self.jam {
                    jam.toggle_since = None;
                }
                let near_end = track.duration_ms > 0
                    && expected.saturating_add(BOUNDARY_MS) >= track.duration_ms;
                if near_end && now.is_some() {
                    return;
                }
                // Another song playing here must stay another for a moment:
                // a player reports its last song until the next one loads.
                if let Some(now) = now.as_ref().filter(|now| now.playing) {
                    let Some(jam) = &mut self.jam else {
                        return;
                    };
                    let since = match &jam.track_since {
                        Some((uri, at)) if *uri == track.uri => *at,
                        _ => {
                            jam.track_since = Some((track.uri.clone(), Instant::now()));
                            return;
                        }
                    };
                    if since.elapsed() < CONFIRM {
                        return;
                    }
                    log::info!("jam: playing {} here, not the host's song", now.uri);
                }
                if self.jam.as_ref().is_some_and(|jam| {
                    jam.followed
                        .as_ref()
                        .is_some_and(|(uri, at)| *uri == track.uri && at.elapsed() < FOLLOW_RETRY)
                }) {
                    return;
                }
                // From the host's playlist or album when it plays from one,
                // so the next song here is the host's next song too.
                let mut request = match playback.context_uri.as_deref().filter(|context| {
                    matches!(util::uri_kind(context), Some("playlist" | "album" | "show"))
                }) {
                    Some(context) => PlayRequest::context(context).starting_at_uri(&track.uri),
                    None => PlayRequest::tracks(vec![track.uri.clone()]),
                };
                let start = expected.saturating_add(LOAD_LEAD_MS);
                request.position_ms = if track.duration_ms > 0 {
                    start.min(track.duration_ms.saturating_sub(1_000))
                } else {
                    start
                };
                log::info!(
                    "jam: following the host to {} at {} ms (from {}; playing here: {})",
                    track.uri,
                    request.position_ms,
                    request.context_uri.as_deref().unwrap_or("the song alone"),
                    now.as_ref().map_or("nothing", |now| now.uri.as_str())
                );
                self.play_request(request, false);
                if let Some(jam) = &mut self.jam {
                    jam.followed = Some((track.uri, Instant::now()));
                    jam.track_since = None;
                }
            }
        }
    }

    /// Whether this listener's play or pause has differed from the host's
    /// long enough to correct, rather than for the moment a player takes.
    fn jam_confirm_toggle(&mut self) -> bool {
        let Some(jam) = &mut self.jam else {
            return false;
        };
        let since = *jam.toggle_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= CONFIRM {
            jam.toggle_since = None;
            true
        } else {
            false
        }
    }

    fn jam_toggle(&mut self) {
        log::info!("jam: matching the host's play or pause");
        if let Some(jam) = &mut self.jam {
            jam.toggled = Some(Instant::now());
        }
        self.toggle_play();
    }

    /// A jam song from what the app knows of `uri`, or just its label.
    fn jam_track(&self, uri: &str, label: &str) -> JamTrack {
        let cached = util::uri_id(uri).and_then(|id| self.track_cache.get(id));
        JamTrack {
            uri: uri.to_owned(),
            title: cached.map_or_else(|| label.to_owned(), |track| track.name.clone()),
            artists: cached
                .map(|track| {
                    track
                        .artists
                        .iter()
                        .map(|artist| artist.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default(),
            duration_ms: cached.map_or(0, |track| track.duration_ms),
            image_url: cached
                .and_then(|track| track.album.as_ref())
                .and_then(|album| crate::api::models::pick_image(&album.images, 64))
                .map(str::to_owned),
        }
    }

    fn jam_item(item: &PlayableItem) -> JamTrack {
        JamTrack {
            uri: item.uri().to_owned(),
            title: item.name().to_owned(),
            artists: item.subtitle(),
            duration_ms: item.duration_ms(),
            image_url: item.image(64).map(str::to_owned),
        }
    }

    /// Songs for the jam's queue, at `position` or at its end.
    fn add_to_jam(&mut self, tracks: Vec<JamTrack>, position: Option<usize>) {
        let Some(jam) = &self.jam else {
            return;
        };
        let tracks: Vec<JamTrack> = tracks
            .into_iter()
            .filter(|track| !track.uri.starts_with("spotify:local:"))
            .collect();
        let Some(first) = tracks.first() else {
            return;
        };
        let message = if tracks.len() == 1 {
            // Translators: {name} is a song, episode or album name.
            gettext(self.locale, "{name} added to the jam").replace("{name}", &first.title)
        } else {
            // Translators: {count} is how many songs were added.
            gettext(self.locale, "{count} songs added to the jam")
                .replace("{count}", &tracks.len().to_string())
        };
        jam.send(JamRequest::Add(tracks, position));
        self.toast(message);
    }

    /// What an action becomes in a jam: `None` when the jam took it over,
    /// otherwise the action to apply as usual.
    pub(crate) fn jam_intercept(&mut self, action: Action) -> Option<Action> {
        let Some(jam) = &self.jam else {
            return Some(action);
        };
        if !jam.joined() {
            return Some(action);
        }
        let host = jam.host;
        // Songs anyone queues go to the jam's queue, which the host's
        // player then plays.
        match action {
            Action::AddToQueue { uri, label } => {
                let track = self.jam_track(&uri, &label);
                self.add_to_jam(vec![track], None);
                None
            }
            Action::QueueMany { songs } => {
                let tracks = songs
                    .iter()
                    .map(|(uri, label)| self.jam_track(uri, label))
                    .collect();
                self.add_to_jam(tracks, None);
                None
            }
            // Play next puts songs first in the jam's queue, for anyone.
            Action::PlayNext { items } => {
                self.add_to_jam(items.iter().map(Self::jam_item).collect(), Some(0));
                None
            }
            // A drop lands among this player's own rows, not the jam's.
            Action::InsertInQueue { items, .. } => {
                self.add_to_jam(items.iter().map(Self::jam_item).collect(), None);
                None
            }
            Action::Quit => {
                self.leave_jam(false);
                Some(Action::Quit)
            }
            Action::ClearQueue => {
                // Clearing what plays next, in a jam, clears the jam's queue.
                self.clear_jam();
                if let Some(jam) = &mut self.jam {
                    jam.handed.clear();
                }
                Some(Action::ClearQueue)
            }
            action if host => Some(action),
            action => {
                // A listener's controls are requests to the host.
                let Some(command) = self.jam_command(&action) else {
                    return Some(action);
                };
                let acts_here = matches!(
                    command,
                    JamCommand::Play
                        | JamCommand::Pause
                        | JamCommand::Seek { .. }
                        | JamCommand::Shuffle { .. }
                        | JamCommand::Repeat { .. }
                );
                log::info!("jam: asking the host for {command:?}");
                if let Some(jam) = &mut self.jam {
                    jam.send(JamRequest::Command(command));
                    if acts_here {
                        jam.hold = Some(Hold {
                            seq: None,
                            at: Instant::now(),
                        });
                    }
                }
                // Pause, seek, shuffle and repeat also act on this player
                // at once; the rest wait for the host's player to change
                // what plays.
                acts_here.then_some(action)
            }
        }
    }

    /// The request a listener's control makes of the host, if it makes one.
    fn jam_command(&self, action: &Action) -> Option<JamCommand> {
        Some(match action {
            Action::TogglePlay => {
                if self.now_playing().is_some_and(|now| now.playing) {
                    JamCommand::Pause
                } else {
                    JamCommand::Play
                }
            }
            Action::Seek(position_ms) => JamCommand::Seek {
                position_ms: *position_ms,
            },
            Action::SeekBy(offset) => {
                let now = self.now_playing()?;
                JamCommand::Seek {
                    position_ms: (i64::from(now.position_ms) + offset)
                        .clamp(0, i64::from(now.duration_ms))
                        as u32,
                }
            }
            Action::Next => JamCommand::Next,
            Action::Previous => JamCommand::Previous,
            Action::ToggleShuffle => JamCommand::Shuffle {
                on: !self
                    .now_playing()
                    .map_or(self.shuffle_wanted, |now| now.shuffle),
            },
            Action::SetShuffle(on) => JamCommand::Shuffle { on: *on },
            Action::CycleRepeat => JamCommand::Repeat {
                mode: repeat_to_jam(
                    self.now_playing()
                        .map(|now| now.repeat)
                        .unwrap_or_default()
                        .next(),
                ),
            },
            Action::SetRepeat(mode) => JamCommand::Repeat {
                mode: repeat_to_jam(*mode),
            },
            Action::PlayContext {
                uri,
                offset_uri,
                offset_index,
            } => JamCommand::PlayContext {
                context_uri: uri.clone(),
                offset_uri: offset_uri.clone(),
                offset_index: *offset_index,
            },
            Action::ShufflePlay(uri) => JamCommand::PlayContext {
                context_uri: uri.clone(),
                offset_uri: None,
                offset_index: None,
            },
            Action::PlayUris { uris, index } => jam_uris(uris, *index)?,
            Action::PlayEpisode { uri, .. } => JamCommand::PlayUris {
                uris: vec![uri.clone()],
                index: 0,
            },
            Action::PlayFromRow {
                context,
                uri,
                index,
            } => match context {
                RowContext::Context {
                    uri: context_uri, ..
                } => JamCommand::PlayContext {
                    context_uri: context_uri.clone(),
                    offset_uri: Some(uri.clone()),
                    offset_index: None,
                },
                RowContext::Uris(uris) | RowContext::View { uris, .. } => {
                    let index = uris
                        .iter()
                        .position(|held| held == uri)
                        .map_or(*index, |at| at as u32);
                    jam_uris(uris, index)?
                }
                RowContext::Queue => JamCommand::PlayUris {
                    uris: vec![uri.clone()],
                    index: 0,
                },
            },
            _ => return None,
        })
    }
}

/// Whether a jam's song came without its name: none, or only its URI or id.
pub(crate) fn track_unnamed(track: &JamTrack) -> bool {
    let title = track.title.trim();
    title.is_empty() || title == track.uri || Some(title) == util::uri_id(&track.uri)
}

fn repeat_to_jam(mode: RepeatMode) -> JamRepeat {
    match mode {
        RepeatMode::Off => JamRepeat::Off,
        RepeatMode::Context => JamRepeat::Context,
        RepeatMode::Track => JamRepeat::Track,
    }
}

fn repeat_from_jam(mode: JamRepeat) -> RepeatMode {
    match mode {
        JamRepeat::Off => RepeatMode::Off,
        JamRepeat::Context => RepeatMode::Context,
        JamRepeat::Track => RepeatMode::Track,
    }
}

/// Up to the jam's limit of songs around `index`, as a request to play
/// them from there. Local files are left out: they play only here.
fn jam_uris(uris: &[String], index: u32) -> Option<JamCommand> {
    let start_uri = uris.get(index as usize)?;
    if start_uri.starts_with("spotify:local:") {
        return None;
    }
    let playable: Vec<&String> = uris
        .iter()
        .filter(|uri| !uri.starts_with("spotify:local:"))
        .collect();
    let at = playable.iter().position(|uri| *uri == start_uri)?;
    let max = crate::jam::protocol::MAX_ADD;
    let first = at
        .saturating_sub(max / 2)
        .min(playable.len().saturating_sub(max));
    let window: Vec<String> = playable
        .iter()
        .skip(first)
        .take(max)
        .map(|uri| (*uri).clone())
        .collect();
    Some(JamCommand::PlayUris {
        index: (at - first) as u32,
        uris: window,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_list_plays_from_the_chosen_song_within_the_jam_limit() {
        let uris: Vec<String> = (0..300).map(|n| format!("spotify:track:t{n}")).collect();
        let Some(JamCommand::PlayUris {
            uris: window,
            index,
        }) = jam_uris(&uris, 250)
        else {
            panic!("a request");
        };
        assert_eq!(window.len(), crate::jam::protocol::MAX_ADD);
        assert_eq!(window[index as usize], "spotify:track:t250");
        let Some(JamCommand::PlayUris {
            uris: window,
            index,
        }) = jam_uris(&uris, 3)
        else {
            panic!("a request");
        };
        assert_eq!(window[index as usize], "spotify:track:t3");
        assert_eq!(index, 3);
    }

    #[test]
    fn local_files_are_not_asked_of_the_host() {
        let uris = vec![
            "spotify:local:a:b:c:1".to_owned(),
            "spotify:track:x".to_owned(),
        ];
        assert_eq!(jam_uris(&uris, 0), None);
        let Some(JamCommand::PlayUris {
            uris: window,
            index,
        }) = jam_uris(&uris, 1)
        else {
            panic!("a request");
        };
        assert_eq!(window, vec!["spotify:track:x".to_owned()]);
        assert_eq!(index, 0);
    }
}

#[cfg(test)]
mod app_tests {
    use std::sync::mpsc;

    use super::*;
    use crate::jam::protocol::{CommandEntry, Playback as JamPlayback};

    const ME: &str = "m-me";
    const OTHER: &str = "m-other";

    fn track(uri: &str) -> JamTrack {
        JamTrack {
            uri: uri.into(),
            title: uri.rsplit(':').next().unwrap_or_default().into(),
            artists: String::new(),
            duration_ms: 200_000,
            image_url: None,
        }
    }

    fn view(host: bool) -> SessionView {
        let host_id = if host { ME } else { OTHER };
        SessionView {
            id: "ABC234".into(),
            version: 5,
            server_time_ms: wire::unix_ms(),
            host_id: host_id.into(),
            members: [ME, OTHER]
                .into_iter()
                .map(|id| Member {
                    id: id.into(),
                    name: id.into(),
                    host: id == host_id,
                })
                .collect(),
            playback: JamPlayback {
                track: Some(track("spotify:track:playing")),
                context_uri: None,
                position_ms: 0,
                playing: true,
                updated_at_ms: wire::unix_ms(),
                applied_seq: 0,
                shuffle: false,
                repeat: JamRepeat::Off,
                next_up: Vec::new(),
            },
            queue: Vec::new(),
            commands: Vec::new(),
            ended: false,
        }
    }

    /// A local player playing `spotify:track:playing`, in a jam.
    fn in_jam(host: bool) -> (App, mpsc::Receiver<JamRequest>) {
        let mut app = crate::app::tests::app_with_local_manual_queue();
        app.backend.set_offline(true);
        app.local.connected = true;
        let (connection, requests, _events) = wire::Connection::detached();
        let mut jam = Jam::new(connection, "https://jam.example.com".into(), host);
        jam.member_id = Some(ME.into());
        jam.view = Some(view(host));
        app.jam = Some(jam);
        (app, requests)
    }

    fn set_view(app: &mut App, change: impl FnOnce(&mut SessionView)) {
        let jam = app.jam.as_mut().unwrap();
        let view = jam.view.as_mut().unwrap();
        view.version += 1;
        change(view);
    }

    #[test]
    fn songs_anyone_queues_go_to_the_jam_not_this_queue() {
        let ctx = egui::Context::default();
        for host in [false, true] {
            let (mut app, requests) = in_jam(host);
            let before = app.manual_queue.clone();
            app.apply(
                Action::AddToQueue {
                    uri: "spotify:track:new".into(),
                    label: "New".into(),
                },
                &ctx,
            );
            assert_eq!(app.manual_queue, before, "host: {host}");
            let sent: Vec<_> = requests.try_iter().collect();
            assert!(
                matches!(sent.as_slice(), [JamRequest::Add(tracks, None)]
                    if tracks[0].uri == "spotify:track:new" && tracks[0].title == "New"),
                "{sent:?}"
            );
        }
    }

    #[test]
    fn a_listener_pause_acts_here_and_is_not_undone_while_the_host_catches_up() {
        let ctx = egui::Context::default();
        let (mut app, requests) = in_jam(false);
        app.backend.take_player_commands();
        app.apply(Action::TogglePlay, &ctx);
        assert_eq!(
            requests.try_iter().collect::<Vec<_>>(),
            vec![JamRequest::Command(JamCommand::Pause)]
        );
        assert_eq!(
            app.backend.take_player_commands(),
            vec![PlayerCommand::Toggle]
        );
        // The jam still says playing: the host has not carried it out yet.
        app.local.playback = Playback::Paused;
        app.optimistic_playing = None;
        app.jam_event(JamEvent::CommandSent(7), &ctx);
        app.follow_jam();
        assert!(app.backend.take_player_commands().is_empty());
        // Once the host reports it, the jam's answer is the one to follow.
        set_view(&mut app, |view| {
            view.playback.playing = false;
            view.playback.applied_seq = 7;
        });
        app.follow_jam();
        assert!(app.backend.take_player_commands().is_empty());
    }

    #[test]
    fn a_listener_next_is_a_request_to_the_host_only() {
        let ctx = egui::Context::default();
        let (mut app, requests) = in_jam(false);
        let before = app.manual_queue.clone();
        app.backend.take_player_commands();
        app.apply(Action::Next, &ctx);
        assert_eq!(
            requests.try_iter().collect::<Vec<_>>(),
            vec![JamRequest::Command(JamCommand::Next)]
        );
        assert_eq!(app.manual_queue, before);
        assert!(app.backend.take_player_commands().is_empty());
    }

    #[test]
    fn a_listener_starts_the_host_song_where_the_host_is() {
        let (mut app, _requests) = in_jam(false);
        app.backend.take_player_commands();
        set_view(&mut app, |view| {
            view.playback.track = Some(track("spotify:track:hosts"));
            view.playback.position_ms = 60_000;
        });
        // Another song playing here is given a moment: a player reports its
        // last song until the next one loads.
        app.follow_jam();
        assert!(app.backend.take_player_commands().is_empty());
        if let Some(jam) = &mut app.jam {
            jam.track_since = Some(("spotify:track:hosts".into(), Instant::now() - CONFIRM));
        }
        app.follow_jam();
        let commands = format!("{:?}", app.backend.take_player_commands());
        assert!(commands.contains("spotify:track:hosts"), "{commands}");
        // Asked once, not again every pass while it loads.
        app.follow_jam();
        assert!(app.backend.take_player_commands().is_empty());
    }

    fn item(id: &str, uri: &str) -> QueueItem {
        QueueItem {
            id: id.into(),
            track: track(uri),
            added_by: OTHER.into(),
            added_by_name: "Other".into(),
        }
    }

    fn removals(requests: &mpsc::Receiver<JamRequest>) -> Vec<Vec<String>> {
        requests
            .try_iter()
            .filter_map(|request| match request {
                JamRequest::Remove(ids) => Some(ids),
                _ => None,
            })
            .collect()
    }

    /// A listener starts each song a little ahead of the host, so its player
    /// moves on to the next song first. It must not go back to the host's
    /// song for its last seconds, and then forward again.
    #[test]
    fn a_listener_that_moved_on_first_waits_for_the_host() {
        let (mut app, _requests) = in_jam(false);
        app.backend.take_player_commands();
        set_view(&mut app, |view| {
            view.playback.track = Some(track("spotify:track:hosts"));
            // 200 s long, two seconds left by the host's clock.
            view.playback.position_ms = 198_000;
        });
        if let Some(jam) = &mut app.jam {
            jam.track_since = Some(("spotify:track:hosts".into(), Instant::now() - CONFIRM));
        }
        app.follow_jam();
        assert!(app.backend.take_player_commands().is_empty());
    }

    #[test]
    fn a_listener_ignores_a_moment_of_pause_and_corrects_one_that_lasts() {
        let (mut app, _requests) = in_jam(false);
        app.backend.take_player_commands();
        // The host's song is playing; this player shows it paused.
        app.local.playback = Playback::Paused;
        app.follow_jam();
        assert!(app.backend.take_player_commands().is_empty());
        if let Some(jam) = &mut app.jam {
            jam.toggle_since = Some(Instant::now() - CONFIRM);
        }
        app.follow_jam();
        assert_eq!(
            app.backend.take_player_commands(),
            vec![PlayerCommand::Toggle]
        );
    }

    #[test]
    fn a_listener_seeks_only_when_far_off_for_a_while() {
        let (mut app, _requests) = in_jam(false);
        app.local.connected = true;
        app.backend.take_player_commands();
        // Ten seconds behind the host on the same song.
        set_view(&mut app, |view| view.playback.position_ms = 10_000);
        app.follow_jam();
        assert!(app.backend.take_player_commands().is_empty());
        if let Some(jam) = &mut app.jam {
            jam.drift_since = Some(Instant::now() - DRIFT_CONFIRM);
        }
        app.follow_jam();
        assert!(matches!(
            app.backend.take_player_commands().as_slice(),
            [PlayerCommand::Seek(_)]
        ));
    }

    #[test]
    fn the_host_publishes_a_pause_only_once_it_lasts() {
        let (mut app, requests) = in_jam(true);
        app.host_jam();
        requests.try_iter().for_each(drop);
        app.local.playback = Playback::Paused;
        if let Some(jam) = &mut app.jam
            && let Some(published) = &mut jam.published
        {
            published.at = Instant::now() - PUBLISH_GAP;
        }
        app.host_jam();
        let paused = |requests: &mpsc::Receiver<JamRequest>| {
            requests.try_iter().any(
                |request| matches!(request, JamRequest::Publish(playback) if !playback.playing),
            )
        };
        assert!(!paused(&requests), "a moment of pause is not published");
        if let Some(jam) = &mut app.jam {
            jam.paused_since = Some(Instant::now() - CONFIRM);
        }
        app.host_jam();
        assert!(paused(&requests), "a lasting pause is");
    }

    #[test]
    fn the_host_carries_out_each_request_once() {
        let (mut app, _requests) = in_jam(true);
        set_view(&mut app, |view| {
            view.commands.push(CommandEntry {
                seq: 1,
                from: OTHER.into(),
                from_name: "Other".into(),
                command: JamCommand::Next,
            });
        });
        app.host_jam();
        assert!(matches!(app.actions.as_slice(), [Action::Next]));
        app.actions.clear();
        app.host_jam();
        assert!(app.actions.is_empty());
    }

    #[test]
    fn the_host_is_handed_only_the_jam_next_song() {
        let (mut app, _requests) = in_jam(true);
        let before = app.manual_queue.len();
        set_view(&mut app, |view| {
            view.queue = vec![
                item("q1", "spotify:track:first"),
                item("q2", "spotify:track:second"),
            ];
        });
        app.host_jam();
        app.host_jam();
        assert_eq!(app.manual_queue.len(), before + 1);
        assert_eq!(
            app.manual_queue.last().map(String::as_str),
            Some("spotify:track:first")
        );
    }

    #[test]
    fn a_reordered_jam_queue_hands_over_its_new_next_song_instead() {
        let (mut app, _requests) = in_jam(true);
        set_view(&mut app, |view| {
            view.queue = vec![
                item("q1", "spotify:track:first"),
                item("q2", "spotify:track:second"),
            ];
        });
        app.host_jam();
        // Someone moves the second song to the top.
        set_view(&mut app, |view| view.queue.swap(0, 1));
        app.host_jam();
        assert!(
            app.manual_queue
                .contains(&"spotify:track:second".to_owned())
        );
        assert!(
            !app.manual_queue.contains(&"spotify:track:first".to_owned()),
            "{:?}",
            app.manual_queue
        );
    }

    #[test]
    fn a_started_jam_song_leaves_the_list_and_the_next_follows() {
        let (mut app, requests) = in_jam(true);
        set_view(&mut app, |view| {
            view.queue = vec![
                item("q1", "spotify:track:first"),
                item("q2", "spotify:track:second"),
            ];
        });
        app.host_jam();
        app.local.track = Some(crate::player::LocalTrack {
            uri: "spotify:track:first".into(),
            ..Default::default()
        });
        app.host_jam();
        assert!(removals(&requests).contains(&vec!["q1".to_owned()]));
        // The server takes it out; the next song is handed over.
        set_view(&mut app, |view| {
            view.queue.remove(0);
        });
        app.host_jam();
        assert!(
            app.manual_queue
                .contains(&"spotify:track:second".to_owned())
        );
    }

    #[test]
    fn playing_a_jam_song_now_skips_those_before_it() {
        let (mut app, requests) = in_jam(true);
        set_view(&mut app, |view| {
            view.queue = vec![
                item("q1", "spotify:track:first"),
                item("q2", "spotify:track:second"),
            ];
        });
        app.host_jam();
        app.backend.take_player_commands();
        app.play_from_jam("q2".into());
        app.host_jam();
        assert!(removals(&requests).contains(&vec!["q1".to_owned()]));
        assert!(
            app.backend
                .take_player_commands()
                .contains(&PlayerCommand::Next),
            "the player skips to the chosen song"
        );
    }

    #[test]
    fn play_next_puts_songs_first_in_the_jam_for_anyone() {
        let ctx = egui::Context::default();
        for host in [false, true] {
            let (mut app, requests) = in_jam(host);
            app.apply(
                Action::PlayNext {
                    items: vec![PlayableItem::Track(crate::api::models::Track {
                        uri: "spotify:track:soon".into(),
                        name: "Soon".into(),
                        ..Default::default()
                    })],
                },
                &ctx,
            );
            let sent: Vec<_> = requests.try_iter().collect();
            assert!(
                matches!(sent.as_slice(), [JamRequest::Add(tracks, Some(0))]
                    if tracks[0].uri == "spotify:track:soon"),
                "{sent:?}"
            );
        }
    }

    #[test]
    fn a_listener_shuffle_acts_here_asks_the_host_and_then_follows_it() {
        let ctx = egui::Context::default();
        let (mut app, requests) = in_jam(false);
        app.apply(Action::SetShuffle(true), &ctx);
        assert_eq!(
            requests.try_iter().collect::<Vec<_>>(),
            vec![JamRequest::Command(JamCommand::Shuffle { on: true })]
        );
        assert!(app.shuffle_wanted);
        // The host turned repeat on and the request has been carried out.
        app.jam_event(JamEvent::CommandSent(3), &ctx);
        set_view(&mut app, |view| {
            view.playback.shuffle = true;
            view.playback.repeat = JamRepeat::Track;
            view.playback.applied_seq = 3;
        });
        app.follow_jam();
        assert_eq!(
            app.now_playing().map(|now| now.repeat),
            Some(RepeatMode::Track)
        );
    }

    #[test]
    fn a_host_that_loses_its_song_pauses_the_jam_on_it() {
        let (mut app, requests) = in_jam(true);
        app.host_jam();
        requests.try_iter().for_each(drop);
        // The connection drops: this player no longer reports a song.
        app.local.track = None;
        app.local.playback = Playback::Paused;
        if let Some(jam) = &mut app.jam
            && let Some(published) = &mut jam.published
        {
            published.at = Instant::now() - PUBLISH_GAP;
        }
        // A moment with no song, as during a seek, changes nothing.
        app.host_jam();
        assert!(
            !requests
                .try_iter()
                .any(|request| matches!(request, JamRequest::Publish(_))),
            "a brief gap is not published"
        );
        // Gone for longer, the jam pauses on the song it had.
        if let Some(jam) = &mut app.jam {
            jam.lost_since = Some(Instant::now() - LOST_GRACE);
        }
        app.host_jam();
        let sent: Vec<_> = requests.try_iter().collect();
        assert!(
            sent.iter().any(|request| matches!(request, JamRequest::Publish(playback)
                if !playback.playing
                    && playback.track.as_ref().is_some_and(|track| track.uri == "spotify:track:playing"))),
            "{sent:?}"
        );
        assert!(
            sent.iter()
                .all(|request| !matches!(request, JamRequest::Publish(playback) if playback.track.is_none())),
            "the jam is never told nothing plays: {sent:?}"
        );
    }

    #[test]
    fn the_clock_keeps_the_quickest_answer_and_ignores_the_network_wobble() {
        let (connection, _requests, _events) = wire::Connection::detached();
        let mut jam = Jam::new(connection, "https://jam.example.com".into(), false);
        assert_eq!(jam.server_clock(), None);
        // The server's clock is 1000 ms ahead; answers take 40, 300 and 120 ms.
        for travel in [40, 300, 120] {
            jam.note_clock(10_000 + 1_000, 10_000 + travel);
        }
        assert_eq!(jam.clock_offset_ms, 1_000 - 40);
        assert!(jam.server_clock().is_some());
    }

    #[test]
    fn songs_the_jam_brings_unnamed_are_looked_up_once() {
        let (mut app, _requests) = in_jam(false);
        let unnamed = |uri: &str| JamTrack {
            uri: uri.into(),
            title: uri.rsplit(':').next().unwrap_or_default().into(),
            ..JamTrack::default()
        };
        set_view(&mut app, |view| {
            view.playback.track = Some(unnamed("spotify:track:abc"));
            view.playback.next_up = vec![JamTrack {
                title: "A Named Song".into(),
                ..track("spotify:track:named")
            }];
        });
        app.ask_jam_details();
        app.ask_jam_details();
        let asked = &app.jam.as_ref().unwrap().details_asked;
        assert_eq!(asked.len(), 1);
        assert!(asked.contains("abc"));
    }

    #[test]
    fn a_jam_that_ended_is_left_with_a_notice() {
        let ctx = egui::Context::default();
        let (mut app, _requests) = in_jam(false);
        let mut ended = view(false);
        ended.version = 99;
        ended.ended = true;
        app.jam_event(JamEvent::View(Box::new(ended), wire::unix_ms()), &ctx);
        assert!(app.jam.is_none());
    }

    #[test]
    fn an_older_answer_never_replaces_a_newer_one() {
        let ctx = egui::Context::default();
        let (mut app, _requests) = in_jam(false);
        let mut older = view(false);
        older.version = 1;
        older.playback.playing = false;
        app.jam_event(JamEvent::View(Box::new(older), wire::unix_ms()), &ctx);
        assert!(
            app.jam
                .as_ref()
                .unwrap()
                .view
                .as_ref()
                .unwrap()
                .playback
                .playing
        );
    }

    #[test]
    fn an_opened_invite_link_joins_its_jam() {
        let ctx = egui::Context::default();
        let mut app = crate::app::tests::app_with_local_manual_queue();
        app.backend.set_offline(true);
        let link = "spotifast-jam://join?server=https%3A%2F%2Fjam.example.com&session=ABC234";
        app.apply(Action::OpenLink(link.into()), &ctx);
        let jam = app.jam.as_ref().expect("joining");
        assert_eq!(jam.server, "https://jam.example.com");
        assert!(!jam.host);
    }

    #[test]
    fn a_bare_code_is_not_enough_to_join() {
        let ctx = egui::Context::default();
        let mut app = crate::app::tests::app_with_local_manual_queue();
        app.apply(Action::JoinJam("ABC234".into()), &ctx);
        assert!(app.jam.is_none());
        assert!(app.jam_problem.is_some());
    }

    #[test]
    fn starting_a_jam_remembers_its_server() {
        let ctx = egui::Context::default();
        let mut app = crate::app::tests::app_with_local_manual_queue();
        app.jam_server_text = " jam.example.com/ ".into();
        app.jam_starting = true;
        app.apply(Action::StartJam, &ctx);
        assert_eq!(app.settings.jam_server, "https://jam.example.com");
        assert!(!app.jam_starting);
        assert!(app.jam.as_ref().is_some_and(|jam| jam.host));
        app.leave_jam(false);
    }
}
