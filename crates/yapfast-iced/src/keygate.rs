//! The key that unlocks this build of Yapfast, when it was built with a key
//! server (`YAPFAST_KEYS_URL`; keys/README.md runs one): asked for before
//! anything else shows, kept in the OS credential store so it's typed once,
//! checked online as Yapfast opens and every minute after, and the window
//! locked, saying why with Try again, while the key isn't valid, was
//! revoked, or the server can't be reached. The account (the saved session,
//! or the preview) starts only once a key was valid; a locked window leaves
//! the call and shows and plays no alerts. My Account shows the key and
//! makes the invite it may make: a code that is a key of its own, for
//! someone else. Built without a key server, Yapfast opens as ever. No DRM
//! and no hardware ID: the key and the server's answer are all there is.

use std::{sync::OnceLock, time::Duration};

use iced::{
    Alignment, Color, Element, Length, Padding, Task,
    widget::{Space, column, container, operation, row, text},
};
use serde::Deserialize;
use yapfast_core::application::Action;

use crate::{
    app::{App, Message},
    controls::{Kind, action, field},
    fonts::BOLD,
    look::Look,
    pictures::Icon,
    style::{card_shadow, fill, rounded, scrollbar},
    theme::Theme,
    widgets::scroll_view,
};

/// The key server this build checks keys with, as it was built.
const SERVER: Option<&str> = option_env!("YAPFAST_KEYS_URL");
/// How often the key is checked again while Yapfast runs.
pub const EVERY: Duration = Duration::from_secs(60);
/// A check that couldn't reach the server is tried once more this much
/// later before the window locks: the network may come up just after
/// Windows does, or blink.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// Where the key is kept in the OS credential store, beside the Discord
/// token.
const SERVICE: &str = "io.github.yapfast.Yapfast";
const ACCOUNT: &str = "license-key";
/// The key's field, for focus.
pub const FIELD: &str = "keygate-key";
const UNREACHABLE: &str =
    "The key server can't be reached. Check your connection, or try again in a moment.";

/// The key server's address, when this build has one.
pub fn server() -> Option<&'static str> {
    SERVER.map(str::trim).filter(|url| !url.is_empty())
}

/// Whether `url` may carry keys: HTTPS, or this computer's own address,
/// which a test build's server answers on.
fn allowed(url: &str) -> bool {
    url.starts_with("https://")
        || ["http://127.0.0.1", "http://localhost", "http://[::1]"]
            .iter()
            .any(|own| {
                url.strip_prefix(own)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/']))
            })
}

/// What the window shows of the gate.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// No key server in this build: Yapfast, as ever.
    Off,
    /// The key's field, with why the last key typed didn't open it.
    Asking { error: Option<String> },
    /// The kept key being checked as Yapfast opens.
    Checking,
    /// The key was valid when last checked.
    Open,
    /// Locked, and why.
    Locked { why: String },
}

/// The invites a key may make, as the server counts them.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Invites {
    #[serde(default)]
    pub quota: u32,
    /// Made and taken up as someone's key.
    #[serde(default)]
    pub used: u32,
    /// Made and not yet used: a new code replaces it.
    #[serde(default)]
    pub pending: u32,
    #[serde(default)]
    pub left: u32,
}

/// What the server said of a key.
#[derive(Clone, Debug)]
pub enum Verdict {
    Valid(Invites),
    Invalid,
    Revoked,
}

/// An invite just made: its code, and the invites left.
#[derive(Clone, Debug)]
pub struct Invited {
    pub code: String,
    pub invites: Invites,
}

#[derive(Clone, Debug)]
pub enum Event {
    Typed(String),
    Submit,
    /// A check's answer, for `key`, typed in the field or not.
    Checked {
        key: String,
        typed: bool,
        result: Result<Verdict, String>,
    },
    /// The minute's check.
    Tick,
    Retry,
    UseAnother,
    /// My Account's invite.
    Invite,
    Invited(Result<Invited, String>),
    CopyInvite,
}

