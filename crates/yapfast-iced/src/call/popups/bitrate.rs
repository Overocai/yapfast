//! The bitrate menu, from the account panel's button beside mute and
//! deafen: whether Discord calls send the microphone in mono or stereo, and
//! its bit rate, as Settings' Voice & video has them too.

use iced::{
    Element, Padding,
    widget::{column, container, text},
};
use yapfast_core::discord_audio::{DEFAULT_BITRATE, LEAST_BITRATE, MOST_BITRATE};

use crate::call::Event;
use crate::{
    app::{App, Message},
    controls::{self, Check, heading, separator},
    pictures::Icon,
};

/// The menu's Mono or Stereo: Discord calls open the microphone that way
/// from now on, a call going on straight away.
pub(in crate::call) fn set_stereo(app: &mut App, on: bool) {
    app.layout.voice.stereo_microphone = on;
    yapfast_core::discord_audio::set_stereo(on);
    crate::layout::store(app.client.root(), "stereo_microphone", on);
}

/// The menu's bit rate, which a call going on takes up from its next frame,
/// kept as the drag lets go.
pub(in crate::call) fn set_bitrate(app: &mut App, value: u32, done: bool) {
    let kbps = value.clamp(LEAST_BITRATE, MOST_BITRATE);
    app.layout.voice.voice_bitrate = Some(kbps);
    yapfast_core::discord_audio::set_bitrate(kbps);
    if done {
        crate::layout::store(app.client.root(), "voice_bitrate", kbps);
    }
}

/// The menu: Mono or Stereo, the bit rate's slider, and Voice Settings.
pub(super) fn bitrate_menu<'a>(ctx: crate::ctx::Ctx<'a>) -> Element<'a, Message> {
    let theme = ctx.look.theme;
    let voice = &ctx.layout.voice;
    let stereo = voice.stereo_microphone;
    let bitrate = voice.voice_bitrate.unwrap_or(DEFAULT_BITRATE);
    let note = if stereo {
        "Both channels go out as your microphone hears them. Wear headphones."
    } else {
        "128 kbps is plenty for a voice; music sounds best higher."
    };
    column![
        column![
            heading(theme, "MICROPHONE IN DISCORD CALLS", 0.0),
            controls::choices(theme, &["Mono", "Stereo"], usize::from(stereo), |index| {
                Message::Call(Event::Stereo(index == 1))
            }),
        ]
        .spacing(2)
        .padding(Padding {
            left: 8.0,
            right: 8.0,
            bottom: 4.0,
            top: 4.0,
        }),
        separator(theme),
        container(
            controls::slider("Voice Bitrate", bitrate, MOST_BITRATE, |value| {
                Message::Call(Event::Bitrate(value, false))
            })
            .mark(false)
            .step(1)
            .shown(format!("{bitrate} kbps"))
            .on_release(Message::Call(Event::Bitrate(bitrate, true)))
            .view(theme),
        )
        .padding([4, 8]),
        container(text(note).size(13).color(theme.muted)).padding([0, 8]),
        separator(theme),
        crate::call::popups::popup_row(
            ctx,
            "Voice Settings",
            None,
            Some(Icon::Settings),
            Check::None,
            Some(crate::app::settings(crate::settings::Event::Open(
                crate::settings::Section::Voice,
            ))),
        ),
    ]
    .spacing(4)
    .padding(6)
    .into()
}
