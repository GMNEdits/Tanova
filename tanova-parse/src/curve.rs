//! Nuke-compatible `{curve ...}` animation curves.
//!
//! Parses, evaluates, and serializes Nuke's per-knob keyframe curve syntax.
//! A curve is a frame-sorted list of keyframe samples; each segment
//! interpolates between its two keyframes as smooth (Hermite smoothstep,
//! Nuke's default), linear, or constant (hold).

/// Interpolation mode for a keyframe segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InterpMode {
    #[default]
    Smooth, // Nuke default smooth/cubic ('s', 'S', 'c', 'C')
    Linear,   // Linear interpolation ('l', 'L')
    Constant, // Step / hold value until next keyframe ('k', 'K')
}

/// A single keyframe sample on an [`AnimationCurve`].
#[derive(Debug, Clone, PartialEq)]
pub struct Keyframe {
    pub frame: f64,
    pub value: f64,
    pub interp: InterpMode,
}

/// A frame-sorted 1D animation curve for a knob channel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnimationCurve {
    pub keys: Vec<Keyframe>,
}

const KEYFRAME_EPSILON: f64 = 1e-6;
const SPAN_EPSILON: f64 = 1e-9;

impl AnimationCurve {
    /// Creates an empty curve.
    pub fn new() -> Self {
        Self { keys: Vec::new() }
    }

    /// Creates a 1-key constant curve at frame `1.0`, used when a multi-channel
    /// knob mixes animated `{curve ...}` channels with static numeric channels.
    pub fn from_constant(value: f64) -> Self {
        let mut curve = Self::new();
        curve.keys.push(Keyframe {
            frame: 1.0,
            value,
            interp: InterpMode::Constant,
        });
        curve
    }

    /// Returns `true` if the curve holds no keyframes.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Returns the number of keyframes.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Inserts or overwrites a keyframe, keeping `keys` sorted ascending by
    /// frame. Non-finite frames or values are ignored.
    pub fn set_key(&mut self, frame: f64, value: f64, interp: InterpMode) {
        if !frame.is_finite() || !value.is_finite() {
            return;
        }
        if let Some(existing) = self
            .keys
            .iter_mut()
            .find(|key| (key.frame - frame).abs() <= KEYFRAME_EPSILON)
        {
            existing.value = value;
            existing.interp = interp;
            return;
        }
        self.keys.push(Keyframe {
            frame,
            value,
            interp,
        });
        self.keys.sort_by(|a, b| a.frame.total_cmp(&b.frame));
    }

    /// Removes the keyframe nearest to `frame`, if one sits within epsilon.
    /// Returns `true` when a keyframe was removed.
    pub fn remove_key(&mut self, frame: f64) -> bool {
        let before = self.keys.len();
        self.keys
            .retain(|key| (key.frame - frame).abs() > KEYFRAME_EPSILON);
        self.keys.len() != before
    }

    /// Returns `true` if a keyframe sits within epsilon of `frame`.
    pub fn has_key_at(&self, frame: f64) -> bool {
        self.keys
            .iter()
            .any(|key| (key.frame - frame).abs() <= KEYFRAME_EPSILON)
    }

    /// Returns deduplicated, rounded keyframe times in ascending order, used
    /// for timeline keyframe tick rendering.
    pub fn keyframe_times(&self) -> Vec<i64> {
        let mut times: Vec<i64> = self
            .keys
            .iter()
            .map(|key| key.frame.round() as i64)
            .collect();
        times.sort_unstable();
        times.dedup();
        times
    }

