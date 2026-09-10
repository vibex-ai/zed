use crate::window::AndroidWindowInner;
use android_activity::input::{
    InputEvent, KeyAction, KeyCharacterMap, KeyMapChar, Keycode, MetaState, MotionAction,
};
use android_activity::{AndroidApp, InputStatus};
use gpui::{
    KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput, Point, ScrollDelta,
    ScrollWheelEvent, TouchPhase, point, px,
};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Distance (logical px) a touch may travel before it stops being a tap and
/// becomes a scroll.
const TOUCH_SLOP: f32 = 8.0;
const DOUBLE_TAP_MILLIS: u128 = 400;
const DOUBLE_TAP_DISTANCE: f32 = 16.0;
const VELOCITY_WINDOW_NANOS: i64 = 100_000_000;
const HOLD_SUPPRESSES_MOMENTUM_NANOS: i64 = 100_000_000;
/// How long a finger must stay down without travelling past the slop before the
/// press counts as a long press rather than a tap. Matches Android's own
/// long-press timeout so the synthetic equivalent does not feel early next to
/// the system's.
const LONG_PRESS_DURATION: Duration = Duration::from_millis(500);
const FLING_MINIMUM_SPEED: f32 = 50.0;
const FLING_MAXIMUM_SPEED: f32 = 8_000.0;
const MOMENTUM_MINIMUM_SPEED: f32 = 10.0;
const MOMENTUM_DECAY_PER_MILLISECOND: f32 = 0.998;

#[derive(Clone, Copy, Debug)]
pub(crate) struct TouchSample {
    position: Point<Pixels>,
    event_time_nanos: i64,
}

#[derive(Default)]
pub(crate) struct VelocityTracker {
    samples: VecDeque<TouchSample>,
}

impl VelocityTracker {
    fn push(&mut self, sample: TouchSample) {
        if self
            .samples
            .back()
            .is_some_and(|last| sample.event_time_nanos < last.event_time_nanos)
        {
            return;
        }
        if self
            .samples
            .back()
            .is_some_and(|last| sample.event_time_nanos == last.event_time_nanos)
        {
            self.samples.pop_back();
        }
        self.samples.push_back(sample);

        let cutoff = sample
            .event_time_nanos
            .saturating_sub(VELOCITY_WINDOW_NANOS);
        while self.samples.len() > 2
            && self
                .samples
                .front()
                .is_some_and(|first| first.event_time_nanos < cutoff)
        {
            self.samples.pop_front();
        }
    }

    fn velocity(&self) -> Option<Point<f32>> {
        let first = self.samples.front()?;
        let last = self.samples.back()?;
        let elapsed_nanos = last.event_time_nanos.saturating_sub(first.event_time_nanos);
        if elapsed_nanos <= 0 {
            return None;
        }
        let elapsed_seconds = elapsed_nanos as f32 / 1_000_000_000.0;
        Some(point(
            f32::from(last.position.x - first.position.x) / elapsed_seconds,
            f32::from(last.position.y - first.position.y) / elapsed_seconds,
        ))
    }
}

fn fling_velocity(velocity: Point<f32>) -> Option<Point<f32>> {
    let speed = velocity.x.hypot(velocity.y);
    if speed < FLING_MINIMUM_SPEED {
        return None;
    }
    let scale = (FLING_MAXIMUM_SPEED / speed).min(1.0);
    Some(point(velocity.x * scale, velocity.y * scale))
}

fn momentum_step(velocity: Point<f32>, elapsed_seconds: f32) -> (Point<Pixels>, Point<f32>) {
    let delta = point(
        px(velocity.x * elapsed_seconds),
        px(velocity.y * elapsed_seconds),
    );
    let decay = MOMENTUM_DECAY_PER_MILLISECOND.powf(elapsed_seconds * 1_000.0);
    let next_velocity = point(velocity.x * decay, velocity.y * decay);
    (delta, next_velocity)
}

#[derive(Default)]
pub(crate) struct ClickState {
    last_position: Point<Pixels>,
    last_time: Option<Instant>,
    current_count: usize,
}

