//! Tanova GPU rendering.
//!
//! Renders to off-screen textures on the kernel thread, using tile aprons to
//! avoid grid seams and GPU-side color transforms (ACES / Rec.709).
