// src/humanizer/input.rs
//
// Math primitives + scheduler for the Humanizer Engine.
//
// Why Bezier curves?
//   Human swipes follow curved trajectories, not straight lines. A 3-point
//   Bezier (start, control, end) produces a natural arc with very little CPU
//   cost, which matters on a 1-vCPU VPS.
//
// Why Gaussian jitter?
//   Bot detectors flag fixed timings. Gaussian noise around a base delay is
//   indistinguishable from human variability, while uniform random tends to
//   look "too uniform" in aggregate.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// Quadratic Bezier (start → control → end).
pub struct BezierPath {
    pub start: Point,
    pub control: Point,
    pub end: Point,
}

impl BezierPath {
    /// Sample the curve at parameter t ∈ [0, 1].
    pub fn sample(&self, t: f64) -> Point {
        let mt = 1.0 - t;
        let x = mt * mt * self.start.x + 2.0 * mt * t * self.control.x + t * t * self.end.x;
        let y = mt * mt * self.start.y + 2.0 * mt * t * self.control.y + t * t * self.end.y;
        Point { x, y }
    }

    /// Build a path between two points with a randomized control point so the
    /// curve arcs naturally. `curvature` controls how far off the straight
    /// line the control point sits.
    pub fn from_endpoints(start: Point, end: Point, curvature: f64, control_offset: f64) -> Self {
        let mid = Point {
            x: (start.x + end.x) / 2.0,
            y: (start.y + end.y) / 2.0,
        };
        // Perpendicular vector to (end - start), normalized and scaled.
        let dx = end.x - start.x;
        let dy = end.y - start.y;
        let len = (dx * dx + dy * dy).sqrt().max(1.0);
        let perp = Point {
            x: -dy / len,
            y: dx / len,
        };
        let control = Point {
            x: mid.x + perp.x * curvature + control_offset,
            y: mid.y + perp.y * curvature + control_offset,
        };
        Self {
            start,
            control,
            end,
        }
    }
}

/// The high-level Humanizer engine. Owns the random state so successive calls
/// produce uncorrelated output.
pub struct HumanizerEngine {
    rng_seed: u64,
}

impl HumanizerEngine {
    pub fn new(seed: u64) -> Self {
        Self { rng_seed: seed }
    }

    /// Sample a Gaussian-distributed delay (ms). Base ± stddev.
    pub fn human_delay_ms(&mut self, base_ms: u32, stddev_ms: u32) -> u32 {
        // Box-Muller transform on a cheap LCG — no external crate needed,
        // which keeps the binary small for low-resource VPS hosts.
        // Clamp u1 away from 0 to avoid ln(0) = -inf.
        let u1 = self.next_uniform().max(1e-10);
        let u2 = self.next_uniform();
        let z0 = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
        let v = base_ms as f64 + z0 * stddev_ms as f64;
        (v.max(1.0) as u32).max(1)
    }

    /// Build a swipe path with natural curvature.
    pub fn build_swipe(&mut self, start: Point, end: Point) -> BezierPath {
        // Curvature between 20 and 60 px, control offset between -10 and 10.
        let curvature = 20.0 + self.next_uniform() * 40.0;
        let control_offset = (self.next_uniform() - 0.5) * 20.0;
        BezierPath::from_endpoints(start, end, curvature, control_offset)
    }

    /// Number of intermediate events to emit for a swipe of `distance` pixels.
    /// Humans move at roughly 400-800 px/s on a touchscreen.
    pub fn swipe_step_count(&mut self, distance_px: f64) -> u32 {
        let speed = 400.0 + self.next_uniform() * 400.0; // px/sec
        let duration_sec = (distance_px / speed).max(0.05);
        // ~60 Hz touchscreen refresh.
        (duration_sec * 60.0).round() as u32
    }

    /// Sample a uniform float in `[0, 1)`. Exposed so callers like the
    /// gesture engine can derive their own distributions (e.g. signed
    /// jitter) without re-implementing xorshift64.
    pub fn next_uniform(&mut self) -> f64 {
        // xorshift64
        let mut x = self.rng_seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_seed = x;
        // Convert to [0,1)
        (x as f64) / (u64::MAX as f64)
    }

    /// Sample a uniform float in `[-1, 1)`. Convenience wrapper around
    /// `next_uniform` for symmetric jitter (e.g. small position drift
    /// during a long-press gesture).
    pub fn next_signed(&mut self) -> f64 {
        self.next_uniform() * 2.0 - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bezier_endpoints_match_at_t_ends() {
        let p = BezierPath::from_endpoints(
            Point { x: 0.0, y: 0.0 },
            Point { x: 100.0, y: 100.0 },
            30.0,
            0.0,
        );
        let s = p.sample(0.0);
        let e = p.sample(1.0);
        assert!((s.x - 0.0).abs() < 0.001);
        assert!((s.y - 0.0).abs() < 0.001);
        assert!((e.x - 100.0).abs() < 0.001);
        assert!((e.y - 100.0).abs() < 0.001);
    }

    #[test]
    fn humanizer_delay_is_nonnegative() {
        let mut h = HumanizerEngine::new(42);
        for _ in 0..1000 {
            let d = h.human_delay_ms(50, 30);
            assert!(d >= 1);
        }
    }
}
