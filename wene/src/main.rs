//! macOS shell: app delegate, menu, browse window, event pump.

mod decoder;
mod e2e;
mod grid;
mod sidebar;
mod slideshow;
mod trash;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Mutex, OnceLock};

use dispatch2::{DispatchQueue, MainThreadBound};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{
    define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType,
    NSColor, NSImage, NSMenu, NSMenuItem, NSOpenPanel, NSScreen, NSScrollView,
    NSSplitViewController, NSSplitViewItem, NSTextField, NSViewController, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGSize};
use objc2_core_graphics::CGImage;
use objc2_foundation::{
    ns_string, NSNotification, NSObject, NSObjectProtocol, NSRect, NSString, NSTimer,
    NSUserDefaults,
};
use wene_core::{Engine, Event, LruCache, Playlist, SortOrder};

use decoder::ImageIoDecoder;
use grid::GridView;
use sidebar::Sidebar;
use slideshow::{SlideState, SlideView, SlideshowWindow};

type Img = CFRetained<CGImage>;

/// Slide memory budget: roughly 15 retina-screen slides, so stepping
/// back through recent history never re-decodes.
const SLIDE_CACHE_BYTES: usize = 512 * 1024 * 1024;

const STATUS_H: f64 = 24.0;

static EVENTS: OnceLock<Mutex<Receiver<Event<Img>>>> = OnceLock::new();
static DELEGATE: OnceLock<MainThreadBound<Retained<AppDelegate>>> = OnceLock::new();

struct Show {
    window: Retained<SlideshowWindow>,
    view: Retained<SlideView>,
    overlay: Retained<NSTextField>,
    playlist: Playlist,
    cache: LruCache<PathBuf, Retained<NSImage>>,
    /// Remembered zoom/rotation/flip/pan per file for this session.
    view_states: HashMap<PathBuf, SlideState>,
    /// Auto-advance seconds; None = off. `timer` is live only while
    /// advancing (paused = interval set, timer gone).
    interval: Option<f64>,
    timer: Option<Retained<NSTimer>>,
    /// A message shown over the slide for a moment, in place of the
    /// usual overlay line. Fullscreen has no status bar.
    flash: Option<String>,
    flash_timer: Option<Retained<NSTimer>>,
}

