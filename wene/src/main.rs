//! macOS shell: app delegate, menu, browse window, event pump.

mod decoder;
mod e2e;
mod grid;
mod slideshow;

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
    NSColor, NSImage, NSMenu, NSMenuItem, NSOpenPanel, NSScreen, NSScrollView, NSTextField,
    NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGSize};
use objc2_core_graphics::CGImage;
use objc2_foundation::{
    ns_string, NSNotification, NSObject, NSObjectProtocol, NSRect, NSString, NSTimer,
};
use wene_core::{Engine, Event, Playlist, SortOrder};

use decoder::ImageIoDecoder;
use grid::GridView;
use slideshow::{SlideView, SlideshowWindow};

type Img = CFRetained<CGImage>;

static EVENTS: OnceLock<Mutex<Receiver<Event<Img>>>> = OnceLock::new();
static DELEGATE: OnceLock<MainThreadBound<Retained<AppDelegate>>> = OnceLock::new();

struct Show {
    window: Retained<SlideshowWindow>,
    view: Retained<SlideView>,
    overlay: Retained<NSTextField>,
    playlist: Playlist,
    cache: HashMap<PathBuf, Retained<NSImage>>,
    /// Auto-advance seconds; None = off. `timer` is live only while
    /// advancing (paused = interval set, timer gone).
    interval: Option<f64>,
    timer: Option<Retained<NSTimer>>,
}

