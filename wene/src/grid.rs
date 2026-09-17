//! Thumbnail grid: custom NSView in an NSScrollView, fixed 160 pt
//! cells, drawRect blitting. Thumbnails are requested visible-first:
//! a cell asks the engine for its thumbnail the first time it is
//! drawn without one.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

use wene_core::{file_info_cmp, FileInfo, LruCache, SortOrder};

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
/// Thumbnail memory budget. Evicted cells re-request on next draw.
const THUMB_CACHE_BYTES: usize = 256 * 1024 * 1024;

pub struct GridIvars {
    pub files: RefCell<Vec<FileInfo>>,
    cell: Cell<f64>,
    thumbs: RefCell<LruCache<PathBuf, Retained<NSImage>>>,
    requested: RefCell<HashSet<PathBuf>>,
    /// Multi-selection: indices into `files`.
    selected: RefCell<BTreeSet<usize>>,
    /// Keyboard focus / last clicked; also the shift-range anchor's
    /// counterpart.
    focus: Cell<Option<usize>>,
    anchor: Cell<Option<usize>>,
    /// Rubber-band drag state (origin, current point) in view coords.
    band: Cell<Option<(CGPoint, CGPoint)>>,
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
            let selected = self.ivars().selected.borrow().clone();

            'rows: for row in first_row..=last_row {
                for col in 0..cols {
                    let index = row * cols + col;
                    if index >= files.len() {
                        break 'rows;
                    }
                    let cell = self.cell_rect(index, cols);
                    if selected.contains(&index) {
                        NSColor::selectedContentBackgroundColor().setFill();
                        let highlight = CGRect::new(
                            CGPoint::new(cell.origin.x - 3.0, cell.origin.y - 3.0),
                            CGSize::new(cell.size.width + 6.0, cell.size.height + 6.0),
                        );
                        objc2_app_kit::NSRectFill(highlight);
                    }
                    let path = &files[index].path;
                    let thumb = self.ivars().thumbs.borrow_mut().get(path).cloned();
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
            // Rubber band on top.
            if let Some((a, b)) = self.ivars().band.get() {
                let rect = band_rect(a, b);
                NSColor::selectedContentBackgroundColor()
                    .colorWithAlphaComponent(0.25)
                    .setFill();
                objc2_app_kit::NSRectFill(rect);
            }
        }

