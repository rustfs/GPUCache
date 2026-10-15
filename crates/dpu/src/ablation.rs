// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! On/off switches for offloads that this slice does not run.
//!
//! The selected candidate is the storage queue, not one of these knobs.
//! Enabling a knob does not execute it. Hot-tier erasure coding cannot be
//! enabled. See docs/adr/0002-no-erasure-coding.md.

use gpucache_core_types::CacheError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Knob {
    P2p,
    Dpa,
    Compression,
    Crypto,
    ErasureCoding,
}

const KNOBS: [Knob; 5] = [
    Knob::P2p,
    Knob::Dpa,
    Knob::Compression,
    Knob::Crypto,
    Knob::ErasureCoding,
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ablation {
    enabled: [bool; 5],
}

impl Default for Ablation {
    fn default() -> Self {
        Self::new()
    }
}

impl Ablation {
    pub fn new() -> Self {
        Self {
            enabled: [false; 5],
        }
    }

    pub fn knobs() -> &'static [Knob] {
        &KNOBS
    }

    pub fn set(&mut self, knob: Knob, on: bool) -> Result<(), CacheError> {
        if knob == Knob::ErasureCoding && on {
            return Err(CacheError::Malformed {
                reason: "hot tier ec",
            });
        }
        self.enabled[index(knob)] = on;
        Ok(())
    }

    pub fn enabled(&self, knob: Knob) -> bool {
        self.enabled[index(knob)]
    }

    /// `OFF` when the knob is disabled. `NOT_RUN_HARDWARE` when it is enabled.
    /// Neither arm changes payload bytes.
    pub fn probe(&self, knob: Knob) -> &'static str {
        if self.enabled(knob) {
            "NOT_RUN_HARDWARE"
        } else {
            "OFF"
        }
    }
}

fn index(knob: Knob) -> usize {
    match knob {
        Knob::P2p => 0,
        Knob::Dpa => 1,
        Knob::Compression => 2,
        Knob::Crypto => 3,
        Knob::ErasureCoding => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_ec_stays_off() {
        let mut ablation = Ablation::new();
        for knob in Ablation::knobs() {
            assert!(!ablation.enabled(*knob));
            assert_eq!(ablation.probe(*knob), "OFF");
        }
        let err = ablation.set(Knob::ErasureCoding, true).unwrap_err();
        assert_eq!(
            err,
            CacheError::Malformed {
                reason: "hot tier ec"
            }
        );
        assert!(!ablation.enabled(Knob::ErasureCoding));
        ablation.set(Knob::P2p, true).unwrap();
        ablation.set(Knob::Crypto, true).unwrap();
        assert_eq!(ablation.probe(Knob::P2p), "NOT_RUN_HARDWARE");
        assert_eq!(ablation.probe(Knob::Crypto), "NOT_RUN_HARDWARE");
        assert_eq!(ablation.probe(Knob::Dpa), "OFF");
        assert_eq!(ablation.probe(Knob::ErasureCoding), "OFF");
    }
}