pub struct DelegateIvars {
    engine: OnceCell<Engine<Img>>,
    window: OnceCell<Retained<NSWindow>>,
    grid: OnceCell<Retained<GridView>>,
    show: RefCell<Option<Show>>,
    loop_enabled: Cell<bool>,
    shuffle_enabled: Cell<bool>,
    overlay_visible: Cell<bool>,
    loop_item: OnceCell<Retained<NSMenuItem>>,
    shuffle_item: OnceCell<Retained<NSMenuItem>>,
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

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            // A folder argument skips the open panel (useful for
            // scripted runs); otherwise ask.
            let arg = std::env::args().nth(1).map(PathBuf::from);
            match arg.filter(|p| p.is_dir()) {
                Some(root) => self.scan_root(root),
                None => self.open_folder(),
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
                _ => SortOrder::Name,
            };
            // Re-picking the current order reverses it, like the
            // original app. A new order starts ascending, except
            // dates, which start newest first.
            if self.ivars().sort_order.get() == order {
                self.ivars().sort_desc.set(!self.ivars().sort_desc.get());
            } else {
                self.ivars().sort_order.set(order);
                self.ivars().sort_desc.set(order == SortOrder::Modified);
            }
            self.apply_sort();
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
            show: RefCell::new(None),
            loop_enabled: Cell::new(false),
            shuffle_enabled: Cell::new(false),
            overlay_visible: Cell::new(false),
            loop_item: OnceCell::new(),
            shuffle_item: OnceCell::new(),
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
        self.scan_root(PathBuf::from(path.to_string()));
    }

    fn scan_root(&self, root: PathBuf) {
        if let Some(grid) = self.ivars().grid.get() {
            grid.reset();
        }
        if let Some(window) = self.ivars().window.get() {
            window.setTitle(&NSString::from_str(&format!(
                "wene — {}",
                root.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
            )));
        }
        self.ivars().engine.get().unwrap().scan(root);
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
            };
            for item in menu.itemArray() {
                item.setState(if item.tag() == selected_tag { 1 } else { 0 });
            }
        }
    }

    fn sort_is_default(&self) -> bool {
        self.ivars().sort_order.get() == SortOrder::Name && !self.ivars().sort_desc.get()
    }

    // ---- slideshow ----

    pub fn start_slideshow(&self, index: usize) {
        let mtm = self.mtm();
        let files = self.ivars().grid.get().unwrap().paths();
        if files.is_empty() {
            return;
        }
        let screen_frame = NSScreen::mainScreen(mtm)
            .map(|s| s.frame())
            .unwrap_or(NSRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1440.0, 900.0)));
        let window = SlideshowWindow::fullscreen(mtm, screen_frame);
        let view = SlideView::new(mtm, screen_frame);
        let _ = view.ivars().delegate.set(unsafe {
            Retained::retain(self as *const Self as *mut Self).unwrap()
        });

        let overlay = NSTextField::labelWithString(ns_string!(""), mtm);
        overlay.setTextColor(Some(&NSColor::whiteColor()));
        overlay.setDrawsBackground(true);
        overlay.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.0, 0.55)));
        // Bottom left: the top edge sits under the menu bar.
        overlay.setFrame(NSRect::new(
            CGPoint::new(20.0, 20.0),
            CGSize::new(screen_frame.size.width - 40.0, 24.0),
        ));
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
            cache: HashMap::new(),
            interval: None,
            timer: None,
        });
        // Apply the remembered pace (menu pick or last key).
        if let Some(seconds) = self.ivars().default_interval.get() {
            self.set_auto_advance(Some(seconds));
        }
        self.show_current();
    }

    pub fn end_slideshow(&self) {
        if let Some(show) = self.ivars().show.borrow_mut().take() {
            if let Some(timer) = show.timer {
                timer.invalidate();
            }
            show.window.close();
        }
        if let Some(window) = self.ivars().window.get() {
            window.makeKeyAndOrderFront(None);
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
        let (overlay, text) = {
            let show = self.ivars().show.borrow();
            let Some(show) = show.as_ref() else { return };
            let name = show
                .playlist
                .current()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
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
            (show.overlay.clone(), text)
        };
        overlay.setStringValue(&NSString::from_str(&text));
    }

    /// Display the current slide from cache or request it, prefetch
    /// the neighbours, and drop everything else (tiny precache per
    /// the map: current, next, previous only).
    fn show_current(&self) {
        let engine = self.ivars().engine.get().unwrap();
        {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };

            let mut keep: Vec<PathBuf> = Vec::with_capacity(3);
            for delta in [-1i64, 0, 1] {
                if let Some(path) = show.playlist.peek(delta) {
                    if !keep.contains(path) {
                        keep.push(path.clone());
                    }
                }
            }
            show.cache.retain(|path, _| keep.contains(path));

            if let Some(current) = show.playlist.current() {
                if let Some(image) = show.cache.get(current) {
                    show.view.show_image(image.clone());
                }
            }
            for path in keep {
                if !show.cache.contains_key(&path) {
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
            Event::ScanDone { total } => {
                println!("scan done: {total} images");
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
                let image = ns_image(&image);
                self.ivars().grid.get().unwrap().set_thumb(path, image);
            }
            Event::SlideReady { path, image } => {
                let image = ns_image(&image);
                let mut show = self.ivars().show.borrow_mut();
                let Some(show) = show.as_mut() else { return };
                let is_current = show.playlist.current() == Some(&path);
                show.cache.insert(path, image.clone());
                if is_current {
                    show.view.show_image(image);
                }
            }
            Event::DecodeFailed { path } => {
                eprintln!("decode failed: {}", path.display());
            }
        }
    }
}

fn ns_image(image: &Img) -> Retained<NSImage> {
    NSImage::initWithCGImage_size(NSImage::alloc(), image, CGSize::new(0.0, 0.0))
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
    file_item.setSubmenu(Some(&file_menu));
    menubar.addItem(&file_item);

    let show_item = NSMenuItem::new(mtm);
    let show_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Slideshow"));
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

    // Browse window: scroll view + grid.
    let frame = NSRect::new(CGPoint::new(200.0, 200.0), CGSize::new(1000.0, 700.0));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(ns_string!("wene"));

    let scroll = NSScrollView::new(mtm);
    scroll.setHasVerticalScroller(true);
    let grid = GridView::new(mtm, frame);
    let _ = grid.ivars().delegate.set(delegate.clone());
    scroll.setDocumentView(Some(&grid));
    window.setContentView(Some(&scroll));
    window.makeFirstResponder(Some(&grid));

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