impl ClickState {
    fn register_click(&mut self, position: Point<Pixels>) -> usize {
        let now = Instant::now();
        let distance = ((f32::from(position.x) - f32::from(self.last_position.x)).powi(2)
            + (f32::from(position.y) - f32::from(self.last_position.y)).powi(2))
        .sqrt();

        let within_double_tap = self
            .last_time
            .is_some_and(|last| now.duration_since(last).as_millis() < DOUBLE_TAP_MILLIS);
        if within_double_tap && distance < DOUBLE_TAP_DISTANCE {
            self.current_count += 1;
        } else {
            self.current_count = 1;
        }

        self.last_position = position;
        self.last_time = Some(now);
        self.current_count
    }
}

/// Single-finger gesture recognizer. GPUI has no touch event variants, so we
/// synthesize: a tap becomes MouseDown + MouseUp, and a drag past the slop
/// becomes a ScrollWheel stream with touch phases (content follows the finger).
#[derive(Default)]
pub(crate) enum TouchGesture {
    #[default]
    None,
    Pending {
        start: TouchSample,
        velocity: VelocityTracker,
        started_at: Instant,
    },
    /// The finger stayed down long enough to become a long press. The right
    /// click it stands for has already been dispatched, so the remainder of the
    /// gesture is consumed rather than also becoming a tap or a scroll.
    LongPress,
    Scrolling {
        last: TouchSample,
        velocity: VelocityTracker,
        last_moved_at_nanos: i64,
    },
    Momentum {
        velocity: Point<f32>,
        position: Point<Pixels>,
        last_tick: Instant,
    },
}

pub(crate) fn tick_scroll_momentum(window: &AndroidWindowInner, gesture: &mut TouchGesture) {
    let now = Instant::now();
    let event = match gesture {
        TouchGesture::Momentum {
            velocity,
            position,
            last_tick,
        } => {
            let elapsed_seconds = now.duration_since(*last_tick).as_secs_f32().min(0.05);
            *last_tick = now;
            let (delta, next_velocity) = momentum_step(*velocity, elapsed_seconds);
            *velocity = next_velocity;
            if velocity.x.hypot(velocity.y) < MOMENTUM_MINIMUM_SPEED {
                let position = *position;
                *gesture = TouchGesture::None;
                ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Pixels(Point::default()),
                    modifiers: Modifiers::default(),
                    touch_phase: TouchPhase::Ended,
                }
            } else {
                ScrollWheelEvent {
                    position: *position,
                    delta: ScrollDelta::Pixels(delta),
                    modifiers: Modifiers::default(),
                    touch_phase: TouchPhase::Moved,
                }
            }
        }
        _ => return,
    };
    window.dispatch_input(PlatformInput::ScrollWheel(event));
}

/// Fire the long press once the finger has been down long enough.
///
/// Android hands a held finger to the IME rather than to the app, so nothing in
/// the touch stream ever reports a long press and every text field in the app is
/// left without a select / cut / copy / paste menu. GPUI has no touch event
/// variants either, so the gesture is surfaced as a synthetic right click:
/// inputs already read a right `MouseDown` as "arm the context menu here" and
/// the matching `MouseUp` as "open it". Both are sent at the threshold so the
/// menu appears while the finger is still down, which is when Android shows it.
///
/// Driven from the frame tick rather than from a touch event because the gesture
/// is defined by the absence of events.
pub(crate) fn tick_long_press(window: &AndroidWindowInner, gesture: &mut TouchGesture) {
    let TouchGesture::Pending {
        start, started_at, ..
    } = gesture
    else {
        return;
    };
    if started_at.elapsed() < LONG_PRESS_DURATION {
        return;
    }
    let position = start.position;
    let modifiers = Modifiers::default();
    // Android selects the word under the finger on a long press, and the inputs
    // read a left double click as exactly that. Without this the menu opens with
    // an empty selection, which leaves every item that acts on a selection
    // disabled and makes the menu useless.
    window.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
        button: MouseButton::Left,
        position,
        modifiers,
        click_count: 2,
        first_mouse: false,
    }));
    window.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
        button: MouseButton::Left,
        position,
        modifiers,
        click_count: 2,
    }));
    window.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
        button: MouseButton::Right,
        position,
        modifiers,
        // Not routed through the click tracker: a long press is not a tap, and
        // counting it would let a following tap read as a double click.
        click_count: 1,
        first_mouse: false,
    }));
    window.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
        button: MouseButton::Right,
        position,
        modifiers,
        click_count: 1,
    }));
    *gesture = TouchGesture::LongPress;
}