pub struct DelegateIvars {
    engine: OnceCell<Engine<Img>>,
    window: OnceCell<Retained<NSWindow>>,
    grid: OnceCell<Retained<GridView>>,
    sidebar: OnceCell<Retained<Sidebar>>,
    split: OnceCell<Retained<NSSplitViewController>>,
    /// The folder the grid shows and whether it was scanned deep, to
    /// skip no-op rescans.
    current_scan: RefCell<Option<(PathBuf, bool)>>,
    sidebar_item: OnceCell<Retained<NSMenuItem>>,
    prefs_window: OnceCell<Retained<NSWindow>>,
    /// Default slideshow mode from prefs; ⌥ at start inverts it.
    default_windowed: Cell<bool>,
    show: RefCell<Option<Show>>,
    loop_enabled: Cell<bool>,
    shuffle_enabled: Cell<bool>,
    overlay_visible: Cell<bool>,
    status: OnceCell<Retained<NSTextField>>,
    loop_item: OnceCell<Retained<NSMenuItem>>,
    shuffle_item: OnceCell<Retained<NSMenuItem>>,
    labels_item: OnceCell<Retained<NSMenuItem>>,
    auto_menu: OnceCell<Retained<NSMenu>>,
    sort_menu: OnceCell<Retained<NSMenu>>,
    sort_order: Cell<SortOrder>,
    sort_desc: Cell<bool>,
    /// Pace applied to new slideshows; menu picks update it.
    default_interval: Cell<Option<f64>>,
    e2e: RefCell<e2e::E2eState>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WeneAppDelegate"]
    #[ivars = DelegateIvars]
    pub struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSWindowDelegate for AppDelegate {
        // Only slideshow windows set us as their delegate. Covers the
        // close button in windowed mode; end_slideshow routes through
        // here too via close().
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            if let Some(show) = self.ivars().show.borrow_mut().take() {
                if let Some(timer) = show.timer {
                    timer.invalidate();
                }
                // The frame timer retains the view; stop it so the
                // view can die with the window.
                show.view.stop_animation();
            }
            if let Some(window) = self.ivars().window.get() {
                window.makeKeyAndOrderFront(None);
            }
        }
    }

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            self.load_prefs();
            // A folder argument skips everything else (useful for
            // scripted runs). Otherwise restore the last folder, then
            // the startup-folder preference, then Pictures.
            let arg = std::env::args().nth(1).map(PathBuf::from).filter(|p| p.is_dir());
            if let Some(root) = arg {
                self.scan_root(root, true);
                return;
            }
            if let Some(root) = last_folder_pref().filter(|p| p.is_dir()) {
                self.scan_root(root, false);
                return;
            }
            let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
            let fallback = startup_folder_pref()
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .or(Some(home.join("Pictures")).filter(|p| p.is_dir()));
            // Nothing readable: leave the grid empty with the tree
            // shown and nothing selected.
            if let Some(root) = fallback {
                self.scan_root_inner(root, false, false);
            }
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn terminate_after_last_window(&self, _app: &NSApplication) -> bool {
            true
        }
    }

    impl AppDelegate {
        #[unsafe(method(openDocument:))]
        fn open_document(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.open_folder();
        }

        #[unsafe(method(startSlideshow:))]
        fn start_slideshow_menu(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.start_from_grid(false);
        }

        #[unsafe(method(startSlideshowWindowed:))]
        fn start_slideshow_windowed_menu(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.start_from_grid(true);
        }

        #[unsafe(method(toggleLoop:))]
        fn toggle_loop(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let enabled = !self.ivars().loop_enabled.get();
            self.ivars().loop_enabled.set(enabled);
            if let Some(item) = self.ivars().loop_item.get() {
                item.setState(if enabled { 1 } else { 0 });
            }
            if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
                show.playlist.looping = enabled;
            }
            self.update_overlay();
            self.save_prefs();
        }

        #[unsafe(method(toggleShuffle:))]
        fn toggle_shuffle(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let enabled = !self.ivars().shuffle_enabled.get();
            self.ivars().shuffle_enabled.set(enabled);
            if let Some(item) = self.ivars().shuffle_item.get() {
                item.setState(if enabled { 1 } else { 0 });
            }
            if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
                show.playlist.set_shuffled(enabled);
            }
            self.update_overlay();
            self.save_prefs();
        }

        #[unsafe(method(setAutoAdvanceMenu:))]
        fn set_auto_advance_menu(&self, sender: Option<&objc2::runtime::AnyObject>) {
            // Item tag = tenths of a second; 0 = off.
            let tag = sender
                .and_then(|s| s.downcast_ref::<NSMenuItem>())
                .map(|item| item.tag())
                .unwrap_or(0);
            let seconds = (tag > 0).then(|| tag as f64 / 10.0);
            self.set_auto_advance(seconds);
        }

        #[unsafe(method(setSortMenu:))]
        fn set_sort_menu(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<NSMenuItem>())
                .map(|item| item.tag())
                .unwrap_or(1);
            let order = match tag {
                2 => SortOrder::Modified,
                3 => SortOrder::Size,
                4 => SortOrder::Path,
                5 => SortOrder::ExifDate,
                6 => SortOrder::Added,
                _ => SortOrder::Name,
            };
            // Re-picking the current order reverses it, like the
            // original app. A new order starts ascending, except
            // dates, which start newest first.
            if self.ivars().sort_order.get() == order {
                self.ivars().sort_desc.set(!self.ivars().sort_desc.get());
            } else {
                self.ivars().sort_order.set(order);
                self.ivars().sort_desc.set(matches!(
                    order,
                    SortOrder::Modified | SortOrder::ExifDate | SortOrder::Added
                ));
            }
            self.apply_sort();
        }

        #[unsafe(method(showPrefs:))]
        fn show_prefs_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.show_prefs();
        }

        #[unsafe(method(prefsToggleWindowed:))]
        fn prefs_toggle_windowed(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.ivars()
                .default_windowed
                .set(!self.ivars().default_windowed.get());
            self.save_prefs();
        }

        #[unsafe(method(prefsAutoAdvance:))]
        fn prefs_auto_advance(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<objc2_app_kit::NSPopUpButton>())
                .map(|p| p.selectedTag())
                .unwrap_or(0);
            self.set_auto_advance((tag > 0).then(|| tag as f64 / 10.0));
        }

        #[unsafe(method(prefsChooseStartup:))]
        fn prefs_choose_startup(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let mtm = self.mtm();
            let panel = NSOpenPanel::openPanel(mtm);
            panel.setCanChooseDirectories(true);
            panel.setCanChooseFiles(false);
            if panel.runModal() != objc2_app_kit::NSModalResponseOK {
                return;
            }
            let Some(path) = panel.URL().and_then(|u| u.path()) else { return };
            if !e2e::enabled() {
                unsafe {
                    NSUserDefaults::standardUserDefaults()
                        .setObject_forKey(Some(&path), ns_string!("startupFolder"));
                }
            }
            self.sync_prefs_controls();
        }

        #[unsafe(method(prefsClearStartup:))]
        fn prefs_clear_startup(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            if !e2e::enabled() {
                unsafe {
                    NSUserDefaults::standardUserDefaults()
                        .setObject_forKey(None, ns_string!("startupFolder"));
                }
            }
            self.sync_prefs_controls();
        }

        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            self.menu_item_enabled(item)
        }

        #[unsafe(method(moveToTrash:))]
        fn move_to_trash(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.trash_selection();
        }

        #[unsafe(method(clearFlash:))]
        fn clear_flash(&self, _timer: Option<&objc2::runtime::AnyObject>) {
            let overlay = {
                let mut show = self.ivars().show.borrow_mut();
                let Some(show) = show.as_mut() else { return };
                show.flash = None;
                show.flash_timer = None;
                show.overlay.clone()
            };
            overlay.setHidden(!self.ivars().overlay_visible.get());
            self.update_overlay();
        }

        #[unsafe(method(toggleSidebarMenu:))]
        fn toggle_sidebar_menu(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.toggle_sidebar();
        }

        #[unsafe(method(toggleLabels:))]
        fn toggle_labels(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let Some(grid) = self.ivars().grid.get() else { return };
            let visible = !grid.labels_visible();
            grid.set_labels(visible);
            if let Some(item) = self.ivars().labels_item.get() {
                item.setState(if visible { 1 } else { 0 });
            }
            self.save_prefs();
        }

        #[unsafe(method(biggerThumbs:))]
        fn bigger_thumbs(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            if let Some(grid) = self.ivars().grid.get() {
                grid.scale_cells(1.25);
            }
        }

        #[unsafe(method(smallerThumbs:))]
        fn smaller_thumbs(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            if let Some(grid) = self.ivars().grid.get() {
                grid.scale_cells(0.8);
            }
        }

        #[unsafe(method(e2eStep:))]
        fn e2e_step(&self, _timer: Option<&objc2::runtime::AnyObject>) {
            e2e::run_step(self);
        }

        #[unsafe(method(advanceSlide:))]
        fn advance_slide(&self, _timer: Option<&objc2::runtime::AnyObject>) {
            let stepped = match self.ivars().show.borrow_mut().as_mut() {
                Some(show) => show.playlist.step(1),
                None => return,
            };
            if stepped {
                self.show_current();
            } else {
                // Reached the end without loop: stop advancing.
                self.set_auto_advance(None);
            }
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars {
            engine: OnceCell::new(),
            window: OnceCell::new(),
            grid: OnceCell::new(),
            sidebar: OnceCell::new(),
            split: OnceCell::new(),
            current_scan: RefCell::new(None),
            sidebar_item: OnceCell::new(),
            prefs_window: OnceCell::new(),
            default_windowed: Cell::new(false),
            show: RefCell::new(None),
            loop_enabled: Cell::new(false),
            shuffle_enabled: Cell::new(false),
            overlay_visible: Cell::new(false),
            status: OnceCell::new(),
            loop_item: OnceCell::new(),
            shuffle_item: OnceCell::new(),
            labels_item: OnceCell::new(),
            auto_menu: OnceCell::new(),
            sort_menu: OnceCell::new(),
            sort_order: Cell::new(SortOrder::Name),
            sort_desc: Cell::new(false),
            default_interval: Cell::new(None),
            e2e: RefCell::new(e2e::E2eState::default()),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn mtm(&self) -> MainThreadMarker {
        MainThreadMarker::new().expect("delegate methods run on the main thread")
    }

    // ---- actions ----

    fn open_folder(&self) {
        let mtm = self.mtm();
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setAllowsMultipleSelection(false);
        panel.setPrompt(Some(ns_string!("Browse")));
        let ok = panel.runModal() == objc2_app_kit::NSModalResponseOK;
        if !ok {
            return;
        }
        let Some(url) = panel.URL() else { return };
        let Some(path) = url.path() else { return };
        self.scan_root(PathBuf::from(path.to_string()), true);
    }

    pub fn scan_root(&self, root: PathBuf, recursive: bool) {
        self.scan_root_inner(root, recursive, true);
    }

    /// The folder the grid shows and how deep, so a repeat of the
    /// same request skips the scan.
    pub fn current_scan(&self) -> Option<(PathBuf, bool)> {
        self.ivars().current_scan.borrow().clone()
    }

    /// `remember` is false only for the launch fallback: a folder the
    /// user never picked must not overwrite the saved one.
    fn scan_root_inner(&self, root: PathBuf, recursive: bool, remember: bool) {
        if let Some(grid) = self.ivars().grid.get() {
            grid.reset();
        }
        if let Some(window) = self.ivars().window.get() {
            window.setTitle(&NSString::from_str(&format!(
                "wene — {}",
                root.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
            )));
        }
        // The sidebar follows the grid, whatever changed the folder.
        if let Some(sidebar) = self.ivars().sidebar.get() {
            sidebar.reveal(&root);
        }
        if remember {
            save_last_folder(&root);
        }
        *self.ivars().current_scan.borrow_mut() = Some((root.clone(), recursive));
        self.ivars().engine.get().unwrap().scan(root, recursive);
    }

    /// Show or hide the sidebar. The split view controller owns the
    /// collapse animation.
    fn toggle_sidebar(&self) {
        let Some(split) = self.ivars().split.get() else { return };
        unsafe { split.toggleSidebar(None) };
        self.sync_sidebar_item();
    }

    /// Tick the View menu item to match the sidebar.
    fn sync_sidebar_item(&self) {
        let (Some(split), Some(item)) =
            (self.ivars().split.get(), self.ivars().sidebar_item.get())
        else {
            return;
        };
        let collapsed = split
            .splitViewItems()
            .firstObject()
            .is_some_and(|item| item.isCollapsed());
        item.setState(if collapsed { 0 } else { 1 });
    }

    pub fn request_thumb(&self, path: PathBuf) {
        self.ivars().engine.get().unwrap().request_thumb(path);
    }

    /// Sort the grid by the current order and sync the menu marks
    /// (`✓` on the order; dash-like off state elsewhere).
    fn apply_sort(&self) {
        let order = self.ivars().sort_order.get();
        let descending = self.ivars().sort_desc.get();
        if let Some(grid) = self.ivars().grid.get() {
            grid.resort(order, descending);
        }
        if let Some(menu) = self.ivars().sort_menu.get() {
            let selected_tag = match order {
                SortOrder::Name => 1,
                SortOrder::Modified => 2,
                SortOrder::Size => 3,
                SortOrder::Path => 4,
                SortOrder::ExifDate => 5,
                SortOrder::Added => 6,
            };
            for item in menu.itemArray() {
                item.setState(if item.tag() == selected_tag { 1 } else { 0 });
            }
        }
    }

    fn sort_is_default(&self) -> bool {
        self.ivars().sort_order.get() == SortOrder::Name && !self.ivars().sort_desc.get()
    }

    // ---- preferences ----

    /// Apply saved settings at launch. Skipped in e2e runs so the
    /// harness starts from a known state.
    fn load_prefs(&self) {
        if e2e::enabled() {
            return;
        }
        let d = NSUserDefaults::standardUserDefaults();
        if d.boolForKey(ns_string!("slideshowLoop")) {
            self.ivars().loop_enabled.set(true);
            if let Some(item) = self.ivars().loop_item.get() {
                item.setState(1);
            }
        }
        if d.boolForKey(ns_string!("slideshowShuffle")) {
            self.ivars().shuffle_enabled.set(true);
            if let Some(item) = self.ivars().shuffle_item.get() {
                item.setState(1);
            }
        }
        self.ivars()
            .default_windowed
            .set(d.boolForKey(ns_string!("slideshowWindowed")));
        let tenths = d.integerForKey(ns_string!("autoAdvanceTenths"));
        let seconds = (tenths > 0).then(|| tenths as f64 / 10.0);
        self.ivars().default_interval.set(seconds);
        self.update_auto_menu(seconds);
        if d.boolForKey(ns_string!("showFilenames")) {
            if let Some(grid) = self.ivars().grid.get() {
                grid.set_labels(true);
            }
            if let Some(item) = self.ivars().labels_item.get() {
                item.setState(1);
            }
        }
    }

    /// Persist everything the prefs window and the menus control.
    fn save_prefs(&self) {
        if e2e::enabled() {
            return;
        }
        let d = NSUserDefaults::standardUserDefaults();
        d.setBool_forKey(self.ivars().loop_enabled.get(), ns_string!("slideshowLoop"));
        d.setBool_forKey(
            self.ivars().shuffle_enabled.get(),
            ns_string!("slideshowShuffle"),
        );
        d.setBool_forKey(
            self.ivars().default_windowed.get(),
            ns_string!("slideshowWindowed"),
        );
        let tenths = self
            .ivars()
            .default_interval
            .get()
            .map(|s| (s * 10.0) as isize)
            .unwrap_or(0);
        d.setInteger_forKey(tenths, ns_string!("autoAdvanceTenths"));
        if let Some(grid) = self.ivars().grid.get() {
            d.setBool_forKey(grid.labels_visible(), ns_string!("showFilenames"));
        }
    }

    pub fn default_windowed(&self) -> bool {
        self.ivars().default_windowed.get()
    }

    pub fn show_prefs(&self) {
        let mtm = self.mtm();
        if self.ivars().prefs_window.get().is_none() {
            let window = build_prefs_window(mtm, self);
            let _ = self.ivars().prefs_window.set(window);
        }
        self.sync_prefs_controls();
        if let Some(window) = self.ivars().prefs_window.get() {
            window.makeKeyAndOrderFront(None);
        }
    }

    /// Push current state into the prefs controls (looked up by tag).
    fn sync_prefs_controls(&self) {
        let Some(window) = self.ivars().prefs_window.get() else { return };
        let Some(content) = window.contentView() else { return };
        let set_check = |tag: isize, on: bool| {
            if let Some(view) = content.viewWithTag(tag) {
                if let Some(button) = view.downcast_ref::<objc2_app_kit::NSButton>() {
                    button.setState(if on { 1 } else { 0 });
                }
            }
        };
        set_check(1, self.ivars().default_windowed.get());
        set_check(2, self.ivars().loop_enabled.get());
        set_check(3, self.ivars().shuffle_enabled.get());
        set_check(
            4,
            self.ivars().grid.get().is_some_and(|g| g.labels_visible()),
        );
        if let Some(view) = content.viewWithTag(6) {
            if let Some(popup) = view.downcast_ref::<objc2_app_kit::NSPopUpButton>() {
                let tenths = self
                    .ivars()
                    .default_interval
                    .get()
                    .map(|s| (s * 10.0) as isize)
                    .unwrap_or(0);
                popup.selectItemWithTag(tenths);
            }
        }
        if let Some(view) = content.viewWithTag(7) {
            if let Some(field) = view.downcast_ref::<NSTextField>() {
                let text = startup_folder_pref().unwrap_or_else(|| "Ask at launch".into());
                field.setStringValue(&NSString::from_str(&text));
            }
        }
    }

    // ---- trash ----

    /// Only "Move to trash" needs a rule: it works on the slideshow's
    /// slide or the grid's selection, so with neither, and with the
    /// preferences window in front, the item goes grey.
    fn menu_item_enabled(&self, item: &NSMenuItem) -> bool {
        if item.action() != Some(sel!(moveToTrash:)) {
            return true;
        }
        let in_show = self.ivars().show.borrow().is_some();
        let selected = self
            .ivars()
            .grid
            .get()
            .is_some_and(|grid| !grid.selected_paths().is_empty());
        let prefs_key = self
            .ivars()
            .prefs_window
            .get()
            .is_some_and(|window| window.isKeyWindow());
        (in_show || selected) && !prefs_key
    }

    /// cmd-Delete: move the slideshow's current slide, or the grid's
    /// selection, to the system trash. No confirmation: the trash is
    /// the safety net.
    pub fn trash_selection(&self) {
        let current = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .and_then(|show| show.playlist.current().cloned());
        let paths = match current {
            Some(path) => vec![path],
            None => self.ivars().grid.get().map(|g| g.selected_paths()).unwrap_or_default(),
        };
        if paths.is_empty() {
            return;
        }

        let failed = trash::move_to_trash(&paths);
        let moved: Vec<PathBuf> = paths
            .iter()
            .filter(|path| !failed.contains(path))
            .cloned()
            .collect();

        // The app's delete is the authority. The rows go now, so the
        // watcher's echo a moment later finds nothing and does
        // nothing.
        if let Some(grid) = self.ivars().grid.get() {
            grid.remove_trashed(&moved);
        }
        self.drop_from_slideshow(&moved);

        let message = trash_message(&moved, failed.len());
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    /// Take gone files out of a running show, wherever the removal
    /// came from, and end the show if that empties it.
    fn drop_from_slideshow(&self, paths: &[PathBuf]) {
        let (emptied, changed) = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            let before_len = show.playlist.len();
            let before_current = show.playlist.current().cloned();
            for path in paths {
                show.playlist.remove(path);
                show.cache.remove(path);
                show.view_states.remove(path);
            }
            let changed = show.playlist.len() != before_len
                || show.playlist.current().cloned() != before_current;
            (show.playlist.is_empty(), changed)
        };
        if emptied {
            self.end_slideshow();
        } else if changed {
            self.show_current();
        }
    }

    /// Put a line in the status bar. The next selection or scan
    /// replaces it.
    fn show_status_message(&self, text: &str) {
        if let Some(status) = self.ivars().status.get() {
            status.setStringValue(&NSString::from_str(text));
        }
    }

    /// Show a line over the slide for a moment, even when the overlay
    /// is off: fullscreen has no status bar and no thumbnail to watch
    /// vanish.
    fn flash_overlay(&self, text: &str) {
        let overlay = {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            show.flash = Some(text.to_string());
            // One timer at a time, so a second cull gets its own full
            // moment on screen.
            if let Some(timer) = show.flash_timer.take() {
                timer.invalidate();
            }
            show.overlay.clone()
        };
        overlay.setHidden(false);
        self.update_overlay();
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                2.0,
                self,
                sel!(clearFlash:),
                None,
                false,
            )
        };
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.flash_timer = Some(timer);
        }
    }

    // ---- status bar ----

    /// The grid calls this after any selection or file-list change.
    pub fn selection_changed(&self) {
        self.update_status();
    }

    /// Count plus name, dimensions, and size of the selection, like
    /// the original app's status bar.
    fn update_status(&self) {
        let Some(status) = self.ivars().status.get() else { return };
        let Some(grid) = self.ivars().grid.get() else { return };
        let (total, selected) = grid.selection_info();
        let text = match selected.as_slice() {
            [] => format!("{total} images"),
            [info] => {
                let name = info
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let dims = decoder::image_dimensions(&info.path)
                    .map(|(w, h)| format!(" — {w}×{h}"))
                    .unwrap_or_default();
                format!("{name}{dims} — {} · {total} images", format_bytes(info.size))
            }
            many => {
                let bytes: u64 = many.iter().map(|f| f.size).sum();
                format!("{} of {total} selected — {}", many.len(), format_bytes(bytes))
            }
        };
        status.setStringValue(&NSString::from_str(&text));
    }

    // ---- slideshow ----

    /// Start with everything in the grid (e2e and default path).
    pub fn start_slideshow(&self, index: usize, windowed: bool) {
        let files = self.ivars().grid.get().unwrap().paths();
        self.start_slideshow_files(files, index, windowed);
    }

    /// Menu path: use the grid's selection semantics.
    fn start_from_grid(&self, windowed: bool) {
        let (files, start) = self.ivars().grid.get().unwrap().slideshow_request();
        self.start_slideshow_files(files, start, windowed);
    }

    /// Start with an explicit file set (the grid's selection
    /// semantics decide what that is).
    pub fn start_slideshow_files(&self, files: Vec<PathBuf>, index: usize, windowed: bool) {
        let mtm = self.mtm();
        if files.is_empty() {
            return;
        }
        let screen_frame = NSScreen::mainScreen(mtm)
            .map(|s| s.frame())
            .unwrap_or(NSRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1440.0, 900.0)));
        let window = if windowed {
            SlideshowWindow::windowed(mtm, screen_frame)
        } else {
            SlideshowWindow::fullscreen(mtm, screen_frame)
        };
        let content_frame = window.contentRectForFrameRect(window.frame());
        let view = SlideView::new(mtm, content_frame);
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let _ = view.ivars().delegate.set(unsafe {
            Retained::retain(self as *const Self as *mut Self).unwrap()
        });

        let overlay = NSTextField::labelWithString(ns_string!(""), mtm);
        overlay.setTextColor(Some(&NSColor::whiteColor()));
        overlay.setDrawsBackground(true);
        overlay.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.0, 0.55)));
        // Bottom left; width follows the window when it resizes.
        overlay.setFrame(NSRect::new(
            CGPoint::new(20.0, 20.0),
            CGSize::new(content_frame.size.width - 40.0, 24.0),
        ));
        overlay.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        overlay.setHidden(!self.ivars().overlay_visible.get());
        view.addSubview(&overlay);

        window.setContentView(Some(&view));
        window.makeKeyAndOrderFront(None);
        window.makeFirstResponder(Some(&view));

        let mut playlist = Playlist::new(files, index);
        playlist.looping = self.ivars().loop_enabled.get();
        playlist.set_shuffled(self.ivars().shuffle_enabled.get());

        *self.ivars().show.borrow_mut() = Some(Show {
            window,
            view,
            overlay,
            playlist,
            cache: LruCache::new(SLIDE_CACHE_BYTES),
            view_states: HashMap::new(),
            interval: None,
            timer: None,
            flash: None,
            flash_timer: None,
        });
        // Apply the remembered pace (menu pick or last key).
        if let Some(seconds) = self.ivars().default_interval.get() {
            self.set_auto_advance(Some(seconds));
        }
        self.show_current();
    }

    pub fn end_slideshow(&self) {
        // Cleanup happens in windowWillClose: (shared with the close
        // button in windowed mode).
        let window = self.ivars().show.borrow().as_ref().map(|s| s.window.clone());
        if let Some(window) = window {
            window.close();
        }
    }

    pub fn step_slideshow(&self, delta: i64) {
        let stepped = match self.ivars().show.borrow_mut().as_mut() {
            Some(show) => show.playlist.step(delta),
            None => return,
        };
        if stepped {
            self.show_current();
        }
    }

    /// Option-arrow: step the original sorted order while shuffled.
    pub fn step_slideshow_original(&self, delta: i64) {
        let stepped = match self.ivars().show.borrow_mut().as_mut() {
            Some(show) => show.playlist.step_original(delta),
            None => return,
        };
        if stepped {
            self.show_current();
        }
    }

    pub fn jump_slideshow_start(&self) {
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.playlist.jump_first();
        }
        self.show_current();
    }

    pub fn jump_slideshow_end(&self) {
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.playlist.jump_last();
        }
        self.show_current();
    }

    /// Space: toggle pause while auto-advancing, plain next otherwise.
    pub fn space_pressed(&self) {
        let interval = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .and_then(|s| s.interval);
        match interval {
            None => self.step_slideshow(1),
            Some(seconds) => {
                let paused = self.ivars().show.borrow().as_ref().is_some_and(|s| s.timer.is_none());
                if paused {
                    self.schedule_timer(seconds);
                } else if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
                    if let Some(timer) = show.timer.take() {
                        timer.invalidate();
                    }
                }
                self.update_overlay();
            }
        }
    }

    /// Keys 1-9, ! (0.5 s), @ (1.5 s) or the menu set the pace;
    /// 0 / Off stops. Also becomes the default for the next show.
    pub fn set_auto_advance(&self, seconds: Option<f64>) {
        self.ivars().default_interval.set(seconds);
        self.update_auto_menu(seconds);
        {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            if let Some(timer) = show.timer.take() {
                timer.invalidate();
            }
            show.interval = seconds;
        }
        if let Some(seconds) = seconds {
            self.schedule_timer(seconds);
        }
        self.update_overlay();
        self.save_prefs();
    }

    fn schedule_timer(&self, seconds: f64) {
        let timer = unsafe {
            NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                seconds,
                self,
                sel!(advanceSlide:),
                None,
                true,
            )
        };
        // Add in common modes explicitly so the timer keeps firing
        // during event tracking too.
        unsafe {
            objc2_foundation::NSRunLoop::mainRunLoop()
                .addTimer_forMode(&timer, objc2_foundation::NSRunLoopCommonModes)
        };
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.timer = Some(timer);
        }
    }

    /// Sync the Auto-advance menu checkmarks to `seconds`.
    fn update_auto_menu(&self, seconds: Option<f64>) {
        let Some(menu) = self.ivars().auto_menu.get() else { return };
        let selected_tag = seconds.map(|s| (s * 10.0) as isize).unwrap_or(0);
        for item in menu.itemArray() {
            item.setState(if item.tag() == selected_tag { 1 } else { 0 });
        }
    }

    /// The slide view changed zoom/rotation/flip/pan: remember it
    /// for the current file and refresh the overlay.
    pub fn slide_state_changed(&self) {
        {
            let mut show = self.ivars().show.borrow_mut();
            if let Some(show) = show.as_mut() {
                if let Some(current) = show.playlist.current().cloned() {
                    let state = show.view.state();
                    if state == SlideState::default() {
                        show.view_states.remove(&current);
                    } else {
                        show.view_states.insert(current, state);
                    }
                }
            }
        }
        self.update_overlay();
    }

    pub fn toggle_overlay(&self) {
        let visible = !self.ivars().overlay_visible.get();
        self.ivars().overlay_visible.set(visible);
        if let Some(show) = self.ivars().show.borrow().as_ref() {
            show.overlay.setHidden(!visible);
        }
        self.update_overlay();
    }

    fn update_overlay(&self) {
        // Build the text and release the borrow BEFORE touching
        // AppKit: setStringValue may re-enter delegate code.
        let (overlay, window, name, text) = {
            let show = self.ivars().show.borrow();
            let Some(show) = show.as_ref() else { return };
            let name = show
                .playlist
                .current()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some(flash) = show.flash.clone() {
                (show.overlay.clone(), show.window.clone(), name, flash)
            } else {
            let mut text = format!(
                "{}/{}  {}",
                show.playlist.current_index() + 1,
                show.playlist.len(),
                name
            );
            if show.playlist.shuffled() {
                text.push_str("  [shuffle]");
            }
            if show.playlist.looping {
                text.push_str("  [loop]");
            }
            if let Some(seconds) = show.interval {
                if show.timer.is_some() {
                    text.push_str(&format!("  [{seconds}s]"));
                } else {
                    text.push_str("  [paused]");
                }
            }
            let state = show.view.state();
            if state.rotation != 0 {
                text.push_str(&format!("  [{}°]", state.rotation));
            }
            if state.flipped {
                text.push_str("  [flipped]");
            }
            if let Some(zoom) = state.zoom {
                text.push_str(&format!("  [{:.0}%]", zoom * 100.0));
            }
            (show.overlay.clone(), show.window.clone(), name, text)
            }
        };
        overlay.setStringValue(&NSString::from_str(&text));
        // Visible as the title bar in windowed mode.
        window.setTitle(&NSString::from_str(&name));
    }

    /// Display the current slide from cache or request it, and
    /// prefetch the neighbours. The LRU keeps recent history within
    /// its byte budget, so stepping back is instant.
    fn show_current(&self) {
        let engine = self.ivars().engine.get().unwrap();
        {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            // A flash is about the slide that just left. The slide
            // arriving now gets the usual line back.
            if let Some(timer) = show.flash_timer.take() {
                timer.invalidate();
            }
            show.flash = None;

            let mut wanted: Vec<PathBuf> = Vec::with_capacity(3);
            for delta in [-1i64, 0, 1] {
                if let Some(path) = show.playlist.peek(delta) {
                    if !wanted.contains(path) {
                        wanted.push(path.clone());
                    }
                }
            }

            if let Some(current) = show.playlist.current().cloned() {
                if let Some(image) = show.cache.get(&current) {
                    show.view.show_image(image.clone());
                    if let Some(state) = show.view_states.get(&current) {
                        show.view.apply_state(*state);
                    }
                }
            }
            let current = show.playlist.current().cloned();
            for path in wanted {
                // Animated slides never enter the cache (frames are
                // big); decode them only when they become current
                // instead of on every neighbour prefetch.
                let animated = decoder::is_animated_ext(&path);
                let is_current = Some(&path) == current.as_ref();
                if animated && !is_current {
                    continue;
                }
                if !show.cache.contains(&path) {
                    engine.request_slide(path);
                }
            }
        }
        self.update_overlay();
    }

    // ---- e2e accessors (state peeks for the harness) ----

    pub fn e2e_state(&self) -> &RefCell<e2e::E2eState> {
        &self.ivars().e2e
    }

    pub fn e2e_file_count(&self) -> usize {
        self.ivars()
            .grid
            .get()
            .map(|g| g.ivars().files.borrow().len())
            .unwrap_or(0)
    }

    pub fn e2e_show_active(&self) -> bool {
        self.ivars().show.borrow().is_some()
    }

    pub fn e2e_current_index(&self) -> Option<usize> {
        self.ivars()
            .show
            .borrow()
            .as_ref()
            .map(|s| s.playlist.current_index())
    }

    pub fn e2e_slide_has_image(&self) -> bool {
        self.ivars()
            .show
            .borrow()
            .as_ref()
            .is_some_and(|s| s.view.has_image())
    }

    pub fn e2e_slide_view(&self) -> Option<Retained<SlideView>> {
        self.ivars().show.borrow().as_ref().map(|s| s.view.clone())
    }

    pub fn e2e_overlay_text(&self) -> Option<String> {
        self.ivars()
            .show
            .borrow()
            .as_ref()
            .map(|s| s.overlay.stringValue().to_string())
    }

    pub fn e2e_sort(&self, order: SortOrder, descending: bool) {
        self.ivars().sort_order.set(order);
        self.ivars().sort_desc.set(descending);
        self.apply_sort();
    }

    pub fn e2e_first_file(&self) -> Option<String> {
        self.ivars().grid.get().and_then(|g| {
            g.paths()
                .first()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
        })
    }

    pub fn e2e_grid_height(&self) -> f64 {
        self.ivars()
            .grid
            .get()
            .map(|g| g.frame().size.height)
            .unwrap_or(0.0)
    }

    pub fn e2e_scale_cells(&self, factor: f64) {
        if let Some(grid) = self.ivars().grid.get() {
            grid.scale_cells(factor);
        }
    }

    pub fn e2e_select(&self, indices: &[usize]) {
        if let Some(grid) = self.ivars().grid.get() {
            grid.e2e_set_selected(indices);
        }
    }

    pub fn e2e_start_from_selection(&self) {
        let (files, start) = self.ivars().grid.get().unwrap().slideshow_request();
        self.start_slideshow_files(files, start, false);
    }

    pub fn e2e_show_is_windowed(&self) -> Option<bool> {
        self.ivars()
            .show
            .borrow()
            .as_ref()
            .map(|s| s.window.styleMask().contains(NSWindowStyleMask::Titled))
    }

    pub fn e2e_playlist_len(&self) -> Option<usize> {
        self.ivars().show.borrow().as_ref().map(|s| s.playlist.len())
    }

    pub fn e2e_status_text(&self) -> String {
        self.ivars()
            .status
            .get()
            .map(|s| s.stringValue().to_string())
            .unwrap_or_default()
    }

    /// Mirror a sidebar click on `path`: open the tree down to it and
    /// run the same handler the selection callback runs.
    pub fn e2e_sidebar_click(&self, path: &str) {
        if let Some(sidebar) = self.ivars().sidebar.get() {
            sidebar.e2e_click(std::path::Path::new(path));
        }
    }

    pub fn e2e_sidebar_path(&self) -> String {
        self.ivars()
            .sidebar
            .get()
            .and_then(|s| s.selected_path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn e2e_sidebar_hidden(&self) -> bool {
        self.ivars()
            .split
            .get()
            .and_then(|split| split.splitViewItems().firstObject().map(|i| i.isCollapsed()))
            .unwrap_or(false)
    }

    pub fn e2e_toggle_sidebar(&self) {
        self.toggle_sidebar();
    }

    pub fn e2e_add_favorite(&self, path: &str) {
        if let Some(sidebar) = self.ivars().sidebar.get() {
            sidebar.add_favorite(std::path::Path::new(path));
        }
    }

    pub fn e2e_remove_favorite(&self, path: &str) {
        if let Some(sidebar) = self.ivars().sidebar.get() {
            sidebar.remove_favorite(std::path::Path::new(path));
        }
    }

    pub fn e2e_favorite_count(&self) -> usize {
        self.ivars()
            .sidebar
            .get()
            .map(|s| s.e2e_favorites().len())
            .unwrap_or(0)
    }

    /// The name of the first selected image, for selection checks.
    pub fn e2e_selected_name(&self) -> Option<String> {
        let (_, selected) = self.ivars().grid.get()?.selection_info();
        selected
            .first()
            .and_then(|info| info.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    }

    pub fn e2e_trash_selection(&self) {
        self.trash_selection();
    }

    pub fn e2e_prefs_visible(&self) -> bool {
        self.ivars()
            .prefs_window
            .get()
            .is_some_and(|w| w.isVisible())
    }

    pub fn e2e_set_default_windowed(&self, windowed: bool) {
        self.ivars().default_windowed.set(windowed);
    }

    pub fn e2e_close_prefs(&self) {
        if let Some(window) = self.ivars().prefs_window.get() {
            window.close();
        }
    }

    pub fn e2e_slide_animating(&self) -> bool {
        self.ivars()
            .show
            .borrow()
            .as_ref()
            .is_some_and(|s| s.view.is_animating())
    }

    pub fn e2e_toggle_labels(&self) {
        if let Some(grid) = self.ivars().grid.get() {
            grid.set_labels(!grid.labels_visible());
        }
    }

    // ---- core events ----

    fn handle_event(&self, event: Event<Img>) {
        match event {
            Event::FilesInserted(inserts) => {
                self.ivars().grid.get().unwrap().insert_files(inserts);
                // Core streams in name order; re-sort the batch into
                // the active order.
                if !self.sort_is_default() {
                    self.apply_sort();
                }
            }
            Event::FilesRemoved(paths) => {
                self.ivars().grid.get().unwrap().remove_paths(&paths);
                self.drop_from_slideshow(&paths);
            }
            Event::FileChanged(info) => {
                let grid = self.ivars().grid.get().unwrap();
                grid.invalidate_thumb(&info.path);
                let is_current = {
                    let mut show = self.ivars().show.borrow_mut();
                    if let Some(show) = show.as_mut() {
                        show.cache.remove(&info.path);
                        show.playlist.current() == Some(&info.path)
                    } else {
                        false
                    }
                };
                if is_current {
                    self.show_current();
                }
            }
            Event::ScanDone { total } => {
                println!("scan done: {total} images");
                self.update_status();
                if e2e::enabled() && !self.ivars().e2e.borrow().started {
                    self.ivars().e2e.borrow_mut().started = true;
                    unsafe {
                        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                            0.9,
                            self,
                            sel!(e2eStep:),
                            None,
                            true,
                        )
                    };
                }
            }
            Event::ThumbReady { path, image } => {
                let cost = image_cost(&image);
                let image = ns_image(&image);
                self.ivars().grid.get().unwrap().set_thumb(path, image, cost);
            }
            Event::SlideReady { path, image } => {
                let is_current = {
                    let cost = image_cost(&image);
                    let image = ns_image(&image);
                    let mut show = self.ivars().show.borrow_mut();
                    let Some(show) = show.as_mut() else { return };
                    let is_current = show.playlist.current() == Some(&path);
                    let state = show.view_states.get(&path).copied();
                    show.cache.insert(path, image.clone(), cost);
                    if is_current {
                        show.view.show_image(image);
                        if let Some(state) = state {
                            show.view.apply_state(state);
                        }
                    }
                    is_current
                };
                if is_current {
                    // Refresh rotation/flip/zoom flags for the slide
                    // that just appeared.
                    self.update_overlay();
                }
            }
            Event::SlideFramesReady { path, frames } => {
                let is_current = {
                    let mut show = self.ivars().show.borrow_mut();
                    let Some(show) = show.as_mut() else { return };
                    let is_current = show.playlist.current() == Some(&path);
                    if is_current {
                        let frames: Vec<_> =
                            frames.iter().map(|(img, d)| (ns_image(img), *d)).collect();
                        let state = show.view_states.get(&path).copied();
                        show.view.show_frames(frames);
                        if let Some(state) = state {
                            show.view.apply_state(state);
                        }
                    }
                    is_current
                };
                if is_current {
                    self.update_overlay();
                }
            }
            Event::DecodeFailed { path } => {
                // A culled image fails a request that was already in
                // flight. That is not worth reporting.
                if path.exists() {
                    eprintln!("decode failed: {}", path.display());
                }
            }
        }
    }
}

fn ns_image(image: &Img) -> Retained<NSImage> {
    NSImage::initWithCGImage_size(NSImage::alloc(), image, CGSize::new(0.0, 0.0))
}

/// Estimated decoded footprint for the LRU budgets: pixels × 4 bytes.
fn image_cost(image: &Img) -> usize {
    CGImage::width(Some(image)) * CGImage::height(Some(image)) * 4
}

/// The folder the grid showed last. A failed restore leaves it
/// alone, so an unplugged disk comes back next time.
fn last_folder_pref() -> Option<PathBuf> {
    if e2e::enabled() {
        return None;
    }
    NSUserDefaults::standardUserDefaults()
        .stringForKey(ns_string!("lastFolder"))
        .map(|s| PathBuf::from(s.to_string()))
        .filter(|p| !p.as_os_str().is_empty())
}

fn save_last_folder(root: &std::path::Path) {
    if e2e::enabled() {
        return;
    }
    let value = NSString::from_str(&root.to_string_lossy());
    unsafe {
        NSUserDefaults::standardUserDefaults()
            .setObject_forKey(Some(&value), ns_string!("lastFolder"));
    }
}

fn startup_folder_pref() -> Option<String> {
    if e2e::enabled() {
        return None;
    }
    NSUserDefaults::standardUserDefaults()
        .stringForKey(ns_string!("startupFolder"))
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// The preferences window: one plain pane, controls looked up by tag
/// (1-5 checkboxes, 6 auto-advance popup, 7 startup folder field).
fn build_prefs_window(mtm: MainThreadMarker, delegate: &AppDelegate) -> Retained<NSWindow> {
    let frame = NSRect::new(CGPoint::new(360.0, 360.0), CGSize::new(430.0, 264.0));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(ns_string!("Preferences"));
    unsafe { window.setReleasedWhenClosed(false) };
    let content = objc2_app_kit::NSView::initWithFrame(
        objc2_app_kit::NSView::alloc(mtm),
        NSRect::new(CGPoint::new(0.0, 0.0), frame.size),
    );

    let label = |text: &str, x: f64, y: f64| {
        let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        l.setFrame(NSRect::new(CGPoint::new(x, y), CGSize::new(150.0, 18.0)));
        content.addSubview(&l);
    };
    let checkbox = |title: &str, action: objc2::runtime::Sel, tag: isize, y: f64| {
        let b = unsafe {
            objc2_app_kit::NSButton::checkboxWithTitle_target_action(
                &NSString::from_str(title),
                Some(delegate),
                Some(action),
                mtm,
            )
        };
        b.setFrame(NSRect::new(CGPoint::new(20.0, y), CGSize::new(380.0, 20.0)));
        b.setTag(tag);
        content.addSubview(&b);
    };

    // Startup folder row (top).
    label("Startup folder:", 20.0, 224.0);
    let field = NSTextField::labelWithString(ns_string!(""), mtm);
    field.setFrame(NSRect::new(CGPoint::new(20.0, 200.0), CGSize::new(250.0, 18.0)));
    field.setTag(7);
    field.setFont(Some(&objc2_app_kit::NSFont::systemFontOfSize(11.0)));
    field.setTextColor(Some(&NSColor::secondaryLabelColor()));
    content.addSubview(&field);
    for (title, action, x) in [
        ("Choose…", sel!(prefsChooseStartup:), 280.0),
        ("Clear", sel!(prefsClearStartup:), 358.0),
    ] {
        let b = unsafe {
            objc2_app_kit::NSButton::buttonWithTitle_target_action(
                &NSString::from_str(title),
                Some(delegate),
                Some(action),
                mtm,
            )
        };
        b.setFrame(NSRect::new(CGPoint::new(x, 194.0), CGSize::new(72.0, 28.0)));
        content.addSubview(&b);
    }

    checkbox(
        "Start slideshows in a window",
        sel!(prefsToggleWindowed:),
        1,
        160.0,
    );
    checkbox("Loop slideshows", sel!(toggleLoop:), 2, 132.0);
    checkbox("Shuffle slideshows", sel!(toggleShuffle:), 3, 104.0);

    label("Auto-advance:", 20.0, 72.0);
    let popup = objc2_app_kit::NSPopUpButton::new(mtm);
    popup.setFrame(NSRect::new(CGPoint::new(150.0, 64.0), CGSize::new(180.0, 26.0)));
    popup.setTag(6);
    for (title, tag) in [
        ("Off", 0isize),
        ("Every second", 10),
        ("Every 3 seconds", 30),
        ("Every 5 seconds", 50),
        ("Every 10 seconds", 100),
    ] {
        popup.addItemWithTitle(&NSString::from_str(title));
        if let Some(item) = popup.lastItem() {
            item.setTag(tag);
        }
    }
    unsafe {
        popup.setTarget(Some(delegate));
        popup.setAction(Some(sel!(prefsAutoAdvance:)));
    }
    content.addSubview(&popup);

    checkbox("Show filenames", sel!(toggleLabels:), 4, 28.0);

    window.setContentView(Some(&content));
    window
}

/// What the status bar and the slide overlay say after a delete.
fn trash_message(moved: &[PathBuf], failed: usize) -> String {
    let total = moved.len() + failed;
    if moved.is_empty() {
        return format!("{failed} of {total} could not be moved to trash");
    }
    // The line is the only record of what went, so it names files,
    // not just a count.
    let names: Vec<String> = moved
        .iter()
        .take(3)
        .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
        .collect();
    let mut moved_text = format!("Moved to trash: {}", names.join(", "));
    if moved.len() > names.len() {
        moved_text.push_str(&format!(" and {} more", moved.len() - names.len()));
    }
    if failed == 0 {
        moved_text
    } else {
        format!("{moved_text} — {failed} of {total} could not be moved to trash")
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn drain_events(mtm: MainThreadMarker) {
    let delegate = DELEGATE.get().unwrap().get(mtm);
    let rx = EVENTS.get().unwrap().lock().unwrap();
    while let Ok(event) = rx.try_recv() {
        delegate.handle_event(event);
    }
}

fn build_menu(mtm: MainThreadMarker, app: &NSApplication, delegate: &AppDelegate) {
    let menubar = NSMenu::new(mtm);

    let app_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);
    let prefs = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Preferences…"),
            Some(sel!(showPrefs:)),
            ns_string!(","),
        )
    };
    app_menu.addItem(&prefs);
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Quit wene"),
            Some(sel!(terminate:)),
            ns_string!("q"),
        )
    };
    app_menu.addItem(&quit);
    app_item.setSubmenu(Some(&app_menu));
    menubar.addItem(&app_item);

    let file_item = NSMenuItem::new(mtm);
    let file_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("File"));
    let open = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Open folder…"),
            Some(sel!(openDocument:)),
            ns_string!("o"),
        )
    };
    file_menu.addItem(&open);
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    // cmd-Delete, Finder's binding, in the grid and the slideshow.
    let trash = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Move to trash"),
            Some(sel!(moveToTrash:)),
            &NSString::from_str("\u{8}"),
        )
    };
    file_menu.addItem(&trash);
    file_item.setSubmenu(Some(&file_menu));
    menubar.addItem(&file_item);

    let show_item = NSMenuItem::new(mtm);
    let show_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Slideshow"));
    let start = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Start slideshow"),
            Some(sel!(startSlideshow:)),
            ns_string!("y"),
        )
    };
    let start_windowed = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Start slideshow in window"),
            Some(sel!(startSlideshowWindowed:)),
            ns_string!("y"),
        )
    };
    start_windowed.setKeyEquivalentModifierMask(
        objc2_app_kit::NSEventModifierFlags::Command | objc2_app_kit::NSEventModifierFlags::Option,
    );
    show_menu.addItem(&start);
    show_menu.addItem(&start_windowed);
    show_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let loop_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Loop"),
            Some(sel!(toggleLoop:)),
            ns_string!("l"),
        )
    };
    let shuffle_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Shuffle"),
            Some(sel!(toggleShuffle:)),
            ns_string!("r"),
        )
    };
    shuffle_item.setKeyEquivalentModifierMask(
        objc2_app_kit::NSEventModifierFlags::Command | objc2_app_kit::NSEventModifierFlags::Option,
    );
    show_menu.addItem(&loop_item);
    show_menu.addItem(&shuffle_item);

    show_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let auto_item = NSMenuItem::new(mtm);
    let auto_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Auto-advance"));
    // Tag = tenths of a second; 0 = off.
    for (title, tag) in [
        ("Off", 0isize),
        ("Every second", 10),
        ("Every 3 seconds", 30),
        ("Every 5 seconds", 50),
        ("Every 10 seconds", 100),
    ] {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(sel!(setAutoAdvanceMenu:)),
                ns_string!(""),
            )
        };
        item.setTag(tag);
        if tag == 0 {
            item.setState(1);
        }
        auto_menu.addItem(&item);
    }
    auto_item.setSubmenu(Some(&auto_menu));
    auto_item.setTitle(ns_string!("Auto-advance"));
    show_menu.addItem(&auto_item);

    show_item.setSubmenu(Some(&show_menu));
    menubar.addItem(&show_item);

    let view_item = NSMenuItem::new(mtm);
    let view_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("View"));
    // Sort orders: tag matches apply_sort's mapping. Re-picking the
    // checked one reverses the direction.
    for (title, tag, key) in [
        ("Sort by name", 1isize, "1"),
        ("Sort by date modified", 2, "2"),
        ("Sort by size", 3, "3"),
        ("Sort by file path", 4, "4"),
        ("Sort by EXIF date", 5, "5"),
        ("Sort by date added", 6, "6"),
    ] {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(sel!(setSortMenu:)),
                &NSString::from_str(key),
            )
        };
        item.setKeyEquivalentModifierMask(
            objc2_app_kit::NSEventModifierFlags::Command
                | objc2_app_kit::NSEventModifierFlags::Control,
        );
        item.setTag(tag);
        if tag == 1 {
            item.setState(1);
        }
        view_menu.addItem(&item);
    }
    view_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let bigger = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Bigger thumbnails"),
            Some(sel!(biggerThumbs:)),
            ns_string!("+"),
        )
    };
    let smaller = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Smaller thumbnails"),
            Some(sel!(smallerThumbs:)),
            ns_string!("-"),
        )
    };
    view_menu.addItem(&bigger);
    view_menu.addItem(&smaller);
    view_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let sidebar_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Show sidebar"),
            Some(sel!(toggleSidebarMenu:)),
            ns_string!("s"),
        )
    };
    sidebar_item.setKeyEquivalentModifierMask(
        objc2_app_kit::NSEventModifierFlags::Command
            | objc2_app_kit::NSEventModifierFlags::Control,
    );
    sidebar_item.setState(1);
    view_menu.addItem(&sidebar_item);
    let _ = delegate.ivars().sidebar_item.set(sidebar_item);
    let labels_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Show filenames"),
            Some(sel!(toggleLabels:)),
            ns_string!(""),
        )
    };
    view_menu.addItem(&labels_item);
    let _ = delegate.ivars().labels_item.set(labels_item);
    view_item.setSubmenu(Some(&view_menu));
    menubar.addItem(&view_item);
    let _ = delegate.ivars().sort_menu.set(view_menu);

    let _ = delegate.ivars().loop_item.set(loop_item);
    let _ = delegate.ivars().shuffle_item.set(shuffle_item);
    let _ = delegate.ivars().auto_menu.set(auto_menu);

    app.setMainMenu(Some(&menubar));
}

