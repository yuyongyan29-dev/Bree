#!/bin/zsh
set -euo pipefail
TASK_CLI_ROOT=${0:A:h:h}
TASK_SUITE_ARGS=()
TASK_SUITE_PREFIX=""
if (( $# == 1 )) && [[ "$1" == "--m3" ]]; then
  TASK_SUITE_ARGS=(--m3)
  TASK_SUITE_PREFIX="m3-"
elif (( $# != 0 )); then
  print -u2 -- "usage: zsh scripts/probe-quit.zsh [--m3]"
  exit 64
fi
TASK_RUN_ROOT="$TASK_CLI_ROOT/.artifacts/quit-probe/${TASK_SUITE_PREFIX}$(date -u +%Y%m%dT%H%M%SZ)-$$"
TASK_APP_ROOT="$TASK_RUN_ROOT/build/BreeQuitFixture.app"
mkdir -p "$TASK_APP_ROOT/Contents/MacOS"
TASK_COMMAND_LOG="$TASK_RUN_ROOT/commands.log"
TASK_SAFE_TO_CLEAN=1
cleanup_build() {
  if (( TASK_SAFE_TO_CLEAN )); then
    /usr/bin/python3 - "$TASK_RUN_ROOT/build" <<'PY'
import pathlib, shutil, sys
build = pathlib.Path(sys.argv[1]).resolve()
if build.name != "build" or build.parents[1].name != "quit-probe":
    raise SystemExit("refusing unexpected fixture cleanup path")
if build.exists():
    shutil.rmtree(build)
PY
    print -r -- "build_cleanup: removed task-created fixture app; source and logs retained" >> "$TASK_COMMAND_LOG"
  fi
}
trap cleanup_build EXIT

run_logged() {
  print -r -- "command: ${(q)@}" >> "$TASK_COMMAND_LOG"
  local task_exit=0
  "$@" >> "$TASK_COMMAND_LOG" 2>&1 || task_exit=$?
  print -r -- "exit_code: $task_exit" >> "$TASK_COMMAND_LOG"
  return "$task_exit"
}

cat > "$TASK_APP_ROOT/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>local.bree.m0.quit-fixture</string>
<key>CFBundleExecutable</key><string>BreeQuitFixture</string>
<key>CFBundleName</key><string>BreeQuitFixture</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSUIElement</key><true/>
</dict></plist>
PLIST
run_logged sw_vers || exit $?
run_logged uname -m || exit $?
run_logged xcrun swiftc --version || exit $?
run_logged cargo --version || exit $?
run_logged xcrun swiftc -target arm64-apple-macosx14.0 \
  "$TASK_CLI_ROOT/tests/fixtures/QuitFixture.swift" \
  -o "$TASK_APP_ROOT/Contents/MacOS/BreeQuitFixture" -framework AppKit || exit $?
run_logged codesign --force --sign - "$TASK_APP_ROOT" || exit $?
run_logged codesign --verify --deep --strict "$TASK_APP_ROOT" || exit $?
run_logged cargo build --locked --manifest-path "$TASK_CLI_ROOT/Cargo.toml" --example quit_probe || exit $?
print -r -- "command: quit_probe [self-built fixture] [run-directory] $TASK_SUITE_ARGS" >> "$TASK_COMMAND_LOG"
TASK_PROBE_EXIT=0
TASK_SAFE_TO_CLEAN=0
"$TASK_CLI_ROOT/target/debug/examples/quit_probe" \
  "$TASK_APP_ROOT/Contents/MacOS/BreeQuitFixture" "$TASK_RUN_ROOT" "${TASK_SUITE_ARGS[@]}" \
  > "$TASK_RUN_ROOT/controller.jsonl" 2> "$TASK_RUN_ROOT/controller.stderr" || TASK_PROBE_EXIT=$?
print -r -- "exit_code: $TASK_PROBE_EXIT" >> "$TASK_COMMAND_LOG"

# Only remove task-created binaries after the controller observed natural child exits.
# If it failed early, leave the fixture binary until its own 23s safety timer has run.
if (( TASK_PROBE_EXIT != 0 )); then
  sleep 25
fi
TASK_SAFE_TO_CLEAN=1
print -r -- "$TASK_RUN_ROOT"
exit "$TASK_PROBE_EXIT"
