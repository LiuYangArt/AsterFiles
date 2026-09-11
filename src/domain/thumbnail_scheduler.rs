use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use super::{EntryId, RequestId, TabId};

pub const THUMBNAIL_QUEUE_CAPACITY: usize = 96;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThumbnailKey {
    pub tab_id: TabId,
    pub request_id: RequestId,
    pub entry_id: EntryId,
    pub path: PathBuf,
    pub requested_px: u32,
}

#[derive(Debug, Default)]
pub struct ThumbnailPlan {
    generation: u64,
    wanted: HashSet<ThumbnailKey>,
    pending: VecDeque<ThumbnailKey>,
    in_flight: HashSet<ThumbnailKey>,
}

impl ThumbnailPlan {
    pub fn replace(&mut self, wanted: impl IntoIterator<Item = ThumbnailKey>) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        let mut next = VecDeque::new();
        let mut seen = HashSet::new();
        for key in wanted.into_iter().take(THUMBNAIL_QUEUE_CAPACITY) {
            if seen.insert(key.clone()) && !self.in_flight.contains(&key) {
                next.push_back(key);
            }
        }
        self.wanted = seen;
        self.pending = next;
        self.generation
    }

    pub fn next(&mut self) -> Option<(u64, ThumbnailKey)> {
        let key = self.pending.pop_front()?;
        self.in_flight.insert(key.clone());
        Some((self.generation, key))
    }

    pub fn finish(&mut self, key: &ThumbnailKey) -> bool {
        self.in_flight.remove(key);
        self.wanted.contains(key)
    }

    pub fn accepts(&self, key: &ThumbnailKey) -> bool {
        self.wanted.contains(key)
    }

    pub fn wanted_keys(&self) -> &HashSet<ThumbnailKey> {
        &self.wanted
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }
}

#[derive(Debug, Default)]
pub struct ThumbnailPlans {
    plans: HashMap<TabId, ThumbnailPlan>,
    preferred_tab: Option<TabId>,
}

impl ThumbnailPlans {
    pub fn replace(
        &mut self,
        tab_id: TabId,
        wanted: impl IntoIterator<Item = ThumbnailKey>,
    ) -> u64 {
        self.preferred_tab = Some(tab_id);
        self.plans.entry(tab_id).or_default().replace(wanted)
    }

    pub fn next_any(&mut self) -> Option<(u64, ThumbnailKey)> {
        if let Some(tab_id) = self.preferred_tab
            && let Some(request) = self.plans.get_mut(&tab_id).and_then(ThumbnailPlan::next)
        {
            return Some(request);
        }
        self.plans.values_mut().find_map(ThumbnailPlan::next)
    }

    pub fn finish(&mut self, key: &ThumbnailKey) -> bool {
        self.plans
            .get_mut(&key.tab_id)
            .is_some_and(|plan| plan.finish(key))
    }

    pub fn accepts(&self, key: &ThumbnailKey) -> bool {
        self.plans
            .get(&key.tab_id)
            .is_some_and(|plan| plan.accepts(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(index: u32) -> ThumbnailKey {
        ThumbnailKey {
            tab_id: TabId(1),
            request_id: RequestId(7),
            entry_id: EntryId(index),
            path: PathBuf::from(format!(r"C:\photos\{index}.png")),
            requested_px: 256,
        }
    }

    #[test]
    fn replacement_is_bounded_and_discards_old_pending_work() {
        let mut plan = ThumbnailPlan::default();
        let first = plan.replace((0..200).map(key));
        assert_eq!(plan.pending_len(), THUMBNAIL_QUEUE_CAPACITY);
        let (_, in_flight) = plan.next().unwrap();

        let second = plan.replace((1_000..1_020).map(key));
        assert_ne!(first, second);
        assert_eq!(plan.pending_len(), 20);
        assert!(!plan.accepts(&in_flight));
        assert!(plan.wanted_keys().contains(&key(1_000)));
    }

    #[test]
    fn only_the_current_visible_generation_accepts_results() {
        let mut plan = ThumbnailPlan::default();
        plan.replace([key(1), key(2)]);
        let (_, first) = plan.next().unwrap();
        assert!(plan.finish(&first));

        plan.replace([key(2)]);
        assert!(!plan.finish(&key(1)));
        assert!(plan.accepts(&key(2)));
    }

    #[test]
    fn visible_in_flight_work_survives_repeated_viewport_refreshes() {
        let mut plan = ThumbnailPlan::default();
        plan.replace((0..24).map(key));
        let (_, first) = plan.next().unwrap();
        let (_, second) = plan.next().unwrap();

        plan.replace((0..24).map(key));

        assert!(plan.accepts(&first));
        assert!(plan.accepts(&second));
        assert!(plan.finish(&first));
        assert!(plan.finish(&second));
        assert_eq!(plan.pending_len(), 22);
        assert_eq!(plan.in_flight_len(), 0);
    }

    #[test]
    fn newest_tab_plan_has_priority_over_background_work() {
        let mut plans = ThumbnailPlans::default();
        plans.replace(TabId(1), [key(1)]);
        let preferred = ThumbnailKey {
            tab_id: TabId(2),
            ..key(2)
        };
        plans.replace(TabId(2), [preferred.clone()]);

        assert_eq!(plans.next_any().map(|(_, key)| key), Some(preferred));
    }
}
