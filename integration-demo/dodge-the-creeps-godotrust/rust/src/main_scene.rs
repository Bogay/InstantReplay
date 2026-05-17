use crate::{hud, mob, player};

use godot::classes::{AudioStreamPlayer, Marker2D, PathFollow2D, RigidBody2D, Timer};
use godot::prelude::*;

use std::f32::consts::PI;

/// How many seconds of footage to keep in the replay buffer.
const REPLAY_SECONDS: f64 = 10.0;

// Deriving GodotClass makes the class available to Godot.
#[derive(GodotClass)]
#[class(base=Node)]
pub struct Main {
    mob_scene: OnReady<Gd<PackedScene>>,
    player: OnReady<Gd<player::Player>>,
    hud: OnReady<Gd<hud::Hud>>,
    music: OnReady<Gd<AudioStreamPlayer>>,
    death_sound: OnReady<Gd<AudioStreamPlayer>>,
    // InstantReplayRecorder is registered by the unienc_godot GDExtension (a
    // separate cdylib), so we hold it as Node and call methods dynamically.
    recorder: OnReady<Gd<Node>>,
    score: i64,
    base: Base<Node>,
}

#[godot_api]
impl INode for Main {
    fn init(base: Base<Node>) -> Self {
        // We could also initialize those manually inside ready(), but OnReady automatically defers initialization.
        // Alternatively to init(), you can use #[init(...)] on the struct fields.
        Self {
            // OnReady::from_loaded(path) == OnReady::new(|| tools::load(path)).
            mob_scene: OnReady::from_loaded("res://Mob.tscn"),
            player: OnReady::from_node("Player"),
            hud: OnReady::from_node("Hud"),
            music: OnReady::from_node("Music"),
            death_sound: OnReady::from_node("DeathSound"),
            recorder: OnReady::from_node("InstantReplayRecorder"),
            score: 0,
            base,
        }
    }

    fn ready(&mut self) {
        // The OnReady instances are now initialized, we can access them like normal fields.

        // Get a Gd<Main> pointer to this instance.
        let main = self.to_gd();

        // Connect Player::hit -> Main::game_over.
        self.player
            .signals()
            .hit()
            .connect_other(&main, Self::game_over);

        // Connect Hud::start_game -> Main::new_game.
        self.hud
            .signals()
            .start_game()
            .connect_other(&main, Self::new_game);

        // Connect Main.ScoreTimer::timeout -> Main::on_score_timer_timeout.
        self.score_timer()
            .signals()
            .timeout()
            .connect_other(&main, Self::on_score_timer_timeout);

        // Connect Main.MobTimer::timeout -> Main::on_mob_timer_timeout.
        self.mob_timer()
            .signals()
            .timeout()
            .connect_other(&main, Self::on_mob_timer_timeout);

        // Main.StartTimer::timeout -> Main::on_start_timer_timeout is set up in the Editor's Inspector UI, but could be done here as well,
        // as follows. Note that signal handlers connected via Rust do not need a #[func] annotation, they can remain entirely visible to Godot.
        //
        // self.start_timer()
        //     .signals()
        //     .timeout()
        //     .connect_other(&main, Self::on_start_timer_timeout);

        // Wire InstantReplayRecorder signals. The recorder is from a foreign
        // GDExtension, so we use string-based connect() rather than typed signals.
        let on_exported = self.base().callable("on_replay_exported");
        let on_error = self.base().callable("on_replay_error");
        self.recorder.connect("export_completed", &on_exported);
        self.recorder.connect("error_occurred", &on_error);
    }
}

#[godot_api]
impl Main {
    // No #[func] here, this method is directly called from Rust (via type-safe signals).
    fn game_over(&mut self) {
        self.score_timer().stop();
        self.mob_timer().stop();

        self.hud.bind_mut().show_game_over();

        self.music.stop();
        self.death_sound.play();

        // Defer so the mutable borrow on Main is released before the recorder
        // emits error_occurred / export_completed back into this node.
        self.recorder
            .call_deferred("export_replay", &[REPLAY_SECONDS.to_variant()]);
    }

    // No #[func].
    pub fn new_game(&mut self) {
        let start_position = self.base().get_node_as::<Marker2D>("StartPosition");

        self.score = 0;

        self.player.bind_mut().start(start_position.get_position());
        self.start_timer().start();

        let hud = self.hud.bind_mut();
        hud.update_score(self.score);
        hud.show_message("Get Ready".into());

        self.music.play();

        // Defer for the same reason: start() may emit error_occurred synchronously,
        // which would try to re-borrow Main while new_game() still holds &mut self.
        self.recorder.call_deferred("start", &[]);
    }

    /// Emitted by InstantReplayRecorder when export finishes.
    #[func]
    fn on_replay_exported(&self, path: GString) {
        godot_print!("[InstantReplay] Saved: {path}");
    }

    /// Emitted by InstantReplayRecorder on any error.
    #[func]
    fn on_replay_error(&self, message: GString) {
        godot_error!("[InstantReplay] {message}");
    }

    #[func] // needed because connected in Editor UI (see ready).
    fn on_start_timer_timeout(&mut self) {
        self.mob_timer().start();
        self.score_timer().start();
    }

    // No #[func], connected in pure Rust.
    fn on_score_timer_timeout(&mut self) {
        self.score += 1;

        self.hud.bind_mut().update_score(self.score);
    }

    // No #[func], connected in pure Rust.
    fn on_mob_timer_timeout(&mut self) {
        let mut mob_spawn_location = self
            .base()
            .get_node_as::<PathFollow2D>("MobPath/MobSpawnLocation");

        let mut mob_scene = self.mob_scene.instantiate_as::<RigidBody2D>();

        let progress = rand::random_range(u32::MIN..u32::MAX);

        mob_spawn_location.set_progress(progress as f32);
        mob_scene.set_position(mob_spawn_location.get_position());

        let mut direction = mob_spawn_location.get_rotation() + PI / 2.0;
        direction += rand::random_range(-PI / 4.0..PI / 4.0);

        mob_scene.set_rotation(direction);

        self.base_mut().add_child(&mob_scene);

        let mut mob = mob_scene.cast::<mob::Mob>();
        let range = {
            // Local scope to bind `mob` user object
            let mob = mob.bind();
            rand::random_range(mob.min_speed..mob.max_speed)
        };

        mob.set_linear_velocity(Vector2::new(range, 0.0).rotated(real::from_f32(direction)));
    }

    // These timers could also be stored as OnReady fields, but are now fetched via function for demonstration purposes.
    fn start_timer(&self) -> Gd<Timer> {
        self.base().get_node_as::<Timer>("StartTimer")
    }

    fn score_timer(&self) -> Gd<Timer> {
        self.base().get_node_as::<Timer>("ScoreTimer")
    }

    fn mob_timer(&self) -> Gd<Timer> {
        self.base().get_node_as::<Timer>("MobTimer")
    }
}
