//! Notifications: desktop notifications, what they show, the taskbar's
//! flash, and the keywords that alert as a mention does.

use iced::{
    Alignment, Element, Length, Padding,
    widget::{container, row, text},
};

use super::{KeywordEvent, Pref, pref};
use crate::settings::{Event, Page, Settings};
use crate::{
    app::Message,
    controls::{Kind, action},
    ctx::Cx,
    form::{self, switch},
    pictures::Icon,
    style::rounded,
};

/// The keyword field's id; a keyword's longest, and how many are kept.
const KEYWORD_FIELD: &str = "settings-keyword";
const KEYWORD_LONGEST: usize = 64;
const KEYWORDS_MOST: usize = 50;
/// A keyword's pill: its height, and its ground's corners.
const PILL: f32 = 28.0;

fn keyword_message(event: KeywordEvent) -> Message {
    crate::app::settings(Event::Keyword(event))
}

impl Settings {
    /// A keyword written, added or removed: the list is kept and the
    /// gateway's session takes it at once. A keyword already there, empty,
    /// too long, or past the most is left out.
    pub(in crate::settings) fn keyword_event(&mut self, event: KeywordEvent, cx: &mut Cx<'_>) {
        {
            let keywords = &mut cx.layout.prefs.keyword_alerts;
            match event {
                KeywordEvent::Draft(written) => {
                    self.keyword = written;
                    return;
                }
                KeywordEvent::Add => {
                    let keyword = self.keyword.trim();
                    let lower = keyword.to_lowercase();
                    let fits = !keyword.is_empty()
                        && keyword.chars().count() <= KEYWORD_LONGEST
                        && !keyword.chars().any(char::is_control);
                    let kept = keywords.iter().any(|kept| kept.to_lowercase() == lower);
                    if !fits || (!kept && keywords.len() >= KEYWORDS_MOST) {
                        return;
                    }
                    if !kept {
                        keywords.push(keyword.to_owned());
                    }
                    self.keyword.clear();
                }
                KeywordEvent::Remove(index) => {
                    if index >= keywords.len() {
                        return;
                    }
                    keywords.remove(index);
                }
            }
        }
        let keywords = &cx.layout.prefs.keyword_alerts;
        cx.keep("keyword_alerts", keywords);
        yapfast_core::discord::set_keywords(keywords);
    }
}

impl<'a> Page<'a> {
    pub(in crate::settings) fn notifications_page(self) -> Element<'a, Message> {
        let theme = self.cx.look.theme;
        let prefs = &self.cx.layout.prefs;
        let on = prefs.notifications.unwrap_or(true);
        let rules = if cfg!(feature = "matrix") && self.linked() {
            "Each Discord server's notification settings and your Matrix notification rules decide"
        } else if cfg!(feature = "matrix") {
            "Your Matrix notification rules decide"
        } else {
            "Each Discord server's notification settings decide"
        };
        let words = prefs.notification_text.unwrap_or(true);
        let mut parts = vec![
            form::title(theme, "Notifications"),
            switch(
                "Show desktop notifications",
                on,
                Some(pref(Pref::Notifications(!on))),
            )
            .description(format!(
                "For new messages while Yapfast isn't in front. {rules} which: usually DMs and mentions. Clicking one opens the conversation."
            ))
            .view(self.cx.look),
            switch(
                "Show what messages say",
                words,
                on.then(|| pref(Pref::NotificationText(!words))),
            )
            .description("Off shows only who wrote and where, for a screen others can see.")
            .view(self.cx.look),
            self.flip(
                "Flash the taskbar",
                "For a message that notifies while Yapfast isn't in front. The window's title counts mentions and DMs waiting either way.",
                prefs.flash_taskbar.unwrap_or(true),
                Pref::FlashTaskbar,
            ),
            form::heading(theme, "KEYWORD ALERTS"),
            form::note(
                theme,
                "A message from someone else that holds one of these words alerts you as a mention does, in any server or DM, though Discord wouldn't notify it: it sounds, shows a notification and is highlighted. Whole words, in any case. Muted channels and Do Not Disturb stay quiet.",
            ),
        ];
        let keywords = &prefs.keyword_alerts;
        if !keywords.is_empty() {
            let (ground, ink) = (theme.raised, theme.text);
            let pills =
                keywords
                    .iter()
                    .enumerate()
                    .map(|(index, keyword)| -> Element<'a, Message> {
                        container(
                            row![
                                text(keyword.as_str())
                                    .size(14)
                                    .color(ink)
                                    .wrapping(text::Wrapping::None),
                                self.cx
                                    .look
                                    .icon_button(
                                        Icon::X,
                                        "Remove",
                                        keyword_message(KeywordEvent::Remove(index)),
                                    )
                                    .size(20.0, 12),
                            ]
                            .spacing(4)
                            .align_y(Alignment::Center),
                        )
                        .height(PILL)
                        .align_y(Alignment::Center)
                        .padding(Padding {
                            left: 12.0,
                            right: 4.0,
                            ..Padding::ZERO
                        })
                        .style(move |_| rounded(ground, PILL / 2.0))
                        .into()
                    });
            parts.push(row(pills).spacing(8).wrap().vertical_spacing(8).into());
        }
        let written = self.s.keyword.trim();
        let full = keywords.len() >= KEYWORDS_MOST;
        let ready = !written.is_empty() && written.chars().count() <= KEYWORD_LONGEST && !full;
        let field = form::field(theme, KEYWORD_FIELD, "Add a keyword", &self.s.keyword)
            .on_input(|written| keyword_message(KeywordEvent::Draft(written)))
            .on_submit_maybe(ready.then(|| keyword_message(KeywordEvent::Add)))
            .width(Length::Fill);
        parts.push(
            row![
                field,
                action(
                    theme,
                    "Add",
                    Kind::Plain,
                    ready.then(|| keyword_message(KeywordEvent::Add))
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
        );
        if full {
            parts.push(form::note(
                theme,
                "That's 50 keywords, the most. Remove one to add another.",
            ));
        } else if written.chars().count() > KEYWORD_LONGEST {
            parts.push(form::note(theme, "A keyword can be up to 64 characters."));
        }
        form::page(parts)
    }
}