pub struct Gate {
    pub phase: Phase,
    /// The key's field as typed.
    pub typed: String,
    /// The key in use: the one kept, or one typed and found valid.
    key: Option<String>,
    /// A check under way, and whether it's for the key typed.
    checking: Option<bool>,
    /// When the key was last found valid, on the clock, and its invites.
    pub checked_at: Option<String>,
    pub invites: Option<Invites>,
    /// The invite code made last, an invite asked for, or why one failed.
    pub invite: Option<String>,
    pub inviting: bool,
    pub invite_error: Option<String>,
    /// A failed check is tried again once before the window locks.
    second_try: bool,
    /// What starts once a key is valid the first time: the saved session
    /// or the preview, held back from the start.
    pub held: Option<Action>,
    opened: bool,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            phase: if server().is_some() {
                Phase::Checking
            } else {
                Phase::Off
            },
            typed: String::new(),
            key: None,
            checking: None,
            checked_at: None,
            invites: None,
            invite: None,
            inviting: false,
            invite_error: None,
            second_try: false,
            held: None,
            opened: false,
        }
    }
}

impl Gate {
    /// Whether the window shows Yapfast: no key server, or a valid key.
    pub fn open(&self) -> bool {
        matches!(self.phase, Phase::Off | Phase::Open)
    }

    /// How often the key is checked again, while there's one to check and
    /// no other being typed.
    pub fn every(&self) -> Option<Duration> {
        let asking = matches!(self.phase, Phase::Off | Phase::Asking { .. });
        (!asking && self.key.is_some()).then_some(EVERY)
    }

    /// The key in use with all but its last group hidden.
    pub fn masked(&self) -> Option<String> {
        let key = self.key.as_deref()?;
        let plain: String = key
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_uppercase();
        let last: String = plain.chars().skip(plain.len().saturating_sub(5)).collect();
        Some(if plain.len() == 27 && plain.starts_with("YF") {
            format!(
                "YF-\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}-\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}-\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}-\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}-{last}"
            )
        } else {
            format!("\u{2022}\u{2022}\u{2022}\u{2022}{last}")
        })
    }
}

/// Starts the gate as the window boots: the kept key is checked, or the
/// field asked for; with no key server, what was held starts at once.
pub fn start(app: &mut App) -> Task<Message> {
    if server().is_none() {
        app.gate.phase = Phase::Off;
        return release(app);
    }
    match kept() {
        Some(key) => {
            app.gate.key = Some(key.clone());
            app.gate.phase = Phase::Checking;
            ask(&mut app.gate, key, false)
        }
        None => {
            app.gate.phase = Phase::Asking { error: None };
            Task::batch([show(app), operation::focus(FIELD)])
        }
    }
}

pub fn update(app: &mut App, event: Event) -> Task<Message> {
    match event {
        Event::Typed(typed) => {
            app.gate.typed = typed;
            if let Phase::Asking { error } = &mut app.gate.phase {
                *error = None;
            }
            Task::none()
        }
        Event::Submit => {
            let key = app.gate.typed.trim().to_owned();
            if key.is_empty() || app.gate.checking.is_some() {
                return Task::none();
            }
            ask(&mut app.gate, key, true)
        }
        Event::Checked { key, typed, result } => checked(app, key, typed, result),
        // While another key is typed, the one before isn't checked.
        Event::Tick if matches!(app.gate.phase, Phase::Asking { .. }) => Task::none(),
        Event::Tick | Event::Retry => match app.gate.key.clone() {
            Some(key) if app.gate.checking.is_none() => ask(&mut app.gate, key, false),
            _ => Task::none(),
        },
        Event::UseAnother => {
            app.gate.phase = Phase::Asking { error: None };
            app.gate.typed.clear();
            operation::focus(FIELD)
        }
        Event::Invite => match app.gate.key.clone() {
            Some(key) if !app.gate.inviting => {
                app.gate.inviting = true;
                app.gate.invite_error = None;
                Task::perform(make_invite(key), |result| {
                    Message::Gate(Event::Invited(result))
                })
            }
            _ => Task::none(),
        },
        Event::Invited(result) => {
            app.gate.inviting = false;
            match result {
                Ok(invited) => {
                    app.gate.invites = Some(invited.invites);
                    app.gate.invite = Some(invited.code);
                }
                Err(why) => app.gate.invite_error = Some(why),
            }
            Task::none()
        }
        Event::CopyInvite => match &app.gate.invite {
            Some(code) => iced::clipboard::write(code.clone()),
            None => Task::none(),
        },
    }
}

