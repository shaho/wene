//! macOS shell: the app delegate's state, the Objective-C methods
//! AppKit calls on it, the event pump, and the window it builds at
//! launch. What the delegate does lives in the modules: browse,
//! show, file_ops, info, prefs and menu.

mod browse;
mod decoder;
mod e2e;
mod file_ops;
mod grid;
mod info;
mod menu;
mod prefs;
mod show;
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
    NSColor, NSControlTextEditingDelegate, NSFont, NSImage, NSMenu, NSMenuDelegate, NSMenuItem,
    NSOpenPanel, NSScreen, NSScrollView, NSSplitViewController, NSSplitViewItem, NSTextField,
    NSTextFieldDelegate, NSTextView, NSViewController, NSWindow, NSWindowDelegate,
    NSWindowStyleMask, NSWorkspace,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGSize};
use objc2_core_graphics::CGImage;
use objc2_foundation::{
    ns_string, NSArray, NSNotification, NSObject, NSObjectProtocol, NSRect, NSString, NSTimer,
    NSUserDefaults, NSURL,
};
use wene_core::history::{Entry, History, Step};
use wene_core::transfer::{self, Transfer};
use wene_core::{Engine, Event, LruCache, Playlist, SortOrder};

use info::{exif_date_text, file_name, rows_text};
use menu::build_menu;
use prefs::{
    last_folder_pref, save_last_folder, startup_folder_pref, thumb_cap_pref, thumb_size_pref,
    THUMB_SLIDER_W,
};

use decoder::ImageIoDecoder;
use grid::GridView;
use sidebar::Sidebar;
use slideshow::{SlideState, SlideView, SlideshowWindow};

