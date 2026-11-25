//! Discord's Edit Image window, which a picture chosen for the avatar or the
//! banner opens over the profile editor: the picture under a frame of the
//! profile's shape (a circle for the avatar, 17 by 6 for the banner), dimmed
//! outside it, which a drag moves and the slider or the wheel zooms; then
//! Reset, Cancel and Apply. Apply hands the core where the frame stands
//! (`Action::PickProfilePicture` with its crop), which cuts the picture at
//! its own size there.
//!
//! The window shows the picture as the core reads it for that
//! (`discord_text::read_to_crop`), on a blocking thread as it opens, at most
//! 1280 px across, and lets it go as it closes.

use std::{cell::RefCell, path::PathBuf};

use iced::{
    Alignment, Color, Element, Length, Size, Task,
    advanced::image::Handle,
    widget::{Space, column, container, mouse_area, opaque, row, slider, stack, text},
};
use yapfast_core::{
    application::Action,
    chat::{PictureCrop, ProfilePicture},
};

use super::{Event as ProfileEvent, editor::Event as EditorEvent};
use crate::{
    app::Message,
    canvas::{Canvas, Ink},
    controls::{Kind, action, placed},
    ctx::Cx,
    fonts::SEMIBOLD,
    pictures::Icon,
    style::rounded,
    widgets::{Pan, pan_half, pan_held},
};

/// The card's width, the room kept round it in a smaller window, and its
/// padding.
const WIDTH: f32 = 440.0;
const ROOM: f32 = 32.0;
const PAD: f32 = 16.0;
/// Where the picture shows: the card's width less its padding, and as tall.
const STAGE_HEIGHT: f32 = 300.0;
/// The avatar's circle across; the banner's frame, 17 by 6.
const CIRCLE: f32 = 260.0;
const BANNER_WIDTH: f32 = 380.0;
const BANNER_HEIGHT: f32 = BANNER_WIDTH * 6.0 / 17.0;
/// How far it zooms in, from where the picture just covers the frame.
const MOST_ZOOM: f32 = 4.0;
/// What dims the picture outside the frame, and the frame's line.
const SHADE: f32 = 0.6;
const LINE: f32 = 2.0;

/// What's done in the window.
#[derive(Clone, Debug)]
pub enum Event {
    /// The picture chosen, read for the window: its pixels in the
    /// renderer's format, the size they're shown at, or why it can't be a
    /// profile picture.
    Read(PathBuf, Result<(Handle, (u32, u32)), String>),
    /// The point of the picture under the frame's middle, as parts of its
    /// width and height.
    Move(f32, f32),
    Zoom(f32),
    Reset,
    Cancel,
    Apply,
}

fn message(event: Event) -> Message {
    Message::Profile(ProfileEvent::Editor(EditorEvent::Crop(event)))
}

/// The window while it's open: the picture chosen, and where the frame
/// stands on it.
pub struct Crop {
    pub kind: ProfilePicture,
    path: PathBuf,
    /// The picture as shown and its size, once read, or why it couldn't
    /// be.
    picture: Option<Result<(Handle, Size), String>>,
    zoom: f32,
    centre: (f32, f32),
    /// What dims the picture outside the frame, made at the display's
    /// scale (by its bits) the window was last drawn at.
    cover: RefCell<Option<(u32, Handle)>>,
}

impl Crop {
    /// The window on the picture at `path`, and its reading, which
    /// [`Event::Read`] brings back.
    pub fn open(kind: ProfilePicture, path: PathBuf) -> (Self, Task<Message>) {
        let reading = path.clone();
        let task = Task::perform(
            async move {
                let read = reading.clone();
                let result = tokio::task::spawn_blocking(move || shown(&read))
                    .await
                    .unwrap_or_else(|_| Err("Yapfast could not read that picture.".into()));
                (reading, result)
            },
            |(path, result)| message(Event::Read(path, result)),
        );
        let crop = Self {
            kind,
            path,
            picture: None,
            zoom: 1.0,
            centre: (0.5, 0.5),
            cover: RefCell::default(),
        };
        (crop, task)
    }

    /// The frame's size on the stage.
    fn frame(&self) -> Size {
        match self.kind {
            ProfilePicture::Avatar => Size::new(CIRCLE, CIRCLE),
            ProfilePicture::Banner => Size::new(BANNER_WIDTH, BANNER_HEIGHT),
        }
    }