    /// Evaluates the curve at `frame`, clamping to the end keyframes outside
    /// the curve's time range.
    pub fn evaluate(&self, frame: f64) -> f64 {
        if self.keys.is_empty() {
            return 0.0;
        }
        if self.keys.len() == 1 || frame <= self.keys[0].frame {
            return self.keys[0].value;
        }
        let last = self.keys.last().expect("non-empty curve");
        if frame >= last.frame {
            return last.value;
        }
        let Some((k0, k1)) = self.keys.windows(2).find_map(|window| {
            let (k0, k1) = (&window[0], &window[1]);
            if k0.frame <= frame && frame <= k1.frame {
                Some((k0, k1))
            } else {
                None
            }
        }) else {
            return 0.0;
        };
        let span = k1.frame - k0.frame;
        if span.abs() <= SPAN_EPSILON {
            return k0.value;
        }
        let t = ((frame - k0.frame) / span).clamp(0.0, 1.0);
        match k0.interp {
            InterpMode::Constant => k0.value,
            InterpMode::Linear => k0.value + (k1.value - k0.value) * t,
            InterpMode::Smooth => {
                let s = t * t * (3.0 - 2.0 * t);
                k0.value + (k1.value - k0.value) * s
            }
        }
    }

    /// Serializes the curve into Nuke TCL syntax, e.g. `"{curve x1 0 x10 100}"`.
    /// Emits `l`/`k` mode tokens for non-smooth interpolation and formats
    /// integer-valued floats without trailing decimal noise.
    pub fn to_nuke_tcl(&self) -> String {
        if self.keys.is_empty() {
            return String::new();
        }
        let mut out = String::from("{curve");
        for key in &self.keys {
            match key.interp {
                InterpMode::Linear => out.push_str(" l"),
                InterpMode::Constant => out.push_str(" k"),
                InterpMode::Smooth => {}
            }
            out.push_str(&format!(
                " x{} {}",
                format_scalar(key.frame),
                format_scalar(key.value)
            ));
        }
        out.push('}');
        out
    }
}

/// Parses Nuke's `{curve ...}` knob syntax into an [`AnimationCurve`].
///
/// Accepts a bare `curve ...` token stream or one wrapped in a single outer
/// pair of braces. Prefix (`{curve l x1 0 x10 1}`) and postfix
/// (`{curve x1 0 l x10 1}`) interpolation flag styles both work. Nuke slope
/// tangent tokens (`s0`, `t1.5`) are skipped gracefully. Returns `None` when
/// the text is not a curve or declares no keyframes.
pub fn parse_nuke_curve(raw: &str) -> Option<AnimationCurve> {
    let mut text = raw.trim();
    if text.starts_with('{') && text.ends_with('}') && text.len() >= 2 {
        text = &text[1..text.len() - 1];
        text = text.trim();
    }
    let rest = text;
    if rest != "curve" && !rest.starts_with("curve ") && !rest.starts_with("curve\t") {
        return None;
    }
    let mut current_interp = InterpMode::Smooth;
    let mut next_frame = 1.0_f64;
    let mut curve = AnimationCurve::new();
    for token in rest.split_whitespace().skip(1) {
        let bytes = token.as_bytes();
        match token {
            "l" | "L" => {
                current_interp = InterpMode::Linear;
                if let Some(last) = curve.keys.last_mut() {
                    last.interp = InterpMode::Linear;
                }
            }
            "k" | "K" => {
                current_interp = InterpMode::Constant;
                if let Some(last) = curve.keys.last_mut() {
                    last.interp = InterpMode::Constant;
                }
            }
            "s" | "S" | "c" | "C" => current_interp = InterpMode::Smooth,
            _ => {
                if matches!(bytes.first(), Some(b'x') | Some(b'X')) {
                    if let Ok(frame) = token[1..].parse::<f64>() {
                        next_frame = frame;
                    }
                }
                if token.len() > 1
                    && matches!(bytes[0], b's' | b't')
                    && token[1..].parse::<f64>().is_ok()
                {
                    // Nuke custom slope tangent token (e.g. `s0`, `t1.5`).
                    continue;
                }
                if let Ok(value) = token.parse::<f64>() {
                    curve.set_key(next_frame, value, current_interp);
                    next_frame += 1.0;
                }
            }
        }
    }
    if curve.is_empty() { None } else { Some(curve) }
}

