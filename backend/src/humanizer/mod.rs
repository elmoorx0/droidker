// src/humanizer/mod.rs
//
// The Humanizer Engine is what makes DroidKer-driven Android apps look like
// they are being driven by a real human finger rather than a bot. It injects
// input events through /dev/input/eventX with:
//   - Bezier-curve swipe paths (not straight lines)
//   - Gaussian jitter on timings (not uniform random)
//   - Variable pressure values for realism
//
// Milestone 1 ships the public API and the math primitives. The actual
// `/dev/input` write path lands in Milestone 3 once we have the runtime
// booted (we need to know which eventX node corresponds to the container's
// virtual touchscreen).

pub mod input;

pub use input::{HumanizerEngine, BezierPath, Point};
