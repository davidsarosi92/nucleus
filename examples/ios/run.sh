#!/bin/sh
# Builds the Notes example and runs it on an iOS simulator (PLAN.md
# §M14.23): the app opens its database, and its first line in the log
# says what it holds.  Launched twice: the second launch finds the first
# one's notes (kept in the app's Application Support directory).
#
#   drivers/swift/build-xcframework.sh   # once
#   examples/ios/run.sh [simulator name or id]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
device=${1:-$(xcrun simctl list devices available | sed -n 's/^ *\(iPhone[^(]*\) (\([0-9A-F-]*\)).*/\2/p' | head -1)}
[ -n "$device" ] || { echo "no iPhone simulator"; exit 1; }
derived="$root/target/xcode-notes"
(cd "$here/Notes.swiftpm" && xcodebuild build -quiet -scheme Notes \
  -destination "platform=iOS Simulator,id=$device,arch=$(uname -m)" -derivedDataPath "$derived")
app="$derived/Build/Products/Debug-iphonesimulator/Notes.app"
xcrun simctl boot "$device" 2>/dev/null || true
xcrun simctl bootstatus "$device" -b >/dev/null
xcrun simctl uninstall "$device" io.github.davidsarosi92.nucleus.notes 2>/dev/null || true
xcrun simctl install "$device" "$app"
launch() {
  out=$(mktemp)
  xcrun simctl launch --console-pty --terminate-running-process "$device" \
    io.github.davidsarosi92.nucleus.notes > "$out" 2>&1 &
  pid=$!
  for _ in $(seq 1 60); do
    grep -q "nucleus notes:" "$out" && break
    sleep 1
  done
  kill "$pid" 2>/dev/null || true
  xcrun simctl terminate "$device" io.github.davidsarosi92.nucleus.notes 2>/dev/null || true
  line=$(grep "nucleus notes:" "$out" || true)
  rm -f "$out"
  [ -n "$line" ] || { echo "the app said nothing" >&2; exit 1; }
  echo "$line"
}
first=$(launch)
second=$(launch)
echo "$first"
echo "$second"
case $first in *"launch 1,"*) ;; *) echo "the first launch is not launch 1"; exit 1 ;; esac
case $second in *"launch 2,"*) ;; *) echo "the second launch did not find the first one's database"; exit 1 ;; esac
