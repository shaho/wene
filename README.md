# wêne

wêne is a fast image viewer and slideshow app for macOS, written in Rust. It is
a port of [Phoenix Slides](https://blyt.net/phxslides/)

## What it does right now

The initial version functions as a basic demonstration.

The software operates as follows:

- The user selects a directory - it identifies every file within that directory
  and its nested folders during a background process. The application displays
  small preview images simultaneously.
- The user navigates a grid of those preview images - they are organized
  according to the naming convention used by the macOS file manager where "img2"
  precedes "img10".
- The user performs a double click on a preview image or presses the Return key.
  It initiates a mode where images occupy the entire screen beginning with the
  selected file.
- In this display mode, the user utilizes the arrow keys and space bar to change
  images. The home and end keys move the view to the first or last file. To
  increase or decrease the image size, the user presses the plus or minus keys.
  The equals key displays the original pixel dimensions. The asterisk key
  returns the image to its standard view. To move the image within the frame,
  the user drags the mouse. The escape or "q" key terminates the display mode.
- The software displays vertical images in a correct upright position - using
  EXIF orientation data.

It supports every file format that the macOS operating system can translate,
like HEIC.

## Run it

```bash
cargo run -p wene
```

Or pass a folder directly:

```bash
cargo run -p wene -- ~/Pictures
```

To build a small .app bundle in `target/wene.app`:

```bash
./scripts/make-app.sh
```

Needs macOS 11.5 or later and a Rust toolchain.

## How it is built

Two crates. `wene-core` is portable Rust with no Apple dependencies: it scans
folders, keeps the sorted file list, and runs the decode worker threads. `wene`
is the macOS shell: AppKit windows and views through the
[objc2](https://github.com/madsmtm/objc2) bindings, with image decoding done by
the system's Image I/O framework. The two halves talk through a channel of typed
events.

Two crates. `wene-core` is portable Rust without any Apple dependencies. Scans
directories, keeps sorted list of files, runs the decode worker threads. `wene`
is the macOS shell: AppKit windows and views through the
[objc2](https://github.com/madsmtm/objc2) bindings. Image decoding done by the
system's Image I/O framework. The two halves communicate through a typed channel
of events.
