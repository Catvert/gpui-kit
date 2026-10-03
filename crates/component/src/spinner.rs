use std::sync::OnceLock;

use crate::{Icon, IconName, Sizable, Size};
use gpui::{
    App, AppContext as _, Context, ElementId, Entity, Hsla, IntoElement, ParentElement, Render,
    RenderOnce, Styled as _, Task, Transformation, Window, div, ease_in_out, percentage,
    prelude::FluentBuilder as _,
};
use instant::{Duration, Instant};

/// How often a spinner moves: sixteen notches a second, where an animation
/// would ask for a frame at every refresh of the screen.
const NOTCH: Duration = Duration::from_nanos(1_000_000_000 / 16);

/// The clock every spinner turns by, so that spinners on screen together
/// move at the same instants and ask one frame between them.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// A cycling loading spinner.
///
/// **It turns in a view of its own, by notches.** An animation in the
/// element would notify the view drawing it at every frame, and that view
/// would be rendered again, with everything it draws, for as long as the
/// spinner shows — a whole panel at the pace of the screen for one button
/// that waits. The view is kept in the element state where the spinner is
/// drawn, never observed by the view around it, and a timer notifies it
/// by id at each notch: nothing else is rendered for it, and a spinner no
/// longer drawn stops after one last tick, which redraws nothing.
///
/// In a `uniform_list` or a `list`, the view holding the list is rendered
/// again at each notch too: a list renders its rows as that view. Sixteen
/// times a second still, where an animation asked it at every frame.
///
/// Two spinners drawn under the same element id share their view: give
/// one of them an [`id`](Self::id).
#[derive(IntoElement)]
pub struct Spinner {
    id: ElementId,
    size: Size,
    icon: Icon,
    speed: Duration,
    easing: Box<dyn Fn(f32) -> f32>,
    color: Option<Hsla>,
}

impl Spinner {
    /// Create a new loading spinner.
    pub fn new() -> Self {
        Self {
            id: "spinner".into(),
            size: Size::Medium,
            speed: Duration::from_secs_f64(0.8),
            easing: Box::new(ease_in_out),
            icon: Icon::new(IconName::Loader),
            color: None,
        }
    }

    /// Set the key the spinner's view is kept under, for a spinner beside
    /// another one under the same element id.
    pub fn id(mut self, id: impl Into<ElementId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set specified icon for the spinner.
    ///
    /// Default is [`IconName::Loader`].
    ///
    /// Please ensure the icon used is suitable for a loading spinner.
    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = icon.into();
        self
    }

    /// Set the icon color.
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    /// Set the easing function. It is read once, when the spinner is first
    /// drawn at its place.
    pub fn ease(mut self, easing: impl Fn(f32) -> f32 + 'static) -> Self {
        self.easing = Box::new(easing);
        self
    }
}

impl Sizable for Spinner {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl RenderOnce for Spinner {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let icon = self
            .icon
            .with_size(self.size)
            .when_some(self.color, |this, color| this.text_color(color));
        if cx.reduce_motion() {
            return div().child(icon);
        }
        let (speed, easing) = (self.speed, self.easing);
        let view = window.with_global_id(self.id, |id, window| {
            window.with_element_state(id, |kept: Option<Entity<Turning>>, _| {
                let view = match kept {
                    Some(view) => {
                        // Told only of a change: an update counts as one for
                        // the views around, and would build them again.
                        let turning = view.read(cx);
                        let changed = !turning.icon.same_look(&icon) || turning.speed != speed;
                        if changed {
                            view.update(cx, |turning, cx| {
                                turning.icon = icon;
                                turning.speed = speed;
                                cx.notify();
                            });
                        }
                        view
                    }
                    None => cx.new(|_| Turning {
                        icon,
                        speed,
                        easing,
                        tick: None,
                    }),
                };
                (view.clone(), view)
            })
        });
        div().child(view)
    }
}

/// The view a spinner turns in.
struct Turning {
    icon: Icon,
    speed: Duration,
    easing: Box<dyn Fn(f32) -> f32>,
    /// The next notch, armed by the last render.
    tick: Option<Task<()>>,
}

impl Render for Turning {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let since = Instant::now().saturating_duration_since(epoch());
        let notch = NOTCH.as_nanos();
        let wait = Duration::from_nanos((notch - since.as_nanos() % notch) as u64);
        let id = cx.entity_id();
        // By id, without updating: the views around read this one, and an
        // update would count as a change for them.
        self.tick = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(wait).await;
            cx.update(|cx| cx.notify(id));
        }));
        let speed = self.speed.as_secs_f32().max(f32::EPSILON);
        let phase = since.as_secs_f32() % speed / speed;
        self.icon
            .clone()
            .transform(Transformation::rotate(percentage((self.easing)(phase))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Render, TestAppContext, px, size};

    struct SpinnerHost;

    impl Render for SpinnerHost {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            Spinner::new()
        }
    }

    #[gpui::test]
    fn reduced_motion_spinner_is_static_and_requests_no_frame(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let window = cx.open_window(size(px(100.), px(100.)), |_, _| SpinnerHost);
        cx.run_until_parked();

        assert_eq!(
            window
                .update(cx, |_, window, cx| window.simulate_next_frame(cx))
                .unwrap(),
            0
        );
    }

    /// A turning spinner asks no animation frame either: it moves by its
    /// timer, which notifies its own view and nothing else.
    #[gpui::test]
    fn a_turning_spinner_requests_no_animation_frame(cx: &mut TestAppContext) {
        let window = cx.open_window(size(px(100.), px(100.)), |_, _| SpinnerHost);
        cx.run_until_parked();

        assert_eq!(
            window
                .update(cx, |_, window, cx| window.simulate_next_frame(cx))
                .unwrap(),
            0
        );
    }
}