/// Checks `key` with the server.
fn ask(gate: &mut Gate, key: String, typed: bool) -> Task<Message> {
    gate.checking = Some(typed);
    let asked = key.clone();
    Task::perform(check(asked), move |result| {
        Message::Gate(Event::Checked { key, typed, result })
    })
}

fn checked(
    app: &mut App,
    key: String,
    typed: bool,
    result: Result<Verdict, String>,
) -> Task<Message> {
    let gate = &mut app.gate;
    gate.checking = None;
    match result {
        Ok(Verdict::Valid(invites)) => {
            if typed {
                keep(&key);
                gate.typed.clear();
                // A new key's invite isn't the old one's.
                if gate.key.as_deref() != Some(key.as_str()) {
                    gate.invite = None;
                    gate.invite_error = None;
                }
            }
            gate.key = Some(key);
            gate.invites = Some(invites);
            gate.checked_at = Some(chrono::Local::now().format("%H:%M").to_string());
            gate.second_try = false;
            gate.phase = Phase::Open;
            release(app)
        }
        Ok(verdict) => {
            let why = match verdict {
                Verdict::Revoked => "This key was revoked.",
                _ => "This key isn't valid.",
            };
            if typed {
                gate.phase = Phase::Asking {
                    error: Some(why.to_owned()),
                };
                return Task::none();
            }
            lock(app, why.to_owned())
        }
        Err(why) => {
            if typed {
                gate.phase = Phase::Asking { error: Some(why) };
                return Task::none();
            }
            // Once more, a moment later, before the window locks.
            let first = matches!(gate.phase, Phase::Open | Phase::Checking);
            if first && !gate.second_try {
                gate.second_try = true;
                return Task::perform(tokio::time::sleep(RETRY_AFTER), |()| {
                    Message::Gate(Event::Retry)
                });
            }
            gate.second_try = false;
            lock(app, why)
        }
    }
}

/// The window locks: no call goes on behind it with no way to mute, and
/// at the start the window opens to say why.
fn lock(app: &mut App, why: String) -> Task<Message> {
    let was_open = app.gate.phase == Phase::Open;
    tracing::warn!(%why, "the key didn't open Yapfast");
    app.gate.phase = Phase::Locked { why };
    app.gate.invite = None;
    app.follow_protected();
    if was_open
        && !matches!(
            app.client.state().voice.phase,
            yapfast_core::voice::Phase::Disconnected
        )
    {
        app.client
            .dispatch(Action::Voice(yapfast_core::voice::Command::Leave));
    }
    if app.gate.opened {
        Task::none()
    } else {
        show(app)
    }
}

/// The window opens to what the key unlocks; the first time, what was held
/// back starts: the saved session, or the preview.
fn release(app: &mut App) -> Task<Message> {
    app.follow_protected();
    if app.gate.opened {
        return Task::none();
    }
    app.gate.opened = true;
    let Some(action) = app.gate.held.take() else {
        return Task::none();
    };
    app.client.dispatch(action);
    // As a start on a saved account shows it (`SignIn::starting`).
    app.sign_in.starting = !crate::signin::workspace(app) && app.client.state().busy;
    app.sync()
}