/// Formats a scalar knob value cleanly: integer-valued floats lose their
/// trailing `.0`, other values keep a bounded fractional part.
fn format_scalar(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    if value.fract() == 0.0 && value.abs() < 1e12 {
        return format!("{:.0}", value);
    }
    let text = format!("{:.6}", value);
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_parse_evaluate_and_tcl_round_trip() {
        let curve = parse_nuke_curve("{curve x1 0 x11 100}").expect("curve parses");
        assert_eq!(curve.len(), 2);
        assert_eq!(curve.evaluate(1.0), 0.0);
        assert_eq!(curve.evaluate(11.0), 100.0);
        assert_eq!(curve.evaluate(6.0), 50.0);
        assert_eq!(curve.evaluate(-5.0), 0.0);
        assert_eq!(curve.evaluate(50.0), 100.0);

        let linear = parse_nuke_curve("{curve l x1 10 x11 20}").expect("linear parses");
        assert_eq!(linear.keys[0].interp, InterpMode::Linear);
        assert_eq!(linear.evaluate(3.5), 12.5);

        let smooth = parse_nuke_curve("{curve x1 10 x11 20}").expect("smooth parses");
        assert_eq!(smooth.evaluate(3.5), 11.5625);

        let constant = parse_nuke_curve("{curve k x1 5 x10 25}").expect("constant parses");
        assert_eq!(constant.evaluate(9.9), 5.0);
        assert_eq!(constant.evaluate(10.0), 25.0);

        let slope = parse_nuke_curve("{curve x1 0 s0 x20 100 t0}").expect("slope tokens parse");
        assert_eq!(slope.len(), 2);

        // Postfix interpolation flag updates the key just pushed.
        let postfix = parse_nuke_curve("{curve x1 0 l x10 1}").expect("postfix flag parses");
        assert_eq!(postfix.keys[0].interp, InterpMode::Linear);
        assert_eq!(postfix.keys[1].interp, InterpMode::Linear);

        // set_key / remove_key / has_key_at / keyframe_times.
        let mut c = AnimationCurve::new();
        c.set_key(1.0, 0.0, InterpMode::Smooth);
        c.set_key(11.0, 100.0, InterpMode::Smooth);
        assert!(c.has_key_at(1.0));
        assert!(!c.has_key_at(5.0));
        c.set_key(1.0, 99.0, InterpMode::Linear); // overwrite in place
        assert_eq!(c.len(), 2);
        assert_eq!(c.keys[0].value, 99.0);
        assert_eq!(c.keys[0].interp, InterpMode::Linear);
        assert!(c.remove_key(11.0));
        assert!(!c.remove_key(999.0));
        assert_eq!(c.len(), 1);
        assert_eq!(c.keyframe_times(), vec![1]);

        // Non-finite frames or values are ignored.
        let mut bad = AnimationCurve::new();
        bad.set_key(f64::NAN, 1.0, InterpMode::Smooth);
        bad.set_key(1.0, f64::INFINITY, InterpMode::Smooth);
        assert!(bad.is_empty());

        // TCL round trip.
        let tcl = curve.to_nuke_tcl();
        let round_tripped = parse_nuke_curve(&tcl).expect("round trip parses");
        assert_eq!(round_tripped, curve);

        // from_constant is a single key at frame 1.0.
        let constant_curve = AnimationCurve::from_constant(42.0);
        assert_eq!(constant_curve.len(), 1);
        assert_eq!(constant_curve.evaluate(1.0), 42.0);
        assert_eq!(constant_curve.keyframe_times(), vec![1]);
    }

    #[test]
    fn parse_nuke_curve_rejects_non_curves() {
        assert_eq!(parse_nuke_curve(""), None);
        assert_eq!(parse_nuke_curve("x1 0 x10 100"), None);
        assert_eq!(parse_nuke_curve("{0.2 0.4 0.6 1}"), None);
        assert_eq!(parse_nuke_curve("{curve}"), None);
        assert_eq!(parse_nuke_curve("not a curve at all"), None);
    }
}
