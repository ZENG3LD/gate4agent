//! Node-local exclusive **browser station profile lease** (Track A follow-on).
//!
//! Behind Cargo feature `dig2-station-probe`, after the cheap path probe succeeds
//! for a spawn that carries `browser_profile_id`. Dig2browser-protocol has **no**
//! Bind/Lease wire verb — this map is g4a node bookkeeping only:
//!
//! - one active holder per opaque `browser_profile_id` (exclusive)
//! - cleared on session end / spawn rollback (never cookie / ImportSession flush
//!   over C2)
//! - capacity-bounded
//!
//! Linux / probe-unavailable: resolve still refuses before lease; unit tests of
//! this map run on all OS. Never print `GATE4AGENT_NODE_TOKEN` or cookie bytes.
//! Plan sketch: `dig2-station-bind-lease-sketch-2026-10-02.md`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use gate4agent_node_protocol::SpawnBrowserProfileId;
use gate4agent_types::AgentInstanceId;

/// Hard cap on concurrent node-local browser-station profile leases.
pub const MAX_BROWSER_STATION_LEASES: usize = 64;

/// One exclusive lease of a dig2browser station profile id by a session holder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowserStationLease {
    pub profile_id: SpawnBrowserProfileId,
    pub holder: AgentInstanceId,
    pub acquired_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserStationLeaseError {
    /// Another live session already holds this profile id.
    ProfileBusy { holder: AgentInstanceId },
    /// Map is at [`MAX_BROWSER_STATION_LEASES`].
    CapacityExceeded,
    /// This holder already has a lease (one profile per session).
    HolderAlreadyLeased,
}

/// Capacity-bounded exclusive lease table (profile id ↔ session holder).
#[derive(Clone, Debug, Default)]
pub struct BrowserStationLeaseMap {
    by_profile: BTreeMap<String, BrowserStationLease>,
    by_holder: BTreeMap<u64, String>,
}

impl BrowserStationLeaseMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.by_profile.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_profile.is_empty()
    }

    pub fn get_by_profile(&self, profile_id: &SpawnBrowserProfileId) -> Option<&BrowserStationLease> {
        self.by_profile.get(profile_id.as_str())
    }

    pub fn get_by_holder(&self, holder: AgentInstanceId) -> Option<&BrowserStationLease> {
        let key = self.by_holder.get(&holder.0)?;
        self.by_profile.get(key)
    }

    /// Insert exclusive lease. Refuses busy profile / duplicate holder / capacity.
    pub fn try_acquire(
        &mut self,
        profile_id: SpawnBrowserProfileId,
        holder: AgentInstanceId,
        acquired_at_unix_ms: u64,
    ) -> Result<(), BrowserStationLeaseError> {
        if self.by_holder.contains_key(&holder.0) {
            return Err(BrowserStationLeaseError::HolderAlreadyLeased);
        }
        if let Some(existing) = self.by_profile.get(profile_id.as_str()) {
            return Err(BrowserStationLeaseError::ProfileBusy {
                holder: existing.holder,
            });
        }
        if self.by_profile.len() >= MAX_BROWSER_STATION_LEASES {
            return Err(BrowserStationLeaseError::CapacityExceeded);
        }
        let key = profile_id.as_str().to_owned();
        self.by_holder.insert(holder.0, key.clone());
        self.by_profile.insert(
            key,
            BrowserStationLease {
                profile_id,
                holder,
                acquired_at_unix_ms,
            },
        );
        Ok(())
    }

    /// Drop lease for holder if present. Idempotent; returns whether a lease was removed.
    pub fn release_holder(&mut self, holder: AgentInstanceId) -> bool {
        let Some(key) = self.by_holder.remove(&holder.0) else {
            return false;
        };
        self.by_profile.remove(&key).is_some()
    }

    /// Drop lease for profile if present. Idempotent.
    pub fn release_profile(&mut self, profile_id: &SpawnBrowserProfileId) -> bool {
        let Some(lease) = self.by_profile.remove(profile_id.as_str()) else {
            return false;
        };
        self.by_holder.remove(&lease.holder.0);
        true
    }
}

/// Shared map handle stored on `NodeShared` (Arc so spawn guards can Drop-release).
pub type BrowserStationLeaseTable = Arc<Mutex<BrowserStationLeaseMap>>;

pub fn new_lease_table() -> BrowserStationLeaseTable {
    Arc::new(Mutex::new(BrowserStationLeaseMap::new()))
}

/// RAII guard: releases the holder's lease on Drop unless [`retain`](Self::retain).
#[derive(Debug)]
pub struct BrowserStationLeaseGuard {
    table: BrowserStationLeaseTable,
    holder: AgentInstanceId,
    armed: bool,
}

impl BrowserStationLeaseGuard {
    pub fn retain(mut self) {
        self.armed = false;
    }
}

