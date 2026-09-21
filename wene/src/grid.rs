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
/// Height of the filename strip under a cell when labels are on.
const LABEL_H: f64 = 16.0;
/// Thumbnail memory budget. Evicted cells re-request on next draw.
const THUMB_CACHE_BYTES: usize = 256 * 1024 * 1024;

pub struct GridIvars {
    /// What the grid draws: the images the filter lets through. With
    /// no filter this is every image in the folder.
    pub files: RefCell<Vec<FileInfo>>,
    /// Every image in the folder, in sort order.
    all: RefCell<Vec<FileInfo>>,
    /// Lower-cased substring of the filename. Empty means no filter.
    filter: RefCell<String>,
    cell: Cell<f64>,
    labels: Cell<bool>,
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
            let first_row = ((dirty.origin.y - PAD) / self.row_pitch()).floor().max(0.0) as usize;
            let last_row =
                ((dirty.origin.y + dirty.size.height) / self.row_pitch()).ceil() as usize;
            let selected = self.ivars().selected.borrow().clone();
            let labels = self.ivars().labels.get();

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
                    if labels {
                        let name = files[index]
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let rect = CGRect::new(
                            CGPoint::new(cell.origin.x, cell.origin.y + cell.size.height + 1.0),
                            CGSize::new(cell.size.width, LABEL_H - 2.0),
                        );
                        draw_label(&name, rect, selected.contains(&index));
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
                self.notify_selection();
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
            self.notify_selection();
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
            self.notify_selection();
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            if self.ivars().band.get().is_some() {
                self.ivars().band.set(None);
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(selectAll:))]
        fn select_all_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.select_all();
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
                53 => {
                    // escape
                    if let Some(delegate) = self.ivars().delegate.get() {
                        delegate.escape_pressed();
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
            self.notify_selection();
        }
    }
);

/// Filename under a cell: small system font, centered, middle
/// truncation like Finder.
fn draw_label(name: &str, rect: CGRect, selected: bool) {
    use objc2_app_kit::{
        NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSLineBreakMode,
        NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSStringDrawing, NSTextAlignment,
    };
    use objc2_foundation::{NSDictionary, NSString};

    let style = NSMutableParagraphStyle::new();
    style.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
    style.setAlignment(NSTextAlignment::Center);
    let font = NSFont::systemFontOfSize(11.0);
    let color = if selected {
        NSColor::labelColor()
    } else {
        NSColor::secondaryLabelColor()
    };
    let attrs = unsafe {
        NSDictionary::from_slices(
            &[
                NSFontAttributeName,
                NSForegroundColorAttributeName,
                NSParagraphStyleAttributeName,
            ],
            &[
                font.as_ref() as &objc2::runtime::AnyObject,
                color.as_ref(),
                style.as_ref(),
            ],
        )
    };
    unsafe { NSString::from_str(name).drawInRect_withAttributes(rect, Some(&attrs)) };
}