/// The window, opened if Yapfast started in the tray.
fn show(app: &mut App) -> Task<Message> {
    if app.shell.window.is_some() {
        Task::none()
    } else {
        app.open_window()
    }
}

/// The key kept in the OS credential store.
fn kept() -> Option<String> {
    match keyring::Entry::new(SERVICE, ACCOUNT).and_then(|entry| entry.get_password()) {
        Ok(key) if !key.trim().is_empty() => Some(key),
        Ok(_) | Err(keyring::Error::NoEntry) => None,
        Err(error) => {
            tracing::warn!(%error, "the key couldn't be read from the OS credential store");
            None
        }
    }
}

/// Keeps a valid key in the OS credential store, so it's typed once.
fn keep(key: &str) {
    let kept = keyring::Entry::new(SERVICE, ACCOUNT).and_then(|entry| entry.set_password(key));
    if let Err(error) = kept {
        tracing::warn!(%error, "the key couldn't be kept in the OS credential store");
    }
}

/// One HTTPS client for the key server: no redirects, short timeouts.
fn client() -> Result<reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            rustls::crypto::aws_lc_rs::default_provider()
                .install_default()
                .ok();
            reqwest::Client::builder()
                .user_agent(concat!("Yapfast/", env!("CARGO_PKG_VERSION")))
                .https_only(server().is_some_and(|url| url.starts_with("https://")))
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(15))
                .build()
                .map_err(|error| error.to_string())
        })
        .clone()
}

/// Sends `key` to the server's `path`: its status and JSON answer.
async fn post(path: &str, key: &str) -> Result<(u16, serde_json::Value), String> {
    let Some(server) = server().filter(|url| allowed(url)) else {
        return Err("This build's key server isn't an HTTPS address.".to_owned());
    };
    let client = client().map_err(|error| {
        tracing::warn!(%error, "the key server's client didn't start");
        UNREACHABLE.to_owned()
    })?;
    let body = serde_json::json!({ "key": key }).to_string();
    let response = client
        .post(format!("{}{path}", server.trim_end_matches('/')))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(error = %error.without_url(), "the key server couldn't be reached");
            UNREACHABLE.to_owned()
        })?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await.map_err(|error| {
        tracing::warn!(error = %error.without_url(), "the key server's answer stopped");
        UNREACHABLE.to_owned()
    })?;
    Ok((
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    ))
}

fn invites_of(answer: &serde_json::Value) -> Invites {
    answer
        .get("invites")
        .cloned()
        .and_then(|invites| serde_json::from_value(invites).ok())
        .unwrap_or_default()
}

async fn check(key: String) -> Result<Verdict, String> {
    let (status, answer) = post("/v1/validate", &key).await?;
    if status != 200 {
        tracing::warn!(status, "the key server refused the check");
        return Err(format!(
            "The key server answered {status}. Try again in a moment."
        ));
    }
    let revoked = answer.get("reason").and_then(serde_json::Value::as_str) == Some("revoked");
    match answer.get("valid").and_then(serde_json::Value::as_bool) {
        Some(true) => Ok(Verdict::Valid(invites_of(&answer))),
        Some(false) if revoked => Ok(Verdict::Revoked),
        Some(false) => Ok(Verdict::Invalid),
        None => Err("The key server's answer made no sense. Try again in a moment.".to_owned()),
    }
}

async fn make_invite(key: String) -> Result<Invited, String> {
    let (status, answer) = post("/v1/invite", &key).await?;
    match status {
        200 => answer
            .get("code")
            .and_then(serde_json::Value::as_str)
            .map(|code| Invited {
                code: code.to_owned(),
                invites: invites_of(&answer),
            })
            .ok_or_else(|| "The key server's answer made no sense.".to_owned()),
        409 => Err("Your key has no invites left.".to_owned()),
        403 => Err("Your key isn't valid anymore, so it can't invite.".to_owned()),
        status => Err(format!(
            "The key server answered {status}. Try again in a moment."
        )),
    }
}

