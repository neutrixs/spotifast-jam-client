//! The messages a jam client and the jam server send each other, as JSON.
//!
//! They match `src/protocol.rs` in spotifast-jam-server exactly. Every field
//! is always sent; a message missing one is invalid. Only a client with the
//! server's [`PROTOCOL`] can start or join a jam, so both sides always agree.

use serde::{Deserialize, Serialize};

/// This client's protocol version. Change it, here and in the server,
/// whenever a message changes.
pub const PROTOCOL: u32 = 2;

/// A song. Only `uri` decides what plays; the rest is for showing it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub uri: String,
    pub title: String,
    pub artists: String,
    pub duration_ms: u32,
    pub image_url: String,
}

/// The host's repeat setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    Off,
    /// Repeat the playlist or album.
    Context,
    /// Repeat the current song.
    Track,
}

/// What the host's player is doing. Only the host writes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Playback {
    /// The song playing now; `None` when the host plays nothing.
    #[serde(deserialize_with = "Option::deserialize")]
    pub track: Option<Track>,
    /// Where in the song the host was at `updated_at_ms`.
    pub position_ms: u32,
    pub playing: bool,
    /// The server's clock (Unix milliseconds) when `position_ms` was true.
    pub updated_at_ms: u64,
    /// The newest command (by `seq`) the host had carried out.
    pub applied_seq: u64,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// The songs the host's player plays after the jam's queue runs out.
    pub next_up: Vec<Track>,
}

/// Someone in the jam.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub host: bool,
}

/// A song someone added to the jam's queue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    /// The server's id for this entry, so the same song added twice is two
    /// entries.
    pub id: String,
    pub track: Track,
    /// The member id of whoever added it.
    pub added_by: String,
    pub added_by_name: String,
}

/// Something a member asks the host's player to do, as if the host had
/// pressed it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Play,
    Pause,
    Seek {
        position_ms: u32,
    },
    Next,
    Previous,
    /// Play a list of songs from the one at `index`.
    PlayUris {
        uris: Vec<String>,
        index: u32,
    },
    /// Play a song from the jam's queue now, skipping the ones before it.
    PlayQueued {
        id: String,
    },
    Shuffle {
        on: bool,
    },
    Repeat {
        mode: Repeat,
    },
}

/// A command as the server keeps it for the host to read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandEntry {
    /// Counts up across the jam; the host carries out each one once.
    pub seq: u64,
    pub from: String,
    pub from_name: String,
    pub command: Command,
}

/// The whole jam as the server sends it, in every answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionView {
    /// The jam's code.
    pub id: String,
    /// Goes up with every change.
    pub version: u64,
    /// The server's clock (Unix milliseconds) when it answered.
    pub server_time_ms: u64,
    pub host_id: String,
    pub members: Vec<Member>,
    pub playback: Playback,
    pub queue: Vec<QueueItem>,
    /// The newest commands; older ones fall off.
    pub commands: Vec<CommandEntry>,
    /// The host ended the jam or went away.
    pub ended: bool,
}

/// Starting a jam (`POST /api/v1/sessions`) or joining one
/// (`POST /api/v1/sessions/{code}/join`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JoinRequest {
    pub name: String,
    /// [`PROTOCOL`]; the server refuses any other.
    pub protocol: u32,
}

/// The answer to starting or joining.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JoinResponse {
    pub session: SessionView,
    pub member_id: String,
    /// Sent as `Authorization: Bearer <token>` with every later request.
    pub token: String,
}

/// The host telling the server what its player does
/// (`PUT /api/v1/sessions/{code}/playback`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublishPlayback {
    #[serde(deserialize_with = "Option::deserialize")]
    pub track: Option<Track>,
    pub position_ms: u32,
    pub playing: bool,
    pub applied_seq: u64,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub next_up: Vec<Track>,
    /// When `position_ms` was true, by the host's estimate of the server's
    /// clock (Unix milliseconds).
    #[serde(deserialize_with = "Option::deserialize")]
    pub measured_at_ms: Option<u64>,
}

/// `POST /api/v1/sessions/{code}/queue`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AddToQueue {
    pub tracks: Vec<Track>,
    /// Where in the queue they go: 0 is the top; `None` is the end.
    #[serde(deserialize_with = "Option::deserialize")]
    pub position: Option<usize>,
}

/// `POST /api/v1/sessions/{code}/queue/move`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MoveInQueue {
    pub id: String,
    /// The entry's place once moved: 0 is the top.
    pub to: usize,
}

/// `POST /api/v1/sessions/{code}/queue/remove`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoveFromQueue {
    pub ids: Vec<String>,
}

/// `POST /api/v1/sessions/{code}/commands`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SendCommand {
    pub command: Command,
}

/// The answer to a command: its number, and the jam with it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandAccepted {
    pub seq: u64,
    pub session: SessionView,
}

/// What the server sends with an error status.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A jam with every field, as the server sends it.
    const JAM: &str = r#"{
        "id": "ABC123", "version": 7, "server_time_ms": 1000,
        "host_id": "m1",
        "members": [{"id": "m1", "name": "Host", "host": true}],
        "playback": {
            "track": {"uri": "spotify:track:a", "title": "A", "artists": "X",
                      "duration_ms": 200000, "image_url": "https://i.scdn.co/image/a"},
            "position_ms": 500, "playing": true, "updated_at_ms": 900,
            "applied_seq": 2, "shuffle": false, "repeat": "context", "next_up": []
        },
        "queue": [],
        "commands": [{"seq": 3, "from": "m2", "from_name": "Guest",
                      "command": {"type": "seek", "position_ms": 4000}}],
        "ended": false
    }"#;

    #[test]
    fn reads_a_jam_with_every_field() {
        let view: SessionView = serde_json::from_str(JAM).unwrap();
        assert_eq!(view.playback.repeat, Repeat::Context);
        assert_eq!(
            view.playback.track.unwrap().image_url,
            "https://i.scdn.co/image/a"
        );
        assert_eq!(
            view.commands[0].command,
            Command::Seek { position_ms: 4000 }
        );
    }

    /// A missing field is an error, never a made-up value: an empty list,
    /// a `null` cover or a `null` song included.
    #[test]
    fn refuses_a_jam_missing_any_field() {
        for field in [
            r#", "next_up": []"#,
            r#", "image_url": "https://i.scdn.co/image/a""#,
            r#""track": {"uri": "spotify:track:a", "title": "A", "artists": "X",
                      "duration_ms": 200000, "image_url": "https://i.scdn.co/image/a"},"#,
        ] {
            assert!(JAM.contains(field), "{field}");
            let broken = JAM.replace(field, "");
            // Still JSON: only the field is gone.
            serde_json::from_str::<serde_json::Value>(&broken).unwrap();
            assert!(
                serde_json::from_str::<SessionView>(&broken).is_err(),
                "{broken}"
            );
        }
    }

    /// Commands go out the way the server reads them.
    #[test]
    fn writes_commands_as_the_server_reads_them() {
        let json = serde_json::to_string(&SendCommand {
            command: Command::Repeat {
                mode: Repeat::Track,
            },
        })
        .unwrap();
        assert_eq!(json, r#"{"command":{"type":"repeat","mode":"track"}}"#);
    }
}
