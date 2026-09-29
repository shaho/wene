//! macOS shell: app delegate, menu, browse window, event pump.

mod decoder;
mod e2e;
mod grid;
mod sidebar;
mod slideshow;
mod trash;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Mutex, OnceLock};

use dispatch2::{DispatchQueue, MainThreadBound};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{
    define_class, msg_send, sel, AnyThread, ClassType, DefinedClass, MainThreadMarker,
    MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType,
    NSColor, NSControlTextEditingDelegate, NSFont, NSTextFieldDelegate, NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSOpenPanel, NSScreen, NSScrollView,
    NSSplitViewController, NSSplitViewItem, NSTextField, NSTextView, NSViewController, NSWindow,
    NSWindowDelegate, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGSize};
use objc2_core_graphics::CGImage;
use objc2_foundation::{
    ns_string, NSArray, NSNotification, NSObject, NSObjectProtocol, NSRect, NSString, NSTimer,
    NSURL, NSUserDefaults,
};
use wene_core::history::{Entry, History, Step};
use wene_core::transfer::Transfer;
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
/// Height of the filter bar above the grid, while it is open.
const FILTER_H: f64 = 28.0;

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
    /// The right-hand pane and the pieces stacked in it, for layout
    /// when the filter bar opens or closes.
    content: OnceCell<Retained<objc2_app_kit::NSView>>,
    scroll: OnceCell<Retained<NSScrollView>>,
    filter_bar: OnceCell<Retained<objc2_app_kit::NSView>>,
    filter_field: OnceCell<Retained<NSTextField>>,
    /// Images Finder asked for, waiting for their folder to finish
    /// scanning so they can be selected.
    pending_open: RefCell<Vec<PathBuf>>,
    /// True once Finder has handed the app something to open, so
    /// launch does not go off and restore the last folder instead.
    opened_from_finder: Cell<bool>,
    /// The folder the grid shows and whether it was scanned deep, to
    /// skip no-op rescans.
    current_scan: RefCell<Option<(PathBuf, bool)>>,
    sidebar_item: OnceCell<Retained<NSMenuItem>>,
    /// Targets of the last move and the last copy, for the repeat
    /// actions. The menu items name them.
    last_move: RefCell<Option<PathBuf>>,
    last_copy: RefCell<Option<PathBuf>>,
    move_again_item: OnceCell<Retained<NSMenuItem>>,
    copy_again_item: OnceCell<Retained<NSMenuItem>>,
    /// True while a batch runs, so a second one cannot start on top
    /// of it and scribble over its progress line.
    transfer_busy: Cell<bool>,
    prefs_window: OnceCell<Retained<NSWindow>>,
    /// The slider in the status bar, and the ceiling the preference
    /// sets for it.
    thumb_slider: OnceCell<Retained<objc2_app_kit::NSSlider>>,
    /// What the app did to files, so cmd-Z can take it back.
    history: RefCell<History>,
    undo_item: OnceCell<Retained<NSMenuItem>>,
    redo_item: OnceCell<Retained<NSMenuItem>>,
    info_window: OnceCell<Retained<NSWindow>>,
    info_label: OnceCell<Retained<NSTextField>>,
    /// Default slideshow mode from prefs; ⌥ at start inverts it.
    default_windowed: Cell<bool>,
    show: RefCell<Option<Show>>,
    loop_enabled: Cell<bool>,
    shuffle_enabled: Cell<bool>,
    overlay_visible: Cell<bool>,
    /// Extra overlay blocks: the file's own EXIF (shift-I) and its
    /// full path (p).
    overlay_exif: Cell<bool>,
    overlay_path: Cell<bool>,
    /// The cheat sheet (h or ?), which takes the overlay over while
    /// it is up.
    overlay_help: Cell<bool>,
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

    unsafe impl NSTextFieldDelegate for AppDelegate {}

    unsafe impl NSControlTextEditingDelegate for AppDelegate {
        // Only the filter field has us as its delegate.
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, _notification: &NSNotification) {
            self.filter_changed();
        }

        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn control_do_command(
            &self,
            _control: &objc2_app_kit::NSControl,
            _text_view: &objc2_app_kit::NSTextView,
            command: objc2::runtime::Sel,
        ) -> bool {
            self.filter_field_command(command)
        }
    }

    // The only menu the delegate owns is "Open with", which has to
    // be rebuilt from the selection every time it opens.
    unsafe impl NSMenuDelegate for AppDelegate {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            self.fill_open_with(menu);
        }
    }

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
        #[unsafe(method(application:openURLs:))]
        fn application_open_urls(
            &self,
            _app: &NSApplication,
            urls: &objc2_foundation::NSArray<objc2_foundation::NSURL>,
        ) {
            let paths: Vec<PathBuf> = urls
                .iter()
                .filter_map(|url| url.path())
                .map(|path| PathBuf::from(path.to_string()))
                .collect();
            self.open_from_finder(paths);
        }

        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            self.load_prefs();
            // Finder's open call arrives before this one on a cold
            // start, and it has already picked the folder.
            if self.ivars().opened_from_finder.get() {
                return;
            }
            // A folder argument skips everything else (useful for
            // scripted runs). Otherwise restore the last folder, then
            // the startup-folder preference, then Pictures.
            let arg = std::env::args().nth(1).map(PathBuf::from).filter(|p| p.exists());
            match arg {
                Some(path) if path.is_dir() => {
                    self.scan_root(path, true);
                    return;
                }
                // An unbundled run gets its files on the command
                // line, since Launch Services never sees it.
                Some(path) => {
                    self.open_from_finder(vec![path]);
                    return;
                }
                None => {}
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

        #[unsafe(method(prefsThumbCap:))]
        fn prefs_thumb_cap(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let Some(popup) = sender.and_then(|s| s.downcast_ref::<objc2_app_kit::NSPopUpButton>())
            else {
                return;
            };
            let cap = popup.selectedItem().map(|item| item.tag()).unwrap_or(320) as f64;
            self.apply_thumb_cap(cap);
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

        #[unsafe(method(contentResized:))]
        fn content_resized(&self, _notification: Option<&NSNotification>) {
            self.layout_content();
        }

        #[unsafe(method(showFilter:))]
        fn show_filter(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.open_filter();
        }

        #[unsafe(method(moveToTrash:))]
        fn move_to_trash(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.trash_selection();
        }

        #[unsafe(method(thumbSliderMoved:))]
        fn thumb_slider_moved_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.thumb_slider_moved();
        }

        #[unsafe(method(undoLast:))]
        fn undo_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.take_back(true);
        }

        #[unsafe(method(redoLast:))]
        fn redo_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.take_back(false);
        }

        #[unsafe(method(openWith:))]
        fn open_with(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let Some(item) = sender.and_then(|s| s.downcast_ref::<NSMenuItem>()) else { return };
            let Some(app) = item.representedObject() else { return };
            let Ok(app) = app.downcast::<NSURL>() else { return };
            self.open_targets_with(&app);
        }

        #[unsafe(method(copyPath:))]
        fn copy_path(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.copy_targets(false);
        }

        #[unsafe(method(copyFileUrl:))]
        fn copy_file_url(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.copy_targets(true);
        }

        #[unsafe(method(setDesktopPicture:))]
        fn set_desktop_picture_action(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.use_as_desktop_picture();
        }

        #[unsafe(method(getInfo:))]
        fn get_info(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.toggle_info();
        }

        #[unsafe(method(revealInFinder:))]
        fn reveal_in_finder(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.reveal_targets();
        }

        #[unsafe(method(moveToFolder:))]
        fn move_to_folder(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            if let Some(folder) = self.choose_folder(Transfer::Move) {
                self.transfer_selection(folder, Transfer::Move);
            }
        }

        #[unsafe(method(copyToFolder:))]
        fn copy_to_folder(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            if let Some(folder) = self.choose_folder(Transfer::Copy) {
                self.transfer_selection(folder, Transfer::Copy);
            }
        }

        #[unsafe(method(moveAgain:))]
        fn move_again(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let folder = self.ivars().last_move.borrow().clone();
            if let Some(folder) = folder {
                self.transfer_selection(folder, Transfer::Move);
            }
        }

        #[unsafe(method(copyAgain:))]
        fn copy_again(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            let folder = self.ivars().last_copy.borrow().clone();
            if let Some(folder) = folder {
                self.transfer_selection(folder, Transfer::Copy);
            }
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
            content: OnceCell::new(),
            scroll: OnceCell::new(),
            filter_bar: OnceCell::new(),
            filter_field: OnceCell::new(),
            pending_open: RefCell::new(Vec::new()),
            opened_from_finder: Cell::new(false),
            current_scan: RefCell::new(None),
            sidebar_item: OnceCell::new(),
            last_move: RefCell::new(None),
            last_copy: RefCell::new(None),
            move_again_item: OnceCell::new(),
            copy_again_item: OnceCell::new(),
            transfer_busy: Cell::new(false),
            prefs_window: OnceCell::new(),
            thumb_slider: OnceCell::new(),
            history: RefCell::new(History::default()),
            undo_item: OnceCell::new(),
            redo_item: OnceCell::new(),
            info_window: OnceCell::new(),
            info_label: OnceCell::new(),
            default_windowed: Cell::new(false),
            show: RefCell::new(None),
            loop_enabled: Cell::new(false),
            shuffle_enabled: Cell::new(false),
            overlay_visible: Cell::new(false),
            overlay_exif: Cell::new(false),
            overlay_path: Cell::new(false),
            overlay_help: Cell::new(false),
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
        // A filter typed for the last folder would explain nothing
        // here, and an almost empty grid reads as a bug.
        self.close_filter();
        if let Some(grid) = self.ivars().grid.get() {
            grid.reset();
        }
        if let Some(window) = self.ivars().window.get() {
            window.setTitle(&NSString::from_str(&format!(
                "Wêne — {}",
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
        // The thumbnail size is the one preference the harness also
        // wants, because the grid is laid out from it.
        self.apply_thumb_cap(thumb_cap_pref());
        if let Some(grid) = self.ivars().grid.get() {
            grid.set_cell_size(thumb_size_pref());
        }
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
        if let Some(view) = content.viewWithTag(8) {
            if let Some(popup) = view.downcast_ref::<objc2_app_kit::NSPopUpButton>() {
                let cap = self
                    .ivars()
                    .grid
                    .get()
                    .map(|grid| grid.cell_cap())
                    .unwrap_or(grid::CELL_CAPS[1]);
                popup.selectItemWithTag(cap as isize);
            }
        }
        if let Some(view) = content.viewWithTag(7) {
            if let Some(field) = view.downcast_ref::<NSTextField>() {
                let text = startup_folder_pref().unwrap_or_else(|| "Ask at launch".into());
                field.setStringValue(&NSString::from_str(&text));
            }
        }
    }

    // ---- opening from Finder ----

    /// Finder handed the app files or a folder. The grid shows the
    /// folder holding them, with the images selected. Starting the
    /// slideshow stays the user's move.
    pub fn open_from_finder(&self, paths: Vec<PathBuf>) {
        // The grid is what the user asked to see, so a slideshow
        // over it and a filter hiding half of it both go.
        self.end_slideshow();
        self.close_filter();
        if let Some(window) = self.ivars().window.get() {
            window.makeKeyAndOrderFront(None);
        }
        let Some(first) = paths.first().cloned() else { return };
        if first.is_dir() {
            // A folder opens as a folder, one level deep, like a
            // click in the sidebar.
            self.ivars().opened_from_finder.set(true);
            self.scan_root(first, false);
            return;
        }
        let files: Vec<PathBuf> = paths.into_iter().filter(|path| path.is_file()).collect();
        // A path that went away between the Finder gesture and the
        // launch leaves the flag alone, so launch still restores the
        // last folder instead of showing nothing.
        let Some(folder) = files.first().and_then(|path| path.parent()).map(PathBuf::from) else {
            return;
        };
        self.ivars().opened_from_finder.set(true);
        // Calls can arrive one after another, so they add up rather
        // than replace each other.
        self.ivars().pending_open.borrow_mut().extend(files);
        // A grid already holding them needs no rescan, which keeps a
        // recursive cmd-O grid intact.
        if self.select_pending() {
            return;
        }
        self.scan_root(folder, false);
    }

    /// Select the images Finder asked for. False means the grid does
    /// not hold them all yet, so their folder still has to be
    /// scanned.
    fn select_pending(&self) -> bool {
        let pending = self.ivars().pending_open.borrow().clone();
        if pending.is_empty() {
            return true;
        }
        let Some(grid) = self.ivars().grid.get() else { return false };
        // Only touch the selection once the grid really holds them.
        // A scan of some other folder must leave it alone.
        if !grid.holds_all(&pending) {
            return false;
        }
        grid.select_paths(&pending);
        self.ivars().pending_open.borrow_mut().clear();
        self.focus_grid();
        true
    }

    /// The folder the images waiting to be opened live in.
    fn pending_folder(&self) -> Option<PathBuf> {
        self.ivars()
            .pending_open
            .borrow()
            .first()
            .and_then(|path| path.parent())
            .map(PathBuf::from)
    }

    // ---- filter ----

    /// The field's text is the filter. Typing lands here.
    fn filter_changed(&self) {
        let Some(field) = self.ivars().filter_field.get() else { return };
        let text = field.stringValue().to_string();
        if let Some(grid) = self.ivars().grid.get() {
            grid.set_filter(&text);
        }
    }

    /// Esc, from anywhere in the browse window.
    pub fn escape_pressed(&self) {
        self.close_filter();
    }

    /// Keys the filter field hands over: Esc clears the filter and
    /// closes the bar, Return leaves the field with the filter still
    /// on. Anything else stays with the field.
    fn filter_field_command(&self, command: objc2::runtime::Sel) -> bool {
        if command == sel!(cancelOperation:) {
            self.close_filter();
            return true;
        }
        if command == sel!(insertNewline:) {
            self.focus_grid();
            return true;
        }
        false
    }

    /// cmd-F: the bar appears above the grid with the field ready to
    /// type in. Filtering happens in bursts, so the bar takes no
    /// room while it is closed.
    fn open_filter(&self) {
        let (Some(bar), Some(field), Some(window)) = (
            self.ivars().filter_bar.get(),
            self.ivars().filter_field.get(),
            self.ivars().window.get(),
        ) else {
            return;
        };
        bar.setHidden(false);
        self.layout_content();
        window.makeFirstResponder(Some(field));
    }

    /// Esc: the filter goes, the bar goes, the grid takes the keys
    /// back.
    fn close_filter(&self) {
        let Some(bar) = self.ivars().filter_bar.get() else { return };
        if bar.isHidden() {
            return;
        }
        bar.setHidden(true);
        if let Some(field) = self.ivars().filter_field.get() {
            field.setStringValue(ns_string!(""));
        }
        if let Some(grid) = self.ivars().grid.get() {
            grid.set_filter("");
        }
        self.layout_content();
        self.focus_grid();
    }

    fn focus_grid(&self) {
        let (Some(window), Some(grid)) = (self.ivars().window.get(), self.ivars().grid.get())
        else {
            return;
        };
        window.makeFirstResponder(Some(grid));
    }

    /// Stack the right-hand pane: filter bar on top when it is open,
    /// then the grid, then the status bar.
    fn layout_content(&self) {
        let (Some(content), Some(scroll), Some(bar)) = (
            self.ivars().content.get(),
            self.ivars().scroll.get(),
            self.ivars().filter_bar.get(),
        ) else {
            return;
        };
        let size = content.bounds().size;
        // The pane runs under the title bar, so the bar starts below
        // the safe area. The grid keeps scrolling under the title bar
        // while the filter is closed.
        let title_h = content.safeAreaInsets().top;
        let taken = if bar.isHidden() { 0.0 } else { title_h + FILTER_H };
        bar.setFrame(NSRect::new(
            CGPoint::new(0.0, size.height - title_h - FILTER_H),
            CGSize::new(size.width, FILTER_H),
        ));
        scroll.setFrame(NSRect::new(
            CGPoint::new(0.0, STATUS_H),
            CGSize::new(size.width, size.height - STATUS_H - taken),
        ));
    }

    // ---- move and copy ----

    /// Ask for the target folder. Cancel returns None.
    fn choose_folder(&self, kind: Transfer) -> Option<PathBuf> {
        let mtm = self.mtm();
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setAllowsMultipleSelection(false);
        // Culling usually invents its target folder ("Keepers") on
        // the spot.
        panel.setCanCreateDirectories(true);
        panel.setPrompt(Some(&NSString::from_str(match kind {
            Transfer::Move => "Move",
            Transfer::Copy => "Copy",
        })));
        if panel.runModal() != objc2_app_kit::NSModalResponseOK {
            return None;
        }
        let path = panel.URL()?.path()?;
        Some(PathBuf::from(path.to_string()))
    }

    /// Send the images to `folder` on a background thread, so a long
    /// batch (a move to another disk is a copy and a delete) leaves
    /// the window usable. Each file reports back as it lands, and the
    /// status bar carries the count and then the result.
    fn transfer_selection(&self, folder: PathBuf, kind: Transfer) {
        self.transfer_paths(self.action_targets(), folder, kind);
    }

    /// The same batch for images named by something other than the
    /// selection, such as a drop on a sidebar folder.
    pub fn transfer_paths(&self, paths: Vec<PathBuf>, folder: PathBuf, kind: Transfer) {
        if paths.is_empty() || self.ivars().transfer_busy.get() {
            return;
        }
        self.ivars().transfer_busy.set(true);

        let total = paths.len();
        std::thread::spawn(move || {
            let mut done: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(total);
            let mut renamed = 0usize;
            let mut failed = 0usize;
            for (index, source) in paths.iter().enumerate() {
                if total > 1 {
                    let line = format!("{} {} of {total}…", kind.present(), index + 1);
                    on_main(move |delegate| delegate.show_status_message(&line));
                }
                match kind.apply(source, &folder) {
                    Ok(transferred) => {
                        renamed += usize::from(transferred.renamed);
                        // The row leaves now, not at the end of the
                        // batch, so the watcher never gets there
                        // first and the selection keeps stepping.
                        if kind == Transfer::Move {
                            let source = source.clone();
                            on_main(move |delegate| delegate.moved_away(&source));
                        }
                        done.push((source.clone(), transferred.target));
                    }
                    Err(error) => {
                        eprintln!("{} failed: {}: {error}", kind.past(), source.display());
                        failed += 1;
                    }
                }
            }
            on_main(move |delegate| delegate.finish_transfer(kind, done, folder, renamed, failed));
        });
    }

    /// One image has left the folder: its row goes, and the
    /// selection steps onto whatever slid into its place.
    fn moved_away(&self, source: &Path) {
        let gone = [source.to_path_buf()];
        if let Some(grid) = self.ivars().grid.get() {
            grid.remove_and_advance(&gone);
        }
        self.drop_from_slideshow(&gone);
    }

    /// Back on the main thread when the batch ends: remember the
    /// folder, name the repeat items after it, and say what happened.
    fn finish_transfer(
        &self,
        kind: Transfer,
        done: Vec<(PathBuf, PathBuf)>,
        folder: PathBuf,
        renamed: usize,
        failed: usize,
    ) {
        self.ivars().transfer_busy.set(false);
        let touched: Vec<PathBuf> = done
            .iter()
            .flat_map(|(from, to)| [from.clone(), to.clone()])
            .collect();
        self.notice_files(&touched);
        let step = match kind {
            Transfer::Move => Step::Moved(done.clone()),
            Transfer::Copy => Step::Copied(done.iter().map(|(_, to)| to.clone()).collect()),
        };
        let label = match kind {
            Transfer::Move => "move",
            Transfer::Copy => "copy",
        };
        self.ivars().history.borrow_mut().record(label, step);
        self.refresh_history_items();
        if !done.is_empty() {
            // Only a folder that took files is worth repeating to.
            let slot = match kind {
                Transfer::Move => &self.ivars().last_move,
                Transfer::Copy => &self.ivars().last_copy,
            };
            *slot.borrow_mut() = Some(folder.clone());
            self.sync_repeat_items();
        }
        let message = transfer_message(kind, &done, &folder, renamed, failed);
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    /// The grid's cells changed size: keep the slider and the saved
    /// size with it.
    pub fn thumb_size_changed(&self, size: f64) {
        if let Some(slider) = self.ivars().thumb_slider.get() {
            slider.setDoubleValue(size);
        }
        if !e2e::enabled() {
            NSUserDefaults::standardUserDefaults()
                .setDouble_forKey(size, ns_string!("thumbSize"));
        }
    }

    /// The slider moved.
    fn thumb_slider_moved(&self) {
        let (Some(slider), Some(grid)) = (self.ivars().thumb_slider.get(), self.ivars().grid.get())
        else {
            return;
        };
        grid.set_cell_size(slider.doubleValue());
    }

    /// The preference moved: the ceiling for the slider, the cells,
    /// and the size thumbnails are decoded at.
    fn apply_thumb_cap(&self, cap: f64) {
        if let Some(grid) = self.ivars().grid.get() {
            grid.set_cell_cap(cap);
            if let Some(slider) = self.ivars().thumb_slider.get() {
                slider.setMaxValue(cap);
                slider.setDoubleValue(grid.cell_size());
            }
        }
        if let Some(engine) = self.ivars().engine.get() {
            // Retina: a cell is drawn at twice its points.
            engine.set_max_thumb_px((cap * 2.0) as i32);
        }
        if !e2e::enabled() {
            NSUserDefaults::standardUserDefaults()
                .setDouble_forKey(cap, ns_string!("thumbCap"));
        }
    }

    /// Tell the engine about files the app itself moved, so its list
    /// matches the disk before the watcher gets there. Only files the
    /// grid is showing count: a file that landed in another folder is
    /// none of this folder's business.
    fn notice_files(&self, paths: &[PathBuf]) {
        let Some(engine) = self.ivars().engine.get() else { return };
        let scan = self.ivars().current_scan.borrow().clone();
        let Some((root, recursive)) = scan else { return };
        let ours: Vec<PathBuf> = paths
            .iter()
            .filter(|path| {
                if recursive {
                    path.starts_with(&root)
                } else {
                    path.parent() == Some(root.as_path())
                }
            })
            .cloned()
            .collect();
        if !ours.is_empty() {
            engine.notice(&ours);
        }
    }

    /// cmd-Z and shift-cmd-Z.    /// cmd-Z and shift-cmd-Z. Undo reverses the last batch; redo
    /// performs it again. Both end in the same place: a list of files
    /// to move, and a batch recorded on the other stack.
    fn take_back(&self, undoing: bool) {
        if self.ivars().transfer_busy.get() {
            return;
        }
        let entry = {
            let mut history = self.ivars().history.borrow_mut();
            if undoing {
                history.take_undo()
            } else {
                history.take_redo()
            }
        };
        let Some(entry) = entry else { return };

        let (moves, next_step) = match (&entry.step, undoing) {
            // Undoing a move or a cull walks the files back.
            (Step::Moved(items), true) => (
                items.iter().map(|(from, to)| (to.clone(), from.clone())).collect::<Vec<_>>(),
                Step::Moved(items.clone()),
            ),
            // Redoing one walks them forward again.
            (Step::Moved(items), false) => (items.clone(), Step::Moved(items.clone())),
            // Undoing a copy trashes the copies. Where they land in
            // the trash becomes the way back, so a redo can fetch
            // them out again.
            (Step::Copied(copies), true) => {
                let trashed = trash::move_to_trash(copies);
                let back: Vec<(PathBuf, PathBuf)> = trashed
                    .moved
                    .iter()
                    .filter(|(_, landed)| !landed.as_os_str().is_empty())
                    .map(|(copy, landed)| (landed.clone(), copy.clone()))
                    .collect();
                let gone: Vec<PathBuf> = trashed.moved.iter().map(|(copy, _)| copy.clone()).collect();
                self.after_take_back(&gone, &[], entry.label.clone(), Step::Moved(back), undoing);
                return;
            }
            // A copy is never redone by copying again: the files are
            // sitting in the trash, so they are fetched back.
            (Step::Copied(copies), false) => (Vec::new(), Step::Copied(copies.clone())),
        };

        let mut arrived: Vec<PathBuf> = Vec::new();
        let mut left: Vec<PathBuf> = Vec::new();
        let mut failed = 0usize;
        for (from, to) in &moves {
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::rename(from, to).or_else(|_| copy_then_delete(from, to)) {
                Ok(()) => {
                    arrived.push(to.clone());
                    left.push(from.clone());
                }
                Err(error) => {
                    eprintln!("undo failed: {} -> {}: {error}", from.display(), to.display());
                    failed += 1;
                }
            }
        }
        if failed > 0 {
            let message = format!("{failed} of {} could not be put back", moves.len());
            self.show_status_message(&message);
            self.flash_overlay(&message);
        }
        self.after_take_back(&left, &arrived, entry.label.clone(), next_step, undoing);
    }

    /// Tidy up after a batch went back or forward: rows that left the
    /// folder go, the other stack gets the batch, and the menu says
    /// what is next.
    fn after_take_back(
        &self,
        left: &[PathBuf],
        arrived: &[PathBuf],
        label: String,
        step: Step,
        undoing: bool,
    ) {
        if let Some(grid) = self.ivars().grid.get() {
            grid.remove_and_advance(left);
        }
        self.drop_from_slideshow(left);
        // The app is the authority for its own changes, here as well:
        // the rows come back now rather than whenever the watcher
        // gets to it.
        let touched: Vec<PathBuf> = left.iter().chain(arrived.iter()).cloned().collect();
        self.notice_files(&touched);
        {
            let mut history = self.ivars().history.borrow_mut();
            let entry = Entry { label: label.clone(), step };
            if undoing {
                history.push_undone(entry);
            } else {
                history.push_done(entry);
            }
        }
        self.refresh_history_items();
        let count = arrived.len().max(left.len());
        let what = if undoing { "Undid" } else { "Redid" };
        let message = match count {
            0 => format!("Nothing to take back for the {label}"),
            1 => format!("{what} the {label}"),
            n => format!("{what} the {label} of {n} images"),
        };
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    /// Keep the Edit menu honest about what cmd-Z would do next.
    fn refresh_history_items(&self) {
        let history = self.ivars().history.borrow();
        if let Some(item) = self.ivars().undo_item.get() {
            item.setTitle(&NSString::from_str(&history.undo_title()));
        }
        if let Some(item) = self.ivars().redo_item.get() {
            item.setTitle(&NSString::from_str(&history.redo_title()));
        }
    }

    /// The apps that can open the first target, as a submenu. Built
    /// fresh every time it opens, because the selection moves.
    pub fn open_with_menu(&self, mtm: MainThreadMarker) -> Retained<NSMenu> {
        let menu = NSMenu::new(mtm);
        self.fill_open_with(&menu);
        menu
    }

    /// Fill a menu with the apps that can open the first target. The
    /// File menu's submenu is refilled every time it opens; the
    /// right-click menu is built fresh anyway.
    fn fill_open_with(&self, menu: &NSMenu) {
        let mtm = self.mtm();
        menu.removeAllItems();
        let targets = self.action_targets();
        let Some(first) = targets.first() else { return };
        let url = NSURL::fileURLWithPath(&NSString::from_str(&first.to_string_lossy()));
        let workspace = NSWorkspace::sharedWorkspace();
        for app in workspace.URLsForApplicationsToOpenURL(&url).iter() {
            let Some(path) = app.path() else { continue };
            let name = objc2_foundation::NSFileManager::defaultManager()
                .displayNameAtPath(&path);
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &name,
                    Some(sel!(openWith:)),
                    ns_string!(""),
                )
            };
            unsafe {
                item.setTarget(Some(self));
                item.setRepresentedObject(Some(&app));
            }
            menu.addItem(&item);
        }
    }

    fn open_targets_with(&self, app: &NSURL) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let urls: Vec<Retained<NSURL>> = targets
            .iter()
            .map(|path| NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy())))
            .collect();
        let configuration = objc2_app_kit::NSWorkspaceOpenConfiguration::configuration();
        NSWorkspace::sharedWorkspace().openURLs_withApplicationAtURL_configuration_completionHandler(
            &NSArray::from_retained_slice(&urls),
            app,
            &configuration,
            None,
        );
    }

    /// Put the targets on the clipboard, as plain paths or as file
    /// URLs, one per line.
    fn copy_targets(&self, as_url: bool) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let text = targets
            .iter()
            .map(|path| {
                if as_url {
                    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
                        .absoluteString()
                        .map(|s| s.to_string())
                        .unwrap_or_default()
                } else {
                    path.to_string_lossy().into_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let pasteboard = objc2_app_kit::NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        unsafe {
            pasteboard.setString_forType(
                &NSString::from_str(&text),
                objc2_app_kit::NSPasteboardTypeString,
            )
        };
        let what = if as_url { "file URL" } else { "path" };
        let message = match targets.len() {
            1 => format!("Copied the {what}"),
            n => format!("Copied {n} {what}s"),
        };
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    /// cmd-D: the current image becomes the desktop picture, on the
    /// screen the window is on.
    fn use_as_desktop_picture(&self) {
        let targets = self.action_targets();
        let Some(path) = targets.first() else { return };
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let screen = self
            .ivars()
            .window
            .get()
            .and_then(|window| window.screen())
            .or_else(|| NSScreen::mainScreen(self.mtm()));
        let Some(screen) = screen else { return };
        let message = match unsafe {
            NSWorkspace::sharedWorkspace()
                .setDesktopImageURL_forScreen_options_error(
                    &url,
                    &screen,
                    &objc2_foundation::NSDictionary::new(),
                )
        } {
            Ok(()) => format!("Desktop picture: {}", file_name(path)),
            Err(_) => "Could not set the desktop picture".to_owned(),
        };
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    /// cmd-I: the info panel, a floating window that follows whatever
    /// an action would work on. A second press puts it away.
    fn toggle_info(&self) {
        let mtm = self.mtm();
        let window = self.ivars().info_window.get().cloned().unwrap_or_else(|| {
            let (window, label) = build_info_window(mtm);
            let _ = self.ivars().info_window.set(window.clone());
            let _ = self.ivars().info_label.set(label);
            window
        });
        if window.isVisible() {
            window.orderOut(None);
            return;
        }
        // Show it first: the refresh fills only a panel that is up,
        // so filling it before would leave it blank.
        window.makeKeyAndOrderFront(None);
        self.refresh_info();
    }

    /// Fill the panel, if it is up. Cheap enough to run on every
    /// selection change: the header read is not a decode.
    fn refresh_info(&self) {
        let Some(window) = self.ivars().info_window.get() else { return };
        let Some(label) = self.ivars().info_label.get() else { return };
        if !window.isVisible() {
            return;
        }
        label.setStringValue(&NSString::from_str(&self.info_text()));
        label.sizeToFit();
    }

    /// What the panel says: one image in full, several as a summary,
    /// none as a line saying so.
    fn info_text(&self) -> String {
        let paths = self.action_targets();
        match paths.as_slice() {
            [] => "No image selected.".to_owned(),
            [path] => {
                let mut rows = vec![("Name".to_owned(), file_name(path))];
                if let Some(kind) = path.extension() {
                    rows.push((
                        "Kind".to_owned(),
                        format!("{} image", kind.to_string_lossy().to_uppercase()),
                    ));
                }
                if let Ok(meta) = std::fs::metadata(path) {
                    rows.push(("Size".to_owned(), format_bytes(meta.len())));
                    if let Ok(modified) = meta.modified() {
                        rows.push(("Modified".to_owned(), date_text(modified)));
                    }
                }
                rows.extend(decoder::image_info(path));
                if let Some(folder) = path.parent() {
                    rows.push(("Folder".to_owned(), folder.to_string_lossy().into_owned()));
                }
                rows_text(&rows)
            }
            many => {
                let bytes: u64 = many
                    .iter()
                    .filter_map(|path| std::fs::metadata(path).ok())
                    .map(|meta| meta.len())
                    .sum();
                let folder = many[0]
                    .parent()
                    .map(|folder| folder.to_string_lossy().into_owned())
                    .unwrap_or_default();
                rows_text(&[
                    ("Selected".to_owned(), format!("{} images", many.len())),
                    ("Size".to_owned(), format_bytes(bytes)),
                    ("Folder".to_owned(), folder),
                ])
            }
        }
    }

    /// cmd-R: show what an action would work on in Finder, selected
    /// inside its folder.
    fn reveal_targets(&self) {
        let paths = self.action_targets();
        if paths.is_empty() {
            return;
        }
        let urls: Vec<Retained<NSURL>> = paths
            .iter()
            .map(|path| NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy())))
            .collect();
        NSWorkspace::sharedWorkspace()
            .activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&urls));
    }

    /// What a repeat item is called: the verb alone until a folder has
    /// been used, the folder's name after that.
    pub fn repeat_title(&self, kind: Transfer) -> String {
        let (verb, folder) = match kind {
            Transfer::Move => ("Move", self.ivars().last_move.borrow().clone()),
            Transfer::Copy => ("Copy", self.ivars().last_copy.borrow().clone()),
        };
        match folder {
            Some(folder) => format!("{verb} again to {}", folder_label(&folder)),
            None => format!("{verb} again"),
        }
    }

    /// Name the repeat items after their folders.
    fn sync_repeat_items(&self) {
        for (item, kind) in [
            (self.ivars().move_again_item.get(), Transfer::Move),
            (self.ivars().copy_again_item.get(), Transfer::Copy),
        ] {
            let Some(item) = item else { continue };
            item.setTitle(&NSString::from_str(&self.repeat_title(kind)));
        }
    }

    // ---- trash ----

    /// Items that act on images need something to act on: the
    /// slideshow's slide or the grid's selection. The repeat items
    /// also need a folder to repeat to.
    fn menu_item_enabled(&self, item: &NSMenuItem) -> bool {
        let action = item.action();
        let prefs_key = self
            .ivars()
            .prefs_window
            .get()
            .is_some_and(|window| window.isKeyWindow());
        if action == Some(sel!(undoLast:)) || action == Some(sel!(redoLast:)) {
            // A text field brings its own undo, and cmd-Z belongs to
            // whatever is being typed while one is being edited.
            let editing = self
                .ivars()
                .window
                .get()
                .and_then(|window| window.firstResponder())
                .is_some_and(|responder| responder.isKindOfClass(NSTextView::class()));
            if editing || prefs_key {
                return false;
            }
            let history = self.ivars().history.borrow();
            return if action == Some(sel!(undoLast:)) {
                history.can_undo()
            } else {
                history.can_redo()
            };
        }
        // Selecting and filtering belong to the grid, so they go
        // grey during a slideshow and in the preferences window.
        if action == Some(sel!(selectAll:)) || action == Some(sel!(showFilter:)) {
            return !prefs_key && self.ivars().show.borrow().is_none();
        }
        let acts_on_images = [
            sel!(moveToTrash:),
            sel!(revealInFinder:),
            sel!(openWith:),
            sel!(copyPath:),
            sel!(copyFileUrl:),
            sel!(setDesktopPicture:),
            sel!(moveToFolder:),
            sel!(copyToFolder:),
            sel!(moveAgain:),
            sel!(copyAgain:),
        ]
        .iter()
        .any(|wanted| action == Some(*wanted));
        if !acts_on_images {
            return true;
        }
        if action == Some(sel!(moveAgain:)) && self.ivars().last_move.borrow().is_none() {
            return false;
        }
        if action == Some(sel!(copyAgain:)) && self.ivars().last_copy.borrow().is_none() {
            return false;
        }
        let in_show = self.ivars().show.borrow().is_some();
        let selected = self
            .ivars()
            .grid
            .get()
            .is_some_and(|grid| !grid.selected_paths().is_empty());
        (in_show || selected) && !prefs_key
    }

    /// The images an action works on: the slide on screen during a
    /// slideshow, the grid's selection otherwise.
    fn action_targets(&self) -> Vec<PathBuf> {
        let current = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .and_then(|show| show.playlist.current().cloned());
        match current {
            Some(path) => vec![path],
            None => self.ivars().grid.get().map(|g| g.selected_paths()).unwrap_or_default(),
        }
    }

    /// cmd-Delete: move the slideshow's current slide, or the grid's
    /// selection, to the system trash. No confirmation: the trash is
    /// the safety net.
    pub fn trash_selection(&self) {
        let paths = self.action_targets();
        if paths.is_empty() {
            return;
        }

        let trashed = trash::move_to_trash(&paths);
        let moved: Vec<PathBuf> = trashed.moved.iter().map(|(from, _)| from.clone()).collect();
        // Only files the trash could point at can come back.
        let restorable: Vec<(PathBuf, PathBuf)> = trashed
            .moved
            .iter()
            .filter(|(_, landed)| !landed.as_os_str().is_empty())
            .cloned()
            .collect();
        self.ivars()
            .history
            .borrow_mut()
            .record("move to trash", Step::Moved(restorable));
        self.refresh_history_items();

        // The app's delete is the authority. The rows go now, so the
        // watcher's echo a moment later finds nothing and does
        // nothing.
        if let Some(grid) = self.ivars().grid.get() {
            grid.remove_and_advance(&moved);
        }
        self.drop_from_slideshow(&moved);
        // Tell the engine too, so its list matches the disk at once.
        // Without this an undo a second later finds the path still
        // listed and no row comes back.
        self.notice_files(&moved);

        let message = trash_message(&moved, trashed.failed.len());
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
        self.refresh_info();
    }

    /// Count plus name, dimensions, and size of the selection, like
    /// the original app's status bar.
    fn update_status(&self) {
        let Some(status) = self.ivars().status.get() else { return };
        let Some(grid) = self.ivars().grid.get() else { return };
        let (total, selected) = grid.selection_info();
        let (shown, in_folder) = grid.counts();
        let all_text = if grid.filtering() {
            format!("{shown} of {in_folder} images")
        } else {
            format!("{total} images")
        };
        let text = match selected.as_slice() {
            [] => all_text,
            [info] => {
                let name = info
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let dims = decoder::image_dimensions(&info.path)
                    .map(|(w, h)| format!(" — {w}×{h}"))
                    .unwrap_or_default();
                format!("{name}{dims} — {} · {all_text}", format_bytes(info.size))
            }
            many => {
                let bytes: u64 = many.iter().map(|f| f.size).sum();
                let mut text =
                    format!("{} of {shown} selected — {}", many.len(), format_bytes(bytes));
                if grid.filtering() {
                    text.push_str(&format!(" · {all_text}"));
                }
                text
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
        overlay.setUsesSingleLineMode(false);
        // Monospaced, so the EXIF block and the cheat sheet line up
        // in columns instead of drifting.
        overlay.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(12.0, 0.0)));
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
    /// Page up and page down: a jump that stops at the ends.
    pub fn jump_slideshow(&self, delta: i64) {
        let moved = match self.ivars().show.borrow_mut().as_mut() {
            Some(show) => show.playlist.jump(delta),
            None => return,
        };
        if moved {
            self.show_current();
        }
    }

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

    /// shift-I: the file's own EXIF under the usual line.
    pub fn toggle_exif_overlay(&self) {
        let on = !self.ivars().overlay_exif.get();
        self.ivars().overlay_exif.set(on);
        self.show_overlay_for_extra(on);
    }

    /// p: the full path under the usual line.
    pub fn toggle_path_overlay(&self) {
        let on = !self.ivars().overlay_path.get();
        self.ivars().overlay_path.set(on);
        self.show_overlay_for_extra(on);
    }

    /// h or ?: the cheat sheet, which replaces the line while it is up.
    pub fn toggle_help_overlay(&self) {
        let on = !self.ivars().overlay_help.get();
        self.ivars().overlay_help.set(on);
        self.show_overlay_for_extra(on);
    }

    /// An extra block is useless behind a hidden overlay, so turning
    /// one on turns the overlay on with it. Turning the last one off
    /// leaves the overlay where the user had it.
    fn show_overlay_for_extra(&self, on: bool) {
        if on && !self.ivars().overlay_visible.get() {
            self.ivars().overlay_visible.set(true);
            if let Some(show) = self.ivars().show.borrow().as_ref() {
                show.overlay.setHidden(false);
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
        self.refresh_info();
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
        let text = if self.ivars().overlay_help.get() {
            SLIDESHOW_HELP.to_owned()
        } else {
            self.with_overlay_extras(text)
        };
        overlay.setStringValue(&NSString::from_str(&text));
        // The block grows downward from a fixed bottom-left corner,
        // so a long EXIF list never walks off the screen.
        let bottom_left = overlay.frame().origin;
        overlay.sizeToFit();
        let mut frame = overlay.frame();
        frame.origin = bottom_left;
        overlay.setFrame(frame);
        // Visible as the title bar in windowed mode.
        window.setTitle(&NSString::from_str(&name));
    }

    /// The usual line, plus whatever extra blocks are switched on.
    fn with_overlay_extras(&self, line: String) -> String {
        let want_exif = self.ivars().overlay_exif.get();
        let want_path = self.ivars().overlay_path.get();
        if !want_exif && !want_path {
            return line;
        }
        let Some(path) = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .and_then(|show| show.playlist.current().cloned())
        else {
            return line;
        };
        let mut text = line;
        if want_path {
            text.push('\n');
            text.push_str(&path.to_string_lossy());
        }
        if want_exif {
            let rows = decoder::image_info(&path);
            if rows.is_empty() {
                text.push_str("\nNo EXIF in this file.");
            } else {
                text.push('\n');
                text.push_str(&rows_text(&rows));
            }
        }
        text
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

    /// Every key equivalent in the menu bar that two items share.
    /// A shortcut bound twice is a shortcut that does the wrong thing
    /// half the time, and it is the kind of mistake that arrives with
    /// the next menu item rather than this one.
    pub fn e2e_menu_conflicts(&self) -> Vec<String> {
        fn walk(menu: &NSMenu, seen: &mut HashMap<(String, usize), String>, clashes: &mut Vec<String>) {
            for item in menu.itemArray() {
                let key = item.keyEquivalent().to_string();
                if !key.is_empty() {
                    let mask = item.keyEquivalentModifierMask().bits() as usize;
                    let title = item.title().to_string();
                    match seen.get(&(key.clone(), mask)) {
                        // The same entry twice is one shortcut, not a
                        // clash: a hidden twin catches the shifted
                        // press of the same key.
                        Some(held) if *held == title => {}
                        Some(held) => clashes.push(format!("{key} ({mask}): {held} and {title}")),
                        None => {
                            seen.insert((key, mask), title);
                        }
                    }
                }
                if let Some(submenu) = item.submenu() {
                    walk(&submenu, seen, clashes);
                }
            }
        }
        let mtm = self.mtm();
        let Some(menubar) = NSApplication::sharedApplication(mtm).mainMenu() else {
            return Vec::new();
        };
        let mut seen = HashMap::new();
        let mut clashes = Vec::new();
        walk(&menubar, &mut seen, &mut clashes);
        clashes
    }

    pub fn e2e_thumb_cap(&self, cap: f64) {
        self.apply_thumb_cap(cap);
    }

    pub fn e2e_thumb_size(&self) -> f64 {
        self.ivars().grid.get().map(|grid| grid.cell_size()).unwrap_or_default()
    }

    pub fn e2e_slider_value(&self) -> f64 {
        self.ivars()
            .thumb_slider
            .get()
            .map(|slider| slider.doubleValue())
            .unwrap_or_default()
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

    pub fn e2e_grid_context_menu(&self, index: usize) -> Vec<(String, bool)> {
        self.ivars()
            .grid
            .get()
            .map(|grid| grid.e2e_context_menu(index))
            .unwrap_or_default()
    }

    pub fn e2e_sidebar_context_menu(&self, path: &str) -> Vec<String> {
        self.ivars()
            .sidebar
            .get()
            .map(|sidebar| sidebar.e2e_context_menu(std::path::Path::new(path)))
            .unwrap_or_default()
    }

    /// Drop a folder into Favorites at `at`, the way a drag does.
    pub fn e2e_insert_favorite(&self, path: &str, at: usize) {
        if let Some(sidebar) = self.ivars().sidebar.get() {
            sidebar.insert_favorite(std::path::Path::new(path), at);
        }
    }

    pub fn e2e_favorite_names(&self) -> Vec<String> {
        self.ivars()
            .sidebar
            .get()
            .map(|sidebar| {
                sidebar
                    .e2e_favorites()
                    .iter()
                    .filter_map(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn e2e_toggle_exif_overlay(&self) {
        self.toggle_exif_overlay();
    }

    pub fn e2e_toggle_path_overlay(&self) {
        self.toggle_path_overlay();
    }

    pub fn e2e_toggle_help_overlay(&self) {
        self.toggle_help_overlay();
    }

    pub fn e2e_undo(&self) {
        self.take_back(true);
    }

    pub fn e2e_redo(&self) {
        self.take_back(false);
    }

    pub fn e2e_undo_title(&self) -> String {
        self.ivars().history.borrow().undo_title()
    }

    pub fn e2e_redo_title(&self) -> String {
        self.ivars().history.borrow().redo_title()
    }

    pub fn e2e_copy_path(&self, as_url: bool) {
        self.copy_targets(as_url);
    }

    pub fn e2e_pasteboard_text(&self) -> String {
        unsafe {
            objc2_app_kit::NSPasteboard::generalPasteboard()
                .stringForType(objc2_app_kit::NSPasteboardTypeString)
        }
        .map(|s| s.to_string())
        .unwrap_or_default()
    }

    pub fn e2e_open_with_names(&self) -> Vec<String> {
        self.open_with_menu(self.mtm())
            .itemArray()
            .iter()
            .map(|item| item.title().to_string())
            .collect()
    }

    pub fn e2e_info_text(&self) -> String {
        self.info_text()
    }

    pub fn e2e_toggle_info(&self) {
        self.toggle_info();
    }

    pub fn e2e_info_open(&self) -> bool {
        self.ivars()
            .info_window
            .get()
            .is_some_and(|window| window.isVisible())
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

    /// Drive a move or a copy without the folder chooser.
    pub fn e2e_transfer(&self, folder: &str, moving: bool) {
        let kind = if moving { Transfer::Move } else { Transfer::Copy };
        self.transfer_selection(PathBuf::from(folder), kind);
    }

    pub fn e2e_open_filter(&self) {
        self.open_filter();
    }

    /// Esc, the same way the field hands it over.
    pub fn e2e_close_filter(&self) {
        self.filter_field_command(sel!(cancelOperation:));
    }

    /// Type into the field and let the delegate do the rest.
    pub fn e2e_type_filter(&self, text: &str) {
        if let Some(field) = self.ivars().filter_field.get() {
            field.setStringValue(&NSString::from_str(text));
        }
        self.filter_changed();
    }

    pub fn e2e_filter_open(&self) -> bool {
        self.ivars()
            .filter_bar
            .get()
            .is_some_and(|bar| !bar.isHidden())
    }

    pub fn e2e_select_all(&self) {
        if let Some(grid) = self.ivars().grid.get() {
            grid.select_all();
        }
    }

    pub fn e2e_transfer_busy(&self) -> bool {
        self.ivars().transfer_busy.get()
    }

    pub fn e2e_repeat_item_title(&self, moving: bool) -> String {
        let item = if moving {
            self.ivars().move_again_item.get()
        } else {
            self.ivars().copy_again_item.get()
        };
        item.map(|i| i.title().to_string()).unwrap_or_default()
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
                // Their own folder has finished scanning and they
                // are still not there: the files are gone from disk,
                // so stop waiting for them. A scan of any other
                // folder leaves them waiting.
                let theirs = self.pending_folder();
                if !self.select_pending()
                    && theirs.is_some()
                    && theirs == self.current_scan().map(|(root, _)| root)
                {
                    self.ivars().pending_open.borrow_mut().clear();
                }
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

/// How wide the thumbnail slider is, and the two preferences behind
/// it: the ceiling the popup sets, and the size last left behind.
const THUMB_SLIDER_W: f64 = 110.0;

fn thumb_cap_pref() -> f64 {
    if e2e::enabled() {
        return grid::CELL_CAPS[1];
    }
    let stored = NSUserDefaults::standardUserDefaults().doubleForKey(ns_string!("thumbCap"));
    if grid::CELL_CAPS.contains(&stored) {
        stored
    } else {
        grid::CELL_CAPS[1]
    }
}

fn thumb_size_pref() -> f64 {
    if e2e::enabled() {
        return grid::CELL;
    }
    let stored = NSUserDefaults::standardUserDefaults().doubleForKey(ns_string!("thumbSize"));
    if stored >= 60.0 {
        stored.min(thumb_cap_pref())
    } else {
        grid::CELL
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
/// (1-5 checkboxes, 6 auto-advance popup, 7 startup folder field,
/// 8 largest thumbnail popup). Rows are laid out from the top.
fn build_prefs_window(mtm: MainThreadMarker, delegate: &AppDelegate) -> Retained<NSWindow> {
    let frame = NSRect::new(CGPoint::new(360.0, 360.0), CGSize::new(430.0, 296.0));
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
    label("Startup folder:", 20.0, 256.0);
    let field = NSTextField::labelWithString(ns_string!(""), mtm);
    field.setFrame(NSRect::new(CGPoint::new(20.0, 236.0), CGSize::new(250.0, 18.0)));
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
        b.setFrame(NSRect::new(CGPoint::new(x, 230.0), CGSize::new(72.0, 28.0)));
        content.addSubview(&b);
    }

    checkbox(
        "Start slideshows in a window",
        sel!(prefsToggleWindowed:),
        1,
        196.0,
    );
    checkbox("Loop slideshows", sel!(toggleLoop:), 2, 168.0);
    checkbox("Shuffle slideshows", sel!(toggleShuffle:), 3, 140.0);

    label("Auto-advance:", 20.0, 108.0);
    let popup = objc2_app_kit::NSPopUpButton::new(mtm);
    popup.setFrame(NSRect::new(CGPoint::new(150.0, 100.0), CGSize::new(180.0, 26.0)));
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

    label("Largest thumbnail:", 20.0, 68.0);
    let thumbs = objc2_app_kit::NSPopUpButton::new(mtm);
    thumbs.setFrame(NSRect::new(CGPoint::new(150.0, 60.0), CGSize::new(180.0, 26.0)));
    thumbs.setTag(8);
    for cap in grid::CELL_CAPS {
        thumbs.addItemWithTitle(&NSString::from_str(&format!("{cap:.0} points")));
        if let Some(item) = thumbs.lastItem() {
            item.setTag(cap as isize);
        }
    }
    unsafe {
        thumbs.setTarget(Some(delegate));
        thumbs.setAction(Some(sel!(prefsThumbCap:)));
    }
    content.addSubview(&thumbs);

    checkbox("Show filenames", sel!(toggleLabels:), 4, 28.0);

    window.setContentView(Some(&content));
    window
}

/// Run `f` on the main thread with the app delegate, from a worker.
fn on_main(f: impl FnOnce(&AppDelegate) + Send + 'static) {
    DispatchQueue::main().exec_async(move || {
        let mtm = MainThreadMarker::new().expect("main queue is the main thread");
        f(DELEGATE.get().expect("delegate is set at launch").get(mtm));
    });
}

/// A folder's own name, for menu titles and status lines.
fn folder_label(folder: &Path) -> String {
    folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| folder.to_string_lossy().into_owned())
}

/// What the status bar and the slide overlay say after a move or a
/// copy. A single file is named by the name it has now, which is the
/// only record of a rename.
fn transfer_message(
    kind: Transfer,
    done: &[(PathBuf, PathBuf)],
    folder: &Path,
    renamed: usize,
    failed: usize,
) -> String {
    let verb = kind.past();
    let target = folder_label(folder);
    let mut text = match done {
        [] => format!("Nothing {} to {target}", verb.to_lowercase()),
        [(_, one)] => format!(
            "{verb} {} to {target}",
            one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        ),
        many => format!("{verb} {} images to {target}", many.len()),
    };
    if renamed > 0 {
        text.push_str(&format!(", {renamed} renamed"));
    }
    if failed > 0 {
        text.push_str(&format!(", {failed} failed"));
    }
    text
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

/// rename(2) refuses to cross disks, so an undo that crosses one
/// copies and then deletes, the way a move does.
fn copy_then_delete(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)
}

/// The cheat sheet, shown over the slide by h or ?. It is written by
/// hand, so it has to be kept honest against the key handler in
/// slideshow.rs.
const SLIDESHOW_HELP: &str = "\
Slideshow keys

→ ↓ l      next slide          ← ↑ j      previous slide
space      next, or pause      ⌥ + arrow  step in sorted order
page up    back ten            page down  on ten
home       first slide         end        last slide
1 to 9     advance every n s   0          advance off
!          every half second   @          every 1.5 seconds
+ -        zoom in and out     =          actual size
*          reset the view      r R f      rotate, rotate back, flip
i          this line           ⇧I         EXIF
p          full path           h or ?     this sheet
⌘⌫         move to trash       ⌘I         info panel
click      next slide          right-click  previous
scroll     step slides         drag       pan a zoomed image
esc or q   end the slideshow";

/// Label and value, one pair per line, labels padded so the values
/// line up in the panel's fixed-width layout.
fn rows_text(rows: &[(String, String)]) -> String {
    let width = rows.iter().map(|(label, _)| label.chars().count()).max().unwrap_or(0);
    rows.iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(label, value)| format!("{label:<width$}   {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A date the way the user's own settings write it.
pub fn date_text(time: std::time::SystemTime) -> String {
    let Ok(since) = time.duration_since(std::time::UNIX_EPOCH) else {
        return String::new();
    };
    let date = objc2_foundation::NSDate::dateWithTimeIntervalSince1970(since.as_secs_f64());
    objc2_foundation::NSDateFormatter::localizedStringFromDate_dateStyle_timeStyle(
        &date,
        objc2_foundation::NSDateFormatterStyle::MediumStyle,
        objc2_foundation::NSDateFormatterStyle::ShortStyle,
    )
    .to_string()
}

/// An EXIF capture time, printed the way the user's settings write
/// dates. EXIF holds the camera's own local time with no zone, so it
/// is read in the local zone and written back in it: the two cancel
/// and the digits stay the ones the camera recorded.
pub fn exif_date_text(raw: &str) -> Option<String> {
    let formatter = objc2_foundation::NSDateFormatter::new();
    formatter.setDateFormat(Some(ns_string!("yyyy:MM:dd HH:mm:ss")));
    let date = formatter.dateFromString(&NSString::from_str(raw))?;
    Some(
        objc2_foundation::NSDateFormatter::localizedStringFromDate_dateStyle_timeStyle(
            &date,
            objc2_foundation::NSDateFormatterStyle::MediumStyle,
            objc2_foundation::NSDateFormatterStyle::ShortStyle,
        )
        .to_string(),
    )
}

/// The info panel: a floating utility window holding one label. It
/// floats so it can sit beside a full-screen slideshow.
fn build_info_window(mtm: MainThreadMarker) -> (Retained<NSWindow>, Retained<NSTextField>) {
    let frame = NSRect::new(CGPoint::new(120.0, 400.0), CGSize::new(380.0, 360.0));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::UtilityWindow
                | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(ns_string!("Info"));
    unsafe { window.setReleasedWhenClosed(false) };
    window.setLevel(objc2_app_kit::NSFloatingWindowLevel as isize);

    let label = NSTextField::labelWithString(ns_string!(""), mtm);
    label.setSelectable(true);
    label.setUsesSingleLineMode(false);
    label.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(11.0, 0.0)));
    label.setFrame(NSRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(frame.size.width - 24.0, frame.size.height - 24.0),
    ));

    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), NSRect::new(
        CGPoint::new(12.0, 12.0),
        CGSize::new(frame.size.width - 24.0, frame.size.height - 24.0),
    ));
    scroll.setHasVerticalScroller(true);
    scroll.setDrawsBackground(false);
    scroll.setDocumentView(Some(&label));
    window.setContentView(Some(&scroll));
    (window, label)
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
    for (title, action, key, alternate) in [
        ("Hide Wêne", sel!(hide:), "h", false),
        ("Hide others", sel!(hideOtherApplications:), "h", true),
        ("Show all", sel!(unhideAllApplications:), "", false),
    ] {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        if alternate {
            item.setKeyEquivalentModifierMask(
                objc2_app_kit::NSEventModifierFlags::Command
                    | objc2_app_kit::NSEventModifierFlags::Option,
            );
        }
        app_menu.addItem(&item);
    }
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Quit Wêne"),
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
    let reveal = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Reveal in Finder"),
            Some(sel!(revealInFinder:)),
            ns_string!("r"),
        )
    };
    file_menu.addItem(&reveal);
    let info = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Get info"),
            Some(sel!(getInfo:)),
            ns_string!("i"),
        )
    };
    file_menu.addItem(&info);
    let open_with = NSMenuItem::new(mtm);
    open_with.setTitle(ns_string!("Open with"));
    let open_with_menu = delegate.open_with_menu(mtm);
    open_with_menu.setDelegate(Some(ProtocolObject::from_ref(delegate)));
    open_with.setSubmenu(Some(&open_with_menu));
    file_menu.addItem(&open_with);
    let desktop = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Set as desktop picture"),
            Some(sel!(setDesktopPicture:)),
            ns_string!("d"),
        )
    };
    file_menu.addItem(&desktop);
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
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    // Plain cmd-M is the system minimize, and cmd-C stays free for a
    // copy to the clipboard later.
    for (title, action, key, shift) in [
        ("Move to…", sel!(moveToFolder:), "m", true),
        ("Copy to…", sel!(copyToFolder:), "c", true),
        ("Move again", sel!(moveAgain:), "m", false),
        ("Copy again", sel!(copyAgain:), "c", false),
    ] {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        item.setKeyEquivalentModifierMask(if shift {
            objc2_app_kit::NSEventModifierFlags::Command
                | objc2_app_kit::NSEventModifierFlags::Shift
        } else {
            objc2_app_kit::NSEventModifierFlags::Command
                | objc2_app_kit::NSEventModifierFlags::Control
        });
        file_menu.addItem(&item);
        if action == sel!(moveAgain:) {
            let _ = delegate.ivars().move_again_item.set(item);
        } else if action == sel!(copyAgain:) {
            let _ = delegate.ivars().copy_again_item.set(item);
        }
    }
    file_item.setSubmenu(Some(&file_menu));
    menubar.addItem(&file_item);

    let edit_item = NSMenuItem::new(mtm);
    let edit_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Edit"));
    let select_all = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Select all"),
            Some(sel!(selectAll:)),
            ns_string!("a"),
        )
    };
    let undo = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Undo"),
            Some(sel!(undoLast:)),
            ns_string!("z"),
        )
    };
    edit_menu.addItem(&undo);
    let _ = delegate.ivars().undo_item.set(undo);
    let redo = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Redo"),
            Some(sel!(redoLast:)),
            ns_string!("z"),
        )
    };
    redo.setKeyEquivalentModifierMask(
        objc2_app_kit::NSEventModifierFlags::Command | objc2_app_kit::NSEventModifierFlags::Shift,
    );
    edit_menu.addItem(&redo);
    let _ = delegate.ivars().redo_item.set(redo);
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    edit_menu.addItem(&select_all);
    let filter = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Filter by name"),
            Some(sel!(showFilter:)),
            ns_string!("f"),
        )
    };
    edit_menu.addItem(&filter);
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    // Plain cmd-C stays free for a copy of the image itself later.
    let copy_path = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Copy path"),
            Some(sel!(copyPath:)),
            ns_string!("c"),
        )
    };
    copy_path.setKeyEquivalentModifierMask(
        objc2_app_kit::NSEventModifierFlags::Command | objc2_app_kit::NSEventModifierFlags::Option,
    );
    edit_menu.addItem(&copy_path);
    let copy_url = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Copy file URL"),
            Some(sel!(copyFileUrl:)),
            ns_string!(""),
        )
    };
    edit_menu.addItem(&copy_url);
    edit_item.setSubmenu(Some(&edit_menu));
    menubar.addItem(&edit_item);

    let window_item = NSMenuItem::new(mtm);
    let window_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Window"));
    for (title, action, key) in [
        ("Close", sel!(performClose:), "w"),
        ("Minimise", sel!(performMiniaturize:), "m"),
        ("Zoom", sel!(performZoom:), ""),
    ] {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        window_menu.addItem(&item);
    }
    window_item.setSubmenu(Some(&window_menu));
    // Telling AppKit which menu this is keeps its own window and tab
    // entries out of View, where they landed before.
    app.setWindowsMenu(Some(&window_menu));

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
    // cmd-plus is really shift-cmd-equals, and a key equivalent of
    // "+" matches nothing a keyboard can send. The key under the plus
    // is the one to bind, with a hidden twin for the shifted press.
    let bigger = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Bigger thumbnails"),
            Some(sel!(biggerThumbs:)),
            ns_string!("="),
        )
    };
    let bigger_shifted = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Bigger thumbnails"),
            Some(sel!(biggerThumbs:)),
            ns_string!("="),
        )
    };
    bigger_shifted.setKeyEquivalentModifierMask(
        objc2_app_kit::NSEventModifierFlags::Command | objc2_app_kit::NSEventModifierFlags::Shift,
    );
    bigger_shifted.setHidden(true);
    let smaller = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Smaller thumbnails"),
            Some(sel!(smallerThumbs:)),
            ns_string!("-"),
        )
    };
    view_menu.addItem(&bigger);
    view_menu.addItem(&bigger_shifted);
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
    menubar.addItem(&window_item);
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
    window.setTitle(ns_string!("Wêne"));
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

    // Thumbnail size, where the original app keeps it: the right end
    // of the status bar.
    let slider = objc2_app_kit::NSSlider::initWithFrame(
        objc2_app_kit::NSSlider::alloc(mtm),
        NSRect::new(
            CGPoint::new(content_size.width - THUMB_SLIDER_W - 8.0, 2.0),
            CGSize::new(THUMB_SLIDER_W, 20.0),
        ),
    );
    slider.setMinValue(60.0);
    slider.setMaxValue(thumb_cap_pref());
    slider.setDoubleValue(thumb_size_pref());
    slider.setAutoresizingMask(
        objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin
            | objc2_app_kit::NSAutoresizingMaskOptions::ViewMaxYMargin,
    );
    unsafe {
        slider.setTarget(Some(&*delegate));
        slider.setAction(Some(sel!(thumbSliderMoved:)));
    }
    let _ = delegate.ivars().thumb_slider.set(slider.clone());
    // The status text stops where the slider starts.
    status.setFrame(NSRect::new(
        CGPoint::new(8.0, 4.0),
        CGSize::new(content_size.width - THUMB_SLIDER_W - 24.0, 16.0),
    ));

    // Filter bar, above the grid and hidden until cmd-F.
    let filter_bar = objc2_app_kit::NSView::initWithFrame(
        objc2_app_kit::NSView::alloc(mtm),
        NSRect::new(
            CGPoint::new(0.0, content_size.height - FILTER_H),
            CGSize::new(content_size.width, FILTER_H),
        ),
    );
    // Its real place is worked out in layout_content, which knows
    // where the title bar ends.

    filter_bar.setAutoresizingMask(
        objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
            | objc2_app_kit::NSAutoresizingMaskOptions::ViewMinYMargin,
    );
    filter_bar.setHidden(true);
    let filter_field = NSTextField::textFieldWithString(ns_string!(""), mtm);
    filter_field.setFrame(NSRect::new(
        CGPoint::new(8.0, 3.0),
        CGSize::new(content_size.width - 16.0, 22.0),
    ));
    filter_field.setAutoresizingMask(
        objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable,
    );
    filter_field.setPlaceholderString(Some(ns_string!("Filter by name")));
    filter_field.setBezelStyle(objc2_app_kit::NSTextFieldBezelStyle::RoundedBezel);
    unsafe { filter_field.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
    filter_bar.addSubview(&filter_field);

    content.addSubview(&filter_bar);
    content.addSubview(&scroll);
    content.addSubview(&status);
    content.addSubview(&slider);

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
    // The bar and the grid are stacked by hand, so a resize has to
    // run the same maths again.
    content.setPostsFrameChangedNotifications(true);
    unsafe {
        objc2_foundation::NSNotificationCenter::defaultCenter()
            .addObserver_selector_name_object(
                &delegate,
                sel!(contentResized:),
                Some(objc2_app_kit::NSViewFrameDidChangeNotification),
                Some(&content),
            );
    }
    let _ = delegate.ivars().content.set(content);
    let _ = delegate.ivars().scroll.set(scroll);
    let _ = delegate.ivars().filter_bar.set(filter_bar);
    let _ = delegate.ivars().filter_field.set(filter_field);
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