fn main() {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    build_menu(mtm, &app, &delegate);

    // Browse window: the sidebar on the left, the grid and the
    // status bar on the right. A split view controller gives the
    // sidebar the native look and collapse animation.
    let frame = NSRect::new(CGPoint::new(200.0, 200.0), CGSize::new(1000.0, 700.0));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable
                | NSWindowStyleMask::FullSizeContentView,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(ns_string!("wene"));
    // A sidebar runs the full height of the window only when the
    // window carries a toolbar. It stays empty until the control
    // strip slice fills it.
    let toolbar = objc2_app_kit::NSToolbar::new(mtm);
    window.setToolbar(Some(&toolbar));
    window.setToolbarStyle(objc2_app_kit::NSWindowToolbarStyle::Unified);
    // Tahoe draws the title as a wide capsule over the content when
    // a sidebar window carries no toolbar; an empty toolbar restores
    // the plain titlebar. The control strip is a later slice.

    let (sidebar, sidebar_scroll) = Sidebar::new(mtm, &delegate);

    let content_size = CGSize::new(frame.size.width - sidebar::WIDTH, frame.size.height);
    let content = objc2_app_kit::NSView::initWithFrame(
        objc2_app_kit::NSView::alloc(mtm),
        NSRect::new(CGPoint::new(0.0, 0.0), content_size),
    );

    let scroll = NSScrollView::new(mtm);
    scroll.setHasVerticalScroller(true);
    scroll.setFrame(NSRect::new(
        CGPoint::new(0.0, STATUS_H),
        CGSize::new(content_size.width, content_size.height - STATUS_H),
    ));
    scroll.setAutoresizingMask(
        objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
            | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    let grid = GridView::new(mtm, frame);
    let _ = grid.ivars().delegate.set(delegate.clone());
    scroll.setDocumentView(Some(&grid));

    let status = NSTextField::labelWithString(ns_string!(""), mtm);
    status.setFont(Some(&objc2_app_kit::NSFont::systemFontOfSize(11.0)));
    status.setTextColor(Some(&NSColor::secondaryLabelColor()));
    status.setFrame(NSRect::new(
        CGPoint::new(8.0, 4.0),
        CGSize::new(content_size.width - 16.0, 16.0),
    ));
    status.setAutoresizingMask(
        objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
            | objc2_app_kit::NSAutoresizingMaskOptions::ViewMaxYMargin,
    );

    content.addSubview(&scroll);
    content.addSubview(&status);

    let sidebar_vc = NSViewController::new(mtm);
    sidebar_vc.setView(&sidebar_scroll);
    let content_vc = NSViewController::new(mtm);
    content_vc.setView(&content);

    let split = NSSplitViewController::new(mtm);
    let sidebar_pane = NSSplitViewItem::sidebarWithViewController(&sidebar_vc);
    sidebar_pane.setMinimumThickness(sidebar::MIN_WIDTH);
    sidebar_pane.setMaximumThickness(sidebar::MAX_WIDTH);
    split.addSplitViewItem(&sidebar_pane);
    split.addSplitViewItem(&NSSplitViewItem::splitViewItemWithViewController(&content_vc));
    window.setContentViewController(Some(&split));
    window.setFrame_display(frame, false);
    window.makeFirstResponder(Some(&grid));

    let _ = delegate.ivars().status.set(status);
    let _ = delegate.ivars().sidebar.set(sidebar);
    let _ = delegate.ivars().split.set(split);
    let _ = delegate.ivars().window.set(window.clone());
    let _ = delegate.ivars().grid.set(grid);

    // Engine: decode sizes. Thumbs at 2x cell size for retina; slides
    // at 2x the screen's long edge.
    let screen_long_edge = NSScreen::mainScreen(mtm)
        .map(|s| {
            let f = s.frame();
            f.size.width.max(f.size.height)
        })
        .unwrap_or(2048.0);
    let (engine, rx) = Engine::start(
        ImageIoDecoder,
        (grid::CELL * 2.0) as i32,
        (screen_long_edge * 2.0) as i32,
        || {
            DispatchQueue::main().exec_async(|| {
                let mtm = MainThreadMarker::new().expect("main queue is the main thread");
                drain_events(mtm);
            });
        },
    );
    let _ = delegate.ivars().engine.set(engine);
    EVENTS.set(Mutex::new(rx)).ok().expect("EVENTS set once");
    DELEGATE
        .set(MainThreadBound::new(delegate, mtm))
        .ok()
        .expect("DELEGATE set once");

    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    app.run();
}