pub(crate) fn handle_input_event(
    event: &InputEvent<'_>,
    window: &AndroidWindowInner,
    gesture: &mut TouchGesture,
    key_maps: &mut HashMap<i32, KeyCharacterMap>,
    app: &AndroidApp,
) -> InputStatus {
    match event {
        InputEvent::MotionEvent(motion_event) => {
            let scale = window.state.borrow().scale_factor;
            let pointer_index = motion_event.pointer_index();
            let pointer = motion_event.pointer_at_index(pointer_index);
            let position = point(px(pointer.x() / scale), px(pointer.y() / scale));
            let sample = TouchSample {
                position,
                event_time_nanos: motion_event.event_time(),
            };
            window.state.borrow_mut().mouse_position = position;

            match motion_event.action() {
                MotionAction::Down => {
                    let mut velocity = VelocityTracker::default();
                    velocity.push(sample);
                    *gesture = TouchGesture::Pending {
                        start: sample,
                        velocity,
                        started_at: Instant::now(),
                    };
                }
                MotionAction::Move => match gesture {
                    TouchGesture::Pending {
                        start, velocity, ..
                    } => {
                        velocity.push(sample);
                        let moved = ((f32::from(position.x) - f32::from(start.position.x)).powi(2)
                            + (f32::from(position.y) - f32::from(start.position.y)).powi(2))
                        .sqrt();
                        if moved > TOUCH_SLOP {
                            let delta =
                                point(position.x - start.position.x, position.y - start.position.y);
                            let anchor = start.position;
                            let velocity = std::mem::take(velocity);
                            *gesture = TouchGesture::Scrolling {
                                last: sample,
                                velocity,
                                last_moved_at_nanos: sample.event_time_nanos,
                            };
                            window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                                position: anchor,
                                delta: ScrollDelta::Pixels(delta),
                                modifiers: Modifiers::default(),
                                touch_phase: TouchPhase::Started,
                            }));
                        }
                    }
                    TouchGesture::Scrolling {
                        last,
                        velocity,
                        last_moved_at_nanos,
                    } => {
                        let delta =
                            point(position.x - last.position.x, position.y - last.position.y);
                        velocity.push(sample);
                        *last = sample;
                        if delta.x != px(0.0) || delta.y != px(0.0) {
                            *last_moved_at_nanos = sample.event_time_nanos;
                        }
                        window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Pixels(delta),
                            modifiers: Modifiers::default(),
                            touch_phase: TouchPhase::Moved,
                        }));
                    }
                    TouchGesture::LongPress => {}
                    TouchGesture::None | TouchGesture::Momentum { .. } => {}
                },
                MotionAction::Up => match std::mem::take(gesture) {
                    TouchGesture::Pending { start, .. } => {
                        let click_count = window
                            .click_state
                            .borrow_mut()
                            .register_click(start.position);
                        window.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
                            position: start.position,
                            pressed_button: None,
                            modifiers: Modifiers::default(),
                        }));
                        window.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
                            button: MouseButton::Left,
                            position: start.position,
                            modifiers: Modifiers::default(),
                            click_count,
                            first_mouse: false,
                        }));
                        window.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
                            button: MouseButton::Left,
                            position: start.position,
                            modifiers: Modifiers::default(),
                            click_count,
                        }));
                        // Mobile keyboard UX: a tap summons the IME when an
                        // editable has focus after the click lands; dismissal
                        // then sticks until the next tap (set_input_handler
                        // deliberately never requests the keyboard).
                        if window.state.borrow().input_handler.is_some() {
                            window.show_soft_keyboard();
                        }
                        // The tap may have moved the caret, and the IME mirror
                        // computes the range of every committed edit from its
                        // own selection, so it has to be told before the IME
                        // commits the next character.
                        window.synchronize_soft_keyboard();
                    }
                    TouchGesture::Scrolling {
                        last,
                        mut velocity,
                        mut last_moved_at_nanos,
                    } => {
                        let final_delta =
                            point(position.x - last.position.x, position.y - last.position.y);
                        if final_delta.x != px(0.0) || final_delta.y != px(0.0) {
                            last_moved_at_nanos = sample.event_time_nanos;
                            window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                                position,
                                delta: ScrollDelta::Pixels(final_delta),
                                modifiers: Modifiers::default(),
                                touch_phase: TouchPhase::Moved,
                            }));
                        }
                        velocity.push(sample);
                        window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Pixels(Point::default()),
                            modifiers: Modifiers::default(),
                            touch_phase: TouchPhase::Ended,
                        }));
                        let released_while_moving =
                            sample.event_time_nanos.saturating_sub(last_moved_at_nanos)
                                <= HOLD_SUPPRESSES_MOMENTUM_NANOS;
                        if released_while_moving
                            && let Some(velocity) = velocity.velocity().and_then(fling_velocity)
                        {
                            *gesture = TouchGesture::Momentum {
                                velocity,
                                position,
                                last_tick: Instant::now(),
                            };
                        }
                    }
                    // Deliberately not a tap: the press already did its work.
                    TouchGesture::LongPress => {}
                    TouchGesture::None | TouchGesture::Momentum { .. } => {}
                },
                MotionAction::Cancel => {
                    if matches!(
                        gesture,
                        TouchGesture::Scrolling { .. } | TouchGesture::Momentum { .. }
                    ) {
                        window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Pixels(Point::default()),
                            modifiers: Modifiers::default(),
                            touch_phase: TouchPhase::Ended,
                        }));
                    }
                    *gesture = TouchGesture::None;
                }
                _ => return InputStatus::Unhandled,
            }
            InputStatus::Handled
        }
        InputEvent::KeyEvent(key_event) => {
            let keycode = key_event.key_code();
            let Some(key) = keycode_to_key(keycode) else {
                return InputStatus::Unhandled;
            };
            let meta_state = key_event.meta_state();
            let modifiers = modifiers_from_meta_state(meta_state);

            {
                let mut state = window.state.borrow_mut();
                state.modifiers = modifiers;
                state.capslock = gpui::Capslock {
                    on: meta_state.caps_lock_on(),
                };
            }
            window.dispatch_input(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                modifiers,
                capslock: gpui::Capslock {
                    on: meta_state.caps_lock_on(),
                },
            }));

            if key.is_empty() {
                return InputStatus::Handled;
            }

            let key_char = key_char_for(key_event.device_id(), keycode, meta_state, key_maps, app);
            let keystroke = Keystroke {
                modifiers,
                key: key.to_owned(),
                key_char: key_char.clone(),
            };

            match key_event.action() {
                KeyAction::Down => {
                    let result = window.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
                        keystroke,
                        is_held: false,
                        prefer_character_input: false,
                    }));

                    let propagate = result.is_none_or(|result| result.propagate);
                    if propagate
                        && modifiers.is_subset_of(&Modifiers::shift())
                        && let Some(text) = key_char
                    {
                        window.with_input_handler(|handler| {
                            handler.replace_text_in_range(None, &text);
                        });
                    }
                    InputStatus::Handled
                }
                KeyAction::Up => {
                    window.dispatch_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
                    InputStatus::Handled
                }
                _ => InputStatus::Unhandled,
            }
        }
        _ => InputStatus::Unhandled,
    }
}

