//! The browse window: the folder on screen, the filter, the sort,
//! the thumbnail size and the status bar.

use super::*;

impl AppDelegate {
    pub(crate) fn open_folder(&self) {
        let mtm = self.mtm();
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setAllowsMultipleSelection(false);
        panel.setPrompt(Some(ns_string!("Browse")));
        let ok = self.run_panel(&panel);
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
    pub(crate) fn scan_root_inner(&self, root: PathBuf, recursive: bool, remember: bool) {
        // A filter typed for the last folder would explain nothing
        // here, and an almost empty grid reads as a bug.
        self.close_filter();
        if let Some(grid) = self.ivars().grid.get() {
            grid.reset();
        }
        if let Some(window) = self.ivars().window.get() {
            window.setTitle(&NSString::from_str(&format!(
                "Wêne — {}",
                root.file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default()
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
    pub(crate) fn toggle_sidebar(&self) {
        let Some(split) = self.ivars().split.get() else {
            return;
        };
        unsafe { split.toggleSidebar(None) };
        self.sync_sidebar_item();
    }

    /// Tick the View menu item to match the sidebar.
    fn sync_sidebar_item(&self) {
        let (Some(split), Some(item)) = (self.ivars().split.get(), self.ivars().sidebar_item.get())
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
    pub(crate) fn apply_sort(&self) {
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

    pub(crate) fn sort_is_default(&self) -> bool {
        self.ivars().sort_order.get() == SortOrder::Name && !self.ivars().sort_desc.get()
    }

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
        let Some(first) = paths.first().cloned() else {
            return;
        };
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
        let Some(folder) = files
            .first()
            .and_then(|path| path.parent())
            .map(PathBuf::from)
        else {
            return;
        };
        self.ivars().opened_from_finder.set(true);
        // Opening an image means looking at it, not at its folder.
        self.ivars().pending_fullscreen.set(true);
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
    pub(crate) fn select_pending(&self) -> bool {
        let pending = self.ivars().pending_open.borrow().clone();
        if pending.is_empty() {
            return true;
        }
        let Some(grid) = self.ivars().grid.get() else {
            return false;
        };
        // Only touch the selection once the grid really holds them.
        // A scan of some other folder must leave it alone.
        if !grid.holds_all(&pending) {
            return false;
        }
        grid.select_paths(&pending);
        self.ivars().pending_open.borrow_mut().clear();
        self.focus_grid();
        if self.ivars().pending_fullscreen.replace(false) {
            self.start_from_grid(false);
        }
        true
    }

    /// The folder the images waiting to be opened live in.
    pub(crate) fn pending_folder(&self) -> Option<PathBuf> {
        self.ivars()
            .pending_open
            .borrow()
            .first()
            .and_then(|path| path.parent())
            .map(PathBuf::from)
    }

    /// The field's text is the filter. Typing lands here.
    pub(crate) fn filter_changed(&self) {
        let Some(field) = self.ivars().filter_field.get() else {
            return;
        };
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
    pub(crate) fn filter_field_command(&self, command: objc2::runtime::Sel) -> bool {
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
    pub(crate) fn open_filter(&self) {
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
        let Some(bar) = self.ivars().filter_bar.get() else {
            return;
        };
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

    pub(crate) fn focus_grid(&self) {
        let (Some(window), Some(grid)) = (self.ivars().window.get(), self.ivars().grid.get())
        else {
            return;
        };
        window.makeFirstResponder(Some(grid));
    }

    /// Stack the right-hand pane: filter bar on top when it is open,
    /// then the grid, then the status bar.
    pub(crate) fn layout_content(&self) {
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
        let taken = if bar.isHidden() {
            0.0
        } else {
            title_h + FILTER_H
        };
        bar.setFrame(NSRect::new(
            CGPoint::new(0.0, size.height - title_h - FILTER_H),
            CGSize::new(size.width, FILTER_H),
        ));
        scroll.setFrame(NSRect::new(
            CGPoint::new(0.0, STATUS_H),
            CGSize::new(size.width, size.height - STATUS_H - taken),
        ));
    }

    /// The grid's cells changed size: keep the slider and the saved
    /// size with it.
    pub fn thumb_size_changed(&self, size: f64) {
        if let Some(slider) = self.ivars().thumb_slider.get() {
            slider.setDoubleValue(size);
        }
        if !e2e::enabled() {
            NSUserDefaults::standardUserDefaults().setDouble_forKey(size, ns_string!("thumbSize"));
        }
    }

    /// The slider moved.
    pub(crate) fn thumb_slider_moved(&self) {
        let (Some(slider), Some(grid)) = (self.ivars().thumb_slider.get(), self.ivars().grid.get())
        else {
            return;
        };
        grid.set_cell_size(slider.doubleValue());
    }

    /// The preference moved: the ceiling for the slider, the cells,
    /// and the size thumbnails are decoded at.
    pub(crate) fn apply_thumb_cap(&self, cap: f64) {
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
            NSUserDefaults::standardUserDefaults().setDouble_forKey(cap, ns_string!("thumbCap"));
        }
    }

    /// Put a line in the status bar. The next selection or scan
    /// replaces it.
    pub(crate) fn show_status_message(&self, text: &str) {
        if let Some(status) = self.ivars().status.get() {
            status.setStringValue(&NSString::from_str(text));
        }
    }

    /// The grid calls this after any selection or file-list change.
    pub fn selection_changed(&self) {
        self.update_status();
        self.refresh_info();
    }

    /// Count plus name, dimensions, and size of the selection, like
    /// the original app's status bar.
    pub(crate) fn update_status(&self) {
        let Some(status) = self.ivars().status.get() else {
            return;
        };
        let Some(grid) = self.ivars().grid.get() else {
            return;
        };
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
                let mut text = format!(
                    "{} of {shown} selected — {}",
                    many.len(),
                    format_bytes(bytes)
                );
                if grid.filtering() {
                    text.push_str(&format!(" · {all_text}"));
                }
                text
            }
        };
        status.setStringValue(&NSString::from_str(&text));
    }
}
