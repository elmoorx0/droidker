// src/humanizer/gestures.rs
//
// High-level gesture builder that bridges the math primitives in
// `humanizer/input.rs` (Bezier curves, Box-Muller Gaussian jitter)
// and the kernel-facing `InputInjector` from `streaming/input.rs`.
//
// Where `InputInjector::inject_touch` writes a single atomic touch
// event (one frame, one set of ABS_MT_* values), the functions in
// this file orchestrate *sequences* of events that look like a human
// finger: many intermediate samples along a Bezier curve, each one
// offset by a Gaussian-jittered delay, each one with slightly
// different pressure.
//
// This is the module the API layer (`/api/v1/containers/{id}/screen/*`)
// calls when the user requests a "tap" or a "swipe". The kernel sees
// the resulting event stream on /dev/uinput and relays it to the
// container's /dev/input/event0, which Android's InputReader picks up
// exactly as if a real finger had touched the screen.
//
// Why a separate module (vs. adding methods on InputInjector)?
//   - Keeps the kernel-IO struct (`InputInjector`) pure: it knows how
//     to write one event frame, nothing about gesture timing.
//   - Lets us unit-test the math + sequence generation without opening
//     /dev/uinput (which requires CAP_SYS_ADMIN + a free uinput slot).
//   - Makes it trivial to swap the gesture engine later (e.g. add
//     pinch-zoom, two-finger scroll) without touching the injector.

use crate::humanizer::input::{HumanizerEngine, Point};
use crate::streaming::input::{InputInjector, TouchEvent, TouchPhase};
use crate::error::Result;
use serde::Deserialize;

/// Configuration for humanized gestures. Defaults are calibrated to
/// look natural on a 540x960 qHD screen — the same resolution the
/// screen streamer serves.
#[derive(Debug, Clone, Deserialize)]
pub struct GestureConfig {
    /// Base delay between swipe samples, in milliseconds. The actual
    /// delay is `base ± stddev` via Box-Muller.
    pub sample_delay_ms: u32,
    pub sample_delay_stddev_ms: u32,
    /// Base delay before a tap's DOWN event (finger approaching screen).
    pub pre_tap_delay_ms: u32,
    /// Base delay between tap DOWN and tap UP (contact duration).
    pub tap_hold_ms: u32,
    /// Pressure mean (0..255). Real fingers vary 110–180.
    pub pressure_mean: u32,
    pub pressure_stddev: u32,
    /// Pause after each gesture before returning (lets the UI settle).
    pub settle_ms: u32,
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            sample_delay_ms: 16,        // ~60 Hz
            sample_delay_stddev_ms: 4,
            pre_tap_delay_ms: 40,
            tap_hold_ms: 60,
            pressure_mean: 140,
            pressure_stddev: 25,
            settle_ms: 30,
        }
    }
}

/// A humanized tap: small pre-delay (finger approaching) → DOWN at
/// (x,y) → short Gaussian-jittered hold → UP. Returns the total wall
/// time spent sleeping, in milliseconds, so the caller can budget
/// their async schedule.
pub fn tap(
    inj: &mut InputInjector,
    humanizer: &mut HumanizerEngine,
    x: i32,
    y: i32,
    cfg: &GestureConfig,
) -> Result<u32> {
    let mut total_sleep = 0u32;

    // Pre-tap delay — finger approaches the screen.
    let d = humanizer.human_delay_ms(cfg.pre_tap_delay_ms, cfg.pre_tap_delay_ms / 2);
    sleep_ms(d);
    total_sleep += d;

    // DOWN with Gaussian pressure.
    let pressure = sample_pressure(humanizer, cfg);
    inj.inject_touch(&TouchEvent {
        x,
        y,
        phase: TouchPhase::Down,
        pressure,
        slot: 0,
    })?;

    // Hold for a natural contact duration.
    let hold = humanizer.human_delay_ms(cfg.tap_hold_ms, cfg.tap_hold_ms / 3);
    sleep_ms(hold);
    total_sleep += hold;

    // UP — same X,Y as DOWN (a real finger doesn't drift on a tap).
    inj.inject_touch(&TouchEvent {
        x,
        y,
        phase: TouchPhase::Up,
        pressure: 0,
        slot: 0,
    })?;

    // Settle — Android's InputReader debounces events; giving it a
    // beat prevents the next gesture from being coalesced into this
    // one's UP event.
    let settle = humanizer.human_delay_ms(cfg.settle_ms, cfg.settle_ms / 2);
    sleep_ms(settle);
    total_sleep += settle;

    Ok(total_sleep)
}

