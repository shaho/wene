//! What the app does to files: move, copy, trash, undo, open with,
//! the clipboard and the desktop picture.

use super::*;

impl AppDelegate {
    /// Run a panel modally. When it closes, AppKit hands the key
    /// window back to the browse window, which buries a slideshow
    /// still on screen, so the slide takes the key back.
    pub(crate) fn run_panel(&self, panel: &NSOpenPanel) -> bool {
        let ok = panel.runModal() == objc2_app_kit::NSModalResponseOK;
        let slide = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .map(|show| show.window.clone());
        if let Some(window) = slide {
            window.makeKeyAndOrderFront(None);
        }
        ok
    }

    /// Ask for the target folder. Cancel returns None.
    pub(crate) fn choose_folder(&self, kind: Transfer) -> Option<PathBuf> {
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
        if !self.run_panel(&panel) {
            return None;
        }
        let path = panel.URL()?.path()?;
        Some(PathBuf::from(path.to_string()))
    }

    /// Send the images to `folder` on a background thread, so a long
    /// batch (a move to another disk is a copy and a delete) leaves
    /// the window usable. Each file reports back as it lands, and the
    /// status bar carries the count and then the result.
    pub(crate) fn transfer_selection(&self, folder: PathBuf, kind: Transfer) {
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

    /// Tell the engine about files the app itself moved, so its list
    /// matches the disk before the watcher gets there. Only files the
    /// grid is showing count: a file that landed in another folder is
    /// none of this folder's business.
    fn notice_files(&self, paths: &[PathBuf]) {
        let Some(engine) = self.ivars().engine.get() else {
            return;
        };
        let scan = self.ivars().current_scan.borrow().clone();
        let Some((root, recursive)) = scan else {
            return;
        };
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
    pub(crate) fn take_back(&self, undoing: bool) {
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
                items
                    .iter()
                    .map(|(from, to)| (to.clone(), from.clone()))
                    .collect::<Vec<_>>(),
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
                let gone: Vec<PathBuf> =
                    trashed.moved.iter().map(|(copy, _)| copy.clone()).collect();
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
            match transfer::move_exact(from, to) {
                Ok(()) => {
                    arrived.push(to.clone());
                    left.push(from.clone());
                }
                Err(error) => {
                    eprintln!(
                        "undo failed: {} -> {}: {error}",
                        from.display(),
                        to.display()
                    );
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
            let entry = Entry {
                label: label.clone(),
                step,
            };
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
    pub(crate) fn fill_open_with(&self, menu: &NSMenu) {
        let mtm = self.mtm();
        menu.removeAllItems();
        let targets = self.action_targets();
        let Some(first) = targets.first() else { return };
        let url = NSURL::fileURLWithPath(&NSString::from_str(&first.to_string_lossy()));
        let workspace = NSWorkspace::sharedWorkspace();
        for app in workspace.URLsForApplicationsToOpenURL(&url).iter() {
            let Some(path) = app.path() else { continue };
            let name = objc2_foundation::NSFileManager::defaultManager().displayNameAtPath(&path);
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

    pub(crate) fn open_targets_with(&self, app: &NSURL) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let urls: Vec<Retained<NSURL>> = targets
            .iter()
            .map(|path| NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy())))
            .collect();
        let configuration = objc2_app_kit::NSWorkspaceOpenConfiguration::configuration();
        NSWorkspace::sharedWorkspace()
            .openURLs_withApplicationAtURL_configuration_completionHandler(
                &NSArray::from_retained_slice(&urls),
                app,
                &configuration,
                None,
            );
    }

    /// cmd-C: the images themselves go on the clipboard, as the files
    /// they are, so a paste in Finder puts copies of them in that
    /// folder.
    pub(crate) fn copy_images_to_clipboard(&self) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let urls: Vec<Retained<ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>>> = targets
            .iter()
            .map(|path| {
                let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
                ProtocolObject::from_retained(url)
            })
            .collect();
        let pasteboard = objc2_app_kit::NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        pasteboard.writeObjects(&NSArray::from_retained_slice(&urls));
        let message = match targets.len() {
            1 => format!("Copied {}", file_name(&targets[0])),
            n => format!("Copied {n} images"),
        };
        self.show_status_message(&message);
        self.flash_overlay(&message);
    }

    /// Put the targets on the clipboard, as plain paths or as file
    /// URLs, one per line.
    pub(crate) fn copy_targets(&self, as_url: bool) {
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
    pub(crate) fn use_as_desktop_picture(&self) {
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
            NSWorkspace::sharedWorkspace().setDesktopImageURL_forScreen_options_error(
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

    /// cmd-R: show what an action would work on in Finder, selected
    /// inside its folder.
    pub(crate) fn reveal_targets(&self) {
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

    /// The images an action works on: the slide on screen during a
    /// slideshow, the grid's selection otherwise.
    pub(crate) fn action_targets(&self) -> Vec<PathBuf> {
        let current = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .and_then(|show| show.playlist.current().cloned());
        match current {
            Some(path) => vec![path],
            None => self
                .ivars()
                .grid
                .get()
                .map(|g| g.selected_paths())
                .unwrap_or_default(),
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
            one.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
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
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
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
