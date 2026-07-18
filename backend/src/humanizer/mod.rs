// src/humanizer/mod.rs
//
// The Humanizer Engine is what makes DroidKer-driven Android apps look like
// they are being driven by a real human finger rather than a bot. It injects
// input events through /dev/input/eventX with:
//   - Bezier-curve swipe paths (not straight lines)
//   - Gaussian jitter on timings (not uniform random)
//   - Variable pressure values for realism
//
// Architecture (M5 final):
//   - `input.rs`    — math primitives (Bezier, Box-Muller, xorshift64 RNG).
//   - `gestures.rs` — high-level tap/swipe/long_press that orchestrate
//                     many events from the math layer to the kernel via
//                     `InputInjector`.
//
// The daemon's API layer (`/api/v1/containers/{id}/screen/human/*`) calls
// into `gestures.rs`; the kernel sees the resulting event stream on
// /dev/uinput and relays it to the container's /dev/input/event0, where
// Android's InputReader picks it up exactly as if a real finger had
// touched the screen.

pub mod gestures;
pub mod input;

pub use gestures::{tap, swipe, long_press, GestureConfig};
pub use input::{HumanizerEngine, BezierPath, Point};
