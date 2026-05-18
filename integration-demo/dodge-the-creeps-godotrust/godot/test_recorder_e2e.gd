extends SceneTree

## E2E test: creates an InstantReplayRecorder, records for a few seconds,
## exports a clip, then exits 0 on success or 1 on failure.
##
## Usage (from test_godot_e2e.sh or the justfile):
##   godot [--display-driver x11 --rendering-driver opengl3] \
##         --path <project_dir> --script res://test_recorder_e2e.gd
##
## Requires:
##   - unienc_godot GDExtension built and registered in extension_list.cfg
##   - A real or virtual (Xvfb) X display so the viewport can produce frames

const RECORD_SECS := 3.0
const WATCHDOG_SECS := 20.0

func _initialize() -> void:
	if not ClassDB.class_exists("InstantReplayRecorder"):
		printerr("FAIL: InstantReplayRecorder class not found — GDExtension not loaded")
		quit(1)
		return

	# Write next to the project file so the result is easy to inspect
	var output_path := ProjectSettings.globalize_path("res://test_replay.mp4")

	# Wait one frame so the tree and viewport are fully set up
	await process_frame

	var recorder: Node = ClassDB.instantiate("InstantReplayRecorder")
	recorder.set("output_path", output_path)
	recorder.set("max_duration", 10.0)
	recorder.set("fps_hint", 10)
	recorder.set("video_width", 160)
	recorder.set("video_height", 120)
	recorder.set("video_bitrate", 500_000)
	recorder.set("audio_bitrate", 64_000)
	recorder.connect("export_completed", _on_export_completed)
	recorder.connect("error_occurred", _on_error)
	get_root().add_child(recorder)

	recorder.call("start")
	print("[test] Output: %s" % output_path)
	print("[test] Recording started. Exporting after %.1fs..." % RECORD_SECS)

	# Safety watchdog
	create_timer(WATCHDOG_SECS).timeout.connect(func():
		printerr("[test] FAIL: watchdog timeout (%.0fs) — no signal received" % WATCHDOG_SECS)
		quit(1)
	)

	await create_timer(RECORD_SECS).timeout
	print("[test] Exporting replay...")
	recorder.call("export_replay", RECORD_SECS)

func _on_export_completed(path: String) -> void:
	var f := FileAccess.open(path, FileAccess.READ)
	if f == null:
		printerr("[test] FAIL: output file missing: %s" % path)
		quit(1)
		return
	var size := f.get_length()
	f.close()
	if size > 0:
		print("[test] PASS: replay exported to %s (%d bytes)" % [path, size])
		quit(0)
	else:
		printerr("[test] FAIL: output file is empty: %s" % path)
		quit(1)

func _on_error(message: String) -> void:
	printerr("[test] FAIL: error_occurred: %s" % message)
	quit(1)
