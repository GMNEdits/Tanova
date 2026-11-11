//! Tanova microkernel.
//!
//! Owns the blackboard types (`Packet`, `EvalContext`), the `NodeTrait`
//! cartridge contract, the lazy-pull DAG with hash caching, and the
//! command/texture message bus. Deliberately blind to VFX math.
