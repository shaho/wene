//! Thumbnail grid: custom NSView in an NSScrollView, fixed 160 pt
//! cells, drawRect blitting. Thumbnails are requested visible-first:
//! a cell asks the engine for its thumbnail the first time it is
//! drawn without one.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use wene_core::{file_info_cmp, FileInfo, SortOrder};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSColor, NSEvent, NSImage, NSView};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::NSRect;

use crate::AppDelegate;

pub const CELL: f64 = 160.0;
pub const PAD: f64 = 8.0;
const MIN_CELL: f64 = 60.0;
const MAX_CELL: f64 = 400.0;

pub struct GridIvars {
    pub files: RefCell<Vec<FileInfo>>,
    cell: Cell<f64>,
    thumbs: RefCell<HashMap<PathBuf, Retained<NSImage>>>,
    requested: RefCell<HashSet<PathBuf>>,
    selection: Cell<Option<usize>>,
    pub delegate: OnceCell<Retained<AppDelegate>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "WeneGridView"]
    #[ivars = GridIvars]
    pub struct GridView;

    impl GridView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: CGSize) {
            // Track the scroll view's width; own the height.
            let height = self.content_height(size.width);
            let _: () = unsafe {
                msg_send![super(self), setFrameSize: CGSize::new(size.width, height)]
            };
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            NSColor::windowBackgroundColor().setFill();
            objc2_app_kit::NSRectFill(dirty);

            let files = self.ivars().files.borrow();
            if files.is_empty() {
                return;
            }
            let cols = self.columns(self.bounds().size.width);
            let first_row = ((dirty.origin.y - PAD) / self.pitch()).floor().max(0.0) as usize;
            let last_row =
                ((dirty.origin.y + dirty.size.height) / self.pitch()).ceil() as usize;
            let selection = self.ivars().selection.get();

            for row in first_row..=last_row {
                for col in 0..cols {
                    let index = row * cols + col;
                    if index >= files.len() {
                        return;
                    }
                    let cell = self.cell_rect(index, cols);
                    if selection == Some(index) {
                        NSColor::selectedContentBackgroundColor().setFill();
                        let highlight = CGRect::new(
                            CGPoint::new(cell.origin.x - 3.0, cell.origin.y - 3.0),
                            CGSize::new(cell.size.width + 6.0, cell.size.height + 6.0),
                        );
                        objc2_app_kit::NSRectFill(highlight);
                    }
                    let path = &files[index].path;
                    let thumb = self.ivars().thumbs.borrow().get(path).cloned();
                    match thumb {
                        Some(image) => {
                            let size = image.size();
                            if size.width > 0.0 && size.height > 0.0 {
                                let scale = (cell.size.width / size.width)
                                    .min(cell.size.height / size.height)
                                    .min(1.0);
                                let w = size.width * scale;
                                let h = size.height * scale;
                                let fit = CGRect::new(
                                    CGPoint::new(
                                        cell.origin.x + (cell.size.width - w) / 2.0,
                                        cell.origin.y + (cell.size.height - h) / 2.0,
                                    ),
                                    CGSize::new(w, h),
                                );
                                image.drawInRect(fit);
                            }
                        }
                        None => {
                            NSColor::quaternaryLabelColor().setFill();
                            objc2_app_kit::NSRectFill(cell);
                            let mut requested = self.ivars().requested.borrow_mut();
                            if requested.insert(path.clone()) {
                                if let Some(delegate) = self.ivars().delegate.get() {
                                    delegate.request_thumb(path.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        #[unsafe(method(magnifyWithEvent:))]
        fn magnify_with_event(&self, event: &NSEvent) {
            self.scale_cells(1.0 + event.magnification());
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let cols = self.columns(self.bounds().size.width);
            let col = ((point.x - PAD) / self.pitch()).floor();
            let row = ((point.y - PAD) / self.pitch()).floor();
            if col < 0.0 || row < 0.0 || col >= cols as f64 {
                return;
            }
            let index = row as usize * cols + col as usize;
            if index >= self.ivars().files.borrow().len() {
                return;
            }
            self.select(Some(index));
            if event.clickCount() >= 2 {
                self.activate();
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let count = self.ivars().files.borrow().len();
            let cols = self.columns(self.bounds().size.width) as i64;
            let delta: i64 = match event.keyCode() {
                36 => {
                    // return
                    if self.ivars().selection.get().is_some() {
                        self.activate();
                    }
                    return;
                }
                123 => -1,    // left
                124 => 1,     // right
                126 => -cols, // up
                125 => cols,  // down
                _ => {
                    let _: () = unsafe { msg_send![super(self), keyDown: event] };
                    return;
                }
            };
            if count == 0 {
                return;
            }
            let next = match self.ivars().selection.get() {
                Some(sel) => (sel as i64 + delta).clamp(0, count as i64 - 1) as usize,
                None => 0,
            };
            self.select(Some(next));
            self.scrollRectToVisible(self.cell_rect(next, cols as usize));
        }
    }
);

impl GridView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(GridIvars {
            files: RefCell::new(Vec::new()),
            cell: Cell::new(CELL),
            thumbs: RefCell::new(HashMap::new()),
            requested: RefCell::new(HashSet::new()),
            selection: Cell::new(None),
            delegate: OnceCell::new(),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn pitch(&self) -> f64 {
        self.ivars().cell.get() + PAD
    }

    /// Grow or shrink cells by a factor, clamped, relayout.
    pub fn scale_cells(&self, factor: f64) {
        let next = (self.ivars().cell.get() * factor).clamp(MIN_CELL, MAX_CELL);
        self.ivars().cell.set(next);
        let width = self.frame().size.width;
        self.setFrameSize(CGSize::new(width, 0.0));
        self.setNeedsDisplay(true);
    }

    /// Current sorted paths, for building a slideshow playlist.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.ivars()
            .files
            .borrow()
            .iter()
            .map(|f| f.path.clone())
            .collect()
    }

    /// Re-sort the whole grid, keeping the selection on the same
    /// file.
    pub fn resort(&self, order: SortOrder, descending: bool) {
        {
            let mut files = self.ivars().files.borrow_mut();
            let selected = self.ivars().selection.get().map(|i| files[i].path.clone());
            files.sort_by(|a, b| file_info_cmp(a, b, order, descending));
            if let Some(path) = selected {
                let index = files.iter().position(|f| f.path == path);
                self.ivars().selection.set(index);
            }
        }
        self.setNeedsDisplay(true);
    }

    fn columns(&self, width: f64) -> usize {
        (((width - PAD) / self.pitch()).floor() as usize).max(1)
    }

    fn content_height(&self, width: f64) -> f64 {
        let count = self.ivars().files.borrow().len();
        let cols = self.columns(width);
        let rows = count.div_ceil(cols).max(1);
        rows as f64 * self.pitch() + PAD
    }

    fn cell_rect(&self, index: usize, cols: usize) -> CGRect {
        let row = index / cols;
        let col = index % cols;
        CGRect::new(
            CGPoint::new(PAD + col as f64 * self.pitch(), PAD + row as f64 * self.pitch()),
            CGSize::new(self.ivars().cell.get(), self.ivars().cell.get()),
        )
    }

    fn select(&self, index: Option<usize>) {
        self.ivars().selection.set(index);
        self.setNeedsDisplay(true);
    }

    fn activate(&self) {
        if let (Some(index), Some(delegate)) =
            (self.ivars().selection.get(), self.ivars().delegate.get())
        {
            delegate.start_slideshow(index);
        }
    }

    /// Mirror the core's sorted-by-name inserts. Indices are valid
    /// when applied in order (same rule as the core's event
    /// contract). The caller re-sorts afterwards when a different
    /// sort order is active.
    pub fn insert_files(&self, inserts: Vec<(usize, FileInfo)>) {
        {
            let mut files = self.ivars().files.borrow_mut();
            let mut selection = self.ivars().selection.get();
            for (index, info) in inserts {
                files.insert(index, info);
                if let Some(sel) = selection {
                    if index <= sel {
                        selection = Some(sel + 1);
                    }
                }
            }
            self.ivars().selection.set(selection);
        }
        let width = self.frame().size.width;
        self.setFrameSize(CGSize::new(width, 0.0));
        self.setNeedsDisplay(true);
    }

    pub fn set_thumb(&self, path: PathBuf, image: Retained<NSImage>) {
        self.ivars().thumbs.borrow_mut().insert(path, image);
        self.setNeedsDisplay(true);
    }

    /// Clear everything before a new scan (⌘O on a new folder).
    pub fn reset(&self) {
        self.ivars().files.borrow_mut().clear();
        self.ivars().thumbs.borrow_mut().clear();
        self.ivars().requested.borrow_mut().clear();
        self.ivars().selection.set(None);
        let width = self.frame().size.width;
        self.setFrameSize(CGSize::new(width, 0.0));
        self.setNeedsDisplay(true);
    }
}
