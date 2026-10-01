//! Lets the host watch the scroll positions a script's elements drive.
//!
//! A host that smooths the wheel — that replays a notch's jump as a short
//! transition — needs the position of each area a script scrolls, every frame
//! it is drawn: to advance the transition before layout reads the offset, and
//! to know, when a wheel event bubbles past, which area gpui just moved. The
//! positions live in window element state under names only the materializer
//! knows, so the materializer says them here as it draws them.
//!
//! One observer per thread, as the default policy is: a host installs it once,
//! at start-up. Nothing is observed until one is installed, and the cost of an
//! area with no observer is one thread-local read.

use std::{cell::RefCell, rc::Rc};

use gpui::{App, ElementId, ScrollHandle, Window};

/// Which axes an area scrolls along.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollAxes {
    pub horizontal: bool,
    pub vertical: bool,
}

impl ScrollAxes {
    pub const VERTICAL: Self = Self {
        horizontal: false,
        vertical: true,
    };
    pub const HORIZONTAL: Self = Self {
        horizontal: true,
        vertical: false,
    };
    pub const BOTH: Self = Self {
        horizontal: true,
        vertical: true,
    };
}

type Observer = Rc<dyn Fn(&ElementId, &ScrollHandle, ScrollAxes, &mut Window, &mut App)>;

thread_local! {
    static OBSERVER: RefCell<Option<Observer>> = const { RefCell::new(None) };
}

/// Installs the observer: called with each scroll area and virtual list a
/// script view draws — its identity, the position it scrolls, its axes —
/// while that view is being drawn, before the area is laid out. Replaces the
/// observer installed before.
///
/// It runs inside the script view's render: it may read and write the
/// position, and ask for an animation frame, but must not render the view.
pub fn observe_scroll_areas(
    observer: impl Fn(&ElementId, &ScrollHandle, ScrollAxes, &mut Window, &mut App) + 'static,
) {
    OBSERVER.with(|slot| *slot.borrow_mut() = Some(Rc::new(observer)));
}

/// Removes the observer.
pub fn clear_scroll_observer() {
    OBSERVER.with(|slot| *slot.borrow_mut() = None);
}

/// Says one area to the observer, if there is one.
pub(crate) fn seen(
    identity: &ElementId,
    handle: &ScrollHandle,
    axes: ScrollAxes,
    window: &mut Window,
    cx: &mut App,
) {
    // Cloned out before the call: the observer may install another.
    let observer = OBSERVER.with(|slot| slot.borrow().clone());
    if let Some(observer) = observer {
        observer(identity, handle, axes, window, cx);
    }
}
