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
        let mut square = false;
        let mut dpad = (0i8, 0i8);

        if let Some(gilrs) = self.gilrs.as_mut() {
            while gilrs.next_event().is_some() {}
            if let Some((_id, gamepad)) = gilrs.gamepads().next() {
                let lx = axis_with_deadzone(gamepad.value(Axis::LeftStickX), PTZ_STICK_DEADZONE);
                let ly = axis_with_deadzone(gamepad.value(Axis::LeftStickY), PTZ_STICK_DEADZONE);
                // Stick up (gilrs Y negative) → tilt+; keyboard already uses +tilt for Up.
                pan = merge_axis(pan, stick_to_speed(lx, PTZ_MOVE_SPEED));
                tilt = merge_axis(tilt, stick_to_speed(ly, PTZ_MOVE_SPEED));

                dpad = dpad_dir(&gamepad);
                // D-pad PTZ only in camera fullscreen; on the grid it moves focus.
                if self.fullscreen_slot.is_some() {
                    if dpad.0 < 0 {
                        pan = merge_axis(pan, -PTZ_MOVE_SPEED);
                    } else if dpad.0 > 0 {
                        pan = merge_axis(pan, PTZ_MOVE_SPEED);
                    }
                    if dpad.1 < 0 {
                        tilt = merge_axis(tilt, PTZ_MOVE_SPEED);
                    } else if dpad.1 > 0 {
                        tilt = merge_axis(tilt, -PTZ_MOVE_SPEED);
                    }
                }

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

                cross = gamepad.is_pressed(Button::South); // Cross / X
                triangle = gamepad.is_pressed(Button::North); // Triangle
                square = gamepad.is_pressed(Button::West); // Square
            }
        }

        let on_grid = self.fullscreen_slot.is_none();
        if on_grid && dpad != (0, 0) && dpad != self.last_dpad {
            self.move_pad_focus(dpad.0 as i32, dpad.1 as i32);
        }
        self.last_dpad = dpad;

        if triangle && !self.last_triangle {
            if self.fullscreen_slot.is_some() {
                self.exit_fullscreen();
            } else {
                let on = !self.window_fullscreen;
                self.set_window_fullscreen(ctx, on);
            }
        }
        self.last_triangle = triangle;

        if on_grid && cross && !self.last_cross {
            self.select_focused_slot();
        }
        self.last_cross = cross;

        let rearm = self.ptz_rearm_buttons;
        self.ptz_rearm_buttons = false;

        let Some(target) = self.active_ptz_target() else {
            self.last_square = square;
            if !self.last_ptz.is_stop() {
                self.ptz_stop();
            }
            self.ptz_hold_keys = false;
            return;
        };

        if rearm {
            self.last_square = square;
        } else if square && !self.last_square {
            self.ptz_stop();
            self.ptz.start_patrol(target.clone(), 1);
        }
        self.last_square = square;

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

    pub(super) fn ensure_pad_focus(&mut self) {
        let n = self.views.active_view().slots.len();
        if n == 0 {
            self.pad_focus_slot = None;
            return;
        }
        if let Some(i) = self.pad_focus_slot {
            if i < n {
                return;
            }
        }
        let selected = self.sidebar_ptz_cam.as_deref();
        let idx = self
            .views
            .active_view()
            .slots
            .iter()
            .position(|s| s.as_deref() == selected)
            .unwrap_or(0);
        self.pad_focus_slot = Some(idx.min(n - 1));
    }

    fn move_pad_focus(&mut self, dx: i32, dy: i32) {
        self.ensure_pad_focus();
        let layout = self.active_layout();
        let cols = layout.cols() as i32;
        let rows = layout.rows() as i32;
        if cols == 0 || rows == 0 {
            return;
        }
        let Some(idx) = self.pad_focus_slot else {
            return;
        };
        let col = (idx as i32 % cols + dx).rem_euclid(cols);
        let row = (idx as i32 / cols + dy).rem_euclid(rows);
        let next = (row * cols + col) as usize;
        self.pad_focus_slot = Some(next);
        if let Some(id) = self
            .views
            .active_view()
            .slots
            .get(next)
            .cloned()
            .flatten()
        {
            self.select_camera(&id);
        }
    }

    fn select_focused_slot(&mut self) {
        self.ensure_pad_focus();
        let Some(idx) = self.pad_focus_slot else {
            return;
        };
        if let Some(id) = self
            .views
            .active_view()
            .slots
            .get(idx)
            .cloned()
            .flatten()
        {
            self.select_camera(&id);
            self.enter_fullscreen(idx);
        }
    }

}

fn dpad_dir(gamepad: &gilrs::Gamepad<'_>) -> (i8, i8) {
    let mut dx: i8 = 0;
    let mut dy: i8 = 0;
    if gamepad.is_pressed(Button::DPadLeft) {
        dx -= 1;
    }
    if gamepad.is_pressed(Button::DPadRight) {
        dx += 1;
    }
    if gamepad.is_pressed(Button::DPadUp) {
        dy -= 1;
    }
    if gamepad.is_pressed(Button::DPadDown) {
        dy += 1;
    }
    let ax = gamepad.value(Axis::DPadX);
    let ay = gamepad.value(Axis::DPadY);
    if ax <= -0.5 {
        dx = -1;
    } else if ax >= 0.5 {
        dx = 1;
    }
    if ay <= -0.5 {
        dy = -1;
    } else if ay >= 0.5 {
        dy = 1;
    }
    (dx, dy)
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
