//! Jams in the interface: the Jam section of the devices popup, where a jam
//! starts, is joined and is left; the player bar's sign that one is on;
//! and the queue panel, which shows the jam's queue while in one.

use std::sync::Arc;

use crate::api::models::{Album, ArtistRef, Image, PlayableItem, Track};
use crate::app::App;
use crate::i18n::gettext;
use crate::model::{Action, RowContext};
use crate::theme::{self, Icon, Palette};

use super::widgets::{self, TrackRow};

/// Where the devices popup keeps the Jam section's height from the last
/// frame, so its device list leaves the section room in the window.
pub const SECTION_HEIGHT_ID: &str = "jam-section-height";
/// The section's height before it has been drawn, about its tallest.
pub const SECTION_HEIGHT_GUESS: f32 = 150.0;

/// The Jam section at the bottom of the devices popup.
pub fn devices_section(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let locale = app.locale;
    let top = ui.cursor().top();
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        theme::text(ui, gettext(locale, "Jam"), theme::bold(16.0), palette.text);
    });
    ui.add_space(4.0);
    ui.indent("jam-section", |ui| {
        match app.jam.as_ref().map(|jam| jam.joined()) {
            Some(true) => current_jam(app, ui, &palette),
            Some(false) => {
                ui.horizontal(|ui| {
                    theme::spinner(ui, 14.0, palette.accent);
                    theme::subtle(ui, &palette, &gettext(locale, "Connecting to the jam…"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::link(
                            ui,
                            gettext(locale, "Cancel"),
                            theme::regular(12.5),
                            palette.secondary,
                        )
                        .clicked()
                        {
                            app.actions.push(Action::LeaveJam);
                        }
                    });
                });
            }
            None if app.jam_starting => start_form(app, ui, &palette),
            None => {
                if theme::pill_button(ui, &palette, &gettext(locale, "Start a jam"), true).clicked()
                {
                    app.jam_starting = true;
                    app.jam_problem = None;
                }
                ui.add_space(8.0);
                join_form(app, ui, &palette);
            }
        }
        if let Some(problem) = app.jam_problem.clone() {
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(problem)
                        .font(theme::regular(12.5))
                        .color(palette.danger),
                )
                .wrap(),
            );
        }
    });
    ui.add_space(4.0);
    // With the gap the popup leaves before the section.
    let height = ui.cursor().top() - top + ui.spacing().item_spacing.y;
    ui.ctx()
        .data_mut(|data| data.insert_temp(egui::Id::new(SECTION_HEIGHT_ID), height));
}

/// A text field in the popup's style, filling the row but `reserve`.
fn field(
    app: &mut App,
    ui: &mut egui::Ui,
    palette: &Palette,
    text: impl FnOnce(&mut App) -> &mut String,
    hint: &str,
    reserve: f32,
) -> egui::Response {
    let locale = app.locale;
    let width = (ui.available_width() - reserve).max(100.0);
    let edit = egui::TextEdit::singleline(text(app))
        .hint_text(egui::RichText::new(hint).color(palette.dim))
        .font(theme::regular(13.0))
        .desired_width(width);
    widgets::text_edit(ui, locale, edit)
}

fn submitted(ui: &egui::Ui, response: &egui::Response) -> bool {
    response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
}

/// Which server to start the jam on, filled in with the last one used.
fn start_form(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    let locale = app.locale;
    theme::text(
        ui,
        gettext(locale, "Jam server"),
        theme::medium(13.0),
        palette.text,
    );
    ui.add_space(4.0);
    let response = field(
        app,
        ui,
        palette,
        |app| &mut app.jam_server_text,
        "https://jam.example.com",
        0.0,
    );
    if response.changed() {
        app.jam_problem = None;
    }
    // Asked for, so it is ready to type into.
    if app.jam_server_text.is_empty() && !response.has_focus() {
        response.request_focus();
    }
    let entered = submitted(ui, &response);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let ready = !app.jam_server_text.trim().is_empty();
        if (theme::pill_button(ui, palette, &gettext(locale, "Start"), true).clicked() || entered)
            && ready
        {
            app.actions.push(Action::StartJam);
        }
        if theme::soft_button(ui, palette, None, &gettext(locale, "Cancel"), false).clicked() {
            app.jam_starting = false;
            app.jam_problem = None;
        }
    });
}

