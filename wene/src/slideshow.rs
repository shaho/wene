//! Fullscreen slideshow: borderless key window + slide view with
//! fit-to-window display, zoom ladder, and drag panning.

use std::cell::{Cell, OnceCell, RefCell};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSEvent, NSGraphicsContext, NSImage, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::NSRect;

use crate::AppDelegate;

/// Half-powers-of-two zoom ladder, same shape as the original.
const ZOOM_LADDER: &[f64] = &[
    0.125, 0.1875, 0.25, 0.375, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 16.0,
];

define_class!(
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "WeneSlideshowWindow"]
    pub struct SlideshowWindow;

    impl SlideshowWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }
    }
);

impl SlideshowWindow {
    pub fn fullscreen(mtm: MainThreadMarker, screen_frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        let window: Retained<Self> = unsafe {
            msg_send![
                super(this),
                initWithContentRect: screen_frame,
                styleMask: NSWindowStyleMask::Borderless,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        window.setBackgroundColor(Some(&NSColor::blackColor()));
        unsafe { window.setReleasedWhenClosed(false) };
        window
    }
}

/// Per-file view state, remembered for the session while a
/// slideshow runs.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct SlideState {
    /// None = fit to window (never scaling up past 100%).
    pub zoom: Option<f64>,
    /// Clockwise quarter turns: 0, 90, 180, 270.
    pub rotation: i32,
    pub flipped: bool,
    pub offset: (f64, f64),
}

pub struct SlideViewIvars {
    image: RefCell<Option<Retained<NSImage>>>,
    zoom: Cell<Option<f64>>,
    rotation: Cell<i32>,
    flipped: Cell<bool>,
    offset: Cell<(f64, f64)>,
    pub delegate: OnceCell<Retained<AppDelegate>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "WeneSlideView"]
    #[ivars = SlideViewIvars]
    pub struct SlideView;

    impl SlideView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            NSColor::blackColor().setFill();
            objc2_app_kit::NSRectFill(dirty);
            let image = self.ivars().image.borrow();
            let Some(image) = image.as_ref() else { return };
            let size = image.size();
            if size.width <= 0.0 || size.height <= 0.0 {
                return;
            }
            let bounds = self.bounds();
            let scale = self.effective_scale(size, bounds.size);
            let (dx, dy) = self.ivars().offset.get();
            let rotation = self.ivars().rotation.get();
            let flipped = self.ivars().flipped.get();
            let w = size.width * scale;
            let h = size.height * scale;

            // Transform around the view center: pan, rotate, flip,
            // then draw the image centered on the origin.
            let Some(ns_ctx) = NSGraphicsContext::currentContext() else { return };
            let cg = ns_ctx.CGContext();
            let cg = Some(cg.as_ref());
            {
                objc2_core_graphics::CGContext::save_g_state(cg);
                objc2_core_graphics::CGContext::translate_ctm(
                    cg,
                    bounds.origin.x + bounds.size.width / 2.0 + dx,
                    bounds.origin.y + bounds.size.height / 2.0 + dy,
                );
                objc2_core_graphics::CGContext::rotate_ctm(
                    cg,
                    -(rotation as f64).to_radians(),
                );
                if flipped {
                    objc2_core_graphics::CGContext::scale_ctm(cg, -1.0, 1.0);
                }
            }
            let rect = CGRect::new(CGPoint::new(-w / 2.0, -h / 2.0), CGSize::new(w, h));
            image.drawInRect(rect);
            objc2_core_graphics::CGContext::restore_g_state(cg);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let (dx, dy) = self.ivars().offset.get();
            let (ex, ey) = (event.deltaX(), event.deltaY());
            // deltaY is flipped relative to the non-flipped view.
            self.ivars().offset.set((dx + ex, dy - ey));
            self.notify_state_change();
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let Some(delegate) = self.ivars().delegate.get() else {
                return;
            };
            let key_code = event.keyCode();
            let option = event
                .modifierFlags()
                .contains(objc2_app_kit::NSEventModifierFlags::Option);
            let chars = event.charactersIgnoringModifiers()
                .map(|s| s.to_string())
                .unwrap_or_default();
            match key_code {
                53 => return delegate.end_slideshow(), // esc
                123 | 126 if option => return delegate.step_slideshow_original(-1),
                124 | 125 if option => return delegate.step_slideshow_original(1),
                123 | 126 => return delegate.step_slideshow(-1), // left, up
                124 | 125 => return delegate.step_slideshow(1),  // right, down
                115 => return delegate.jump_slideshow_start(),   // home
                119 => return delegate.jump_slideshow_end(),     // end
                _ => {}
            }
            match chars.as_str() {
                "q" => delegate.end_slideshow(),
                " " => delegate.space_pressed(),
                "j" => delegate.step_slideshow(-1), // vim-like previous
                "l" => delegate.step_slideshow(1),  // vim-like next
                "r" => self.rotate(90),
                "R" => self.rotate(-90),
                "f" => self.flip(),
                "i" => delegate.toggle_overlay(),
                "0" => delegate.set_auto_advance(None),
                "!" => delegate.set_auto_advance(Some(0.5)),
                "@" => delegate.set_auto_advance(Some(1.5)),
                d @ ("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9") => {
                    delegate.set_auto_advance(Some(d.parse::<f64>().unwrap()));
                }
                "+" => self.zoom_step(1),
                "-" => self.zoom_step(-1),
                "=" => self.set_zoom(Some(1.0)),
                "*" => {
                    self.apply_state(SlideState::default());
                    self.notify_state_change();
                }
                _ => {
                    let _: () = unsafe { msg_send![super(self), keyDown: event] };
                }
            }
        }
    }
);

