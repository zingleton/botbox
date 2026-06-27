//! SSH module (U2 onward).
//!
//! U2 introduces the [`signer`] submodule: the `Signer` trait + the v1 ed25519
//! implementation. Later units add `connection`, `pipeline`, `channels`,
//! `forward`, and `known_hosts` (see the plan's Output Structure).

pub mod channels;
pub mod connection;
pub mod forward;
pub mod known_hosts;
pub mod pipeline;
pub mod signer;
