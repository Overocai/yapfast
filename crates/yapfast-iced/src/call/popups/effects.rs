//! The voice effects' menu, from the account panel's button beside the
//! bitrate's: the effects on or off; Hear Yourself, which plays what calls
//! send of the microphone, through the effects, on the call's speakers,
//! with its volume while it's on; each plugin with a dot for how it stands
//! (running, bypassed, or failed to load), its box that runs or bypasses
//! it and Edit for its editor; and Voice Settings, where plugins are added.

use iced::{
    Alignment, Element, Length, Padding, Task,
    widget::{Space, column, container, row, text},
};
use yapfast_core::voice_effects::{self, Request};

use crate::call::Event;
use crate::{
    app::{App, Message},
    controls::{self, Check, Kind, action, heading, separator},
    pictures::Icon,
    style::rounded,
};

/// A plugin's dot, and the room it's given.
const DOT: f32 = 8.0;
const DOT_ROOM: f32 = 16.0;

/// Asks the voice effects for `request`; the window's drawn again once
/// it's done, and the menu reads the chain as it stands.
pub(in crate::call) fn ask(request: Request) -> Task<Message> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    voice_effects::request(
        request,
        Box::new(move |result| {
            // A menu closed meanwhile takes no answer.
            drop(sender.send(result));
        }),
    );
    Task::perform(
        async move {
            receiver
                .await
                .unwrap_or_else(|_| Err("Yapfast's voice effects stopped.".into()))
        },
        |result| Message::Call(Event::EffectsDone(result)),
    )
}

/// Hear Yourself on or off, from now on in calls, kept in `layout.json`.
pub(in crate::call) fn hear_yourself(app: &mut App, on: bool) {
    app.layout.voice.hear_yourself = on;
    yapfast_core::discord_audio::set_monitor(on);
    crate::layout::store(app.client.root(), "hear_yourself", on);
}

/// Hear Yourself's volume as its slider stands, from 0 to 200%, kept as the
/// drag lets go.
pub(in crate::call) fn hear_volume(app: &mut App, value: u32, done: bool) {
    let value = value.min(200);
    app.layout.voice.hear_yourself_volume = Some(value);
    yapfast_core::discord_audio::set_monitor_volume(value as f32 / 100.0);
    if done {
        crate::layout::store(app.client.root(), "hear_yourself_volume", value);
    }
}

/// The menu.
pub(super) fn effects_menu<'a>(ctx: crate::ctx::Ctx<'a>) -> Element<'a, Message> {
    let theme = ctx.look.theme;
    let chain = voice_effects::view();
    let voice = &ctx.layout.voice;
    let pad = Padding {
        left: 8.0,
        right: 8.0,
        top: 4.0,
        bottom: 0.0,
    };
    let running = chain.on
        && chain
            .slots
            .iter()
            .any(|slot| !slot.bypassed && slot.failed.is_none());
    let state = if running {
        "Your microphone runs through them"
    } else if chain.on {
        "On, with no plugin running"
    } else {
        "Off"
    };
    let mut menu = column![
        container(heading(theme, "VOICE EFFECTS", 0.0)).padding(pad),
        crate::call::popups::popup_row(
            ctx,
            "Voice Effects",
            Some(state.into()),
            Some(Icon::Effects),
            Check::Box(chain.on),
            Some(Message::Call(Event::EffectsOn(!chain.on))),
        ),
        crate::call::popups::popup_row(
            ctx,
            "Hear Yourself",
            Some("Your voice as calls send it".into()),
            Some(Icon::Headphones),
            Check::Box(voice.hear_yourself),
            Some(Message::Call(Event::HearYourself(!voice.hear_yourself))),
        ),
    ]
    .spacing(4);
    if voice.hear_yourself {
        let volume = voice.hear_yourself_volume.unwrap_or(100);
        menu = menu.push(
            column![
                controls::slider("Hear Yourself Volume", volume, 200, |value| {
                    Message::Call(Event::HearVolume(value, false))
                })
                .on_release(Message::Call(Event::HearVolume(volume, true)))
                .view(theme),
                text("Wear headphones, or it echoes. You hear yourself only while you talk in a call.")
                    .size(12)
                    .color(theme.muted),
            ]
            .spacing(4)
            .padding([2, 8]),
        );
    }
    menu = menu
        .push(separator(theme))
        .push(container(heading(theme, "PLUGINS", 0.0)).padding(pad));
    if chain.slots.is_empty() {
        menu = menu.push(
            container(
                text("None yet. Add VST plugins in Voice Settings, then switch them here.")
                    .size(13)
                    .color(theme.muted),
            )
            .padding([0, 8]),
        );
    }
    for slot in &chain.slots {
        let (dot, subtitle) = match &slot.failed {
            Some(_) => (
                theme.danger,
                "Didn't load \u{b7} see Voice Settings".to_owned(),
            ),
            None if slot.bypassed => (
                theme.muted,
                format!("{} \u{b7} Bypassed", slot.format.label()),
            ),
            None if slot.vendor.is_empty() => (theme.live, slot.format.label().to_owned()),
            None => (
                theme.live,
                format!("{} \u{b7} {}", slot.vendor, slot.format.label()),
            ),
        };
        let dot = container(
            container(Space::new())
                .width(DOT)
                .height(DOT)
                .style(move |_| rounded(dot, DOT / 2.0)),
        )
        .center_x(DOT_ROOM);
        let toggle = crate::call::popups::popup_row(
            ctx,
            slot.name.clone(),
            Some(subtitle.into()),
            None,
            Check::Box(!slot.bypassed && slot.failed.is_none()),
            Some(Message::Call(Event::EffectBypass(slot.id, !slot.bypassed))),
        );
        let mut line = row![dot, container(toggle).width(Length::Fill)]
            .spacing(2)
            .align_y(Alignment::Center);
        if slot.editor {
            line = line.push(
                action(
                    theme,
                    if slot.open { "Show" } else { "Edit" },
                    Kind::Plain,
                    Some(Message::Call(Event::EffectEdit(slot.id))),
                )
                .small(),
            );
        }
        menu = menu.push(line);
    }
    menu.push(separator(theme))
        .push(crate::call::popups::popup_row(
            ctx,
            "Voice Settings",
            None,
            Some(Icon::Settings),
            Check::None,
            Some(crate::app::settings(crate::settings::Event::Open(
                crate::settings::Section::Voice,
            ))),
        ))
        .padding(6)
        .into()
}