/// Pasting an invite link joins its jam: the link names its server.
fn join_form(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    let locale = app.locale;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let join = gettext(locale, "Join");
        let reserve = theme::soft_button_width(ui, &join) + 6.0;
        let response = field(
            app,
            ui,
            palette,
            |app| &mut app.jam_join_text,
            &gettext(locale, "Paste an invite link"),
            reserve,
        );
        if response.changed() {
            app.jam_problem = None;
        }
        let entered = submitted(ui, &response);
        let ready = !app.jam_join_text.trim().is_empty();
        if (theme::soft_button(ui, palette, None, &join, false).clicked() || entered) && ready {
            app.actions.push(Action::JoinJam(app.jam_join_text.clone()));
        }
    });
}

fn current_jam(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    let locale = app.locale;
    let Some(jam) = &app.jam else {
        return;
    };
    let host = jam.host;
    let invite = jam.invite();
    let code = jam.code().unwrap_or_default().to_owned();
    let role = if host {
        gettext(locale, "you're hosting").into_owned()
    } else {
        // Translators: {name} is the person hosting the jam.
        gettext(locale, "hosted by {name}").replace("{name}", jam.host_name().unwrap_or_default())
    };
    let names = jam
        .members()
        .iter()
        .map(|member| member.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let trouble = jam.trouble.is_some();
    // Translators: {code} is the jam's six-letter code; {role} says who hosts it.
    let title = gettext(locale, "Jam {code} · {role}")
        .replace("{code}", &code)
        .replace("{role}", &role);
    theme::text(ui, title, theme::semibold(13.5), palette.text);
    ui.add(
        egui::Label::new(
            egui::RichText::new(names)
                .font(theme::regular(12.5))
                .color(palette.secondary),
        )
        .wrap(),
    );
    if trouble {
        theme::text(
            ui,
            gettext(locale, "Reconnecting to the jam server…"),
            theme::regular(12.5),
            palette.warning,
        );
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if let Some(invite) = invite
            && theme::pill_button(ui, palette, &gettext(locale, "Copy invite link"), true).clicked()
        {
            app.actions.push(Action::CopyText(invite));
        }
        let leave = if host {
            gettext(locale, "End jam")
        } else {
            gettext(locale, "Leave jam")
        };
        if theme::soft_button(ui, palette, None, &leave, false).clicked() {
            app.actions.push(Action::LeaveJam);
        }
    });
}

/// Beside the player bar's devices button while in a jam, so a jam is never
/// forgotten: "Jam · 3", which opens the devices popup like the button.
pub fn player_bar_status(app: &mut App, ui: &mut egui::Ui) {
    let Some(jam) = app.jam.as_ref().filter(|jam| jam.joined()) else {
        return;
    };
    let palette = app.palette;
    // Translators: {count} is how many people are in the jam.
    let label =
        gettext(app.locale, "Jam · {count}").replace("{count}", &jam.members().len().to_string());
    let color = if jam.trouble.is_some() {
        palette.warning
    } else {
        palette.accent
    };
    if theme::link(ui, label, theme::semibold(12.5), color)
        .on_hover_text(gettext(app.locale, "You're in a jam"))
        .clicked()
    {
        app.actions.push(Action::ToggleDevicesPopup);
    }
}

/// A jam's song as the app's own rows draw it: the app's own details for
/// it when it has them, with its cover and artist links, otherwise what the
/// jam brought.
fn song(app: &App, track: &crate::jam::protocol::Track) -> PlayableItem {
    if let Some(known) = crate::util::uri_id(&track.uri)
        .filter(|_| crate::util::uri_kind(&track.uri) == Some("track"))
        .and_then(|id| app.track_cache.get(id))
    {
        return PlayableItem::Track(known.clone());
    }
    let unnamed = crate::app::jam::track_unnamed(track);
    PlayableItem::Track(Track {
        id: crate::util::uri_id(&track.uri).map(str::to_owned),
        // Until Spotify answers, a song without a name is said to be loading
        // rather than shown as its URI.
        name: if unnamed {
            gettext(app.locale, "Loading…").into_owned()
        } else {
            track.title.clone()
        },
        uri: track.uri.clone(),
        duration_ms: track.duration_ms,
        artists: (!track.artists.is_empty())
            .then(|| ArtistRef {
                name: track.artists.clone(),
                ..ArtistRef::default()
            })
            .into_iter()
            .collect(),
        album: track.image_url.as_ref().map(|url| Album {
            images: vec![Image {
                url: url.clone(),
                ..Image::default()
            }],
            ..Album::default()
        }),
        ..Track::default()
    })
}

