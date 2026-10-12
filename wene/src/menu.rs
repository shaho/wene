//! The menu bar, and which of its items are on.

use super::*;

/// One menu item. AppKit sends `action` up the responder chain, so it
/// must name a method that something in the chain answers: the app
/// delegate's action table, or AppKit's own.
pub(crate) fn menu_item(
    mtm: MainThreadMarker,
    title: &NSString,
    action: objc2::runtime::Sel,
    key: &NSString,
) -> Retained<NSMenuItem> {
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            title,
            Some(action),
            key,
        )
    }
}

impl AppDelegate {
    /// Items that act on images need something to act on: the
    /// slideshow's slide or the grid's selection. The repeat items
    /// also need a folder to repeat to.
    pub(crate) fn menu_item_enabled(&self, item: &NSMenuItem) -> bool {
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
        // One slideshow at a time.
        if action == Some(sel!(startSlideshow:)) || action == Some(sel!(startSlideshowWindowed:)) {
            return self.ivars().show.borrow().is_none();
        }
        let acts_on_images = [
            sel!(moveToTrash:),
            sel!(revealInFinder:),
            sel!(openWith:),
            sel!(copyImages:),
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
}

pub(crate) fn build_menu(mtm: MainThreadMarker, app: &NSApplication, delegate: &AppDelegate) {
    let menubar = NSMenu::new(mtm);

    let app_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);
    let prefs = menu_item(
        mtm,
        ns_string!("Preferences…"),
        sel!(showPrefs:),
        ns_string!(","),
    );
    app_menu.addItem(&prefs);
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    for (title, action, key, alternate) in [
        ("Hide Wêne", sel!(hide:), "h", false),
        ("Hide others", sel!(hideOtherApplications:), "h", true),
        ("Show all", sel!(unhideAllApplications:), "", false),
    ] {
        let item = menu_item(
            mtm,
            &NSString::from_str(title),
            action,
            &NSString::from_str(key),
        );
        if alternate {
            item.setKeyEquivalentModifierMask(
                objc2_app_kit::NSEventModifierFlags::Command
                    | objc2_app_kit::NSEventModifierFlags::Option,
            );
        }
        app_menu.addItem(&item);
    }
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let quit = menu_item(
        mtm,
        ns_string!("Quit Wêne"),
        sel!(terminate:),
        ns_string!("q"),
    );
    app_menu.addItem(&quit);
    app_item.setSubmenu(Some(&app_menu));
    menubar.addItem(&app_item);

    let file_item = NSMenuItem::new(mtm);
    let file_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("File"));
    let open = menu_item(
        mtm,
        ns_string!("Open folder…"),
        sel!(openDocument:),
        ns_string!("o"),
    );
    file_menu.addItem(&open);
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let reveal = menu_item(
        mtm,
        ns_string!("Reveal in Finder"),
        sel!(revealInFinder:),
        ns_string!("r"),
    );
    file_menu.addItem(&reveal);
    let info = menu_item(mtm, ns_string!("Get info"), sel!(getInfo:), ns_string!("i"));
    file_menu.addItem(&info);
    let open_with = NSMenuItem::new(mtm);
    open_with.setTitle(ns_string!("Open with"));
    let open_with_menu = delegate.open_with_menu(mtm);
    open_with_menu.setDelegate(Some(ProtocolObject::from_ref(delegate)));
    open_with.setSubmenu(Some(&open_with_menu));
    file_menu.addItem(&open_with);
    let desktop = menu_item(
        mtm,
        ns_string!("Set as desktop picture"),
        sel!(setDesktopPicture:),
        ns_string!("d"),
    );
    file_menu.addItem(&desktop);
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    // cmd-Delete, Finder's binding, in the grid and the slideshow.
    let trash = menu_item(
        mtm,
        ns_string!("Move to trash"),
        sel!(moveToTrash:),
        &NSString::from_str("\u{8}"),
    );
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
        let item = menu_item(
            mtm,
            &NSString::from_str(title),
            action,
            &NSString::from_str(key),
        );
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
    let select_all = menu_item(
        mtm,
        ns_string!("Select all"),
        sel!(selectAll:),
        ns_string!("a"),
    );
    let undo = menu_item(mtm, ns_string!("Undo"), sel!(undoLast:), ns_string!("z"));
    edit_menu.addItem(&undo);
    let _ = delegate.ivars().undo_item.set(undo);
    let redo = menu_item(mtm, ns_string!("Redo"), sel!(redoLast:), ns_string!("z"));
    redo.setKeyEquivalentModifierMask(
        objc2_app_kit::NSEventModifierFlags::Command | objc2_app_kit::NSEventModifierFlags::Shift,
    );
    edit_menu.addItem(&redo);
    let _ = delegate.ivars().redo_item.set(redo);
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    edit_menu.addItem(&select_all);
    let filter = menu_item(
        mtm,
        ns_string!("Filter by name"),
        sel!(showFilter:),
        ns_string!("f"),
    );
    edit_menu.addItem(&filter);
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    // Plain cmd-C stays free for a copy of the image itself later.
    let copy_images = menu_item(mtm, ns_string!("Copy"), sel!(copyImages:), ns_string!("c"));
    edit_menu.addItem(&copy_images);
    let copy_path = menu_item(
        mtm,
        ns_string!("Copy path"),
        sel!(copyPath:),
        ns_string!("c"),
    );
    copy_path.setKeyEquivalentModifierMask(
        objc2_app_kit::NSEventModifierFlags::Command | objc2_app_kit::NSEventModifierFlags::Option,
    );
    edit_menu.addItem(&copy_path);
    let copy_url = menu_item(
        mtm,
        ns_string!("Copy file URL"),
        sel!(copyFileUrl:),
        ns_string!(""),
    );
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
        let item = menu_item(
            mtm,
            &NSString::from_str(title),
            action,
            &NSString::from_str(key),
        );
        window_menu.addItem(&item);
    }
    window_item.setSubmenu(Some(&window_menu));
    // Telling AppKit which menu this is keeps its own window and tab
    // entries out of View, where they landed before.
    app.setWindowsMenu(Some(&window_menu));

    let show_item = NSMenuItem::new(mtm);
    let show_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Slideshow"));
    let start = menu_item(
        mtm,
        ns_string!("Start slideshow"),
        sel!(startSlideshow:),
        ns_string!("y"),
    );
    let start_windowed = menu_item(
        mtm,
        ns_string!("Start slideshow in window"),
        sel!(startSlideshowWindowed:),
        ns_string!("y"),
    );
    start_windowed.setKeyEquivalentModifierMask(
        objc2_app_kit::NSEventModifierFlags::Command | objc2_app_kit::NSEventModifierFlags::Option,
    );
    show_menu.addItem(&start);
    show_menu.addItem(&start_windowed);
    show_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let loop_item = menu_item(mtm, ns_string!("Loop"), sel!(toggleLoop:), ns_string!("l"));
    let shuffle_item = menu_item(
        mtm,
        ns_string!("Shuffle"),
        sel!(toggleShuffle:),
        ns_string!("r"),
    );
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
        let item = menu_item(
            mtm,
            &NSString::from_str(title),
            sel!(setAutoAdvanceMenu:),
            ns_string!(""),
        );
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
        let item = menu_item(
            mtm,
            &NSString::from_str(title),
            sel!(setSortMenu:),
            &NSString::from_str(key),
        );
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
    let bigger = menu_item(
        mtm,
        ns_string!("Bigger thumbnails"),
        sel!(biggerThumbs:),
        ns_string!("="),
    );
    let bigger_shifted = menu_item(
        mtm,
        ns_string!("Bigger thumbnails"),
        sel!(biggerThumbs:),
        ns_string!("="),
    );
    bigger_shifted.setKeyEquivalentModifierMask(
        objc2_app_kit::NSEventModifierFlags::Command | objc2_app_kit::NSEventModifierFlags::Shift,
    );
    bigger_shifted.setHidden(true);
    let smaller = menu_item(
        mtm,
        ns_string!("Smaller thumbnails"),
        sel!(smallerThumbs:),
        ns_string!("-"),
    );
    view_menu.addItem(&bigger);
    view_menu.addItem(&bigger_shifted);
    view_menu.addItem(&smaller);
    view_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let scale_up = menu_item(
        mtm,
        ns_string!("Fill the screen with small images"),
        sel!(toggleScaleUp:),
        ns_string!("e"),
    );
    view_menu.addItem(&scale_up);
    let _ = delegate.ivars().scale_up_item.set(scale_up);
    view_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let sidebar_item = menu_item(
        mtm,
        ns_string!("Show sidebar"),
        sel!(toggleSidebarMenu:),
        ns_string!("s"),
    );
    sidebar_item.setKeyEquivalentModifierMask(
        objc2_app_kit::NSEventModifierFlags::Command | objc2_app_kit::NSEventModifierFlags::Control,
    );
    sidebar_item.setState(1);
    view_menu.addItem(&sidebar_item);
    let _ = delegate.ivars().sidebar_item.set(sidebar_item);
    let labels_item = menu_item(
        mtm,
        ns_string!("Show filenames"),
        sel!(toggleLabels:),
        ns_string!(""),
    );
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
