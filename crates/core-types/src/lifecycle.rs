// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::error::CacheError;

/// Block lifecycle. `Quarantined` is the failure holding state: the buffer
/// stays allocated until drain or fencing proves the transfer has stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockState {
    Absent,
    Reserved,
    Writing,
    Committed,
    Evicting,
    Quarantined,
}

impl BlockState {
    pub const fn is_externally_readable(self) -> bool {
        matches!(self, Self::Committed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Completion {
    NotStarted,
    InFlight,
    Completed,
    Unknown,
}

/// What the caller claims about in-flight DMA or a copy.
///
/// `KnownIdle` is not accepted as proof for a registered region. A lease
/// expiry is not proof either. Only `Drained` from the owning request, or a
/// later fenced reclaim, may reuse the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Attestation {
    KnownIdle,
    Unknown,
    Drained,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Reserve,
    BeginWrite,
    Commit,
    BeginEvict,
    FinishEvict,
    WriterCancel(Attestation),
    WriterTimeout,
    ReaderPin,
    ReaderUnpin(Attestation),
    ReaderLeaseExpire,
    ChecksumRejected,
    /// Stored bytes failed a read-time checksum. Hide them and hold the buffer.
    CorruptHold,
    Fence,
    /// Drop `count` stuck reader refs after the store epoch has moved.
    DropFencedRefs {
        count: u32,
    },
    ReclaimFenced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    /// The buffer may be dropped and its registration freed.
    ReleaseBuffer,
    /// The buffer must stay allocated and must not be handed to a new request.
    HoldBuffer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lifecycle {
    pub state: BlockState,
    pub registered: bool,
    pub refs: u32,
    pub completion: Completion,
    pub reuse_blocked: bool,
    pub fenced: bool,
}

impl Lifecycle {
    pub fn absent() -> Self {
        Self {
            state: BlockState::Absent,
            registered: false,
            refs: 0,
            completion: Completion::NotStarted,
            reuse_blocked: false,
            fenced: false,
        }
    }

    /// Hit visibility. Quarantine, eviction, and partial writes are never hits.
    pub fn is_hit_visible(&self) -> bool {
        self.state == BlockState::Committed && self.completion == Completion::Completed
    }

    pub fn apply(&mut self, event: Event) -> Result<Effect, CacheError> {
        let name = event_name(event);
        match event {
            Event::Reserve => self.reserve(name),
            Event::BeginWrite => self.begin_write(name),
            Event::Commit => self.commit(name),
            Event::BeginEvict => self.begin_evict(name),
            Event::FinishEvict => self.finish_evict(),
            Event::WriterCancel(att) => self.writer_cancel(att),
            Event::WriterTimeout => self.writer_timeout(),
            Event::ReaderPin => self.reader_pin(name),
            Event::ReaderUnpin(att) => self.reader_unpin(att),
            Event::ReaderLeaseExpire => self.reader_lease_expire(),
            Event::ChecksumRejected => self.checksum_rejected(),
            Event::CorruptHold => self.corrupt_hold(),
            Event::Fence => self.fence(),
            Event::DropFencedRefs { count } => self.drop_fenced_refs(count),
            Event::ReclaimFenced => self.reclaim_fenced(),
        }
    }

    fn reserve(&mut self, event: &'static str) -> Result<Effect, CacheError> {
        self.require(BlockState::Absent, event)?;
        if self.refs != 0 || self.registered {
            return Err(CacheError::NotReclaimable { from: self.state });
        }
        self.state = BlockState::Reserved;
        self.completion = Completion::NotStarted;
        self.reuse_blocked = false;
        self.fenced = false;
        Ok(Effect::None)
    }

    fn begin_write(&mut self, event: &'static str) -> Result<Effect, CacheError> {
        self.require(BlockState::Reserved, event)?;
        if self.registered {
            return Err(CacheError::InvalidTransition {
                from: self.state,
                event,
            });
        }
        self.state = BlockState::Writing;
        self.registered = true;
        self.completion = Completion::InFlight;
        Ok(Effect::HoldBuffer)
    }

    fn commit(&mut self, event: &'static str) -> Result<Effect, CacheError> {
        self.require(BlockState::Writing, event)?;
        if !self.registered || self.completion != Completion::InFlight {
            return Err(CacheError::InvalidTransition {
                from: self.state,
                event,
            });
        }
        self.state = BlockState::Committed;
        self.completion = Completion::Completed;
        Ok(Effect::None)
    }

    fn corrupt_hold(&mut self) -> Result<Effect, CacheError> {
        if !matches!(self.state, BlockState::Committed | BlockState::Evicting) {
            return Err(CacheError::InvalidTransition {
                from: self.state,
                event: "corrupt_hold",
            });
        }
        self.enter_quarantine(Completion::Unknown, self.fenced)
    }

    fn checksum_rejected(&mut self) -> Result<Effect, CacheError> {
        self.require(BlockState::Writing, "checksum_rejected")?;
        // The bytes were fully observed and rejected. Completion is known,
        // so the buffer is reclaimable and must not become readable.
        self.clear_to_absent(Completion::Completed);
        Ok(Effect::ReleaseBuffer)
    }

    fn begin_evict(&mut self, event: &'static str) -> Result<Effect, CacheError> {
        self.require(BlockState::Committed, event)?;
        self.state = BlockState::Evicting;
        Ok(Effect::None)
    }

    fn finish_evict(&mut self) -> Result<Effect, CacheError> {
        if self.state != BlockState::Evicting {
            return Err(CacheError::InvalidTransition {
                from: self.state,
                event: "finish_evict",
            });
        }
        if self.refs != 0 {
            return Err(CacheError::InUse { refs: self.refs });
        }
        if self.reuse_blocked || self.completion == Completion::Unknown {
            return Err(CacheError::NotReclaimable { from: self.state });
        }
        self.clear_to_absent(Completion::Completed);
        Ok(Effect::ReleaseBuffer)
    }

    fn writer_timeout(&mut self) -> Result<Effect, CacheError> {
        match self.state {
            BlockState::Reserved if !self.registered => {
                self.clear_to_absent(Completion::NotStarted);
                Ok(Effect::ReleaseBuffer)
            }
            BlockState::Writing => self.enter_quarantine(Completion::Unknown, false),
            other => Err(CacheError::InvalidTransition {
                from: other,
                event: "writer_timeout",
            }),
        }
    }

    fn writer_cancel(&mut self, attestation: Attestation) -> Result<Effect, CacheError> {
        match (self.state, attestation) {
            (BlockState::Reserved, _) if !self.registered => {
                self.clear_to_absent(Completion::NotStarted);
                Ok(Effect::ReleaseBuffer)
            }
            (BlockState::Writing, Attestation::Drained) => {
                self.clear_to_absent(Completion::Completed);
                Ok(Effect::ReleaseBuffer)
            }
            (BlockState::Writing, Attestation::Unknown | Attestation::KnownIdle) => {
                self.enter_quarantine(Completion::Unknown, false)
            }
            (BlockState::Quarantined, Attestation::Drained) if self.refs == 0 => {
                self.clear_to_absent(Completion::Completed);
                Ok(Effect::ReleaseBuffer)
            }
            (state, _) => Err(CacheError::InvalidTransition {
                from: state,
                event: "writer_cancel",
            }),
        }
    }

    fn reader_pin(&mut self, event: &'static str) -> Result<Effect, CacheError> {
        self.require(BlockState::Committed, event)?;
        self.refs = self
            .refs
            .checked_add(1)
            .ok_or(CacheError::InUse { refs: u32::MAX })?;
        Ok(Effect::HoldBuffer)
    }

    fn reader_unpin(&mut self, attestation: Attestation) -> Result<Effect, CacheError> {
        if !matches!(self.state, BlockState::Committed | BlockState::Evicting) || self.refs == 0 {
            return Err(CacheError::InvalidTransition {
                from: self.state,
                event: "reader_unpin",
            });
        }
        match attestation {
            Attestation::Drained => {
                self.refs -= 1;
                if self.refs == 0 && !self.reuse_blocked {
                    Ok(Effect::None)
                } else {
                    Ok(Effect::HoldBuffer)
                }
            }
            Attestation::Unknown | Attestation::KnownIdle => {
                // Losing track of a reader is not permission to recycle the buffer.
                // Checksummed committed bytes stay readable to other callers.
                self.reuse_blocked = true;
                Ok(Effect::HoldBuffer)
            }
        }
    }

    fn reader_lease_expire(&mut self) -> Result<Effect, CacheError> {
        if !matches!(self.state, BlockState::Committed | BlockState::Evicting) {
            return Err(CacheError::InvalidTransition {
                from: self.state,
                event: "reader_lease_expire",
            });
        }
        if self.refs == 0 {
            return Ok(Effect::None);
        }
        // Expiry does not drop the ref and does not prove DMA stopped.
        self.reuse_blocked = true;
        Ok(Effect::HoldBuffer)
    }

    fn fence(&mut self) -> Result<Effect, CacheError> {
        match self.state {
            BlockState::Reserved if !self.registered => {
                self.clear_to_absent(Completion::NotStarted);
                Ok(Effect::ReleaseBuffer)
            }
            BlockState::Writing => self.enter_quarantine(Completion::Unknown, true),
            BlockState::Quarantined => {
                self.fenced = true;
                self.reuse_blocked = true;
                Ok(Effect::HoldBuffer)
            }
            BlockState::Evicting => {
                self.fenced = true;
                Ok(Effect::HoldBuffer)
            }
            BlockState::Committed | BlockState::Absent => {
                self.fenced = true;
                Ok(Effect::None)
            }
            BlockState::Reserved => self.enter_quarantine(Completion::Unknown, true),
        }
    }

    fn drop_fenced_refs(&mut self, count: u32) -> Result<Effect, CacheError> {
        if !self.fenced {
            return Err(CacheError::FencingRejected {
                reason: "epoch has not moved",
            });
        }
        if count > self.refs {
            return Err(CacheError::Malformed {
                reason: "stuck ref count",
            });
        }
        self.refs -= count;
        if self.refs == 0 {
            self.reuse_blocked = false;
            if self.state == BlockState::Committed {
                self.completion = Completion::Completed;
            }
        }
        Ok(Effect::None)
    }

    fn reclaim_fenced(&mut self) -> Result<Effect, CacheError> {
        if !self.fenced {
            return Err(CacheError::FencingRejected {
                reason: "reclaim requires a newer epoch",
            });
        }
        if self.refs != 0 {
            return Err(CacheError::InUse { refs: self.refs });
        }
        match self.state {
            BlockState::Quarantined | BlockState::Evicting => {
                self.clear_to_absent(Completion::Completed);
                Ok(Effect::ReleaseBuffer)
            }
            other => Err(CacheError::NotReclaimable { from: other }),
        }
    }

    fn enter_quarantine(
        &mut self,
        completion: Completion,
        fenced: bool,
    ) -> Result<Effect, CacheError> {
        self.state = BlockState::Quarantined;
        self.completion = completion;
        self.reuse_blocked = true;
        self.fenced = fenced;
        self.registered = true;
        Ok(Effect::HoldBuffer)
    }

    fn clear_to_absent(&mut self, completion: Completion) {
        self.state = BlockState::Absent;
        self.registered = false;
        self.refs = 0;
        self.completion = completion;
        self.reuse_blocked = false;
        self.fenced = false;
    }

    fn require(&self, expected: BlockState, event: &'static str) -> Result<(), CacheError> {
        if self.state != expected {
            return Err(CacheError::InvalidTransition {
                from: self.state,
                event,
            });
        }
        Ok(())
    }
}

fn event_name(event: Event) -> &'static str {
    match event {
        Event::Reserve => "reserve",
        Event::BeginWrite => "begin_write",
        Event::Commit => "commit",
        Event::BeginEvict => "begin_evict",
        Event::FinishEvict => "finish_evict",
        Event::WriterCancel(_) => "writer_cancel",
        Event::WriterTimeout => "writer_timeout",
        Event::ReaderPin => "reader_pin",
        Event::ReaderUnpin(_) => "reader_unpin",
        Event::ReaderLeaseExpire => "reader_lease_expire",
        Event::ChecksumRejected => "checksum_rejected",
        Event::CorruptHold => "corrupt_hold",
        Event::Fence => "fence",
        Event::DropFencedRefs { .. } => "drop_fenced_refs",
        Event::ReclaimFenced => "reclaim_fenced",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(events: &[Event]) -> Result<Lifecycle, CacheError> {
        let mut life = Lifecycle::absent();
        for ev in events {
            life.apply(*ev)?;
        }
        Ok(life)
    }

    #[test]
    fn happy_path_passes_through_every_published_state() {
        let mut life = Lifecycle::absent();
        assert_eq!(life.apply(Event::Reserve).unwrap(), Effect::None);
        assert_eq!(life.state, BlockState::Reserved);
        assert!(!life.is_hit_visible());
        assert_eq!(life.apply(Event::BeginWrite).unwrap(), Effect::HoldBuffer);
        assert_eq!(life.state, BlockState::Writing);
        assert!(!life.is_hit_visible());
        life.apply(Event::Commit).unwrap();
        assert!(life.is_hit_visible());
        life.apply(Event::BeginEvict).unwrap();
        assert_eq!(life.state, BlockState::Evicting);
        assert!(!life.is_hit_visible());
        assert_eq!(
            life.apply(Event::FinishEvict).unwrap(),
            Effect::ReleaseBuffer
        );
        assert_eq!(life.state, BlockState::Absent);
    }

    #[test]
    fn commit_is_rejected_before_write_and_after_commit() {
        let mut life = Lifecycle::absent();
        assert!(life.apply(Event::Commit).is_err());
        life.apply(Event::Reserve).unwrap();
        assert!(matches!(
            life.apply(Event::Commit),
            Err(CacheError::InvalidTransition { .. })
        ));
        life.apply(Event::BeginWrite).unwrap();
        life.apply(Event::Commit).unwrap();
        assert!(life.apply(Event::Commit).is_err());
        assert!(life.apply(Event::Reserve).is_err());
    }

    #[test]
    fn timeout_on_a_write_quarantines_instead_of_freeing() {
        let mut life = drive(&[Event::Reserve, Event::BeginWrite, Event::WriterTimeout]).unwrap();
        assert_eq!(life.state, BlockState::Quarantined);
        assert!(life.reuse_blocked);
        assert_eq!(life.completion, Completion::Unknown);
        assert!(!life.is_hit_visible());
        assert!(life.apply(Event::ReclaimFenced).is_err());
    }

    #[test]
    fn known_idle_does_not_reclaim_a_registered_write() {
        let life = drive(&[
            Event::Reserve,
            Event::BeginWrite,
            Event::WriterCancel(Attestation::KnownIdle),
        ])
        .unwrap();
        assert_eq!(life.state, BlockState::Quarantined);
        assert_eq!(
            drive(&[
                Event::Reserve,
                Event::BeginWrite,
                Event::WriterCancel(Attestation::Unknown),
            ])
            .unwrap()
            .state,
            BlockState::Quarantined
        );
    }

    #[test]
    fn drained_owner_can_reclaim_and_unregistered_timeout_frees() {
        let drained = drive(&[
            Event::Reserve,
            Event::BeginWrite,
            Event::WriterCancel(Attestation::Drained),
        ])
        .unwrap();
        assert_eq!(drained.state, BlockState::Absent);
        assert!(!drained.registered);
        let reserved = drive(&[Event::Reserve, Event::WriterTimeout]).unwrap();
        assert_eq!(reserved.state, BlockState::Absent);
    }

    #[test]
    fn fenced_reclaim_requires_the_fence_bit_and_zero_refs() {
        let mut life = drive(&[Event::Reserve, Event::BeginWrite, Event::Fence]).unwrap();
        assert!(life.fenced);
        assert_eq!(life.state, BlockState::Quarantined);
        assert_eq!(
            life.apply(Event::ReclaimFenced).unwrap(),
            Effect::ReleaseBuffer
        );
        assert_eq!(life.state, BlockState::Absent);
    }

    #[test]
    fn reader_lease_expiry_blocks_reuse_until_fence_drops_the_ref() {
        let mut life = drive(&[
            Event::Reserve,
            Event::BeginWrite,
            Event::Commit,
            Event::ReaderPin,
        ])
        .unwrap();
        life.apply(Event::ReaderLeaseExpire).unwrap();
        life.apply(Event::BeginEvict).unwrap();
        assert!(matches!(
            life.apply(Event::FinishEvict),
            Err(CacheError::InUse { .. } | CacheError::NotReclaimable { .. })
        ));
        assert!(life.apply(Event::DropFencedRefs { count: 1 }).is_err());
        life.apply(Event::Fence).unwrap();
        life.apply(Event::DropFencedRefs { count: 1 }).unwrap();
        assert_eq!(life.refs, 0);
        assert_eq!(
            life.apply(Event::ReclaimFenced).unwrap(),
            Effect::ReleaseBuffer
        );
        assert_eq!(life.state, BlockState::Absent);
    }

    #[test]
    fn checksum_rejection_frees_without_exposing_the_block() {
        let life = drive(&[Event::Reserve, Event::BeginWrite, Event::ChecksumRejected]).unwrap();
        assert_eq!(life.state, BlockState::Absent);
        assert!(!life.is_hit_visible());
    }

    #[test]
    fn absent_to_committed_is_impossible_in_one_event() {
        let mut life = Lifecycle::absent();
        assert!(life.apply(Event::Commit).is_err());
        assert!(life.apply(Event::BeginEvict).is_err());
        assert!(life.apply(Event::FinishEvict).is_err());
        assert_eq!(life.state, BlockState::Absent);
    }
}
