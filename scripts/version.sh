#!/bin/sh
# Print the app version. wene/Cargo.toml is its only source; every
# script that needs the version asks this one.
set -eu

cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' wene/Cargo.toml | head -1)
if [ -z "$VERSION" ]; then
  echo "no version = \"...\" line in wene/Cargo.toml" >&2
  exit 1
fi
echo "$VERSION"
