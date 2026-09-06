//! Opt-in pointer-rate and frame-interval evidence, without modifying input.

use std::time::{Duration, Instant};

use bevy::input::mouse::MouseMotion;
use bevy::prelude::*;
use bevy::window::CursorMoved;

const LABELS: [&str; 4] = ["idle", "viewport-motion", "ui-motion", "button-held"];

#[derive(Default)]
struct Samples {
    frames: u64,
    interval: Duration,
    main_schedule: Duration,
    cursor_events: usize,
    raw_events: usize,
}

impl Samples {
    fn record(&mut self, interval: Duration, main: Duration, cursor: usize, raw: usize) {
        self.frames += 1;
        self.interval += interval;
        self.main_schedule += main;
        self.cursor_events += cursor;
        self.raw_events += raw;
    }

    fn averages_ms(&self) -> (f64, f64) {
        let scale = 1000.0 / self.frames.max(1) as f64;
        (
            self.interval.as_secs_f64() * scale,
            self.main_schedule.as_secs_f64() * scale,
        )
    }
}

#[derive(Resource)]
struct PointerDiagnostics {
    started: Instant,
    last_report: Instant,
    samples: [Samples; 4],
}

pub(super) fn install(app: &mut App) {
    if std::env::var_os("RENZORA_POINTER_DIAGNOSTICS").as_deref() != Some(std::ffi::OsStr::new("1"))
    {
        return;
    }
    info!("[pointer-perf] Enabled; frame intervals include render/event-loop waits, main timings span First to Last, neither is a GPU measurement");
    app.insert_resource(PointerDiagnostics {
        started: Instant::now(),
        last_report: Instant::now(),
        samples: default(),
    });
    app.add_systems(First, begin_frame);
    app.add_systems(Last, finish_frame);
}

fn begin_frame(mut diagnostic: ResMut<PointerDiagnostics>) {
    diagnostic.started = Instant::now();
}

fn category(cursor: usize, raw: usize, hovered: bool, held: bool) -> usize {
    if held {
        3
    } else if cursor == 0 && raw == 0 {
        0
    } else if hovered {
        1
    } else {
        2
    }
}

fn finish_frame(
    mut diagnostic: ResMut<PointerDiagnostics>,
    mut cursor: MessageReader<CursorMoved>,
    mut raw: MessageReader<MouseMotion>,
    buttons: Res<ButtonInput<MouseButton>>,
    time: Res<Time<Real>>,
    viewport: Option<Res<renzora::core::viewport_types::ViewportState>>,
) {
    // Independent readers leave all original events available to editor systems.
    let cursor_count = cursor.read().count();
    let raw_count = raw.read().count();
    let index = category(
        cursor_count,
        raw_count,
        viewport.is_some_and(|v| v.hovered),
        buttons.get_pressed().next().is_some(),
    );
    let main = diagnostic.started.elapsed();
    diagnostic.samples[index].record(time.delta(), main, cursor_count, raw_count);
    if diagnostic.last_report.elapsed() < Duration::from_secs(10) {
        return;
    }
    for (label, samples) in LABELS.iter().zip(&diagnostic.samples) {
        if samples.frames == 0 {
            continue;
        }
        let (interval, main) = samples.averages_ms();
        info!("[pointer-perf] {label}: frames={} interval_ms={interval:.3} main_ms={main:.3} cursor_events={} raw_events={}", samples.frames, samples.cursor_events, samples.raw_events);
    }
    diagnostic.samples = default();
    diagnostic.last_report = Instant::now();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observing_motion_leaves_messages_for_other_consumers() {
        use bevy::ecs::message::MessageCursor;
        use bevy::ecs::system::RunSystemOnce;
        let mut world = World::new();
        world.init_resource::<Messages<CursorMoved>>();
        world.init_resource::<Messages<MouseMotion>>();
        world.init_resource::<ButtonInput<MouseButton>>();
        world.init_resource::<Time<Real>>();
        world.insert_resource(PointerDiagnostics {
            started: Instant::now(),
            last_report: Instant::now(),
            samples: default(),
        });
        world
            .resource_mut::<Messages<MouseMotion>>()
            .write(MouseMotion { delta: Vec2::X });
        world
            .run_system_once(finish_frame)
            .expect("diagnostic system");
        assert_eq!(
            world.resource::<PointerDiagnostics>().samples[2].raw_events,
            1
        );
        assert_eq!(
            MessageCursor::<MouseMotion>::default()
                .read(world.resource::<Messages<MouseMotion>>())
                .count(),
            1
        );
    }

    #[test]
    fn distinguishes_motion_from_buttons_and_averages_all_frames() {
        assert_eq!(category(0, 0, true, false), 0);
        assert_eq!(category(2, 0, true, false), 1);
        assert_eq!(category(0, 2, false, false), 2);
        assert_eq!(category(0, 0, true, true), 3);
        let mut samples = Samples::default();
        for _ in 0..1000 {
            samples.record(Duration::from_millis(10), Duration::from_millis(2), 3, 4);
        }
        assert_eq!(samples.averages_ms(), (10.0, 2.0));
        assert_eq!((samples.cursor_events, samples.raw_events), (3000, 4000));
    }
}
