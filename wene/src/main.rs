//! macOS shell: app delegate, menu, browse window, event pump.

mod decoder;
mod grid;
mod slideshow;

use std::cell::{OnceCell, RefCell};
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
    NSImage, NSMenu, NSMenuItem, NSOpenPanel, NSScreen, NSScrollView, NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGSize};
use objc2_core_graphics::CGImage;
use objc2_foundation::{ns_string, NSNotification, NSObject, NSObjectProtocol, NSRect, NSString};
use wene_core::{Engine, Event};

use decoder::ImageIoDecoder;
use grid::GridView;
use slideshow::{SlideView, SlideshowWindow};

type Img = CFRetained<CGImage>;

static EVENTS: OnceLock<Mutex<Receiver<Event<Img>>>> = OnceLock::new();
static DELEGATE: OnceLock<MainThreadBound<Retained<AppDelegate>>> = OnceLock::new();

struct Show {
    window: Retained<SlideshowWindow>,
    view: Retained<SlideView>,
    files: Vec<PathBuf>,
    index: usize,
    cache: HashMap<PathBuf, Retained<NSImage>>,
}

pub struct DelegateIvars {
    engine: OnceCell<Engine<Img>>,
    window: OnceCell<Retained<NSWindow>>,
    grid: OnceCell<Retained<GridView>>,
    show: RefCell<Option<Show>>,
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
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars {
            engine: OnceCell::new(),
            window: OnceCell::new(),
            grid: OnceCell::new(),
            show: RefCell::new(None),
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

    // ---- slideshow ----

    pub fn start_slideshow(&self, index: usize) {
        let mtm = self.mtm();
        let files = self.ivars().grid.get().unwrap().ivars().files.borrow().clone();
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
        window.setContentView(Some(&view));
        window.makeKeyAndOrderFront(None);
        window.makeFirstResponder(Some(&view));

        *self.ivars().show.borrow_mut() = Some(Show {
            window,
            view,
            files,
            index: index.min(usize::MAX),
            cache: HashMap::new(),
        });
        self.show_current();
    }

    pub fn end_slideshow(&self) {
        if let Some(show) = self.ivars().show.borrow_mut().take() {
            show.window.close();
        }
        if let Some(window) = self.ivars().window.get() {
            window.makeKeyAndOrderFront(None);
        }
    }

    pub fn step_slideshow(&self, delta: i64) {
        {
            let mut show = self.ivars().show.borrow_mut();
            let Some(show) = show.as_mut() else { return };
            let len = show.files.len() as i64;
            let next = show.index as i64 + delta;
            if next < 0 || next >= len {
                return; // no loop mode in the demo
            }
            show.index = next as usize;
        }
        self.show_current();
    }

    pub fn jump_slideshow_start(&self) {
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.index = 0;
        }
        self.show_current();
    }

    pub fn jump_slideshow_end(&self) {
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.index = show.files.len().saturating_sub(1);
        }
        self.show_current();
    }

    /// Display the current slide from cache or request it, prefetch
    /// the neighbours, and drop everything else (tiny precache per
    /// the map: current, next, previous only).
    fn show_current(&self) {
        let engine = self.ivars().engine.get().unwrap();
        let mut show = self.ivars().show.borrow_mut();
        let Some(show) = show.as_mut() else { return };

        let keep: Vec<PathBuf> = [-1i64, 0, 1]
            .iter()
            .filter_map(|d| {
                let i = show.index as i64 + d;
                (i >= 0 && (i as usize) < show.files.len()).then(|| show.files[i as usize].clone())
            })
            .collect();
        show.cache.retain(|path, _| keep.contains(path));

        let current = show.files[show.index].clone();
        if let Some(image) = show.cache.get(&current) {
            show.view.show_image(image.clone());
        }
        for path in keep {
            if !show.cache.contains_key(&path) {
                engine.request_slide(path);
            }
        }
    }

    // ---- core events ----

    fn handle_event(&self, event: Event<Img>) {
        match event {
            Event::FilesInserted(inserts) => {
                self.ivars().grid.get().unwrap().insert_files(inserts);
            }
            Event::ScanDone { total } => {
                println!("scan done: {total} images");
            }
            Event::ThumbReady { path, image } => {
                let image = ns_image(&image);
                self.ivars().grid.get().unwrap().set_thumb(path, image);
            }
            Event::SlideReady { path, image } => {
                let image = ns_image(&image);
                let mut show = self.ivars().show.borrow_mut();
                let Some(show) = show.as_mut() else { return };
                let is_current = show.files.get(show.index) == Some(&path);
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

fn build_menu(mtm: MainThreadMarker, app: &NSApplication) {
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

    app.setMainMenu(Some(&menubar));
}

fn main() {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    build_menu(mtm, &app);

    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

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