/// A song row in the queue's style, for a song that is not the queue's own.
fn song_row(
    app: &mut App,
    ui: &mut egui::Ui,
    item: &PlayableItem,
    index: usize,
    number: usize,
    context: &RowContext,
    compact: bool,
) {
    widgets::track_row(
        ui,
        app,
        TrackRow {
            index,
            number: Some(number),
            item,
            context,
            show_cover: true,
            show_album: false,
            added_at: None,
            added_by: None,
            show_added_by: false,
            show_plays: false,
            compact,
            thin: false,
            shift: 0.0,
            picked: false,
            picked_songs: &[],
            picked_positions: &[],
            position: None,
        },
    );
}

const SECTION_GAP: f32 = 14.0;

/// The queue panel while in a jam, the same for everyone in it: the host's
/// song, the jam's queue, which anyone plays, reorders and clears, and what
/// the host's playlist or album plays after it. This player's own queue is
/// not what the jam plays, so it is not shown. False when not in a jam.
pub fn queue_view(app: &mut App, ui: &mut egui::Ui, compact: bool) -> bool {
    let Some(jam) = app.jam.as_ref().filter(|jam| jam.joined()) else {
        return false;
    };
    let palette = app.palette;
    let locale = app.locale;
    let code = jam.code().unwrap_or_default().to_owned();
    let host = jam.host;
    let count = jam.members().len();
    let trouble = jam.trouble.is_some();
    let host_name = jam.host_name().unwrap_or_default().to_owned();
    let items: Vec<_> = jam.queue().to_vec();
    let playback = jam
        .view
        .as_ref()
        .map(|view| view.playback.clone())
        .unwrap_or_default();
    let mut actions = Vec::new();

    theme::text(
        ui,
        gettext(locale, "Jam {code}").replace("{code}", &code),
        theme::semibold(14.0),
        palette.text,
    );
    let status = if host {
        // Translators: {count} is how many people are in the jam.
        gettext(locale, "You're hosting. {count} listening.").replace("{count}", &count.to_string())
    } else {
        // Translators: {name} hosts the jam; {count} is how many are in it.
        gettext(locale, "Following {name}. {count} listening.")
            .replace("{name}", &host_name)
            .replace("{count}", &count.to_string())
    };
    theme::text(ui, status, theme::regular(12.5), palette.secondary);
    if trouble {
        theme::text(
            ui,
            gettext(locale, "Reconnecting to the jam server…"),
            theme::regular(12.5),
            palette.warning,
        );
    }
    ui.add_space(SECTION_GAP);

    let mut number = 1;
    if let Some(track) = &playback.track {
        theme::text(
            ui,
            gettext(locale, "Now playing"),
            theme::semibold(14.0),
            palette.text,
        );
        ui.add_space(4.0);
        let item = song(app, track);
        let context = RowContext::Uris(Arc::from([track.uri.clone()]));
        song_row(app, ui, &item, 0, number, &context, compact);
        number += 1;
        ui.add_space(SECTION_GAP);
    }

    ui.horizontal(|ui| {
        theme::text(
            ui,
            gettext(locale, "Queue"),
            theme::semibold(14.0),
            palette.text,
        );
        if !items.is_empty() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::link(
                    ui,
                    gettext(locale, "Clear"),
                    theme::regular(12.5),
                    palette.dim,
                )
                .clicked()
                {
                    actions.push(Action::ClearJam);
                }
            });
        }
    });
    ui.add_space(4.0);
    if items.is_empty() {
        theme::text(
            ui,
            gettext(locale, "Songs anyone adds to the queue play here in order."),
            theme::regular(12.5),
            palette.dim,
        );
    }
    let last = items.len().saturating_sub(1);
    for (index, item) in items.iter().enumerate() {
        jam_queue_row(ui, app, item, index, last, number, &mut actions);
        number += 1;
    }
    ui.add_space(SECTION_GAP);

    if !playback.next_up.is_empty() {
        theme::text(
            ui,
            gettext(
                locale,
                // Translators: Upcoming songs from the current playlist or album, after manually queued songs.
                "Next up",
            ),
            theme::semibold(14.0),
            palette.text,
        );
        ui.add_space(4.0);
        // Played from the host's playlist or album at the chosen song, so
        // the songs after it follow as they would have.
        let context = match playback.context_uri.as_deref().filter(|context| {
            matches!(
                crate::util::uri_kind(context),
                Some("playlist" | "album" | "show")
            )
        }) {
            Some(context) => RowContext::Context {
                uri: context.to_owned(),
                editable_playlist: None,
            },
            None => RowContext::Uris(
                playback
                    .next_up
                    .iter()
                    .map(|track| track.uri.clone())
                    .collect::<Vec<_>>()
                    .into(),
            ),
        };
        for (index, track) in playback.next_up.iter().enumerate() {
            let item = song(app, track);
            song_row(app, ui, &item, index, number, &context, compact);
            number += 1;
        }
    }
    app.actions.extend(actions);
    true
}

