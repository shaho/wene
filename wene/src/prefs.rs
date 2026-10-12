//! Preferences: the settings window, and what is kept between
//! launches.

use super::*;

impl AppDelegate {
    /// Apply saved settings at launch. Skipped in e2e runs so the
    /// harness starts from a known state.
    pub(crate) fn load_prefs(&self) {
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
        self.ivars()
            .drag_out_copies
            .set(d.boolForKey(ns_string!("dragOutCopies")));
        if d.boolForKey(ns_string!("scaleUpSmall")) {
            self.ivars().scale_up_small.set(true);
            if let Some(item) = self.ivars().scale_up_item.get() {
                item.setState(1);
            }
        }
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
    pub(crate) fn save_prefs(&self) {
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
    pub(crate) fn sync_prefs_controls(&self) {
        let Some(window) = self.ivars().prefs_window.get() else {
            return;
        };
        let Some(content) = window.contentView() else {
            return;
        };
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
        set_check(9, self.ivars().drag_out_copies.get());
        set_check(10, self.ivars().scale_up_small.get());
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

    pub fn scales_small_images_up(&self) -> bool {
        self.ivars().scale_up_small.get()
    }

    /// The preference and the View menu item move together, and a
    /// slideshow already on screen redraws at the new rule.
    pub(crate) fn toggle_scale_up_small(&self) {
        let on = !self.ivars().scale_up_small.get();
        self.ivars().scale_up_small.set(on);
        if let Some(item) = self.ivars().scale_up_item.get() {
            item.setState(if on { 1 } else { 0 });
        }
        if !e2e::enabled() {
            NSUserDefaults::standardUserDefaults().setBool_forKey(on, ns_string!("scaleUpSmall"));
        }
        if let Some(show) = self.ivars().show.borrow().as_ref() {
            show.view.setNeedsDisplay(true);
        }
        self.sync_prefs_controls();
    }

    pub fn drag_out_copies(&self) -> bool {
        self.ivars().drag_out_copies.get()
    }

    pub(crate) fn toggle_drag_out_copies(&self) {
        let on = !self.ivars().drag_out_copies.get();
        self.ivars().drag_out_copies.set(on);
        if !e2e::enabled() {
            NSUserDefaults::standardUserDefaults().setBool_forKey(on, ns_string!("dragOutCopies"));
        }
        self.sync_prefs_controls();
    }
}

/// The folder the grid showed last. A failed restore leaves it
/// alone, so an unplugged disk comes back next time.
pub(crate) fn last_folder_pref() -> Option<PathBuf> {
    if e2e::enabled() {
        return None;
    }
    NSUserDefaults::standardUserDefaults()
        .stringForKey(ns_string!("lastFolder"))
        .map(|s| PathBuf::from(s.to_string()))
        .filter(|p| !p.as_os_str().is_empty())
}

pub(crate) fn save_last_folder(root: &std::path::Path) {
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
pub(crate) const THUMB_SLIDER_W: f64 = 110.0;

pub(crate) fn thumb_cap_pref() -> f64 {
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

pub(crate) fn thumb_size_pref() -> f64 {
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

pub(crate) fn startup_folder_pref() -> Option<String> {
    if e2e::enabled() {
        return None;
    }
    NSUserDefaults::standardUserDefaults()
        .stringForKey(ns_string!("startupFolder"))
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// The preferences window: one plain pane, controls looked up by tag
/// (1-5 and 9 checkboxes, 6 auto-advance popup, 7 startup folder
/// field, 8 largest thumbnail popup). Rows are laid out from the top.
fn build_prefs_window(mtm: MainThreadMarker, delegate: &AppDelegate) -> Retained<NSWindow> {
    let frame = NSRect::new(CGPoint::new(360.0, 360.0), CGSize::new(430.0, 352.0));
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
    label("Startup folder:", 20.0, 312.0);
    let field = NSTextField::labelWithString(ns_string!(""), mtm);
    field.setFrame(NSRect::new(
        CGPoint::new(20.0, 292.0),
        CGSize::new(250.0, 18.0),
    ));
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
        b.setFrame(NSRect::new(CGPoint::new(x, 286.0), CGSize::new(72.0, 28.0)));
        content.addSubview(&b);
    }

    checkbox(
        "Start slideshows in a window",
        sel!(prefsToggleWindowed:),
        1,
        252.0,
    );
    checkbox("Loop slideshows", sel!(toggleLoop:), 2, 224.0);
    checkbox("Shuffle slideshows", sel!(toggleShuffle:), 3, 196.0);
    checkbox(
        "Fill the screen with small images",
        sel!(toggleScaleUp:),
        10,
        168.0,
    );

    label("Auto-advance:", 20.0, 136.0);
    let popup = objc2_app_kit::NSPopUpButton::new(mtm);
    popup.setFrame(NSRect::new(
        CGPoint::new(150.0, 128.0),
        CGSize::new(180.0, 26.0),
    ));
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

    label("Largest thumbnail:", 20.0, 96.0);
    let thumbs = objc2_app_kit::NSPopUpButton::new(mtm);
    thumbs.setFrame(NSRect::new(
        CGPoint::new(150.0, 88.0),
        CGSize::new(180.0, 26.0),
    ));
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

    checkbox("Show filenames", sel!(toggleLabels:), 4, 56.0);
    checkbox(
        "Dragging out of Wêne copies, never moves",
        sel!(prefsDragCopies:),
        9,
        28.0,
    );

    window.setContentView(Some(&content));
    window
}