    /// Where the frame stands on the picture, as the core cuts it.
    fn chosen(&self, size: Size) -> PictureCrop {
        let (half_x, half_y) = pan_half(size, self.frame(), self.zoom);
        PictureCrop {
            x: self.centre.0 - half_x,
            y: self.centre.1 - half_y,
            width: 2.0 * half_x,
            height: 2.0 * half_y,
        }
    }
}

/// The picture at `path` as the window shows it: in the renderer's format,
/// with the size it's shown at.
fn shown(path: &std::path::Path) -> Result<(Handle, (u32, u32)), String> {
    let mut source = yapfast_core::discord_text::read_to_crop(path)?;
    crate::paint::to_native(&mut source.rgba);
    let (width, height) = source.shown;
    Ok((
        Handle::from_rgba(width, height, source.rgba),
        (width, height),
    ))
}

/// What's done in the window, on the profile editor it's open over.
pub fn update(editor: &mut super::editor::Editor, event: Event, cx: &mut Cx<'_>) -> Task<Message> {
    let Some(crop) = &mut editor.crop else {
        return Task::none();
    };
    match event {
        Event::Read(path, result) => {
            if path == crop.path {
                crop.picture = Some(result.map(|(handle, (width, height))| {
                    (handle, Size::new(width as f32, height as f32))
                }));
            }
        }
        Event::Move(x, y) => {
            if let Some(Ok((_, size))) = &crop.picture {
                crop.centre = pan_held(*size, crop.frame(), crop.zoom, (x, y));
            }
        }
        Event::Zoom(zoom) => {
            crop.zoom = zoom.clamp(1.0, MOST_ZOOM);
            if let Some(Ok((_, size))) = &crop.picture {
                crop.centre = pan_held(*size, crop.frame(), crop.zoom, crop.centre);
            }
        }
        Event::Reset => {
            crop.zoom = 1.0;
            crop.centre = (0.5, 0.5);
        }
        Event::Cancel => editor.crop = None,
        Event::Apply => {
            let Some(crop) = editor.crop.take() else {
                return Task::none();
            };
            if let Some(Ok((_, size))) = &crop.picture {
                cx.dispatch(Action::PickProfilePicture {
                    kind: crop.kind,
                    path: crop.path.clone(),
                    crop: Some(crop.chosen(*size)),
                });
            }
        }
    }
    cx.touch();
    Task::none()
}

/// Escape closes it, choosing nothing. Whether it took the key.
pub fn escape(editor: &mut super::editor::Editor) -> bool {
    editor.crop.take().is_some()
}

