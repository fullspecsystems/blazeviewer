//! Cache-residency planning.
//!
//! Given what is resident now, the prioritized targets we *want* resident, and a
//! capacity (bounded by VRAM budget), decide what to load and what to evict.
//! Keeping this pure isolates the eviction policy — exactly the kind of
//! architectural knob we want to A/B — from the GPU code that executes it.

use std::collections::HashSet;

/// The movements a cache should perform to reach the desired residency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidencyPlan {
    /// Items to load, highest priority first.
    pub to_load: Vec<usize>,
    /// Items to evict (order not significant).
    pub to_evict: Vec<usize>,
}

/// Plan cache movements.
///
/// Policy: keep the highest-priority `capacity` unique `targets`; evict any
/// resident item not in that set; load the desired items that aren't resident
/// yet, in priority order. The result never exceeds `capacity`.
pub fn plan_residency(resident: &[usize], targets: &[usize], capacity: usize) -> ResidencyPlan {
    // Desired = first `capacity` unique targets, in priority order.
    let mut desired: Vec<usize> = Vec::new();
    let mut desired_set: HashSet<usize> = HashSet::new();
    for &t in targets {
        if desired.len() >= capacity {
            break;
        }
        if desired_set.insert(t) {
            desired.push(t);
        }
    }

    let resident_set: HashSet<usize> = resident.iter().copied().collect();

    let to_evict: Vec<usize> = resident
        .iter()
        .copied()
        .filter(|i| !desired_set.contains(i))
        .collect();

    let to_load: Vec<usize> = desired
        .iter()
        .copied()
        .filter(|i| !resident_set.contains(i))
        .collect();

    ResidencyPlan { to_load, to_evict }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_targets_into_empty_cache() {
        let plan = plan_residency(&[], &[0, 1, 2], 4);
        assert_eq!(plan.to_load, vec![0, 1, 2]);
        assert!(plan.to_evict.is_empty());
    }

    #[test]
    fn truncates_to_capacity_in_priority_order() {
        let plan = plan_residency(&[], &[10, 11, 12, 13], 2);
        assert_eq!(plan.to_load, vec![10, 11]);
    }

    #[test]
    fn evicts_items_not_in_targets() {
        let plan = plan_residency(&[5, 6], &[0, 1], 4);
        let mut evicted = plan.to_evict.clone();
        evicted.sort();
        assert_eq!(evicted, vec![5, 6]);
        assert_eq!(plan.to_load, vec![0, 1]);
    }

    #[test]
    fn keeps_already_resident_desired_items() {
        let plan = plan_residency(&[0, 1, 2], &[0, 1], 5);
        assert!(plan.to_load.is_empty());
        assert_eq!(plan.to_evict, vec![2]);
    }

    #[test]
    fn zero_capacity_evicts_everything_loads_nothing() {
        let plan = plan_residency(&[0, 1, 2], &[0, 1, 2], 0);
        assert!(plan.to_load.is_empty());
        let mut evicted = plan.to_evict.clone();
        evicted.sort();
        assert_eq!(evicted, vec![0, 1, 2]);
    }

    #[test]
    fn deduplicates_targets() {
        let plan = plan_residency(&[], &[7, 7, 7, 8], 4);
        assert_eq!(plan.to_load, vec![7, 8]);
    }

    #[test]
    fn steady_state_is_a_noop() {
        let plan = plan_residency(&[0, 1, 2], &[0, 1, 2], 3);
        assert!(plan.to_load.is_empty());
        assert!(plan.to_evict.is_empty());
    }
}