impl Drop for BrowserStationLeaseGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut map = self
            .table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = map.release_holder(self.holder);
    }
}

/// Try to acquire; on success returns an armed guard (Drop clears unless retain).
pub fn acquire_lease(
    table: &BrowserStationLeaseTable,
    profile_id: SpawnBrowserProfileId,
    holder: AgentInstanceId,
    acquired_at_unix_ms: u64,
) -> Result<BrowserStationLeaseGuard, BrowserStationLeaseError> {
    {
        let mut map = table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.try_acquire(profile_id, holder, acquired_at_unix_ms)?;
    }
    Ok(BrowserStationLeaseGuard {
        table: Arc::clone(table),
        holder,
        armed: true,
    })
}

/// Idempotent clear by session holder (session end / remove_binding).
pub fn release_lease_for_holder(table: &BrowserStationLeaseTable, holder: AgentInstanceId) -> bool {
    let mut map = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.release_holder(holder)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str) -> SpawnBrowserProfileId {
        SpawnBrowserProfileId::new(id).unwrap()
    }

    #[test]
    fn exclusive_lease_refuses_second_holder() {
        let mut map = BrowserStationLeaseMap::new();
        map.try_acquire(profile("station-a"), AgentInstanceId(1), 100)
            .unwrap();
        let err = map
            .try_acquire(profile("station-a"), AgentInstanceId(2), 101)
            .unwrap_err();
        assert_eq!(
            err,
            BrowserStationLeaseError::ProfileBusy {
                holder: AgentInstanceId(1)
            }
        );
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn release_holder_clears_and_allows_reacquire() {
        let mut map = BrowserStationLeaseMap::new();
        map.try_acquire(profile("station-a"), AgentInstanceId(7), 1)
            .unwrap();
        assert!(map.release_holder(AgentInstanceId(7)));
        assert!(!map.release_holder(AgentInstanceId(7)));
        map.try_acquire(profile("station-a"), AgentInstanceId(8), 2)
            .unwrap();
        assert_eq!(
            map.get_by_profile(&profile("station-a"))
                .unwrap()
                .holder,
            AgentInstanceId(8)
        );
    }

    #[test]
    fn holder_may_not_hold_two_profiles() {
        let mut map = BrowserStationLeaseMap::new();
        map.try_acquire(profile("a"), AgentInstanceId(3), 1)
            .unwrap();
        assert_eq!(
            map.try_acquire(profile("b"), AgentInstanceId(3), 2)
                .unwrap_err(),
            BrowserStationLeaseError::HolderAlreadyLeased
        );
    }

    #[test]
    fn capacity_bound_refuses() {
        let mut map = BrowserStationLeaseMap::new();
        for i in 0..MAX_BROWSER_STATION_LEASES {
            map.try_acquire(
                profile(&format!("p{i}")),
                AgentInstanceId(i as u64 + 1),
                i as u64,
            )
            .unwrap();
        }
        assert_eq!(
            map.try_acquire(
                profile("overflow"),
                AgentInstanceId(9_001),
                0
            )
            .unwrap_err(),
            BrowserStationLeaseError::CapacityExceeded
        );
    }

    #[test]
    fn guard_drop_releases_unless_retained() {
        let table = new_lease_table();
        {
            let guard = acquire_lease(
                &table,
                profile("g1"),
                AgentInstanceId(11),
                42,
            )
            .unwrap();
            assert!(table
                .lock()
                .unwrap()
                .get_by_holder(AgentInstanceId(11))
                .is_some());
            drop(guard);
        }
        assert!(table.lock().unwrap().is_empty());

        let guard = acquire_lease(&table, profile("g2"), AgentInstanceId(12), 43).unwrap();
        guard.retain();
        assert_eq!(table.lock().unwrap().len(), 1);
        assert!(release_lease_for_holder(&table, AgentInstanceId(12)));
        assert!(table.lock().unwrap().is_empty());
    }

    #[test]
    fn release_profile_clears_holder_index() {
        let mut map = BrowserStationLeaseMap::new();
        map.try_acquire(profile("px"), AgentInstanceId(9), 1)
            .unwrap();
        assert!(map.release_profile(&profile("px")));
        assert!(map.is_empty());
        assert!(!map.release_profile(&profile("px")));
    }

    #[test]
    fn never_imports_session_shape_in_lease_api() {
        // Compile-time / API honesty: lease carries opaque id + holder only.
        let lease = BrowserStationLease {
            profile_id: profile("opaque-only"),
            holder: AgentInstanceId(1),
            acquired_at_unix_ms: 0,
        };
        let debug = format!("{lease:?}");
        assert!(!debug.to_ascii_lowercase().contains("cookie"));
        assert!(!debug.to_ascii_lowercase().contains("importsession"));
        assert!(!debug.contains("GATE4AGENT_NODE_TOKEN"));
    }
}