/// A humanized swipe: build a Bezier curve from start to end, walk it
/// in N steps where N is derived from the swipe distance and a random
/// human speed (400–800 px/s), and inject one MOVE event per step
/// with Gaussian-jittered delays and pressure.
pub fn swipe(
    inj: &mut InputInjector,
    humanizer: &mut HumanizerEngine,
    start: (i32, i32),
    end: (i32, i32),
    cfg: &GestureConfig,
) -> Result<u32> {
    let mut total_sleep = 0u32;

    let p_start = Point { x: start.0 as f64, y: start.1 as f64 };
    let p_end = Point { x: end.0 as f64, y: end.1 as f64 };
    let path = humanizer.build_swipe(p_start, p_end);

    let distance = ((end.0 - start.0).pow(2) as f64
        + (end.1 - start.1).pow(2) as f64)
        .sqrt();
    let steps = humanizer.swipe_step_count(distance).max(2);

    // DOWN at the start of the curve.
    let pressure = sample_pressure(humanizer, cfg);
    inj.inject_touch(&TouchEvent {
        x: start.0,
        y: start.1,
        phase: TouchPhase::Down,
        pressure,
        slot: 0,
    })?;

    // Walk the curve. We emit steps+1 samples so both endpoints are
    // included; the first sample is the DOWN above, so the loop runs
    // steps times.
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let p = path.sample(t);

        // Gaussian-jittered inter-sample delay.
        let d = humanizer.human_delay_ms(cfg.sample_delay_ms, cfg.sample_delay_stddev_ms);
        sleep_ms(d);
        total_sleep += d;

        // Pressure drifts slightly during the swipe — real fingers
        // press harder in the middle, lighter at the edges.
        let pressure_mod = sample_pressure(humanizer, cfg);
        inj.inject_touch(&TouchEvent {
            x: p.x.round() as i32,
            y: p.y.round() as i32,
            phase: TouchPhase::Move,
            pressure: pressure_mod,
            slot: 0,
        })?;
    }

    // UP at the end of the curve.
    inj.inject_touch(&TouchEvent {
        x: end.0,
        y: end.1,
        phase: TouchPhase::Up,
        pressure: 0,
        slot: 0,
    })?;

    let settle = humanizer.human_delay_ms(cfg.settle_ms, cfg.settle_ms / 2);
    sleep_ms(settle);
    total_sleep += settle;

    Ok(total_sleep)
}

/// A humanized long-press: tap DOWN, hold for `hold_ms` (with small
/// Gaussian jitter), then UP. Useful for context menus and drag
/// operations.
pub fn long_press(
    inj: &mut InputInjector,
    humanizer: &mut HumanizerEngine,
    x: i32,
    y: i32,
    hold_ms: u32,
    cfg: &GestureConfig,
) -> Result<u32> {
    let mut total_sleep = 0u32;

    let d = humanizer.human_delay_ms(cfg.pre_tap_delay_ms, cfg.pre_tap_delay_ms / 2);
    sleep_ms(d);
    total_sleep += d;

    let pressure = sample_pressure(humanizer, cfg);
    inj.inject_touch(&TouchEvent {
        x,
        y,
        phase: TouchPhase::Down,
        pressure,
        slot: 0,
    })?;

    // While holding, emit periodic MOVE events with tiny position
    // jitter (real fingers drift 1-2 px during a long press). This
    // keeps Android's InputReader from classifying the touch as
    // "stationary" and suppressing the long-press detection.
    let jitter_steps = (hold_ms / 50).max(1);
    for _ in 0..jitter_steps {
        let step_delay = humanizer.human_delay_ms(50, 8);
        sleep_ms(step_delay);
        total_sleep += step_delay;

        let jx = x + (humanizer.next_signed() * 2.0).round() as i32;
        let jy = y + (humanizer.next_signed() * 2.0).round() as i32;
        let p = sample_pressure(humanizer, cfg);
        inj.inject_touch(&TouchEvent {
            x: jx,
            y: jy,
            phase: TouchPhase::Move,
            pressure: p,
            slot: 0,
        })?;
    }

    inj.inject_touch(&TouchEvent {
        x,
        y,
        phase: TouchPhase::Up,
        pressure: 0,
        slot: 0,
    })?;

    let settle = humanizer.human_delay_ms(cfg.settle_ms, cfg.settle_ms / 2);
    sleep_ms(settle);
    total_sleep += settle;

    Ok(total_sleep)
}

// ----- helpers --------------------------------------------------------------

fn sample_pressure(h: &mut HumanizerEngine, cfg: &GestureConfig) -> u32 {
    // Box-Muller via the humanizer's existing human_delay_ms machinery:
    // we treat pressure as a Gaussian around `pressure_mean` with
    // `pressure_stddev`, then clamp to [40, 255] (anything below 40
    // is treated as "no touch" by some Android drivers).
    let raw = h.human_delay_ms(cfg.pressure_mean, cfg.pressure_stddev);
    raw.clamp(40, 255)
}

fn sleep_ms(ms: u32) {
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
    }
}

// ----- tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gesture_config_defaults_are_reasonable() {
        let cfg = GestureConfig::default();
        assert!(cfg.sample_delay_ms >= 8 && cfg.sample_delay_ms <= 32);
        assert!(cfg.tap_hold_ms >= 30 && cfg.tap_hold_ms <= 200);
        assert!(cfg.pressure_mean >= 80 && cfg.pressure_mean <= 200);
    }

    #[test]
    fn sample_pressure_stays_in_bounds() {
        let mut h = HumanizerEngine::new(42);
        let cfg = GestureConfig::default();
        for _ in 0..1000 {
            let p = sample_pressure(&mut h, &cfg);
            assert!(p >= 40 && p <= 255);
        }
    }

    #[test]
    fn next_signed_is_bounded() {
        let mut h = HumanizerEngine::new(7);
        for _ in 0..1000 {
            let v = h.next_signed();
            assert!(v >= -1.0 && v < 1.0, "next_signed returned {v}");
        }
    }
}
