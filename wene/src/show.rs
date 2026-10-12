//! The slideshow: its window, its keys, its timer and its overlay.

use super::*;

impl AppDelegate {
    /// Take gone files out of a running show, wherever the removal
    /// came from, and end the show if that empties it.
    pub(crate) fn drop_from_slideshow(&self, paths: &[PathBuf]) {
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

    /// Show a line over the slide for a moment, even when the overlay
    /// is off: fullscreen has no status bar and no thumbnail to watch
    /// vanish.
    pub(crate) fn flash_overlay(&self, text: &str) {
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

    /// Start with everything in the grid (e2e and default path).
    pub fn start_slideshow(&self, index: usize, windowed: bool) {
        let files = self.ivars().grid.get().unwrap().paths();
        self.start_slideshow_files(files, index, windowed);
    }

    /// Menu path: use the grid's selection semantics.
    pub(crate) fn start_from_grid(&self, windowed: bool) {
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
        // One slideshow at a time. A second one on top would leave the
        // first one's window and timer running, so the new one takes
        // its place.
        self.end_slideshow();
        let screen_frame = NSScreen::mainScreen(mtm)
            .map(|s| s.frame())
            .unwrap_or(NSRect::new(
                CGPoint::new(0.0, 0.0),
                CGSize::new(1440.0, 900.0),
            ));
        let window = if windowed {
            SlideshowWindow::windowed(mtm, screen_frame)
        } else {
            // Full screen means the whole screen: no menu bar over
            // the slide, no Dock under it.
            NSApplication::sharedApplication(mtm).setPresentationOptions(
                objc2_app_kit::NSApplicationPresentationOptions::HideMenuBar
                    | objc2_app_kit::NSApplicationPresentationOptions::HideDock,
            );
            SlideshowWindow::fullscreen(mtm, screen_frame)
        };
        let content_frame = window.contentRectForFrameRect(window.frame());
        let view = SlideView::new(mtm, content_frame);
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let _ = view
            .ivars()
            .delegate
            .set(unsafe { Retained::retain(self as *const Self as *mut Self).unwrap() });

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
            pending_decode: None,
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
        let window = self
            .ivars()
            .show
            .borrow()
            .as_ref()
            .map(|s| s.window.clone());
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
        let interval = self.ivars().show.borrow().as_ref().and_then(|s| s.interval);
        match interval {
            None => self.step_slideshow(1),
            Some(seconds) => {
                let paused = self
                    .ivars()
                    .show
                    .borrow()
                    .as_ref()
                    .is_some_and(|s| s.timer.is_none());
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
    pub(crate) fn update_auto_menu(&self, seconds: Option<f64>) {
        let Some(menu) = self.ivars().auto_menu.get() else {
            return;
        };
        let selected_tag = seconds.map(|s| (s * 10.0) as isize).unwrap_or(0);
        for item in menu.itemArray() {
            item.setState(if item.tag() == selected_tag { 1 } else { 0 });
        }
    }

    /// The slide view changed zoom/rotation/flip/pan: remember it
    /// for the current file and refresh the overlay.
    /// A zoomed slide draws from a decode made to fit the screen, so
    /// past a point it is an upscale of that decode rather than the
    /// file's own pixels. Ask for a decode that carries them.
    fn ensure_zoom_resolution(&self) {
        let Some(engine) = self.ivars().engine.get() else {
            return;
        };
        let request = {
            let show = self.ivars().show.borrow();
            let Some(show) = show.as_ref() else { return };
            let Some(path) = show.playlist.current().cloned() else {
                return;
            };
            // An animated slide plays from its frames; a still decode
            // in their place would stop the animation.
            if decoder::is_animated_ext(&path) {
                return;
            }
            if show
                .pending_decode
                .as_ref()
                .is_some_and(|(waiting, _)| *waiting == path)
            {
                return;
            }
            let Some(zoom) = show.view.state().zoom else {
                return;
            };
            let natural = show.view.natural_size();
            let backing = show.window.backingScaleFactor();
            wene_core::zoom_decode_request(
                natural.width.max(natural.height) as i32,
                show.view.decoded_long_edge() as i32,
                zoom,
                backing,
            )
            .map(|max_px| (path, max_px))
        };
        let Some((path, max_px)) = request else {
            return;
        };
        if let Some(show) = self.ivars().show.borrow_mut().as_mut() {
            show.pending_decode = Some((path.clone(), max_px));
        }
        engine.request_slide_at(path, max_px);
    }

    /// The file's own pixel size, for a slide the view is about to
    /// draw. A header read, not a decode.
    pub(crate) fn natural_size(path: &std::path::Path) -> CGSize {
        decoder::image_dimensions(path)
            .map(|(w, h)| CGSize::new(w as f64, h as f64))
            .unwrap_or(CGSize::new(0.0, 0.0))
    }

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
        self.ensure_zoom_resolution();
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

    pub(crate) fn update_overlay(&self) {
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
            let rows = self.header_rows(&path);
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
    pub(crate) fn show_current(&self) {
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
                    show.view
                        .show_image(image.clone(), Self::natural_size(&current));
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

    /// The menu a right-click on a slide opens: the grid's own menu,
    /// which already acts on the slide on screen during a show.
    pub fn slide_context_menu(&self) -> Option<Retained<NSMenu>> {
        self.ivars().grid.get().map(|grid| grid.context_menu(true))
    }
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
