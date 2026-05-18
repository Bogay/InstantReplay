extends SceneTree

## E2E test: runs the real Main.tscn scene, plays the game for a few seconds
## so the recorder captures actual gameplay (mobs, player, audio), then
## triggers game over and verifies:
##   1. export_started fires (before background thread starts)
##   2. _process keeps running while the background export runs
##   3. Output file exists and is non-empty
##   4. A second simultaneous hit (double mob-hit race) does NOT emit error_occurred

const PLAY_SECS := 4.0   ## long enough for StartTimer (2s) + some mob activity
const WATCHDOG_SECS := 60.0

var _output_path: String
var _recorder: Node
var _frame_count := 0
var _frame_count_at_export := -1
var _export_started := false
var _error_fired := false

func _initialize() -> void:
	if not ClassDB.class_exists("InstantReplayRecorder"):
		printerr("FAIL: InstantReplayRecorder class not found — GDExtension not loaded")
		quit(1)
		return

	_output_path = ProjectSettings.globalize_path("res://test_replay.mp4")
	process_frame.connect(func(): _frame_count += 1)

	create_timer(WATCHDOG_SECS).timeout.connect(func():
		printerr("[test] FAIL: watchdog timeout (%.0fs)" % WATCHDOG_SECS)
		quit(1)
	)

	# Instantiate the real game scene rather than an empty tree.
	var main_packed := load("res://Main.tscn") as PackedScene
	var main := main_packed.instantiate()
	get_root().add_child(main)
	await process_frame

	_recorder = main.get_node("InstantReplayRecorder")
	_recorder.set("output_path", _output_path)
	_recorder.connect("export_started", _on_export_started)
	_recorder.connect("export_completed", _on_export_completed)
	_recorder.connect("error_occurred", _on_error)

	# Start the game — this triggers recorder.start() and mob spawning.
	main.call("new_game")
	print("[test] Output: %s" % _output_path)
	print("[test] Playing game for %.1fs (StartTimer=2s + mob activity)..." % PLAY_SECS)

	await create_timer(PLAY_SECS).timeout

	_frame_count_at_export = _frame_count

	# Trigger game over by emitting the player's hit signal (simulates mob collision).
	var player := main.get_node("Player")
	player.emit_signal("hit")

	# Assert 4: a second simultaneous hit must NOT emit error_occurred.
	player.emit_signal("hit")

	print("[test] Hit signal emitted — waiting for deferred export_replay()...")

	# export_replay() is called_deferred inside game_over(); wait one frame for it.
	await process_frame

	# Assert 1: export_started must have fired by now.
	if not _export_started:
		printerr("[test] FAIL: export_started signal did not fire after game_over()")
		quit(1)

func _on_export_started(path: String) -> void:
	print("[test] export_started: %s" % path)
	_export_started = true

func _on_export_completed(path: String) -> void:
	var frames_during_export := _frame_count - _frame_count_at_export
	print("[test] Main loop ran %d frames while export was in progress" % frames_during_export)

	# Assert 2: main loop must not have frozen during export.
	if frames_during_export == 0:
		printerr("[test] FAIL: 0 frames rendered during export — main thread was frozen")
		quit(1)
		return

	# Assert 4: second hit must not have emitted error_occurred.
	if _error_fired:
		printerr("[test] FAIL: error_occurred fired — double hit must be silently ignored")
		quit(1)
		return

	var f := FileAccess.open(path, FileAccess.READ)
	if f == null:
		printerr("[test] FAIL: output file missing: %s" % path)
		quit(1)
		return
	var size := f.get_length()
	f.close()

	# Assert 3: output file must contain actual encoded data.
	if size > 0:
		print("[test] PASS: %s (%d bytes), %d frames during export" % [path, size, frames_during_export])
		quit(0)
	else:
		printerr("[test] FAIL: output file is empty: %s" % path)
		quit(1)

func _on_error(message: String) -> void:
	printerr("[test] error_occurred: %s" % message)
	_error_fired = true
