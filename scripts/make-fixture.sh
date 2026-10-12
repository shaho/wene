#!/bin/sh
# Build the folder the end-to-end harness runs against: four images
# whose names, sizes and dates the harness asserts on. The pictures
# themselves are the app icon at four sizes, converted with sips, so
# this needs nothing that is not already on a Mac.
#
#   sh scripts/make-fixture.sh [folder]     # default ~/wene-e2e-fixture
#   WENE_E2E=1 cargo run -p wene -- ~/wene-e2e-fixture
set -eu

FIXTURE=${1:-$HOME/wene-e2e-fixture}
# A relative folder means relative to where the script was run, so it
# is made absolute before the cd below changes that.
case "$FIXTURE" in
  /*) ;;
  *) FIXTURE=$PWD/$FIXTURE ;;
esac
cd "$(dirname "$0")/.."

# Two places break the harness for reasons that have nothing to do
# with wene: under the Desktop an unsigned binary is refused the files
# it needs, and under /tmp the folder watcher never reports a change.
# Both fill the run with failures that look like bugs.
case "$FIXTURE" in
  "$HOME/Desktop"/* | /tmp/* | /private/tmp/*)
    echo "refusing $FIXTURE: pick a folder outside the Desktop and /tmp," >&2
    echo "or the watcher and the trash will fail for reasons of their own" >&2
    exit 1
    ;;
esac

# The folder is deleted and built again, so it must be one this script
# built. The marker says so; the scan skips it, like any hidden file.
MARKER=.wene-fixture
if [ -e "$FIXTURE" ] && [ ! -e "$FIXTURE/$MARKER" ]; then
  echo "refusing $FIXTURE: it exists and this script did not build it" >&2
  exit 1
fi

ICONS=branding/macos/AppIcon.appiconset
rm -rf "$FIXTURE"
mkdir -p "$FIXTURE"
touch "$FIXTURE/$MARKER"

# name order is apple, img1, img2, img10, and the sizes differ so the
# size sort has something to sort.
for pair in apple:icon-512 img1:icon-256 img2:icon-128 img10:icon-512@2x; do
  name=${pair%%:*}
  icon=${pair##*:}
  sips -s format heic "$ICONS/$icon.png" --out "$FIXTURE/$name.heic" >/dev/null
done

# Modified dates in name order, so the date sorts are checkable.
touch -t 202601010101 "$FIXTURE/apple.heic"
touch -t 202602010101 "$FIXTURE/img1.heic"
touch -t 202603010101 "$FIXTURE/img2.heic"
touch -t 202604010101 "$FIXTURE/img10.heic"

echo "built $FIXTURE"
ls "$FIXTURE"
