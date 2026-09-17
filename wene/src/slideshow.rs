//! Fullscreen slideshow: borderless key window + slide view with
//! fit-to-window display, zoom ladder, and drag panning.

use std::cell::{Cell, OnceCell, RefCell};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSEvent, NSImage, NSView, NSWindow, NSWindowStyleMask,
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

pub struct SlideViewIvars {
    image: RefCell<Option<Retained<NSImage>>>,
    /// None = fit to window (never scaling up past 100%).
    zoom: Cell<Option<f64>>,
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
            let w = size.width * scale;
            let h = size.height * scale;
            let rect = CGRect::new(
                CGPoint::new(
                    bounds.origin.x + (bounds.size.width - w) / 2.0 + dx,
                    bounds.origin.y + (bounds.size.height - h) / 2.0 + dy,
                ),
                CGSize::new(w, h),
            );
            image.drawInRect(rect);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let (dx, dy) = self.ivars().offset.get();
            let (ex, ey) = (event.deltaX(), event.deltaY());
            // deltaY is flipped relative to the non-flipped view.
            self.ivars().offset.set((dx + ex, dy - ey));
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let Some(delegate) = self.ivars().delegate.get() else {
                return;
            };
            let key_code = event.keyCode();
            let chars = event.charactersIgnoringModifiers()
                .map(|s| s.to_string())
                .unwrap_or_default();
            match key_code {
                53 => return delegate.end_slideshow(),          // esc
                123 | 126 => return delegate.step_slideshow(-1), // left, up
                124 | 125 => return delegate.step_slideshow(1),  // right, down
                115 => return delegate.jump_slideshow_start(),   // home
                119 => return delegate.jump_slideshow_end(),     // end
                _ => {}
            }
            match chars.as_str() {
                "q" => delegate.end_slideshow(),
                " " => delegate.step_slideshow(1),
                "+" => self.zoom_step(1),
                "-" => self.zoom_step(-1),
                "=" => self.set_zoom(Some(1.0)),
                "*" => self.set_zoom(None),
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
            offset: Cell::new((0.0, 0.0)),
            delegate: OnceCell::new(),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn effective_scale(&self, image: CGSize, bounds: CGSize) -> f64 {
        match self.ivars().zoom.get() {
            Some(zoom) => zoom,
            None => (bounds.width / image.width)
                .min(bounds.height / image.height)
                .min(1.0),
        }
    }

    fn set_zoom(&self, zoom: Option<f64>) {
        self.ivars().zoom.set(zoom);
        if zoom.is_none() {
            self.ivars().offset.set((0.0, 0.0));
        }
        self.setNeedsDisplay(true);
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
        self.setNeedsDisplay(true);
    }

    /// New slide: reset zoom and pan (per-file zoom memory is a
    /// next-map feature).
    pub fn show_image(&self, image: Retained<NSImage>) {
        *self.ivars().image.borrow_mut() = Some(image);
        self.ivars().zoom.set(None);
        self.ivars().offset.set((0.0, 0.0));
        self.setNeedsDisplay(true);
    }
}
