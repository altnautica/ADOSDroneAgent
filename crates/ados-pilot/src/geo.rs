//! Small 2-D vector helpers in the local NED horizontal plane (`[n, e]`).

pub type V2 = [f32; 2];

pub fn add(a: V2, b: V2) -> V2 {
    [a[0] + b[0], a[1] + b[1]]
}

pub fn sub(a: V2, b: V2) -> V2 {
    [a[0] - b[0], a[1] - b[1]]
}

pub fn scale(a: V2, k: f32) -> V2 {
    [a[0] * k, a[1] * k]
}

pub fn dot(a: V2, b: V2) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}

pub fn norm(a: V2) -> f32 {
    a[0].hypot(a[1])
}

pub fn dist(a: V2, b: V2) -> f32 {
    norm(sub(a, b))
}

/// `a` scaled down to at most `max` length.
pub fn clamp_len(a: V2, max: f32) -> V2 {
    let n = norm(a);
    if n > max && n > 0.0 {
        scale(a, max / n)
    } else {
        a
    }
}

/// Unit vector of a bearing in radians (0 = north, clockwise).
pub fn bearing_unit(bearing: f32) -> V2 {
    [bearing.cos(), bearing.sin()]
}

/// Bearing in radians of a vector (0 = north, clockwise).
pub fn bearing_of(a: V2) -> f32 {
    a[1].atan2(a[0])
}

/// Body (forward, right) to NED by heading `yaw`.
pub fn body_to_ned(fwd: f32, right: f32, yaw: f32) -> V2 {
    let (s, c) = yaw.sin_cos();
    [fwd * c - right * s, fwd * s + right * c]
}

/// Wrap an angle to (-pi, pi].
pub fn wrap_pi(a: f32) -> f32 {
    let mut x = a % std::f32::consts::TAU;
    if x > std::f32::consts::PI {
        x -= std::f32::consts::TAU;
    } else if x <= -std::f32::consts::PI {
        x += std::f32::consts::TAU;
    }
    x
}
