use bevy::input::gamepad::{Gamepad, GamepadAxis, GamepadButton};
use bevy::prelude::*;
use std::collections::HashMap;

// Re-export ScriptInput from renzora
pub use renzora::ScriptInput;

/// All buttons mirrored into [`ScriptInput`], in the order scripts index them.
pub const SCRIPT_GAMEPAD_BUTTONS: [GamepadButton; 16] = [
    GamepadButton::South,
    GamepadButton::East,
    GamepadButton::West,
    GamepadButton::North,
    GamepadButton::LeftTrigger,
    GamepadButton::RightTrigger,
    GamepadButton::LeftTrigger2,
    GamepadButton::RightTrigger2,
    GamepadButton::Select,
    GamepadButton::Start,
    GamepadButton::LeftThumb,
    GamepadButton::RightThumb,
    GamepadButton::DPadUp,
    GamepadButton::DPadDown,
    GamepadButton::DPadLeft,
    GamepadButton::DPadRight,
];

/// System to update ScriptInput from Bevy input resources
pub fn update_script_input(
    mut script_input: ResMut<ScriptInput>,
    keyboard_events: Option<MessageReader<bevy::input::keyboard::KeyboardInput>>,
    mouse_buttons: Option<Res<ButtonInput<MouseButton>>>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    mouse_motion: Option<MessageReader<bevy::input::mouse::MouseMotion>>,
    scroll: Option<MessageReader<bevy::input::mouse::MouseWheel>>,
    gamepads: Query<(Entity, &Gamepad)>,
    mut gamepad_slots: Local<HashMap<Entity, u32>>,
) {
    script_input.keys_just_pressed.clear();
    script_input.keys_just_released.clear();
    script_input.mouse_just_pressed.clear();
    script_input.mouse_delta = Vec2::ZERO;
    script_input.scroll_delta = Vec2::ZERO;

    if let Some(mut keyboard_events) = keyboard_events {
        for event in keyboard_events.read() {
            if event.state.is_pressed() {
                if !script_input.keys_pressed.contains_key(&event.key_code) {
                    script_input.keys_just_pressed.insert(event.key_code, true);
                }
                script_input.keys_pressed.insert(event.key_code, true);
            } else {
                script_input.keys_pressed.remove(&event.key_code);
                script_input.keys_just_released.insert(event.key_code, true);
            }
        }
    }

    if let Some(mouse_buttons) = mouse_buttons {
        for button in mouse_buttons.get_pressed() {
            script_input.mouse_pressed.insert(*button, true);
        }
        for button in mouse_buttons.get_just_pressed() {
            script_input.mouse_just_pressed.insert(*button, true);
        }
        for button in mouse_buttons.get_just_released() {
            script_input.mouse_pressed.remove(button);
        }
    }

    if let Ok(window) = windows.single() {
        if let Some(pos) = window.cursor_position() {
            script_input.mouse_position = pos;
        }
    }

    if let Some(mut mouse_motion) = mouse_motion {
        for event in mouse_motion.read() {
            script_input.mouse_delta += event.delta;
        }
    }
    if let Some(mut scroll) = scroll {
        for event in scroll.read() {
            script_input.scroll_delta += Vec2::new(event.x, event.y);
        }
    }

    script_input.connected_gamepads.clear();

    // Stable slot assignment: a pad keeps its slot for as long as it stays
    // connected; a new pad takes the lowest free slot. Query iteration order
    // is not stable, so without this pads could swap ids between frames.
    gamepad_slots.retain(|entity, _| gamepads.contains(*entity));
    let mut new_pads: Vec<Entity> = gamepads
        .iter()
        .map(|(entity, _)| entity)
        .filter(|e| !gamepad_slots.contains_key(e))
        .collect();
    new_pads.sort();
    for entity in new_pads {
        let mut slot = 0u32;
        while gamepad_slots.values().any(|&s| s == slot) {
            slot += 1;
        }
        gamepad_slots.insert(entity, slot);
    }

    for (entity, gamepad) in gamepads.iter() {
        let id = gamepad_slots[&entity];
        script_input.connected_gamepads.push(id);

        let axes = script_input.gamepad_axes.entry(id).or_default();
        axes.clear();
        let ls = gamepad.left_stick();
        let rs = gamepad.right_stick();
        axes.insert(GamepadAxis::LeftStickX, ls.x);
        axes.insert(GamepadAxis::LeftStickY, ls.y);
        axes.insert(GamepadAxis::RightStickX, rs.x);
        axes.insert(GamepadAxis::RightStickY, rs.y);
        // Analog triggers: the Z axes on some controllers, the
        // LeftTrigger2/RightTrigger2 analog buttons on others (e.g. Windows
        // XInput). Take whichever reports a value.
        axes.insert(
            GamepadAxis::LeftZ,
            gamepad
                .get(GamepadAxis::LeftZ)
                .unwrap_or(0.0)
                .max(gamepad.get(GamepadButton::LeftTrigger2).unwrap_or(0.0)),
        );
        axes.insert(
            GamepadAxis::RightZ,
            gamepad
                .get(GamepadAxis::RightZ)
                .unwrap_or(0.0)
                .max(gamepad.get(GamepadButton::RightTrigger2).unwrap_or(0.0)),
        );
        let input = &mut *script_input;
        let buttons = input.gamepad_buttons.entry(id).or_default();
        let just_pressed = input.gamepad_buttons_just_pressed.entry(id).or_default();
        buttons.clear();
        just_pressed.clear();
        for btn in SCRIPT_GAMEPAD_BUTTONS {
            buttons.insert(btn, gamepad.pressed(btn));
            just_pressed.insert(btn, gamepad.just_pressed(btn));
        }
    }
    script_input.connected_gamepads.sort_unstable();
    // Keep the inner tables while their pad is connected; discard only slots
    // that disappeared. A reused slot is cleared/refilled above before access.
    let input = &mut *script_input;
    input
        .gamepad_axes
        .retain(|id, _| input.connected_gamepads.binary_search(id).is_ok());
    input
        .gamepad_buttons
        .retain(|id, _| input.connected_gamepads.binary_search(id).is_ok());
    input
        .gamepad_buttons_just_pressed
        .retain(|id, _| input.connected_gamepads.binary_search(id).is_ok());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamepad_tables_are_reused_and_disconnects_clear_the_slot() {
        let mut app = App::new();
        app.init_resource::<ScriptInput>()
            .add_systems(Update, update_script_input);
        let mut pad = Gamepad::default();
        pad.digital_mut().press(GamepadButton::South);
        pad.analog_mut().set(GamepadAxis::LeftStickX, 0.5);
        let first = app.world_mut().spawn(pad).id();
        // Connect separately: simultaneous assignment follows Entity ordering,
        // which is not a promise of spawn order.
        app.update();
        let second = app.world_mut().spawn(Gamepad::default()).id();
        app.update();
        let input = app.world().resource::<ScriptInput>();
        assert_eq!(input.connected_gamepads, [0, 1]);
        assert!(input.gamepad_buttons_just_pressed[&0][&GamepadButton::South]);
        let storage = (
            &input.gamepad_axes[&0][&GamepadAxis::LeftStickX] as *const f32,
            &input.gamepad_buttons[&0][&GamepadButton::South] as *const bool,
            &input.gamepad_buttons_just_pressed[&0][&GamepadButton::South] as *const bool,
        );
        app.world_mut()
            .get_mut::<Gamepad>(first)
            .unwrap()
            .digital_mut()
            .clear();
        for _ in 0..1_000 {
            app.update();
            let input = app.world().resource::<ScriptInput>();
            assert_eq!(
                storage,
                (
                    &input.gamepad_axes[&0][&GamepadAxis::LeftStickX] as *const f32,
                    &input.gamepad_buttons[&0][&GamepadButton::South] as *const bool,
                    &input.gamepad_buttons_just_pressed[&0][&GamepadButton::South] as *const bool,
                )
            );
            assert_eq!(input.gamepad_axes[&0][&GamepadAxis::LeftStickX], 0.5);
            assert!(input.gamepad_buttons[&0][&GamepadButton::South]);
            assert!(!input.gamepad_buttons_just_pressed[&0][&GamepadButton::South]);
        }
        app.world_mut().despawn(first);
        app.update();
        let input = app.world().resource::<ScriptInput>();
        assert_eq!(input.connected_gamepads, [1]);
        assert!(!input.gamepad_axes.contains_key(&0));
        assert!(!input.gamepad_buttons.contains_key(&0));
        assert!(!input.gamepad_buttons_just_pressed.contains_key(&0));
        app.world_mut().spawn(Gamepad::default());
        app.update();
        let input = app.world().resource::<ScriptInput>();
        assert_eq!(input.connected_gamepads, [0, 1]);
        assert!(!input.gamepad_buttons[&0][&GamepadButton::South]);
        assert_eq!(input.gamepad_axes[&0][&GamepadAxis::LeftStickX], 0.0);
        app.world_mut().despawn(second);
        app.update();
        assert_eq!(
            app.world().resource::<ScriptInput>().connected_gamepads,
            [0]
        );
    }
}
