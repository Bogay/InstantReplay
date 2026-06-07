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
	if size == 0:
		printerr("[test] FAIL: output file is empty: %s" % path)
		quit(1)
		return

	# Assert 5+6: video frame content — no 4× tiling, has actual color.
	if not _check_video_content(path):
		quit(1)
		return

	print("[test] PASS: %s (%d bytes), %d frames during export" % [path, size, frames_during_export])
	quit(0)

## Verify video content against two assertions:
##   Assert 5: Frame count must be proportional to recording duration.
##             The FORMAT_RGB8 viewport bug (3 bytes/pixel instead of 4) causes FFmpeg to
##             accumulate 4 raw frames per 3 encoded frames → only 75% of expected frames.
##             The FORMAT_L8 bug (1 byte/pixel) causes only 25%. Either way, frame count
##             drops well below 70% of the expected rate and we can catch it here.
##   Assert 6: At least some pixels have non-zero color saturation (not greyscale).
##             The mobile-renderer FORMAT_L8 data mis-read as BGRA channels produces
##             grey output; this check catches that even if frame count is somehow right.
func _check_video_content(mp4_path: String) -> bool:
	# Assert 5: frame count must be ≥ 70% of duration × 30 fps.
	var dur_out: Array = []
	OS.execute("ffprobe", PackedStringArray([
		"-v", "error",
		"-show_entries", "format=duration",
		"-of", "default=nokey=1:noprint_wrappers=1",
		mp4_path
	]), dur_out)
	var duration_secs := float(dur_out[0].strip_edges()) if not dur_out.is_empty() else 0.0

	var frames_out: Array = []
	OS.execute("ffprobe", PackedStringArray([
		"-v", "error",
		"-select_streams", "v:0",
		"-show_entries", "stream=nb_frames",
		"-of", "default=nokey=1:noprint_wrappers=1",
		mp4_path
	]), frames_out)
	var frame_count := int(frames_out[0].strip_edges()) if not frames_out.is_empty() else 0

	var expected_min := int(duration_secs * 30.0 * 0.7)
	if frame_count < expected_min:
		printerr(
			"[test] FAIL: video has too few frames — got %d, need ≥%d (%.1fs × 30fps × 70%%). " \
			% [frame_count, expected_min, duration_secs] +
			"Possible bytes-per-pixel mismatch (FORMAT_RGB8/L8 from viewport)."
		)
		return false

	# Assert 6: has actual color (not monochrome/greyscale).
	# Extract one frame and sample a pixel grid for maximum per-pixel saturation.
	var frame_path := "/tmp/ir_test_verify_frame.png"
	var extract_exit := OS.execute("ffmpeg", PackedStringArray([
		"-y", "-loglevel", "error",
		"-i", mp4_path,
		"-ss", "2", "-vframes", "1",
		frame_path
	]))
	if extract_exit != 0:
		printerr("[test] FAIL: ffmpeg could not extract a frame for color verification (exit %d)" % extract_exit)
		return false

	var img := Image.load_from_file(frame_path)
	DirAccess.remove_absolute(frame_path)
	if img == null:
		printerr("[test] FAIL: could not load extracted verification frame: %s" % frame_path)
		return false

	var w := img.get_width()
	var h := img.get_height()
	var max_sat := 0.0
	for xi in range(0, w, max(1, w / 16)):
		for yi in range(0, h, max(1, h / 24)):
			var p := img.get_pixel(xi, yi)
			var lo := minf(p.r, minf(p.g, p.b))
			var hi := maxf(p.r, maxf(p.g, p.b))
			if hi - lo > max_sat:
				max_sat = hi - lo
	if max_sat < 0.08:
		printerr(
			"[test] FAIL: video appears monochrome (max saturation=%.3f, need ≥0.08). " \
			% max_sat +
			"Possible FORMAT_L8 channel mis-read producing grey output."
		)
		return false

	print("[test] Video content OK: %d frames in %.1fs, max saturation=%.3f" \
			% [frame_count, duration_secs, max_sat])
	return true

func _on_error(message: String) -> void:
	printerr("[test] error_occurred: %s" % message)
	_error_fired = true