fn key_char_for(
    device_id: i32,
    keycode: Keycode,
    meta_state: MetaState,
    key_maps: &mut HashMap<i32, KeyCharacterMap>,
    app: &AndroidApp,
) -> Option<String> {
    const VIRTUAL_KEYBOARD_DEVICE_ID: i32 = -1;

    if let std::collections::hash_map::Entry::Vacant(entry) = key_maps.entry(device_id) {
        // Some devices (e.g. the emulator's forwarded host keyboard, id 0)
        // have no per-device map; the built-in virtual keyboard map always
        // exists. Cache whichever we got under the original id.
        let map = app.device_key_character_map(device_id).or_else(|error| {
            log::info!(
                "no key character map for device {device_id} ({error:?}); \
                 falling back to the virtual keyboard map"
            );
            app.device_key_character_map(VIRTUAL_KEYBOARD_DEVICE_ID)
        });
        match map {
            Ok(map) => {
                entry.insert(map);
            }
            Err(error) => {
                log::warn!("failed to load key character map for device {device_id}: {error:?}");
                return None;
            }
        }
    }
    let map = key_maps.get(&device_id)?;
    match map.get(keycode, meta_state) {
        Ok(KeyMapChar::Unicode(character)) => Some(character.to_string()),
        Ok(_) => None,
        Err(error) => {
            log::warn!("KeyCharacterMap.get failed: {error:?}");
            None
        }
    }
}