type Img = CFRetained<CGImage>;
/// Label and value pairs from a file header, as the info panel and
/// the overlay show them.
type HeaderRows = Vec<(String, String)>;

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
    /// A bigger decode a zoom asked for, while it is on its way, so
    /// a pinch does not queue one decode per frame.
    pending_decode: Option<(PathBuf, i32)>,
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
    /// The waiting images came from a Finder open of files, so they
    /// go straight to full screen once the grid holds them.
    pending_fullscreen: Cell<bool>,
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
    /// Preference: a small image fills the screen instead of sitting
    /// at its own size in the middle of it.
    scale_up_small: Cell<bool>,
    /// Preference: a drag out of the app copies rather than moves,
    /// so dropping an image on the desktop leaves the folder alone.
    drag_out_copies: Cell<bool>,
    scale_up_item: OnceCell<Retained<NSMenuItem>>,
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
    /// The last file's header rows. A pan or a pinch redraws the
    /// overlay and the info panel on every event, and the file has
    /// not changed, so neither reads it again.
    header_rows: RefCell<Option<(PathBuf, HeaderRows)>>,
    /// How many times a header was read from disk, for the harness.
    header_reads: Cell<usize>,
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
            // The menu bar and the Dock come back with the window.
            NSApplication::sharedApplication(self.mtm()).setPresentationOptions(
                objc2_app_kit::NSApplicationPresentationOptions::Default,
            );
            let mut last_slide = None;
            if let Some(show) = self.ivars().show.borrow_mut().take() {
                last_slide = show.playlist.current().cloned();
                if let Some(timer) = show.timer {
                    timer.invalidate();
                }
                // The frame timer retains the view; stop it so the
                // view can die with the window.
                show.view.stop_animation();
            }
            if let Some(window) = self.ivars().window.get() {
                window.makeKeyAndOrderFront(None);
                // A full-screen slide launched straight from Finder
                // leaves Finder the active app underneath, so it takes
                // the focus back when the slide closes. Ask for it.
                #[allow(deprecated)]
                NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
            }
            // The grid picks up where the slideshow stopped, so the
            // slide that was on screen is the active thumbnail.
            if let (Some(path), Some(grid)) = (last_slide, self.ivars().grid.get()) {
                if grid.holds_all(std::slice::from_ref(&path)) {
                    grid.select_paths(std::slice::from_ref(&path));
                    self.focus_grid();
                }
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

        #[unsafe(method(toggleScaleUp:))]
        fn toggle_scale_up(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.toggle_scale_up_small();
        }

        #[unsafe(method(prefsDragCopies:))]
        fn prefs_drag_copies(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.toggle_drag_out_copies();
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
            if !self.run_panel(&panel) {
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

        #[unsafe(method(copyImages:))]
        fn copy_images(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.copy_images_to_clipboard();
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
            pending_fullscreen: Cell::new(false),
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
            scale_up_small: Cell::new(false),
            drag_out_copies: Cell::new(false),
            scale_up_item: OnceCell::new(),
            default_windowed: Cell::new(false),
            show: RefCell::new(None),
            loop_enabled: Cell::new(false),
            shuffle_enabled: Cell::new(false),
            overlay_visible: Cell::new(false),
            overlay_exif: Cell::new(false),
            overlay_path: Cell::new(false),
            header_rows: RefCell::new(None),
            header_reads: Cell::new(0),
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

    fn handle_event(&self, event: Event<Img>) {
        // The grid was reset for the new folder already.
        if self.ivars().engine.get().unwrap().is_stale(&event) {
            return;
        }
        match event {
            Event::FilesInserted { files, .. } => {
                self.ivars().grid.get().unwrap().insert_files(files);
                // Core streams in name order; re-sort the batch into
                // the active order.
                if !self.sort_is_default() {
                    self.apply_sort();
                }
            }
            Event::FilesRemoved { paths, .. } => {
                self.ivars().grid.get().unwrap().remove_paths(&paths);
                self.drop_from_slideshow(&paths);
            }
            Event::FileChanged { info, .. } => {
                let stale = matches!(
                    self.ivars().header_rows.borrow().as_ref(),
                    Some((cached, _)) if *cached == info.path
                );
                if stale {
                    *self.ivars().header_rows.borrow_mut() = None;
                }
                let grid = self.ivars().grid.get().unwrap();
                grid.file_changed(&info);
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
            Event::ScanDone { total, .. } => {
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
                    self.ivars().pending_fullscreen.set(false);
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
                self.ivars()
                    .grid
                    .get()
                    .unwrap()
                    .set_thumb(path, image, cost);
            }
            Event::SlideReady { path, image } => {
                let is_current = {
                    let cost = image_cost(&image);
                    let image = ns_image(&image);
                    let mut show = self.ivars().show.borrow_mut();
                    let Some(show) = show.as_mut() else { return };
                    let is_current = show.playlist.current() == Some(&path);
                    let state = show.view_states.get(&path).copied();
                    let natural = Self::natural_size(&path);
                    if show
                        .pending_decode
                        .as_ref()
                        .is_some_and(|(waiting, _)| *waiting == path)
                    {
                        show.pending_decode = None;
                    }
                    show.cache.insert(path, image.clone(), cost);
                    if is_current {
                        // A bigger decode of the slide already up is
                        // a swap: the zoom that asked for it stays.
                        if show.view.has_image() && show.view.natural_size() == natural {
                            show.view.swap_decode(image);
                        } else {
                            show.view.show_image(image, natural);
                            if let Some(state) = state {
                                show.view.apply_state(state);
                            }
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
                        show.view.show_frames(frames, Self::natural_size(&path));
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

/// Run `f` on the main thread with the app delegate, from a worker.
fn on_main(f: impl FnOnce(&AppDelegate) + Send + 'static) {
    DispatchQueue::main().exec_async(move || {
        let mtm = MainThreadMarker::new().expect("main queue is the main thread");
        f(DELEGATE.get().expect("delegate is set at launch").get(mtm));
    });
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
    // Tahoe drew the name twice: once beside the traffic lights and
    // once in the toolbar's own capsule. The window keeps its title
    // for the Window menu and Mission Control, and draws neither.
    window.setTitleVisibility(objc2_app_kit::NSWindowTitleVisibility::Hidden);

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
    filter_field.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
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
    // AppKit remembers the divider under a name of its own, which
    // would hand an old window its old width and hide this default.
    // A name of ours, never used before, starts everyone at the new
    // width and still keeps whatever they drag it to. The harness
    // gets no name at all: a restored divider is one more thing
    // arriving late in a run that measures the window.
    if !e2e::enabled() {
        split
            .splitView()
            .setAutosaveName(Some(ns_string!("sidebarWidth2")));
    }
    split.addSplitViewItem(&sidebar_pane);
    split.addSplitViewItem(&NSSplitViewItem::splitViewItemWithViewController(
        &content_vc,
    ));
    window.setContentViewController(Some(&split));
    window.setFrame_display(frame, false);
    window.makeFirstResponder(Some(&grid));

    let _ = delegate.ivars().status.set(status);
    // The bar and the grid are stacked by hand, so a resize has to
    // run the same maths again.
    content.setPostsFrameChangedNotifications(true);
    unsafe {
        objc2_foundation::NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
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
        // Slides decode to twice the screen's longest edge, which
        // covers fit and a little zoom. The harness caps it far
        // lower, so its small fixture images outgrow their decode
        // the way a photo does on a real screen.
        if e2e::enabled() {
            256
        } else {
            (screen_long_edge * 2.0) as i32
        },
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
