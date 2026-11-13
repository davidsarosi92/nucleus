#!/bin/sh
# Builds the Notes example and runs it on an Android emulator or device
# (PLAN.md §M14.23): the app opens its database, and its line in the log
# says what it holds.  Launched twice: the second launch finds the first
# one's notes (kept in the app's files).
#
#   drivers/android/build-native.sh       # once
#   (an emulator running, or a device: adb devices)
#   examples/android/run.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}
adb="$sdk/platform-tools/adb"
app=io.github.davidsarosi92.nucleus.notes
(cd "$root/drivers/android" && ./gradlew -q :notes:assembleDebug)
"$adb" wait-for-device
"$adb" install -r "$root/examples/android/notes/build/outputs/apk/debug/notes-debug.apk" >/dev/null
"$adb" shell pm clear "$app" >/dev/null
launch() {
  "$adb" logcat -c
  "$adb" shell am start -W -n "$app/.NotesActivity" >/dev/null
  line=""
  for _ in $(seq 1 30); do
    line=$("$adb" logcat -d -s nucleus:I | grep "nucleus notes:" || true)
    [ -n "$line" ] && break
    sleep 1
  done
  "$adb" shell am force-stop "$app"
  [ -n "$line" ] || { echo "the app said nothing" >&2; "$adb" logcat -d -b crash >&2; exit 1; }
  echo "$line" | sed 's/.*nucleus notes:/nucleus notes:/'
}
first=$(launch)
second=$(launch)
echo "$first"
echo "$second"
case $first in *"launch 1,"*) ;; *) echo "the first launch is not launch 1"; exit 1 ;; esac
case $second in *"launch 2,"*) ;; *) echo "the second launch did not find the first one's database"; exit 1 ;; esac