/// Case-insensitive substring of the filename. An empty filter
/// matches everything.
fn matches_filter(info: &FileInfo, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    info.path
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase().contains(filter))
        .unwrap_or(false)
}

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
            all: RefCell::new(Vec::new()),
            filter: RefCell::new(String::new()),
            cell: Cell::new(CELL),
            labels: Cell::new(false),
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

    /// Cell index under a view point, if any. The label strip counts
    /// as part of its cell.
    fn index_at(&self, point: CGPoint) -> Option<usize> {
        let cols = self.columns(self.bounds().size.width);
        let col = ((point.x - PAD) / self.pitch()).floor();
        let row = ((point.y - PAD) / self.row_pitch()).floor();
        if col < 0.0 || row < 0.0 || col >= cols as f64 {
            return None;
        }
        // Only count hits inside the cell, not the padding gutter.
        let cell = self.ivars().cell.get();
        let hit_h = cell + if self.ivars().labels.get() { LABEL_H } else { 0.0 };
        let in_x = point.x - PAD - col * self.pitch();
        let in_y = point.y - PAD - row * self.row_pitch();
        if in_x > cell || in_y > hit_h {
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

    /// Horizontal spacing between cells.
    fn pitch(&self) -> f64 {
        self.ivars().cell.get() + PAD
    }

    /// Vertical spacing: adds the label strip when labels are on.
    fn row_pitch(&self) -> f64 {
        self.pitch() + if self.ivars().labels.get() { LABEL_H } else { 0.0 }
    }

    pub fn labels_visible(&self) -> bool {
        self.ivars().labels.get()
    }

    pub fn set_labels(&self, visible: bool) {
        self.ivars().labels.set(visible);
        let width = self.frame().size.width;
        self.setFrameSize(CGSize::new(width, 0.0));
        self.setNeedsDisplay(true);
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
        self.ivars()
            .all
            .borrow_mut()
            .sort_by(|a, b| file_info_cmp(a, b, order, descending));
        self.rebuild_visible();
    }

    /// Narrow the grid to filenames holding `text`, case ignored. An
    /// empty text shows the whole folder again.
    pub fn set_filter(&self, text: &str) {
        *self.ivars().filter.borrow_mut() = text.to_lowercase();
        self.rebuild_visible();
    }

    pub fn filtering(&self) -> bool {
        !self.ivars().filter.borrow().is_empty()
    }

    /// (shown, in the folder). They differ while a filter is on.
    pub fn counts(&self) -> (usize, usize) {
        (
            self.ivars().files.borrow().len(),
            self.ivars().all.borrow().len(),
        )
    }

    /// Work out what the grid shows, keeping the selection, the
    /// focus, and the anchor on the same files. Every change to the
    /// folder or the filter ends here.
    ///
    /// It copies the file list each time, which a scan does once per
    /// batch of 64 files. If that ever costs too much, the list of
    /// what is shown can become indices into `all` instead.
    fn rebuild_visible(&self) {
        {
            let (selected_paths, focus_path, anchor_path) = {
                let files = self.ivars().files.borrow();
                let selected = self.ivars().selected.borrow();
                let paths: Vec<PathBuf> = selected
                    .iter()
                    .filter_map(|&i| files.get(i).map(|f| f.path.clone()))
                    .collect();
                let by_index = |slot: &Cell<Option<usize>>| {
                    slot.get().and_then(|i| files.get(i).map(|f| f.path.clone()))
                };
                (
                    paths,
                    by_index(&self.ivars().focus),
                    by_index(&self.ivars().anchor),
                )
            };
            let filter = self.ivars().filter.borrow().clone();
            let visible: Vec<FileInfo> = self
                .ivars()
                .all
                .borrow()
                .iter()
                .filter(|info| matches_filter(info, &filter))
                .cloned()
                .collect();
            let mut selected = self.ivars().selected.borrow_mut();
            selected.clear();
            for path in &selected_paths {
                if let Some(index) = visible.iter().position(|f| &f.path == path) {
                    selected.insert(index);
                }
            }
            self.ivars()
                .focus
                .set(focus_path.and_then(|p| visible.iter().position(|f| f.path == p)));
            // The anchor is the other end of a shift-selection, so it
            // keeps its own file rather than snapping to the focus.
            self.ivars().anchor.set(
                anchor_path
                    .and_then(|p| visible.iter().position(|f| f.path == p))
                    .or_else(|| self.ivars().focus.get()),
            );
            *self.ivars().files.borrow_mut() = visible;
        }
        let width = self.frame().size.width;
        self.setFrameSize(CGSize::new(width, 0.0));
        self.setNeedsDisplay(true);
        self.notify_selection();
    }

    fn columns(&self, width: f64) -> usize {
        (((width - PAD) / self.pitch()).floor() as usize).max(1)
    }

    fn content_height(&self, width: f64) -> f64 {
        let count = self.ivars().files.borrow().len();
        let cols = self.columns(width);
        let rows = count.div_ceil(cols).max(1);
        rows as f64 * self.row_pitch() + PAD
    }

    fn cell_rect(&self, index: usize, cols: usize) -> CGRect {
        let row = index / cols;
        let col = index % cols;
        CGRect::new(
            CGPoint::new(
                PAD + col as f64 * self.pitch(),
                PAD + row as f64 * self.row_pitch(),
            ),
            CGSize::new(self.ivars().cell.get(), self.ivars().cell.get()),
        )
    }

    /// Total file count plus the selected files' info, for the
    /// status bar.
    pub fn selection_info(&self) -> (usize, Vec<FileInfo>) {
        let files = self.ivars().files.borrow();
        let selected = self.ivars().selected.borrow();
        let infos = selected.iter().map(|&i| files[i].clone()).collect();
        (files.len(), infos)
    }

    fn notify_selection(&self) {
        if let Some(delegate) = self.ivars().delegate.get() {
            delegate.selection_changed();
        }
    }

    /// Start a slideshow in the preferred mode; ⌥ held inverts it.
    fn activate(&self) {
        if let Some(delegate) = self.ivars().delegate.get() {
            let (files, start) = self.slideshow_request();
            let option = NSEvent::modifierFlags_class()
                .contains(objc2_app_kit::NSEventModifierFlags::Option);
            let windowed = delegate.default_windowed() != option;
            if !files.is_empty() {
                delegate.start_slideshow_files(files, start, windowed);
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
            let mut all = self.ivars().all.borrow_mut();
            for (index, info) in inserts {
                let at = index.min(all.len());
                all.insert(at, info);
            }
        }
        self.rebuild_visible();
    }

    /// Removals from disk: the files leave the folder, and the
    /// selection and focus follow the survivors. A removal the app
    /// already applied changes nothing, and must not redraw or
    /// re-announce the selection: the status bar is showing what the
    /// app just did.
    pub fn remove_paths(&self, paths: &[PathBuf]) {
        // Where the focus was, in case the focused file is one of
        // the ones going.
        let focus_index = self.ivars().focus.get();
        let mut removed = 0;
        {
            let mut all = self.ivars().all.borrow_mut();
            for path in paths {
                let Some(index) = all.iter().position(|f| &f.path == path) else {
                    continue;
                };
                all.remove(index);
                removed += 1;
                self.ivars().thumbs.borrow_mut().remove(path);
                self.ivars().requested.borrow_mut().remove(path);
            }
        }
        if removed == 0 {
            return;
        }
        // The selection follows the surviving files by path.
        self.rebuild_visible();
        // The focused file itself may be gone. Keep the focus where
        // it was in the grid, on whatever slid into that place.
        let count = self.ivars().files.borrow().len();
        if self.ivars().focus.get().is_none() && count > 0 {
            if let Some(index) = focus_index {
                let index = index.min(count - 1);
                self.ivars().focus.set(Some(index));
                self.ivars().anchor.set(Some(index));
            }
        }
    }

    /// The files the user picked, for an action that works on them.
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        let files = self.ivars().files.borrow();
        self.ivars()
            .selected
            .borrow()
            .iter()
            .filter_map(|&i| files.get(i).map(|f| f.path.clone()))
            .collect()
    }

    /// Drop rows the app itself removed (trashed or moved away) and
    /// select the image that slid into the last one's place, so
    /// holding the keys works through a run. Removing the last image
    /// steps back instead.
    pub fn remove_and_advance(&self, paths: &[PathBuf]) {
        let follower = {
            let files = self.ivars().files.borrow();
            let mut indices: Vec<usize> = paths
                .iter()
                .filter_map(|path| files.iter().position(|f| &f.path == path))
                .collect();
            indices.sort_unstable();
            indices
                .last()
                .map(|&last| (last + 1).saturating_sub(indices.len()))
        };
        self.remove_paths(paths);
        let count = self.ivars().files.borrow().len();
        let Some(follower) = follower.filter(|_| count > 0) else {
            self.notify_selection();
            return;
        };
        let index = follower.min(count - 1);
        {
            let mut selected = self.ivars().selected.borrow_mut();
            selected.clear();
            selected.insert(index);
        }
        self.ivars().focus.set(Some(index));
        self.ivars().anchor.set(Some(index));
        let cols = self.columns(self.bounds().size.width);
        self.scrollRectToVisible(self.cell_rect(index, cols));
        self.setNeedsDisplay(true);
        self.notify_selection();
    }

    /// Everything the grid shows, which with a filter on is only
    /// the matches.
    pub fn select_all(&self) {
        let count = self.ivars().files.borrow().len();
        if count == 0 {
            return;
        }
        {
            let mut selected = self.ivars().selected.borrow_mut();
            selected.clear();
            selected.extend(0..count);
        }
        self.ivars().focus.set(Some(0));
        self.ivars().anchor.set(Some(0));
        self.setNeedsDisplay(true);
        self.notify_selection();
    }

    /// Whether every one of these files is in the grid right now.
    pub fn holds_all(&self, paths: &[PathBuf]) -> bool {
        let files = self.ivars().files.borrow();
        paths
            .iter()
            .all(|path| files.iter().any(|f| &f.path == path))
    }

    /// Select exactly these files, as far as the grid holds them,
    /// and put the focus on the first one it found.
    pub fn select_paths(&self, paths: &[PathBuf]) {
        let indices: Vec<usize> = {
            let files = self.ivars().files.borrow();
            paths
                .iter()
                .filter_map(|path| files.iter().position(|f| &f.path == path))
                .collect()
        };
        {
            let mut selected = self.ivars().selected.borrow_mut();
            selected.clear();
            selected.extend(indices.iter().copied());
        }
        let first = indices.first().copied();
        self.ivars().focus.set(first);
        self.ivars().anchor.set(first);
        if let Some(index) = first {
            let cols = self.columns(self.bounds().size.width);
            self.scrollRectToVisible(self.cell_rect(index, cols));
        }
        self.setNeedsDisplay(true);
        self.notify_selection();
    }

    /// Test hook: set the selection directly.
    pub fn e2e_set_selected(&self, indices: &[usize]) {
        {
            let mut selected = self.ivars().selected.borrow_mut();
            selected.clear();
            selected.extend(indices.iter().copied());
        }
        self.ivars().focus.set(indices.first().copied());
        self.ivars().anchor.set(indices.first().copied());
        self.notify_selection();
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
        self.ivars().all.borrow_mut().clear();
        self.ivars().filter.borrow_mut().clear();
        self.ivars().thumbs.borrow_mut().clear();
        self.ivars().requested.borrow_mut().clear();
        self.ivars().selected.borrow_mut().clear();
        self.ivars().focus.set(None);
        self.ivars().anchor.set(None);
        let width = self.frame().size.width;
        self.setFrameSize(CGSize::new(width, 0.0));
        self.setNeedsDisplay(true);
        self.notify_selection();
    }
}
