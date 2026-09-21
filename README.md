![wêne](https://shieldcn.dev/header/gradient.svg?title=w%C3%AAne&subtitle=fast+image+viewer+and+slideshow+app&size=wide&mode=light)

# wêne

wêne is a fast image viewer and slideshow app for macOS, written in Rust. It is
a port of [Phoenix Slides](https://blyt.net/phxslides/)

## What it does right now

You can browse a folder, look through it, throw out what is bad, and file what
is good. It reads every format macOS itself reads, including HEIC and animated
GIFs.

### Browsing

- A Finder-style sidebar holds your favourites (Pictures, Desktop, home, plus
  whatever you add by right-clicking) and every mounted disk. Click a folder to
  see it; press ⌘O to scan a folder and everything under it.
- The grid shows thumbnails in the order the macOS file manager uses, so "img2"
  comes before "img10". Sort by name, date modified, size, path, EXIF date, or
  date added.
- The grid keeps itself current: files that appear, change, or vanish on disk
  show up, refresh, or drop out on their own.
- ⌘F narrows the grid to filenames holding what you type. Esc clears it.
- The status bar counts the images and describes the selection.

### Looking

- Return or a double-click starts a slideshow, fullscreen or in a window.
- Arrow keys and the space bar change slides; home and end jump to the ends.
  Plus and minus zoom, the equals key shows actual pixels, the asterisk fits the
  image to the screen, and dragging moves it around. Esc or "q" ends the show.
- Loop, shuffle, and auto-advance are in the Slideshow menu. Animated GIFs and
  WebPs play with their real frame timings.
- Vertical images stand upright, from their EXIF orientation.

### Working

- ⌘⌫ moves the selected images, or the slide on screen, to the system trash. No
  confirmation: the trash is the safety net. The selection steps forward, so
  holding the keys works through a run.
- ⇧⌘M and ⇧⌘C move or copy images to a folder you pick. ⌃⌘M and ⌃⌘C repeat to
  the last folder, which the menu names. A name the target folder already holds
  keeps both files, Finder style.
- Nothing is ever deleted for real, and nothing is ever replaced.

### Opening from Finder

Once the app is on your disk, wêne shows up under "Open with" for JPEG, PNG,
HEIC, GIF, WebP, and TIFF, and Get Info ▸ Open with ▸ Change All makes it the
default. Opening images shows their folder with them selected; opening a folder
shows that folder. No slideshow starts on its own.

## Run it

```bash
cargo run -p wene
```

Or pass a folder, or a file:

```bash
cargo run -p wene -- ~/Pictures
```

To build the .app bundle in `target/wene.app`:

```bash
./scripts/make-app.sh
```

To build a disk image in `target/wene.dmg`:

```bash
./scripts/make-dmg.sh
```

Opening files from Finder needs the bundle, not `cargo run`: a bare binary has
no Info.plist, so macOS never offers it. The app is not signed or notarized yet.

Needs macOS 11.5 or later and a Rust toolchain.

## How it is built

Two crates. `wene-core` is portable Rust with no Apple dependencies: it scans
folders, keeps the sorted file list, moves and copies files, and runs the decode
worker threads. `wene` is the macOS shell: AppKit windows and views through the
[objc2](https://github.com/madsmtm/objc2) bindings, with image decoding done by
the system's Image I/O framework. The two halves talk through a channel of typed
events.

## How it is tested

`cargo test -p wene-core` covers the pure logic: sorting, the cache, the EXIF
date parser, and the move and copy rules. The app itself has an end-to-end
harness that drives the real program — it scans a folder, runs slideshows, fires
the actual menu items, culls and files test copies, filters the grid, and checks
the status bar text:

```bash
WENE_E2E=1 cargo run -p wene -- /path/to/a/folder/with/images
```

It exits 0 with a report, or 1 listing what failed.
