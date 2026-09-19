use super::ViewerApp;
use crate::ptz::{PtzVector, PTZ_MOVE_SPEED, PTZ_ZOOM_SPEED};
use eframe::egui;
use gilrs::{Axis, Button};

const PTZ_STICK_DEADZONE: f32 = 0.2;

impl ViewerApp {
    pub(super) fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.pending_gate_confirm {
                self.pending_gate_confirm = false;
            } else if self.show_settings {
                self.show_settings = false;
            } else if self.pending_clear_slot.is_some() {
                self.pending_clear_slot = None;
            } else if self.fullscreen_slot.is_some() {
                self.exit_fullscreen();
            } else if self.aux_fullscreen_slot.is_some() {
                self.exit_aux_fullscreen();
            } else if self.window_fullscreen {
                self.set_window_fullscreen(ctx, false);
            }
        }
        if self.pending_gate_confirm && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            self.pending_gate_confirm = false;
            self.open_gates();
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
            // Focus: , near; . far (same idea as L1 / R1).
            if i.key_down(egui::Key::Comma) {
                focus -= PTZ_ZOOM_SPEED;
            }
            if i.key_down(egui::Key::Period) {
                focus += PTZ_ZOOM_SPEED;
            }
        });

        let mut cross = false;
        let mut triangle = false;
        let mut square = false;
        let mut l1 = false;
        let mut extra_cancel = false;
        let mut dpad = (0i8, 0i8);

        let cam_fs = if self.aux_open && self.aux_focused {
            self.aux_fullscreen_slot.is_some()
        } else {
            self.fullscreen_slot.is_some()
        };

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
                if cam_fs {
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
                    let ry =
                        axis_with_deadzone(gamepad.value(Axis::RightStickY), PTZ_STICK_DEADZONE);
                    zoom = merge_axis(zoom, stick_to_speed(-ry, PTZ_ZOOM_SPEED));
                }

                if gamepad.is_pressed(Button::LeftTrigger) {
                    focus = -PTZ_ZOOM_SPEED;
                } else if gamepad.is_pressed(Button::RightTrigger) {
                    focus = PTZ_ZOOM_SPEED;
                }

                l1 = gamepad.is_pressed(Button::Select);
                cross = gamepad.is_pressed(Button::South);
                triangle = gamepad.is_pressed(Button::North);
                square = gamepad.is_pressed(Button::West);
                extra_cancel = gamepad.is_pressed(Button::East)
                    || gamepad.is_pressed(Button::Start)
                    || gamepad.is_pressed(Button::RightTrigger)
                    || gamepad.is_pressed(Button::LeftTrigger2)
                    || gamepad.is_pressed(Button::RightTrigger2)
                    || gamepad.is_pressed(Button::LeftThumb)
                    || gamepad.is_pressed(Button::RightThumb);
            }
        }

        if self.pending_gate_confirm {
            let other_edge = (triangle && !self.last_triangle)
                || (square && !self.last_square)
                || (extra_cancel && !self.last_gate_cancel)
                || (l1 && !self.last_l1)
                || (dpad != (0, 0) && dpad != self.last_dpad);
            if cross && !self.last_cross {
                self.pending_gate_confirm = false;
                self.open_gates();
            } else if other_edge {
                self.pending_gate_confirm = false;
            }
            self.last_cross = cross;
            self.last_triangle = triangle;
            self.last_square = square;
            self.last_l1 = l1;
            self.last_dpad = dpad;
            self.last_gate_cancel = extra_cancel;
            if !self.last_ptz.is_stop() {
                self.ptz_stop();
            }
            self.ptz_hold_keys = false;
            return;
        }

        let on_aux = self.aux_open && self.aux_focused;
        let on_grid = !cam_fs;
        if on_grid && dpad != (0, 0) && dpad != self.last_dpad {
            self.move_pad_focus(dpad.0 as i32, dpad.1 as i32, on_aux);
        }
        self.last_dpad = dpad;

        if triangle && !self.last_triangle {
            if on_aux {
                if self.aux_fullscreen_slot.is_some() {
                    self.exit_aux_fullscreen();
                } else {
                    self.aux_window_fullscreen = !self.aux_window_fullscreen;
                }
            } else if self.fullscreen_slot.is_some() {
                self.exit_fullscreen();
            } else {
                let on = !self.window_fullscreen;
                self.set_window_fullscreen(ctx, on);
            }
        }
        self.last_triangle = triangle;

        if on_grid && cross && !self.last_cross {
            self.select_focused_slot(on_aux);
        }
        self.last_cross = cross;

        if l1 && !self.last_l1 && !self.show_settings {
            self.prompt_open_gates();
        }
        self.last_l1 = l1;
        self.last_gate_cancel = extra_cancel;

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
        self.ensure_pad_focus_for(self.views.active, false);
    }

    pub(super) fn ensure_aux_pad_focus(&mut self) {
        self.ensure_pad_focus_for(self.aux_view, true);
    }

    fn ensure_pad_focus_for(&mut self, view_idx: usize, aux: bool) {
        let n = self.views.view(view_idx).slots.len();
        let slot = if aux {
            &mut self.aux_pad_focus_slot
        } else {
            &mut self.pad_focus_slot
        };
        if n == 0 {
            *slot = None;
            return;
        }
        if let Some(i) = *slot {
            if i < n {
                return;
            }
        }
        let selected = self.sidebar_ptz_cam.as_deref();
        let idx = self
            .views
            .view(view_idx)
            .slots
            .iter()
            .position(|s| s.as_deref() == selected)
            .unwrap_or(0);
        *slot = Some(idx.min(n - 1));
    }

    fn move_pad_focus(&mut self, dx: i32, dy: i32, aux: bool) {
        if aux {
            self.ensure_aux_pad_focus();
        } else {
            self.ensure_pad_focus();
        }
        let view_idx = if aux {
            self.aux_view
        } else {
            self.views.active
        };
        let layout = self.views.view(view_idx).layout;
        let cols = layout.cols() as i32;
        let rows = layout.rows() as i32;
        if cols == 0 || rows == 0 {
            return;
        }
        let idx = if aux {
            self.aux_pad_focus_slot
        } else {
            self.pad_focus_slot
        };
        let Some(idx) = idx else {
            return;
        };
        let col = (idx as i32 % cols + dx).rem_euclid(cols);
        let row = (idx as i32 / cols + dy).rem_euclid(rows);
        let next = (row * cols + col) as usize;
        if aux {
            self.aux_pad_focus_slot = Some(next);
        } else {
            self.pad_focus_slot = Some(next);
        }
        if let Some(id) = self.views.view(view_idx).slots.get(next).cloned().flatten() {
            self.select_camera(&id);
        }
    }

    fn select_focused_slot(&mut self, aux: bool) {
        if aux {
            self.ensure_aux_pad_focus();
        } else {
            self.ensure_pad_focus();
        }
        let view_idx = if aux {
            self.aux_view
        } else {
            self.views.active
        };
        let idx = if aux {
            self.aux_pad_focus_slot
        } else {
            self.pad_focus_slot
        };
        let Some(idx) = idx else {
            return;
        };
        if let Some(id) = self.views.view(view_idx).slots.get(idx).cloned().flatten() {
            self.select_camera(&id);
            self.enter_fullscreen_at(view_idx, idx, aux);
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
    let v = gamepad
        .button_data(button)
        .map(|d| d.value())
        .unwrap_or(0.0);
    if v < 0.05 {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}