/// A row of the jam's queue: its song, who added it, and the controls
/// everyone in the jam has.
fn jam_queue_row(
    ui: &mut egui::Ui,
    app: &App,
    item: &crate::jam::protocol::QueueItem,
    index: usize,
    last: usize,
    number: usize,
    actions: &mut Vec<Action>,
) {
    let palette = app.palette;
    let locale = app.locale;
    // The same details as the other rows: the app's own once it knows the
    // song, and "Loading…" rather than an id until then.
    let known = song(app, &item.track);
    let title = known.name().to_owned();
    let artists = known.subtitle();
    let cover = known
        .image(crate::api::models::SMALL_ART_WIDTH)
        .map(str::to_owned);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        theme::text(ui, number.to_string(), theme::regular(12.5), palette.dim);
        widgets::cover(ui, &palette, cover.as_deref(), 40.0, 4.0, Icon::Music);
        let controls = 4.0 * 24.0;
        let row = ui
            .vertical(|ui| {
                ui.set_width((ui.available_width() - controls).max(60.0));
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(title)
                            .font(theme::medium(13.5))
                            .color(palette.text),
                    )
                    .truncate(),
                );
                // Translators: {name} added the song to the jam.
                let added =
                    gettext(locale, "Added by {name}").replace("{name}", &item.added_by_name);
                let line = if artists.is_empty() {
                    added
                } else {
                    format!("{artists} · {added}")
                };
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(line)
                            .font(theme::regular(12.0))
                            .color(palette.secondary),
                    )
                    .truncate(),
                );
            })
            .response;
        if row.interact(egui::Sense::click()).double_clicked() {
            actions.push(Action::PlayFromJam(item.id.clone()));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let button = |ui: &mut egui::Ui, icon, tooltip: &str| {
                theme::icon_button(ui, icon, 14.0, palette.dim, palette.text, tooltip)
            };
            if button(ui, Icon::X, &gettext(locale, "Remove from the jam")).clicked() {
                actions.push(Action::RemoveFromJam(item.id.clone()));
            }
            let down = ui.add_enabled_ui(index < last, |ui| {
                button(ui, Icon::ChevronDown, &gettext(locale, "Move down"))
            });
            if down.inner.clicked() {
                actions.push(Action::MoveInJam {
                    id: item.id.clone(),
                    to: index + 1,
                });
            }
            let up = ui.add_enabled_ui(index > 0, |ui| {
                button(ui, Icon::ChevronUp, &gettext(locale, "Move up"))
            });
            if up.inner.clicked() {
                actions.push(Action::MoveInJam {
                    id: item.id.clone(),
                    to: index - 1,
                });
            }
            if button(ui, Icon::Play, &gettext(locale, "Play now")).clicked() {
                actions.push(Action::PlayFromJam(item.id.clone()));
            }
        });
    });
    ui.add_space(6.0);
}