fn modifiers_from_meta_state(meta_state: MetaState) -> Modifiers {
    Modifiers {
        control: meta_state.ctrl_on(),
        alt: meta_state.alt_on(),
        shift: meta_state.shift_on(),
        platform: meta_state.meta_on(),
        function: meta_state.function_on(),
    }
}

/// Maps an Android keycode to GPUI's key names (see `Keystroke::parse`).
/// Returns `Some("")` for modifier keys (handled via ModifiersChanged) and
/// `None` for keys we don't handle so the OS can apply default behavior
/// (volume, etc.). The back key is mapped as well: the app consumes it for
/// in-app navigation, and Android already moves the host activity to the
/// background when no event handler claims the keystroke.
fn keycode_to_key(keycode: Keycode) -> Option<&'static str> {
    use Keycode::*;
    Some(match keycode {
        A => "a",
        B => "b",
        C => "c",
        D => "d",
        E => "e",
        F => "f",
        G => "g",
        H => "h",
        I => "i",
        J => "j",
        K => "k",
        L => "l",
        M => "m",
        N => "n",
        O => "o",
        P => "p",
        Q => "q",
        R => "r",
        S => "s",
        T => "t",
        U => "u",
        V => "v",
        W => "w",
        X => "x",
        Y => "y",
        Z => "z",
        Keycode0 => "0",
        Keycode1 => "1",
        Keycode2 => "2",
        Keycode3 => "3",
        Keycode4 => "4",
        Keycode5 => "5",
        Keycode6 => "6",
        Keycode7 => "7",
        Keycode8 => "8",
        Keycode9 => "9",
        Space => "space",
        Enter | NumpadEnter => "enter",
        Tab => "tab",
        Del => "backspace",
        ForwardDel => "delete",
        Escape => "escape",
        Back => "back",
        DpadUp => "up",
        DpadDown => "down",
        DpadLeft => "left",
        DpadRight => "right",
        PageUp => "pageup",
        PageDown => "pagedown",
        MoveHome => "home",
        MoveEnd => "end",
        Comma => ",",
        Period => ".",
        Minus => "-",
        Equals => "=",
        LeftBracket => "[",
        RightBracket => "]",
        Backslash => "\\",
        Semicolon => ";",
        Apostrophe => "'",
        Slash => "/",
        Grave => "`",
        ShiftLeft | ShiftRight | CtrlLeft | CtrlRight | AltLeft | AltRight | MetaLeft
        | MetaRight | CapsLock => "",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn velocity_tracker_uses_android_event_timestamps() {
        let mut tracker = VelocityTracker::default();
        tracker.push(TouchSample {
            position: point(px(0.0), px(0.0)),
            event_time_nanos: 1_000_000_000,
        });
        tracker.push(TouchSample {
            position: point(px(0.0), px(-100.0)),
            event_time_nanos: 1_100_000_000,
        });

        let velocity = tracker.velocity().unwrap();
        assert!(velocity.x.abs() < f32::EPSILON);
        assert!((velocity.y + 1_000.0).abs() < 0.01);
    }

    #[test]
    fn fling_velocity_rejects_taps_and_bounds_outliers() {
        assert!(fling_velocity(point(0.0, 20.0)).is_none());
        let bounded = fling_velocity(point(0.0, 20_000.0)).unwrap();
        assert!((bounded.y - FLING_MAXIMUM_SPEED).abs() < 0.01);
    }

    #[test]
    fn momentum_decay_is_time_based() {
        let (_, after_one_frame) = momentum_step(point(0.0, 2_000.0), 0.016);
        let (_, after_two_frames) = momentum_step(after_one_frame, 0.016);
        let (_, after_combined_frame) = momentum_step(point(0.0, 2_000.0), 0.032);

        assert!((after_two_frames.y - after_combined_frame.y).abs() < 0.01);
        assert!(after_two_frames.y < 2_000.0);
    }
}
