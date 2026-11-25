//! A picture moved and zoomed under a frame: [`Pan`], the profile editor's
//! Edit Image.

use iced::{
    Element, Length, Point, Rectangle, Size, Theme,
    advanced::{
        Clipboard, Layout, Shell, Widget,
        image::Handle,
        layout, mouse, renderer,
        widget::{Tree, tree},
    },
};

use super::{Renderer, draw_picture, draw_picture_in};

/// How far one of the wheel's steps zooms: a tenth.
const WHEEL_STEP: f32 = 1.1;
/// A touchpad's pixels that make one of the wheel's steps.
const STEP_PIXELS: f32 = 60.0;

/// How many of the widget's units one of the picture's pixels takes at
/// `zoom`: at 1 the picture just covers the frame, as small as it can be.
pub fn pan_scale(picture: Size, frame: Size, zoom: f32) -> f32 {
    let cover = (frame.width / picture.width.max(1.0)).max(frame.height / picture.height.max(1.0));
    cover * zoom.max(1.0)
}

/// Half the frame across and down, as parts of the picture's width and
/// height, at `zoom`.
pub fn pan_half(picture: Size, frame: Size, zoom: f32) -> (f32, f32) {
    let scale = pan_scale(picture, frame, zoom);
    let half = |frame: f32, picture: f32| (frame / (2.0 * picture.max(1.0) * scale)).min(0.5);
    (
        half(frame.width, picture.width),
        half(frame.height, picture.height),
    )
}

/// `centre` held where the frame stays on the picture at `zoom`: the point
/// of the picture under the frame's middle, as parts of its width and
/// height.
pub fn pan_held(picture: Size, frame: Size, zoom: f32, centre: (f32, f32)) -> (f32, f32) {
    let (half_x, half_y) = pan_half(picture, frame, zoom);
    (
        centre.0.clamp(half_x, 1.0 - half_x),
        centre.1.clamp(half_y, 1.0 - half_y),
    )
}

/// A picture under a frame the size of `frame`, centred in the widget,
/// which a drag moves and the wheel zooms: the picture's `centre` (the
/// point under the frame's middle, as parts of its width and height) and
/// `zoom` (from 1, where it just covers the frame, to `most`) are its
/// owner's, told with `moved` and `zoomed`. What lies outside the frame
/// is covered by `over`, drawn at the widget's size.
pub struct Pan<'a, M> {
    picture: Handle,
    picture_size: Size,
    size: Size,
    frame: Size,
    zoom: f32,
    most: f32,
    centre: (f32, f32),
    over: Option<Handle>,
    moved: Box<dyn Fn(f32, f32) -> M + 'a>,
    zoomed: Box<dyn Fn(f32) -> M + 'a>,
}

impl<'a, M> Pan<'a, M> {
    #[allow(
        clippy::too_many_arguments,
        reason = "the picture, the frame it's seen through, where it stands, and what moving it says"
    )]
    pub fn new(
        picture: Handle,
        picture_size: Size,
        size: Size,
        frame: Size,
        zoom: f32,
        centre: (f32, f32),
        moved: impl Fn(f32, f32) -> M + 'a,
        zoomed: impl Fn(f32) -> M + 'a,
    ) -> Self {
        Self {
            picture,
            picture_size,
            size,
            frame,
            zoom,
            most: 4.0,
            centre,
            over: None,
            moved: Box::new(moved),
            zoomed: Box::new(zoomed),
        }
    }

    /// The most it zooms to.
    pub fn most(mut self, most: f32) -> Self {
        self.most = most.max(1.0);
        self
    }

    /// What covers it outside the frame, drawn at the widget's size.
    pub fn over(mut self, over: Handle) -> Self {
        self.over = Some(over);
        self
    }

    /// Where the picture is drawn within `bounds`, at its zoom.
    fn placed(&self, bounds: Rectangle) -> Rectangle {
        let scale = pan_scale(self.picture_size, self.frame, self.zoom);
        let (width, height) = (
            self.picture_size.width * scale,
            self.picture_size.height * scale,
        );
        let middle = bounds.center();
        Rectangle {
            x: middle.x - self.centre.0 * width,
            y: middle.y - self.centre.1 * height,
            width,
            height,
        }
    }
}

/// Where the button went down while it's held, and the centre then.
#[derive(Default)]
struct PanState {
    held: Option<(Point, (f32, f32))>,
}

impl<M> Widget<M, Theme, Renderer> for Pan<'_, M> {
    fn size(&self) -> Size<Length> {
        Size::new(
            Length::Fixed(self.size.width),
            Length::Fixed(self.size.height),
        )
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<PanState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(PanState::default())
    }

    fn layout(&mut self, _: &mut Tree, _: &Renderer, _: &layout::Limits) -> layout::Node {
        layout::Node::new(self.size)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &iced::Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, M>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<PanState>();
        let bounds = layout.bounds();
        match event {
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(at) = cursor.position_over(bounds) {
                    state.held = Some((at, self.centre));
                    shell.capture_event();
                    shell.request_redraw();
                }
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let (Some((from, centre)), Some(at)) = (state.held, cursor.position()) else {
                    return;
                };
                let scale = pan_scale(self.picture_size, self.frame, self.zoom);
                let moved = (
                    centre.0 - (at.x - from.x) / (self.picture_size.width.max(1.0) * scale),
                    centre.1 - (at.y - from.y) / (self.picture_size.height.max(1.0) * scale),
                );
                let moved = pan_held(self.picture_size, self.frame, self.zoom, moved);
                if moved != self.centre {
                    shell.publish((self.moved)(moved.0, moved.1));
                }
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if state.held.take().is_some() =>
            {
                shell.request_redraw();
            }
            iced::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                if !cursor.is_over(bounds) {
                    return;
                }
                let steps = match *delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / STEP_PIXELS,
                };
                if steps == 0.0 {
                    return;
                }
                let zoom = (self.zoom * WHEEL_STEP.powf(steps)).clamp(1.0, self.most);
                if zoom != self.zoom {
                    shell.publish((self.zoomed)(zoom));
                }
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<PanState>();
        if state.held.is_some() {
            mouse::Interaction::Grabbing
        } else if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::None
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        draw_picture_in(
            renderer,
            &self.picture,
            self.placed(bounds),
            bounds,
            [0.0; 4],
            1.0,
        );
        if let Some(over) = &self.over {
            draw_picture(renderer, over, bounds, [0.0; 4], 1.0);
        }
    }
}

impl<'a, M: 'a> From<Pan<'a, M>> for Element<'a, M> {
    fn from(pan: Pan<'a, M>) -> Self {
        Element::new(pan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_one_the_picture_just_covers_the_frame() {
        let picture = Size::new(2000.0, 1000.0);
        let frame = Size::new(260.0, 260.0);
        assert_eq!(pan_scale(picture, frame, 1.0), 0.26);
        // The whole height shows, and the middle half of the width.
        assert_eq!(pan_half(picture, frame, 1.0), (0.25, 0.5));
        assert_eq!(pan_half(picture, frame, 2.0), (0.125, 0.25));
    }

    #[test]
    fn the_frame_stays_on_the_picture() {
        let picture = Size::new(2000.0, 1000.0);
        let frame = Size::new(260.0, 260.0);
        assert_eq!(
            pan_held(picture, frame, 1.0, (0.0, 0.9)),
            (0.25, 0.5),
            "as far left as it goes, and no room up or down"
        );
        assert_eq!(pan_held(picture, frame, 2.0, (0.9, 0.9)), (0.875, 0.75));
    }
}