/// The gate, in place of the window's content: a card on the rail's ground,
/// as the sign-in screen's, asking for the key, checking it, or saying why
/// Yapfast is locked.
pub fn view(app: &App) -> Element<'_, Message> {
    let look = app.look();
    let theme = look.theme;
    let gate = &app.gate;
    let mut card = column![]
        .spacing(10)
        .width(Length::Fill)
        .align_x(Alignment::Center);
    match &gate.phase {
        Phase::Off | Phase::Open => {}
        Phase::Checking => {
            card = card
                .push(mark(look, Icon::Lock, theme.accent, theme.raised))
                .push(title(theme, "Checking your key"))
                .push(line(
                    "Yapfast checks it online before it opens.",
                    theme.secondary,
                ));
        }
        Phase::Asking { error } => {
            let busy = gate.checking == Some(true);
            let submit =
                (!busy && !gate.typed.trim().is_empty()).then_some(Message::Gate(Event::Submit));
            let mut input = field(
                theme,
                FIELD,
                "YF-XXXXX-XXXXX-XXXXX-XXXXX-XXXXX",
                &gate.typed,
                false,
                !busy,
            )
            .width(Length::Fill);
            if !busy {
                input = input.on_input(|typed| Message::Gate(Event::Typed(typed)));
            }
            if let Some(submit) = submit.clone() {
                input = input.on_submit(submit);
            }
            card = card
                .push(mark(look, Icon::Lock, theme.accent, theme.raised))
                .push(title(theme, "Enter your key"))
                .push(line(
                    "You type it once: Windows keeps it in Credential Manager.",
                    theme.secondary,
                ))
                .push(Space::new().height(4))
                .push(input)
                .push(
                    action(
                        theme,
                        if busy { "Checking\u{2026}" } else { "Unlock" },
                        Kind::Primary,
                        submit,
                    )
                    .wide(36.0),
                );
            if let Some(error) = error {
                card = card.push(line(error.as_str(), theme.danger));
            }
            card = card.push(line(
                "Yapfast checks it online as it opens, and every minute.",
                theme.muted,
            ));
        }
        Phase::Locked { why } => {
            let busy = gate.checking.is_some();
            let refresh = look.icon(Icon::Refresh, 18, crate::theme::rgb(theme.on_accent));
            card = card
                .push(mark(look, Icon::Lock, theme.danger, theme.raised))
                .push(title(theme, "Yapfast is locked"))
                .push(line(why.as_str(), theme.danger))
                .push(Space::new().height(4))
                .push(
                    row![
                        action(
                            theme,
                            if busy {
                                "Checking\u{2026}"
                            } else {
                                "Try again"
                            },
                            Kind::Primary,
                            (!busy).then_some(Message::Gate(Event::Retry)),
                        )
                        .icon(refresh),
                        action(
                            theme,
                            "Use another key",
                            Kind::Plain,
                            Some(Message::Gate(Event::UseAnother)),
                        ),
                    ]
                    .spacing(10),
                )
                .push(line(
                    "It also tries again by itself every minute.",
                    theme.muted,
                ));
        }
    }
    let chat = theme.chat;
    let card = container(card)
        .padding(32)
        .max_width(460)
        .style(move |_| container::Style {
            shadow: card_shadow(),
            ..rounded(chat, 12)
        });
    let page = scroll_view(
        container(card)
            .padding(Padding {
                top: 0.0,
                right: 24.0,
                bottom: 24.0,
                left: 24.0,
            })
            .width(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fixed((look.window.height - 64.0).max(0.0))),
    )
    .style(scrollbar(theme))
    .height(Length::Fill);
    let wordmark = container(text("Yapfast").size(24).font(BOLD).color(theme.accent))
        .padding(Padding {
            top: 24.0,
            right: 0.0,
            bottom: 0.0,
            left: 32.0,
        })
        .height(64);
    let rail = theme.rail;
    container(column![wordmark, page])
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_| fill(rail))
        .into()
}