        #[unsafe(method(magnifyWithEvent:))]
        fn magnify_with_event(&self, event: &NSEvent) {
            self.scale_cells(1.0 + event.magnification());
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let flags = event.modifierFlags();
            let cmd = flags.contains(objc2_app_kit::NSEventModifierFlags::Command);
            let shift = flags.contains(objc2_app_kit::NSEventModifierFlags::Shift);

            let Some(index) = self.index_at(point) else {
                // Empty space: start a rubber band (cmd keeps the
                // existing selection as a base).
                if !cmd {
                    self.ivars().selected.borrow_mut().clear();
                }
                self.ivars().band.set(Some((point, point)));
                self.setNeedsDisplay(true);
                return;
            };

            if shift {
                let anchor = self.ivars().anchor.get().unwrap_or(index);
                let (lo, hi) = (anchor.min(index), anchor.max(index));
                let mut selected = self.ivars().selected.borrow_mut();
                selected.clear();
                selected.extend(lo..=hi);
            } else if cmd {
                let mut selected = self.ivars().selected.borrow_mut();
                if !selected.remove(&index) {
                    selected.insert(index);
                }
                self.ivars().anchor.set(Some(index));
            } else {
                let mut selected = self.ivars().selected.borrow_mut();
                selected.clear();
                selected.insert(index);
                self.ivars().anchor.set(Some(index));
            }
            self.ivars().focus.set(Some(index));
            self.setNeedsDisplay(true);
            if event.clickCount() >= 2 {
                self.activate();
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let Some((origin, _)) = self.ivars().band.get() else { return };
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            self.ivars().band.set(Some((origin, point)));
            self.select_band(band_rect(origin, point));
            self.autoscroll(event);
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            if self.ivars().band.get().is_some() {
                self.ivars().band.set(None);
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let count = self.ivars().files.borrow().len();
            let cols = self.columns(self.bounds().size.width) as i64;
            let delta: i64 = match event.keyCode() {
                36 => {
                    // return
                    if self.ivars().focus.get().is_some()
                        || !self.ivars().selected.borrow().is_empty()
                    {
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
            let next = match self.ivars().focus.get() {
                Some(focus) => (focus as i64 + delta).clamp(0, count as i64 - 1) as usize,
                None => 0,
            };
            {
                let mut selected = self.ivars().selected.borrow_mut();
                selected.clear();
                selected.insert(next);
            }
            self.ivars().focus.set(Some(next));
            self.ivars().anchor.set(Some(next));
            self.setNeedsDisplay(true);
            self.scrollRectToVisible(self.cell_rect(next, cols as usize));
        }
    }
);

fn band_rect(a: CGPoint, b: CGPoint) -> CGRect {
    CGRect::new(
        CGPoint::new(a.x.min(b.x), a.y.min(b.y)),
        CGSize::new((a.x - b.x).abs(), (a.y - b.y).abs()),
    )
}

impl GridView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(GridIvars {
            files: RefCell::new(Vec::new()),
            cell: Cell::new(CELL),
            thumbs: RefCell::new(LruCache::new(THUMB_CACHE_BYTES)),
            requested: RefCell::new(HashSet::new()),
            selected: RefCell::new(BTreeSet::new()),
            focus: Cell::new(None),
            anchor: Cell::new(None),
            band: Cell::new(None),
            delegate: OnceCell::new(),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Cell index under a view point, if any.
    fn index_at(&self, point: CGPoint) -> Option<usize> {
        let cols = self.columns(self.bounds().size.width);
        let col = ((point.x - PAD) / self.pitch()).floor();
        let row = ((point.y - PAD) / self.pitch()).floor();
        if col < 0.0 || row < 0.0 || col >= cols as f64 {
            return None;
        }
        // Only count hits inside the cell, not the padding gutter.
        let cell = self.ivars().cell.get();
        let in_x = point.x - PAD - col * self.pitch();
        let in_y = point.y - PAD - row * self.pitch();
        if in_x > cell || in_y > cell {
            return None;
        }
        let index = row as usize * cols + col as usize;
        (index < self.ivars().files.borrow().len()).then_some(index)
    }

    /// Select every cell intersecting the rubber-band rect.
    fn select_band(&self, rect: CGRect) {
        let cols = self.columns(self.bounds().size.width);
        let count = self.ivars().files.borrow().len();
        let mut selected = self.ivars().selected.borrow_mut();
        selected.clear();
        for index in 0..count {
            let cell = self.cell_rect(index, cols);
            let intersects = cell.origin.x < rect.origin.x + rect.size.width
                && rect.origin.x < cell.origin.x + cell.size.width
                && cell.origin.y < rect.origin.y + rect.size.height
                && rect.origin.y < cell.origin.y + cell.size.height;
            if intersects {
                selected.insert(index);
            }
        }
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
    /// files.
    pub fn resort(&self, order: SortOrder, descending: bool) {
        {
            let mut files = self.ivars().files.borrow_mut();
            let selected_paths: Vec<PathBuf> = {
                let selected = self.ivars().selected.borrow();
                selected.iter().map(|&i| files[i].path.clone()).collect()
            };
            let focus_path = self.ivars().focus.get().map(|i| files[i].path.clone());
            files.sort_by(|a, b| file_info_cmp(a, b, order, descending));
            let mut selected = self.ivars().selected.borrow_mut();
            selected.clear();
            for path in &selected_paths {
                if let Some(i) = files.iter().position(|f| &f.path == path) {
                    selected.insert(i);
                }
            }
            self.ivars()
                .focus
                .set(focus_path.and_then(|p| files.iter().position(|f| f.path == p)));
            self.ivars().anchor.set(self.ivars().focus.get());
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

    fn activate(&self) {
        if let Some(delegate) = self.ivars().delegate.get() {
            let (files, start) = self.slideshow_request();
            if !files.is_empty() {
                delegate.start_slideshow_files(files, start);
            }
        }
    }

    /// The original app's selection semantics: none or one selected
    /// plays everything (starting at the selection); several selected
    /// play just those, starting at the focused one.
    pub fn slideshow_request(&self) -> (Vec<PathBuf>, usize) {
        let selected = self.ivars().selected.borrow();
        if selected.len() >= 2 {
            let files = self.ivars().files.borrow();
            let paths: Vec<PathBuf> = selected.iter().map(|&i| files[i].path.clone()).collect();
            let start = self
                .ivars()
                .focus
                .get()
                .and_then(|f| selected.iter().position(|&i| i == f))
                .unwrap_or(0);
            (paths, start)
        } else {
            let start = self
                .ivars()
                .focus
                .get()
                .or_else(|| selected.iter().next().copied())
                .unwrap_or(0);
            (self.paths(), start)
        }
    }

    /// Mirror the core's sorted-by-name inserts. Indices are valid
    /// when applied in order (same rule as the core's event
    /// contract). The caller re-sorts afterwards when a different
    /// sort order is active.
    pub fn insert_files(&self, inserts: Vec<(usize, FileInfo)>) {
        {
            let mut files = self.ivars().files.borrow_mut();
            let mut selected = self.ivars().selected.borrow_mut();
            for (index, info) in inserts {
                files.insert(index, info);
                *selected = selected
                    .iter()
                    .map(|&i| if i >= index { i + 1 } else { i })
                    .collect();
                for slot in [&self.ivars().focus, &self.ivars().anchor] {
                    if let Some(v) = slot.get() {
                        if v >= index {
                            slot.set(Some(v + 1));
                        }
                    }
                }
            }
        }
        let width = self.frame().size.width;
        self.setFrameSize(CGSize::new(width, 0.0));
        self.setNeedsDisplay(true);
    }

    /// Watcher removals: drop rows, remap selection and focus.
    pub fn remove_paths(&self, paths: &[PathBuf]) {
        {
            let mut files = self.ivars().files.borrow_mut();
            for path in paths {
                let Some(index) = files.iter().position(|f| &f.path == path) else {
                    continue;
                };
                files.remove(index);
                self.ivars().thumbs.borrow_mut().remove(path);
                self.ivars().requested.borrow_mut().remove(path);
                let mut selected = self.ivars().selected.borrow_mut();
                *selected = selected
                    .iter()
                    .filter(|&&i| i != index)
                    .map(|&i| if i > index { i - 1 } else { i })
                    .collect();
                let len = files.len();
                for slot in [&self.ivars().focus, &self.ivars().anchor] {
                    match slot.get() {
                        Some(v) if v > index => slot.set(Some(v - 1)),
                        Some(v) if v == index => {
                            slot.set((len > 0).then(|| index.min(len - 1)))
                        }
                        _ => {}
                    }
                }
            }
        }
        let width = self.frame().size.width;
        self.setFrameSize(CGSize::new(width, 0.0));
        self.setNeedsDisplay(true);
    }

    /// Test hook: set the selection directly.
    pub fn e2e_set_selected(&self, indices: &[usize]) {
        let mut selected = self.ivars().selected.borrow_mut();
        selected.clear();
        selected.extend(indices.iter().copied());
        self.ivars().focus.set(indices.first().copied());
        self.ivars().anchor.set(indices.first().copied());
    }

    /// A file changed on disk: forget its thumbnail so the next draw
    /// re-requests it.
    pub fn invalidate_thumb(&self, path: &PathBuf) {
        self.ivars().thumbs.borrow_mut().remove(path);
        self.ivars().requested.borrow_mut().remove(path);
        self.setNeedsDisplay(true);
    }

    pub fn set_thumb(&self, path: PathBuf, image: Retained<NSImage>, cost: usize) {
        let evicted = self.ivars().thumbs.borrow_mut().insert(path, image, cost);
        // Evicted cells must re-request when they scroll back in.
        let mut requested = self.ivars().requested.borrow_mut();
        for path in evicted {
            requested.remove(&path);
        }
        self.setNeedsDisplay(true);
    }

    /// Clear everything before a new scan (⌘O on a new folder).
    pub fn reset(&self) {
        self.ivars().files.borrow_mut().clear();
        self.ivars().thumbs.borrow_mut().clear();
        self.ivars().requested.borrow_mut().clear();
        self.ivars().selected.borrow_mut().clear();
        self.ivars().focus.set(None);
        self.ivars().anchor.set(None);
        let width = self.frame().size.width;
        self.setFrameSize(CGSize::new(width, 0.0));
        self.setNeedsDisplay(true);
    }
}