/// The window over the editor dimmed by half; a click on the dim cancels
/// it.
pub fn crop_view<'a>(ctx: crate::ctx::Ctx<'a>, crop: &'a Crop) -> Element<'a, Message> {
    let theme = ctx.look.theme;
    let width = WIDTH.min(ctx.look.window.width - ROOM);
    let stage = Size::new(width - 2.0 * PAD, STAGE_HEIGHT);
    let title = text("Edit Image").size(20).font(SEMIBOLD).color(theme.text);
    let ground = Color::BLACK;
    let picture: Element<'a, Message> = match &crop.picture {
        Some(Ok((handle, size))) => Pan::new(
            handle.clone(),
            *size,
            stage,
            crop.frame(),
            crop.zoom,
            crop.centre,
            |x, y| message(Event::Move(x, y)),
            |zoom| message(Event::Zoom(zoom)),
        )
        .most(MOST_ZOOM)
        .over(cover(crop, stage, ctx.scale))
        .into(),
        Some(Err(why)) => container(
            text(why.as_str())
                .size(14)
                .color(theme.danger)
                .align_x(Alignment::Center),
        )
        .padding(24)
        .center_x(stage.width)
        .center_y(stage.height)
        .into(),
        None => container(text("Loading...").size(14).color(theme.muted))
            .center_x(stage.width)
            .center_y(stage.height)
            .into(),
    };
    let stage_box = container(picture)
        .width(stage.width)
        .height(stage.height)
        .clip(true)
        .style(move |_| rounded(ground, 8));
    let ready = matches!(crop.picture, Some(Ok(_)));
    let (rail, rest, knob) = (theme.accent, theme.line, theme.text);
    let zoom = slider(1.0..=MOST_ZOOM, crop.zoom, |zoom| {
        message(Event::Zoom(zoom))
    })
    .step(0.01_f32)
    .style(move |_, _| slider::Style {
        rail: slider::Rail {
            backgrounds: (rail.into(), rest.into()),
            width: 4.0,
            border: crate::style::round(2.0),
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle { radius: 8.0 },
            background: knob.into(),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    });
    let zoom_row = row![
        ctx.look.glyph(Icon::Image, 16, theme.secondary),
        zoom,
        ctx.look.glyph(Icon::Image, 24, theme.secondary),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    let foot = row![
        action(
            theme,
            "Reset",
            Kind::Ghost,
            ready.then(|| message(Event::Reset))
        ),
        Space::new().width(Length::Fill),
        action(theme, "Cancel", Kind::Plain, Some(message(Event::Cancel))),
        action(
            theme,
            "Apply",
            Kind::Primary,
            ready.then(|| message(Event::Apply))
        ),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let body = column![title, stage_box, zoom_row, foot]
        .spacing(16)
        .padding(PAD);
    let card_ground = theme.sidebar;
    let card = container(body)
        .width(width)
        .style(move |_| rounded(card_ground, 16).shadow(crate::style::window_shadow()));
    let close = ctx
        .look
        .icon_button(Icon::X, "Close", message(Event::Cancel))
        .size(32.0, 20)
        .below();
    let card = stack![card, placed(close, width - 32.0 - 12.0, 12.0)];
    let dim = Color {
        a: 0.5,
        ..Color::BLACK
    };
    let backdrop = mouse_area(
        container(Space::new())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_| crate::style::fill(dim)),
    )
    .on_press(message(Event::Cancel));
    opaque(stack![
        opaque(backdrop),
        container(opaque(card)).center(Length::Fill)
    ])
}

/// What dims the picture outside the frame, with the frame's line, made
/// once for the display's `scale` and kept while it stays.
fn cover(crop: &Crop, stage: Size, scale: f32) -> Handle {
    let key = scale.to_bits();
    if let Some((made, handle)) = &*crop.cover.borrow()
        && *made == key
    {
        return handle.clone();
    }
    let pixels = |units: f32| (units * scale).round().max(1.0);
    let (width, height) = (pixels(stage.width), pixels(stage.height));
    let mut canvas = Canvas::new(width as u32, height as u32);
    canvas.rounded(0.0, 0.0, width, height, 0.0, Ink(0x000000, SHADE));
    let (middle_x, middle_y) = (width / 2.0, height / 2.0);
    let line = pixels(LINE);
    match crop.kind {
        ProfilePicture::Avatar => {
            let radius = pixels(CIRCLE) / 2.0;
            let outer = radius + line;
            canvas.circle(
                middle_x - outer,
                middle_y - outer,
                2.0 * outer,
                Ink(0xffffff, 1.0),
            );
            canvas.erase_circle(middle_x, middle_y, radius);
        }
        ProfilePicture::Banner => {
            let (frame_width, frame_height) = (pixels(BANNER_WIDTH), pixels(BANNER_HEIGHT));
            let (left, top) = (middle_x - frame_width / 2.0, middle_y - frame_height / 2.0);
            canvas.rounded(
                left - line,
                top - line,
                frame_width + 2.0 * line,
                frame_height + 2.0 * line,
                0.0,
                Ink(0xffffff, 1.0),
            );
            clear(&mut canvas, left, top, frame_width, frame_height);
        }
    }
    let handle = crate::canvas::handle(canvas);
    *crop.cover.borrow_mut() = Some((key, handle.clone()));
    handle
}

/// Clears a rectangle of `canvas` to see-through, edges and all.
fn clear(canvas: &mut Canvas, left: f32, top: f32, width: f32, height: f32) {
    let x0 = left.round().max(0.0) as u32;
    let y0 = top.round().max(0.0) as u32;
    let x1 = ((left + width).round().max(0.0) as u32).min(canvas.width);
    let y1 = ((top + height).round().max(0.0) as u32).min(canvas.height);
    for y in y0..y1 {
        let from = ((y * canvas.width + x0) * 4) as usize;
        let to = ((y * canvas.width + x1) * 4) as usize;
        canvas.pixels[from..to].fill(0);
    }
}
