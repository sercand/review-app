//! Centre the macOS traffic lights in the app's title bar.
//!
//! The window draws its content under a transparent title bar, but AppKit
//! still lays the close/minimise/zoom buttons out for its own 28 pt bar, so
//! they sit above the centre of the app's 40 pt [`super::widgets::title_bar`].
//! egui has no setting for their position; this grows the buttons' container
//! to the bar's height and centres them in it, the way Tauri's
//! `trafficLightPosition` does. AppKit puts them back on some resizes and on
//! leaving full screen, so it runs every frame — cheaply, since it stops at
//! a frame check once they are in place.

/// The buttons' origins inside a container `bar_h` tall: the first `inset`
/// from the left, the next two `spacing` apart, all vertically centred.
pub fn button_origins(inset: f64, spacing: f64, bar_h: f64, button_h: f64) -> [(f64, f64); 3] {
    let y = ((bar_h - button_h) / 2.0).round();
    [0.0, 1.0, 2.0].map(|i| (inset + i * spacing, y))
}

/// The close button's distance from the window's left edge.
pub const INSET: f64 = 14.0;

#[cfg(target_os = "macos")]
pub fn center(frame: &eframe::Frame, bar_h: f64) {
    use objc2_app_kit::{NSView, NSWindowButton};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    // SAFETY: winit hands out the window's live content view, and egui calls
    // `ui` on the main thread, where AppKit objects may be touched.
    let view: &NSView = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let Some(window) = view.window() else {
        return;
    };
    let buttons = [
        NSWindowButton::CloseButton,
        NSWindowButton::MiniaturizeButton,
        NSWindowButton::ZoomButton,
    ]
    .map(|kind| window.standardWindowButton(kind));
    let [Some(close), Some(minimise), Some(zoom)] = buttons else {
        return;
    };
    // The buttons live in the title bar view, inside its container.
    // SAFETY: `superview` only reads the view hierarchy, on the main thread.
    let Some(container) = (unsafe { close.superview() }).and_then(|bar| unsafe { bar.superview() })
    else {
        return;
    };

    let spacing = minimise.frame().origin.x - close.frame().origin.x;
    let origins = button_origins(INSET, spacing, bar_h, close.frame().size.height);
    let mut bounds = container.frame();
    let placed = (bounds.size.height - bar_h).abs() < 0.5
        && (close.frame().origin.x - origins[0].0).abs() < 0.5
        && (close.frame().origin.y - origins[0].1).abs() < 0.5;
    if placed {
        return;
    }
    bounds.size.height = bar_h;
    bounds.origin.y = window.frame().size.height - bar_h;
    container.setFrame(bounds);
    for (button, (x, y)) in [close, minimise, zoom].iter().zip(origins) {
        let mut rect = button.frame();
        rect.origin.x = x;
        rect.origin.y = y;
        button.setFrameOrigin(rect.origin);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_centre_in_the_bar() {
        // AppKit's 14 pt buttons, 20 pt apart, in the app's 40 pt bar.
        assert_eq!(
            button_origins(INSET, 20.0, 40.0, 14.0),
            [(14.0, 13.0), (34.0, 13.0), (54.0, 13.0)]
        );
    }
}
