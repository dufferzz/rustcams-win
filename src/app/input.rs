use super::ViewerApp;
use crate::ptz::{PtzVector, PTZ_MOVE_SPEED, PTZ_ZOOM_SPEED};
use eframe::egui;
use gilrs::{Axis, Button};
use tracing::info;

const PTZ_STICK_DEADZONE: f32 = 0.2;

impl ViewerApp {
    pub(super) fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.show_settings {
                self.show_settings = false;
            } else if self.pending_clear_slot.is_some() {
                self.pending_clear_slot = None;
            } else if self.fullscreen_slot.is_some() {
                self.exit_fullscreen();
            } else if self.window_fullscreen {
                self.set_window_fullscreen(ctx, false);
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::D)) {
            self.debug_overlay = !self.debug_overlay;
            info!(debug = self.debug_overlay, "debug overlay toggled");
        }
        if ctx.input(|i| i.key_pressed(egui::Key::L)) {
            self.show_log = !self.show_log;
        }
    }
    /// Poll keyboard + DualShock axes into a continuous PTZ command.
    pub(super) fn handle_ptz_input(&mut self, ctx: &egui::Context) {
        let (mut pan, mut tilt, mut zoom, mut focus) = (0i32, 0i32, 0i32, 0i32);

        ctx.input(|i| {
            if i.key_down(egui::Key::ArrowLeft) {
                pan -= PTZ_MOVE_SPEED;
            }
            if i.key_down(egui::Key::ArrowRight) {
                pan += PTZ_MOVE_SPEED;
            }
            if i.key_down(egui::Key::ArrowUp) {
                tilt += PTZ_MOVE_SPEED;
            }
            if i.key_down(egui::Key::ArrowDown) {
                tilt -= PTZ_MOVE_SPEED;
            }
            // Zoom: =/+ / PageUp in; - / PageDown out.
            if i.key_down(egui::Key::Equals)
                || i.key_down(egui::Key::Plus)
                || i.key_down(egui::Key::PageUp)
            {
                zoom += PTZ_ZOOM_SPEED;
            }
            if i.key_down(egui::Key::Minus) || i.key_down(egui::Key::PageDown) {
                zoom -= PTZ_ZOOM_SPEED;
            }
        });

        let mut cross = false;
        let mut triangle = false;
        let mut circle = false;

        if let Some(gilrs) = self.gilrs.as_mut() {
            while gilrs.next_event().is_some() {}
            if let Some((_id, gamepad)) = gilrs.gamepads().next() {
                let lx = axis_with_deadzone(gamepad.value(Axis::LeftStickX), PTZ_STICK_DEADZONE);
                let ly = axis_with_deadzone(gamepad.value(Axis::LeftStickY), PTZ_STICK_DEADZONE);
                // Stick up (gilrs Y negative) → tilt+; keyboard already uses +tilt for Up.
                pan = merge_axis(pan, stick_to_speed(lx, PTZ_MOVE_SPEED));
                tilt = merge_axis(tilt, stick_to_speed(ly, PTZ_MOVE_SPEED));

                // D-pad: same mapping as arrows / left stick (up = tilt+).
                if gamepad.is_pressed(Button::DPadLeft) {
                    pan = merge_axis(pan, -PTZ_MOVE_SPEED);
                }
                if gamepad.is_pressed(Button::DPadRight) {
                    pan = merge_axis(pan, PTZ_MOVE_SPEED);
                }
                if gamepad.is_pressed(Button::DPadUp) {
                    tilt = merge_axis(tilt, PTZ_MOVE_SPEED);
                }
                if gamepad.is_pressed(Button::DPadDown) {
                    tilt = merge_axis(tilt, -PTZ_MOVE_SPEED);
                }
                let dx = axis_with_deadzone(gamepad.value(Axis::DPadX), 0.5);
                let dy = axis_with_deadzone(gamepad.value(Axis::DPadY), 0.5);
                pan = merge_axis(pan, stick_to_speed(dx, PTZ_MOVE_SPEED));
                tilt = merge_axis(tilt, stick_to_speed(dy, PTZ_MOVE_SPEED));

                let l2 = trigger_value(&gamepad, Button::LeftTrigger2);
                let r2 = trigger_value(&gamepad, Button::RightTrigger2);
                if l2 > 0.05 || r2 > 0.05 {
                    zoom = merge_axis(zoom, stick_to_speed(r2 - l2, PTZ_ZOOM_SPEED));
                } else {
                    let ry = axis_with_deadzone(
                        gamepad.value(Axis::RightStickY),
                        PTZ_STICK_DEADZONE,
                    );
                    zoom = merge_axis(zoom, stick_to_speed(-ry, PTZ_ZOOM_SPEED));
                }

                if gamepad.is_pressed(Button::LeftTrigger) {
                    focus = -PTZ_ZOOM_SPEED;
                } else if gamepad.is_pressed(Button::RightTrigger) {
                    focus = PTZ_ZOOM_SPEED;
                }

                cross = gamepad.is_pressed(Button::South); // Cross / A
                triangle = gamepad.is_pressed(Button::North); // Triangle / Y
                circle = gamepad.is_pressed(Button::East); // Circle / B
            }
        }

        if circle && !self.last_circle {
            if self.fullscreen_slot.is_some() {
                self.exit_fullscreen();
            } else if self.window_fullscreen {
                self.set_window_fullscreen(ctx, false);
            }
        }
        self.last_circle = circle;

        let rearm = self.ptz_rearm_buttons;
        self.ptz_rearm_buttons = false;

        let Some(target) = self.active_ptz_target() else {
            self.last_cross = cross;
            self.last_triangle = triangle;
            if !self.last_ptz.is_stop() {
                self.ptz_stop();
            }
            self.ptz_hold_keys = false;
            return;
        };

        if rearm {
            self.last_cross = cross;
            self.last_triangle = triangle;
        } else {
            if cross && !self.last_cross {
                self.ptz_stop();
                self.ptz.goto_preset(target.clone(), 1);
            }
            if triangle && !self.last_triangle {
                self.ptz_stop();
                self.ptz.start_patrol(target.clone(), 1);
            }
            self.last_cross = cross;
            self.last_triangle = triangle;
        }

        let vec = PtzVector {
            pan,
            tilt,
            zoom,
            focus,
        };
        if !vec.is_stop() {
            self.apply_ptz_vector(vec);
            self.ptz_hold_keys = true;
        } else if self.ptz_hold_keys {
            self.apply_ptz_vector(PtzVector::STOP);
            self.ptz_hold_keys = false;
        }
    }

}

