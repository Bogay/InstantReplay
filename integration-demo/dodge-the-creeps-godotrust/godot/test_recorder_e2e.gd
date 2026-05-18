extends SceneTree

## E2E test: records for a few seconds, triggers export, then verifies:
##   1. export_started fires immediately (before export_completed)
##   2. export_replay() returns quickly — main thread not blocked
##   3. _process keeps firing while the background export runs
##   4. The output file exists and is non-empty

const RECORD_SECS := 3.0
const WATCHDOG_SECS := 20.0

var _output_path: String
var _recorder: Node
var _frame_count := 0
var _frame_count_at_export := -1
var _export_started := false

func _initialize() -> void:
	if not ClassDB.class_exists("InstantReplayRecorder"):
		printerr("FAIL: InstantReplayRecorder class not found — GDExtension not loaded")
		quit(1)
		return

	_output_path = ProjectSettings.globalize_path("res://test_replay.mp4")
	process_frame.connect(func(): _frame_count += 1)

	await process_frame

	_recorder = ClassDB.instantiate("InstantReplayRecorder")
	_recorder.set("output_path", _output_path)
	_recorder.set("max_duration", 10.0)
	_recorder.set("fps_hint", 10)
	_recorder.set("video_width", 160)
	_recorder.set("video_height", 120)
	_recorder.set("video_bitrate", 500_000)
	_recorder.set("audio_bitrate", 64_000)
	_recorder.connect("export_started", _on_export_started)
	_recorder.connect("export_completed", _on_export_completed)
	_recorder.connect("error_occurred", _on_error)
	get_root().add_child(_recorder)

	_recorder.call("start")
	print("[test] Output: %s" % _output_path)
	print("[test] Recording for %.1fs..." % RECORD_SECS)

	create_timer(WATCHDOG_SECS).timeout.connect(func():
		printerr("[test] FAIL: watchdog timeout (%.0fs)" % WATCHDOG_SECS)
		quit(1)
	)

	await create_timer(RECORD_SECS).timeout

	_frame_count_at_export = _frame_count
	var t0 := Time.get_ticks_msec()

	print("[test] Calling export_replay()...")
	_recorder.call("export_replay", RECORD_SECS)

	var elapsed_ms := Time.get_ticks_msec() - t0
	print("[test] export_replay() returned in %dms" % elapsed_ms)

	# Assert 1: export_started must fire synchronously inside export_replay()
	if not _export_started:
		printerr("[test] FAIL: export_started signal did not fire during export_replay()")
		quit(1)
		return

	# Assert 2: export_replay() must not block the main thread
	if elapsed_ms > 500:
		printerr("[test] FAIL: export_replay() blocked main thread for %dms" % elapsed_ms)
		quit(1)

func _on_export_started(path: String) -> void:
	print("[test] export_started: %s" % path)
	_export_started = true

func _on_export_completed(path: String) -> void:
	var frames_during_export := _frame_count - _frame_count_at_export
	print("[test] Main loop ran %d frames while export was in progress" % frames_during_export)

	# Assert 3: main loop must not have frozen during export
	if frames_during_export == 0:
		printerr("[test] FAIL: 0 frames rendered during export — main thread was frozen")
		quit(1)
		return

	var f := FileAccess.open(path, FileAccess.READ)
	if f == null:
		printerr("[test] FAIL: output file missing: %s" % path)
		quit(1)
		return
	var size := f.get_length()
	f.close()

	if size > 0:
		print("[test] PASS: %s (%d bytes), %d frames during export" % [path, size, frames_during_export])
		quit(0)
	else:
		printerr("[test] FAIL: output file is empty: %s" % path)
		quit(1)

func _on_error(message: String) -> void:
	printerr("[test] FAIL: error_occurred: %s" % message)
	quit(1)
