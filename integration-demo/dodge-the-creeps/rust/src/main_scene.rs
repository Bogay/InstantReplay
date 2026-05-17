use crate::{hud, mob, player};

use godot::classes::{AudioStreamPlayer, Marker2D, PathFollow2D, RigidBody2D, Timer};
use godot::prelude::*;

use std::f32::consts::PI;

/// How many seconds of replay to export on death.
const REPLAY_SECONDS: f64 = 10.0;

#[derive(GodotClass)]
#[class(base=Node)]
pub struct Main {
    mob_scene: OnReady<Gd<PackedScene>>,
    player: OnReady<Gd<player::Player>>,
    hud: OnReady<Gd<hud::Hud>>,
    music: OnReady<Gd<AudioStreamPlayer>>,
    death_sound: OnReady<Gd<AudioStreamPlayer>>,
    // InstantReplayRecorder is registered by the unienc_godot GDExtension, so we
    // hold it as a plain Node and call methods dynamically.
    recorder: OnReady<Gd<Node>>,
    score: i64,
    base: Base<Node>,
}

#[godot_api]
impl INode for Main {
    fn init(base: Base<Node>) -> Self {
        Self {
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
        let main = self.to_gd();

        self.player
            .signals()
            .hit()
            .connect_other(&main, Self::game_over);

        self.hud
            .signals()
            .start_game()
            .connect_other(&main, Self::new_game);

        self.score_timer()
            .signals()
            .timeout()
            .connect_other(&main, Self::on_score_timer_timeout);

        self.mob_timer()
            .signals()
            .timeout()
            .connect_other(&main, Self::on_mob_timer_timeout);

        // Wire InstantReplayRecorder signals.
        let callable_exported = self.base().callable("on_replay_exported");
        let callable_error = self.base().callable("on_replay_error");
        self.recorder.connect("export_completed", &callable_exported);
        self.recorder.connect("error_occurred", &callable_error);
    }
}

#[godot_api]
impl Main {
    fn game_over(&mut self) {
        self.score_timer().stop();
        self.mob_timer().stop();

        self.hud.bind_mut().show_game_over();

        self.music.stop();
        self.death_sound.play();

        // Export the last REPLAY_SECONDS of footage.
        self.recorder
            .call("export_replay", &[REPLAY_SECONDS.to_variant()]);
    }

    pub fn new_game(&mut self) {
        let start_position = self.base().get_node_as::<Marker2D>("StartPosition");

        self.score = 0;

        self.player.bind_mut().start(start_position.get_position());
        self.start_timer().start();

        let hud = self.hud.bind_mut();
        hud.update_score(self.score);
        hud.show_message("Get Ready".into());

        self.music.play();

        // Begin buffering frames for the new round.
        self.recorder.call("start", &[]);
    }

    #[func]
    fn on_start_timer_timeout(&mut self) {
        self.mob_timer().start();
        self.score_timer().start();
    }

    fn on_score_timer_timeout(&mut self) {
        self.score += 1;
        self.hud.bind_mut().update_score(self.score);
    }

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
            let mob = mob.bind();
            rand::random_range(mob.min_speed..mob.max_speed)
        };
        mob.set_linear_velocity(Vector2::new(range, 0.0).rotated(real::from_f32(direction)));
    }

    /// Called when InstantReplayRecorder emits `export_completed`.
    #[func]
    fn on_replay_exported(&self, path: GString) {
        godot_print!("[InstantReplay] Saved replay to {path}");
    }

    /// Called when InstantReplayRecorder emits `error_occurred`.
    #[func]
    fn on_replay_error(&self, message: GString) {
        godot_error!("[InstantReplay] {message}");
    }

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