/// The card's icon, in a rounded square.
fn mark<'a>(look: Look<'_>, icon: Icon, ink: Color, ground: Color) -> Element<'a, Message> {
    look.glyph(icon, 36, ink)
        .boxed(64)
        .style(move |_| rounded(ground, 20))
        .into()
}

fn title<'a>(theme: &Theme, words: &'a str) -> Element<'a, Message> {
    text(words)
        .size(24)
        .font(BOLD)
        .color(theme.text)
        .align_x(Alignment::Center)
        .width(Length::Fill)
        .into()
}

fn line<'a>(words: &'a str, ink: Color) -> Element<'a, Message> {
    text(words)
        .size(14)
        .color(ink)
        .align_x(Alignment::Center)
        .width(Length::Fill)
        .into()
}

/// My Account's key and invite, in a build with a key server: the key in
/// use with all but its end hidden and when it was last found valid, and
/// the invite it may make, with the code made last and Copy.
pub fn account<'a>(cx: &crate::ctx::Ctx<'a>) -> Option<Element<'a, Message>> {
    let gate = cx.gate;
    if gate.phase == Phase::Off {
        return None;
    }
    let theme = cx.look.theme;
    let ink = crate::theme::rgb(theme.text);
    let checked = match &gate.checked_at {
        Some(at) => format!("Valid \u{b7} checked at {at}"),
        None => "Not checked yet".to_owned(),
    };
    let key = crate::form::card(
        theme,
        row![
            cx.look.glyph(Icon::Lock, 18, theme.accent),
            text(gate.masked().unwrap_or_default())
                .size(15)
                .color(theme.text)
                .width(Length::Fill),
            text(checked).size(13).color(theme.secondary),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    );
    let invites = gate.invites.unwrap_or_default();
    let can = invites.left > 0 || invites.pending > 0;
    let about = if gate.invites.is_none() {
        "The key server hasn't said yet how many people this key may invite.".to_owned()
    } else if invites.quota == 0 {
        "This key can't invite anyone.".to_owned()
    } else if can {
        let people = invites.left.max(1);
        format!(
            "You can invite {people} {}. The code is their key: it works for one person.",
            if people == 1 { "person" } else { "people" }
        )
    } else if invites.used > 0 {
        "Your invite was used: this key has no invites left.".to_owned()
    } else {
        "This key has no invites left.".to_owned()
    };
    let mut parts = vec![
        crate::form::heading(theme, "KEY AND INVITE"),
        key,
        text("Invite a friend")
            .size(15)
            .font(BOLD)
            .color(theme.text)
            .into(),
        crate::form::note(theme, about),
    ];
    if let Some(code) = &gate.invite {
        parts.push(
            row![
                crate::form::card(theme, text(code.as_str()).size(15).color(theme.text)),
                action(
                    theme,
                    "Copy",
                    Kind::Plain,
                    Some(Message::Gate(Event::CopyInvite)),
                )
                .icon(cx.look.icon(Icon::Copy, 18, ink)),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .into(),
        );
    }
    let label = if gate.inviting {
        "Generating\u{2026}"
    } else if gate.invite.is_some() || invites.pending > 0 {
        "Generate a new code"
    } else {
        "Generate invite code"
    };
    let invite: Element<'a, Message> = action(
        theme,
        label,
        Kind::Plain,
        (can && !gate.inviting && gate.open()).then_some(Message::Gate(Event::Invite)),
    )
    .icon(cx.look.icon(Icon::UserPlus, 18, ink))
    .into();
    parts.push(crate::form::buttons([invite]));
    if let Some(why) = &gate.invite_error {
        parts.push(crate::form::note_in(why.as_str(), theme.danger));
    }
    if can {
        parts.push(crate::form::note(
            theme,
            "A new code replaces the last one while it's unused.",
        ));
    }
    Some(crate::form::group(parts))
}