impl SlideView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SlideViewIvars {
            image: RefCell::new(None),
            zoom: Cell::new(None),
            rotation: Cell::new(0),
            flipped: Cell::new(false),
            offset: Cell::new((0.0, 0.0)),
            delegate: OnceCell::new(),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn effective_scale(&self, image: CGSize, bounds: CGSize) -> f64 {
        // A quarter-turned image fits by its swapped dimensions.
        let (iw, ih) = if self.ivars().rotation.get() % 180 == 90 {
            (image.height, image.width)
        } else {
            (image.width, image.height)
        };
        match self.ivars().zoom.get() {
            Some(zoom) => zoom,
            None => (bounds.width / iw).min(bounds.height / ih).min(1.0),
        }
    }

    pub fn rotate(&self, delta_degrees: i32) {
        let next = (self.ivars().rotation.get() + delta_degrees).rem_euclid(360);
        self.ivars().rotation.set(next);
        self.notify_state_change();
    }

    pub fn flip(&self) {
        self.ivars().flipped.set(!self.ivars().flipped.get());
        self.notify_state_change();
    }

    fn notify_state_change(&self) {
        self.setNeedsDisplay(true);
        if let Some(delegate) = self.ivars().delegate.get() {
            delegate.slide_state_changed();
        }
    }

    pub fn state(&self) -> SlideState {
        SlideState {
            zoom: self.ivars().zoom.get(),
            rotation: self.ivars().rotation.get(),
            flipped: self.ivars().flipped.get(),
            offset: self.ivars().offset.get(),
        }
    }

    pub fn apply_state(&self, state: SlideState) {
        self.ivars().zoom.set(state.zoom);
        self.ivars().rotation.set(state.rotation);
        self.ivars().flipped.set(state.flipped);
        self.ivars().offset.set(state.offset);
        self.setNeedsDisplay(true);
    }

    fn set_zoom(&self, zoom: Option<f64>) {
        self.ivars().zoom.set(zoom);
        if zoom.is_none() {
            self.ivars().offset.set((0.0, 0.0));
        }
        self.notify_state_change();
    }

    fn zoom_step(&self, direction: i32) {
        let image = self.ivars().image.borrow();
        let Some(image) = image.as_ref() else { return };
        let current = self.effective_scale(image.size(), self.bounds().size);
        let next = if direction > 0 {
            ZOOM_LADDER
                .iter()
                .find(|&&z| z > current * 1.001)
                .copied()
                .unwrap_or(*ZOOM_LADDER.last().unwrap())
        } else {
            ZOOM_LADDER
                .iter()
                .rev()
                .find(|&&z| z < current * 0.999)
                .copied()
                .unwrap_or(*ZOOM_LADDER.first().unwrap())
        };
        self.ivars().zoom.set(Some(next));
        self.notify_state_change();
    }

    pub fn has_image(&self) -> bool {
        self.ivars().image.borrow().is_some()
    }

    /// New slide: reset to defaults. The delegate re-applies any
    /// remembered per-file state afterwards.
    pub fn show_image(&self, image: Retained<NSImage>) {
        *self.ivars().image.borrow_mut() = Some(image);
        self.apply_state(SlideState::default());
    }
}
