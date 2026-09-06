use bevy::prelude::*;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct ScriptTimer {
    pub duration: f32,
    pub elapsed: f32,
    pub repeat: bool,
    pub paused: bool,
    pub just_finished: bool,
    pub times_finished: u32,
}

impl ScriptTimer {
    pub fn new(duration: f32, repeat: bool) -> Self {
        Self {
            duration,
            elapsed: 0.0,
            repeat,
            paused: false,
            just_finished: false,
            times_finished: 0,
        }
    }

    pub fn tick(&mut self, delta: f32) {
        // Completion is a one-frame pulse, including when paused immediately
        // after finishing. A one-shot must not complete again on later ticks.
        self.just_finished = false;
        if self.paused || (!self.repeat && self.times_finished > 0) {
            return;
        }
        self.elapsed += delta;
        if self.elapsed >= self.duration {
            self.just_finished = true;
            self.times_finished += 1;
            if self.repeat {
                self.elapsed -= self.duration;
            } else {
                self.elapsed = self.duration;
            }
        }
    }

    pub fn progress(&self) -> f32 {
        (self.elapsed / self.duration).min(1.0)
    }
}

#[derive(Resource, Default)]
pub struct ScriptTimers {
    timers: HashMap<String, ScriptTimer>,
}

impl ScriptTimers {
    pub fn start(&mut self, name: impl Into<String>, duration: f32, repeat: bool) {
        self.timers
            .insert(name.into(), ScriptTimer::new(duration, repeat));
    }

    pub fn stop(&mut self, name: &str) -> bool {
        self.timers.remove(name).is_some()
    }

    pub fn pause(&mut self, name: &str) {
        if let Some(t) = self.timers.get_mut(name) {
            t.paused = true;
        }
    }

    pub fn resume(&mut self, name: &str) {
        if let Some(t) = self.timers.get_mut(name) {
            t.paused = false;
        }
    }

    pub fn tick_all(&mut self, delta: f32) {
        for timer in self.timers.values_mut() {
            timer.tick(delta);
        }
    }

    pub fn get_just_finished(&self) -> Vec<String> {
        self.timers
            .iter()
            .filter(|(_, t)| t.just_finished)
            .map(|(n, _)| n.clone())
            .collect()
    }

    pub fn clear(&mut self) {
        self.timers.clear();
    }
}

/// System to tick all timers each frame
pub fn update_script_timers(time: Res<Time>, mut timers: ResMut<ScriptTimers>) {
    // Avoid publishing a resource change when there is no work. Clear a
    // previous completion pulse once, even for a now-paused/completed timer.
    if !timers.timers.values().any(|timer| {
        timer.just_finished || (!timer.paused && (timer.repeat || timer.times_finished == 0))
    }) {
        return;
    }
    timers.tick_all(time.delta_secs());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shot_completes_once_and_restarting_rearms_it() {
        let mut timers = ScriptTimers::default();
        timers.start("once", 0.5, false);
        timers.tick_all(0.5);
        assert_eq!(timers.get_just_finished(), ["once"]);
        for _ in 0..1000 {
            timers.tick_all(0.5);
            assert!(timers.get_just_finished().is_empty());
        }
        assert_eq!(timers.timers["once"].times_finished, 1);
        timers.start("once", 0.5, false);
        timers.tick_all(0.5);
        assert_eq!(timers.get_just_finished(), ["once"]);
    }

    #[test]
    fn paused_repeat_clears_its_pulse_and_resumes() {
        let mut timer = ScriptTimer::new(1.0, true);
        timer.tick(1.0);
        assert!(timer.just_finished);
        timer.paused = true;
        timer.tick(10.0);
        assert!(!timer.just_finished);
        assert_eq!(timer.times_finished, 1);
        assert_eq!(timer.elapsed, 0.0);
        timer.paused = false;
        timer.tick(1.0);
        assert!(timer.just_finished);
        assert_eq!(timer.times_finished, 2);
    }

    #[test]
    fn zero_duration_one_shot_still_fires_once() {
        let mut timer = ScriptTimer::new(0.0, false);
        timer.tick(0.0);
        assert!(timer.just_finished);
        timer.tick(0.0);
        assert!(!timer.just_finished);
        assert_eq!(timer.times_finished, 1);
    }

    #[test]
    fn settled_timer_store_does_not_publish_changes() {
        use bevy::ecs::system::RunSystemOnce;
        let mut world = World::new();
        world.init_resource::<Time>();
        world.init_resource::<ScriptTimers>();
        for mode in 0..3 {
            if mode == 1 {
                world
                    .resource_mut::<ScriptTimers>()
                    .start("paused", 1.0, true);
                world.resource_mut::<ScriptTimers>().pause("paused");
            } else if mode == 2 {
                world
                    .resource_mut::<ScriptTimers>()
                    .start("once", 0.0, false);
                world.run_system_once(update_script_timers).unwrap();
                assert_eq!(
                    world.resource::<ScriptTimers>().get_just_finished(),
                    ["once"]
                );
                world.run_system_once(update_script_timers).unwrap();
                assert!(world
                    .resource::<ScriptTimers>()
                    .get_just_finished()
                    .is_empty());
            }
            for _ in 0..1000 {
                world.clear_trackers();
                world.run_system_once(update_script_timers).unwrap();
                assert!(!world.is_resource_changed::<ScriptTimers>());
            }
        }
    }
}