fn stick_to_speed(v: f32, base: i32) -> i32 {
    if v.abs() < 0.01 {
        return 0;
    }
    // Four stepped buckets: base, +10, +20, +30 (clamped to 100).
    let mag = v.abs().clamp(0.0, 1.0);
    let step = if mag < 0.35 {
        0
    } else if mag < 0.6 {
        1
    } else if mag < 0.85 {
        2
    } else {
        3
    };
    let speed = (base + step * 10).clamp(1, 100);
    if v < 0.0 {
        -speed
    } else {
        speed
    }
}

fn axis_with_deadzone(v: f32, deadzone: f32) -> f32 {
    if v.abs() < deadzone {
        0.0
    } else {
        let sign = v.signum();
        let mag = ((v.abs() - deadzone) / (1.0 - deadzone)).clamp(0.0, 1.0);
        sign * mag
    }
}

fn merge_axis(a: i32, b: i32) -> i32 {
    // Prefer the larger magnitude when both sources contribute.
    if a.abs() >= b.abs() {
        a
    } else {
        b
    }
}

fn trigger_value(gamepad: &gilrs::Gamepad<'_>, button: Button) -> f32 {
    let v = gamepad.button_data(button).map(|d| d.value()).unwrap_or(0.0);
    if v < 0.05 {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}
