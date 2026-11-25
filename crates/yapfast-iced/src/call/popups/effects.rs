//! The voice effects' menu, from the account panel's button beside the
//! bitrate's: the effects on or off, each plugin running or bypassed with
//! its editor's button, and Voice Settings, where plugins are added.

use iced::{
    Alignment, Element, Length, Padding, Task,
    widget::{column, container, row, text},
};
use yapfast_core::voice_effects::{self, Request};

use crate::call::Event;
use crate::{
    app::Message,
    controls::{Check, heading, separator},
    pictures::Icon,
};

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

/// The menu: the effects' switch, each plugin with its box and editor,
/// and Voice Settings.
pub(super) fn effects_menu<'a>(ctx: crate::ctx::Ctx<'a>) -> Element<'a, Message> {
    let theme = ctx.look.theme;
    let chain = voice_effects::view();
    let mut menu = column![
        container(heading(theme, "VOICE EFFECTS (VST)", 0.0)).padding(Padding {
            left: 8.0,
            right: 8.0,
            top: 4.0,
            bottom: 0.0,
        }),
        crate::call::popups::popup_row(
            ctx,
            "Voice Effects",
            None,
            None,
            Check::Box(chain.on),
            Some(Message::Call(Event::EffectsOn(!chain.on))),
        ),
    ]
    .spacing(4);
    if chain.slots.is_empty() {
        menu = menu.push(
            container(
                text("Add VST plugins in Voice Settings, then turn them on here.")
                    .size(13)
                    .color(theme.muted),
            )
            .padding([0, 8]),
        );
    } else {
        menu = menu.push(separator(theme));
    }
    for slot in &chain.slots {
        let subtitle = match &slot.failed {
            Some(_) => "Didn't load".to_owned(),
            None if slot.bypassed => format!("{} \u{b7} Bypassed", slot.format.label()),
            None => slot.format.label().to_owned(),
        };
        let toggle = crate::call::popups::popup_row(
            ctx,
            slot.name.clone(),
            Some(subtitle.into()),
            None,
            Check::Box(!slot.bypassed && slot.failed.is_none()),
            Some(Message::Call(Event::EffectBypass(slot.id, !slot.bypassed))),
        );
        let line: Element<'a, Message> = if slot.editor {
            row![
                container(toggle).width(Length::Fill),
                ctx.look
                    .icon_button(
                        Icon::SquarePen,
                        "Open its editor",
                        Message::Call(Event::EffectEdit(slot.id)),
                    )
                    .size(28.0, 16),
            ]
            .spacing(4)
            .align_y(Alignment::Center)
            .into()
        } else {
            toggle
        };
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
