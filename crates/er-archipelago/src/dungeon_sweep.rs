//! Dungeon sweep: beating a dungeon's boss sends every location inside that
//! dungeon at once.
//!
//! The server's slot data (`options.dungeon_sweep`) decides whether it's on to
//! begin with. The player can override that mid run with `!sweep`, see
//! [DungeonSweep::set_override].
//!
//! Whether sweeps are announced in chat is a separate switch, toggled with
//! `!uisweep`, see [DungeonSweep::toggle_announce]. Sweeping itself still
//! happens while announcements are off. Only the most recently swept dungeon
//! is announced, see [DungeonSweep::to_announce].
//!
//! No dungeon list is needed from the server. Dungeon maps keep their event
//! flags together: a flag `AABBxxxx` belongs to map `mAA_BB`, and the main
//! boss's defeat flag is `AABB0800`. So the locations of a dungeon are the
//! ones whose flags share its `AABB` prefix, and the dungeon counts as cleared
//! when that boss flag is set. Locations are grouped once from the same
//! location -> flags map `checks.rs` builds.
//!
//! Newly swept locations go into the checked set. The core loop already
//! reports that set to the server and grants local items for new entries.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::slot_data::EventFlagId;

/// How often boss flags are looked at. A clear isn't urgent, and this keeps
/// the per-frame cost at nothing.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Map areas that are swept: catacombs, caves, tunnels and divine towers.
/// Legacy dungeons and the overworld are left alone.
const DUNGEON_AREAS: [u32; 4] = [30, 31, 32, 34];

/// Added to `AABB0000` to get the dungeon boss's defeat flag.
const BOSS_FLAG_OFFSET: u32 = 800;

/// Real names for swept dungeons, keyed by `AABB`. Anything not listed falls
/// back to the map id (like `m30_02`), so a gap here never breaks anything.
const DUNGEON_NAMES: &[(u32, &str)] = &[
    // Catacombs and hero's graves
    (3000, "Tombsward Catacombs"),
    (3001, "Impaler's Catacombs"),
    (3002, "Stormfoot Catacombs"),
    (3003, "Road's End Catacombs"),
    (3004, "Murkwater Catacombs"),
    (3005, "Black Knife Catacombs"),
    (3006, "Cliffbottom Catacombs"),
    (3007, "Wyndham Catacombs"),
    (3008, "Sainted Hero's Grave"),
    (3009, "Gelmir Hero's Grave"),
    (3010, "Auriza Hero's Grave"),
    (3011, "Deathtouched Catacombs"),
    (3012, "Unsightly Catacombs"),
    (3013, "Auriza Side Tomb"),
    (3014, "Minor Erdtree Catacombs"),
    (3015, "Caelid Catacombs"),
    (3016, "War-Dead Catacombs"),
    (3017, "Giant-Conquering Hero's Grave"),
    (3018, "Giants' Mountaintop Catacombs"),
    (3019, "Consecrated Snowfield Catacombs"),
    // Caves
    (3100, "Coastal Cave"),
    (3101, "Groveside Cave"),
    (3102, "Murkwater Cave"),
    (3103, "Stillwater Cave"),
    (3104, "Lakeside Crystal Cave"),
    (3105, "Academy Crystal Cave"),
    (3106, "Seethewater Cave"),
    (3107, "Volcano Cave"),
    (3109, "Dragonbarrow Cave"),
    (3110, "Sellia Hideaway"),
    (3111, "Cave of the Forlorn"),
    // Tunnels
    (3200, "Limgrave Tunnels"),
    (3201, "Raya Lucaria Crystal Tunnel"),
    (3202, "Old Altus Tunnel"),
    (3204, "Altus Tunnel"),
    (3205, "Gael Tunnel"),
    (3207, "Sellia Crystal Tunnel"),
    (3208, "Yelough Anix Tunnel"),
];

/// The `AABB` of a flag, if it lies in a swept dungeon map.
fn dungeon_of_flag(flag: EventFlagId) -> Option<u32> {
    let dungeon = flag.0 / 10_000;
    DUNGEON_AREAS.contains(&(dungeon / 100)).then_some(dungeon)
}

/// The flag set when the boss of dungeon `dungeon` (its `AABB`) is beaten.
fn boss_flag(dungeon: u32) -> EventFlagId {
    EventFlagId(dungeon * 10_000 + BOSS_FLAG_OFFSET)
}

/// Dungeon name for chat, like `Stormfoot Catacombs`. Dungeons without a
/// known name show their map id instead, like `m30_02`.
fn dungeon_name(dungeon: u32) -> String {
    DUNGEON_NAMES
        .iter()
        .find(|&&(id, _)| id == dungeon)
        .map(|&(_, name)| name.to_string())
        .unwrap_or_else(|| format!("m{}_{:02}", dungeon / 100, dungeon % 100))
}

/// A dungeon that was swept, for the caller to announce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepResult {
    pub name: String,
    /// How many of the dungeon's locations weren't checked yet.
    pub newly_checked: usize,
}

/// Session state for dungeon sweeping.
#[derive(Debug, Default)]
pub struct DungeonSweep {
    /// Dungeon `AABB` to its locations. `None` until built.
    groups: Option<HashMap<u32, Vec<i64>>>,
    /// Dungeons already swept this session.
    swept: HashSet<u32>,
    last_poll: Option<Instant>,
    /// What the player chose with `!sweep`, if anything. Wins over the server's
    /// setting until the game is restarted.
    enabled_override: Option<bool>,
    /// Set by `!uisweep`. When true, sweeps still happen but aren't announced.
    /// Stored as "muted" so the default (announce) needs no setup.
    announce_muted: bool,
}

impl DungeonSweep {
    /// Whether sweeping is on, given the server's setting.
    pub fn is_enabled(&self, server_enabled: bool) -> bool {
        self.enabled_override.unwrap_or(server_enabled)
    }

    /// Turns sweeping on or off regardless of the server's setting. Dungeons
    /// whose boss was beaten while it was off are swept once it's turned on.
    pub fn set_override(&mut self, enabled: bool) {
        self.enabled_override = Some(enabled);
    }

    /// Whether sweep messages are shown.
    pub fn announces(&self) -> bool {
        !self.announce_muted
    }

    /// Flips sweep messages on or off, for `!uisweep`. Returns the new state
    /// (`true` means messages are shown).
    pub fn toggle_announce(&mut self) -> bool {
        self.announce_muted = !self.announce_muted;
        self.announces()
    }

    /// Picks the one result worth announcing: the most recently swept dungeon,
    /// or nothing if sweep messages are off. Results from [DungeonSweep::poll]
    /// are ordered by dungeon id, so when several are swept in one poll (for
    /// example when sweeping is turned on late) the pick is stable.
    pub fn to_announce<'a>(&self, results: &'a [SweepResult]) -> Option<&'a SweepResult> {
        if self.announce_muted {
            None
        } else {
            results.last()
        }
    }

    /// Whether [DungeonSweep::build_groups] still needs to be called.
    pub fn needs_groups(&self) -> bool {
        self.groups.is_none()
    }

    /// Groups locations by the dungeon their flags are in. Run once the
    /// location -> flags map is available.
    pub fn build_groups(&mut self, location_flags: &HashMap<i64, Vec<EventFlagId>>) {
        let mut groups: HashMap<u32, HashSet<i64>> = HashMap::new();
        for (&location, flags) in location_flags {
            for dungeon in flags.iter().filter_map(|&flag| dungeon_of_flag(flag)) {
                groups.entry(dungeon).or_default().insert(location);
            }
        }
        self.groups = Some(
            groups
                .into_iter()
                .map(|(dungeon, locations)| (dungeon, locations.into_iter().collect()))
                .collect(),
        );
    }

    /// Forgets which dungeons were swept. Call this when a different save file
    /// is loaded, so its beaten bosses get swept too.
    pub fn reset(&mut self) {
        self.swept.clear();
        self.last_poll = None;
    }

    /// Sweeps every dungeon whose boss is newly beaten into `checked` and
    /// returns what was swept, ordered by dungeon id. Rate limited, so it's
    /// cheap to call every frame.
    ///
    /// `get_flag` returns `None` when flags can't be read right now, in which
    /// case nothing is swept.
    pub fn poll(
        &mut self,
        enabled: bool,
        checked: &mut HashSet<i64>,
        get_flag: impl Fn(EventFlagId) -> Option<bool>,
    ) -> Vec<SweepResult> {
        let Some(groups) = &self.groups else {
            return Vec::new();
        };
        if !enabled {
            return Vec::new();
        }
        let now = Instant::now();
        if let Some(last) = self.last_poll {
            if now.duration_since(last) < POLL_INTERVAL {
                return Vec::new();
            }
        }
        self.last_poll = Some(now);

        // Walk dungeons in id order so the result order is stable.
        let mut dungeons: Vec<_> = groups.iter().collect();
        dungeons.sort_by_key(|(dungeon, _)| **dungeon);

        let mut results = Vec::new();
        for (&dungeon, locations) in dungeons {
            if self.swept.contains(&dungeon) {
                continue;
            }
            // A flag that can't be read yet means "try again later", not "no".
            if !get_flag(boss_flag(dungeon)).unwrap_or(false) {
                continue;
            }
            let newly_checked = locations
                .iter()
                .filter(|&&location| checked.insert(location))
                .count();
            self.swept.insert(dungeon);
            results.push(SweepResult {
                name: dungeon_name(dungeon),
                newly_checked,
            });
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location_flags() -> HashMap<i64, Vec<EventFlagId>> {
        HashMap::from([
            (1, vec![EventFlagId(30_020_800)]),                  // boss drop, m30_02
            (2, vec![EventFlagId(30_027_500)]),                  // pickup, m30_02
            (3, vec![EventFlagId(31_007_100), EventFlagId(99)]), // m31_00
            (4, vec![EventFlagId(10_007_800)]),                  // legacy dungeon, ignored
            (5, vec![EventFlagId(1_043_362_000)]),               // overworld, ignored
        ])
    }

    fn flags(set: &[u32]) -> impl Fn(EventFlagId) -> Option<bool> {
        let set: HashSet<u32> = set.iter().copied().collect();
        move |f| Some(set.contains(&f.0))
    }

    fn built() -> DungeonSweep {
        let mut sweep = DungeonSweep::default();
        sweep.build_groups(&location_flags());
        sweep
    }

    #[test]
    fn flag_math() {
        assert_eq!(dungeon_of_flag(EventFlagId(30_027_500)), Some(3002));
        assert_eq!(dungeon_of_flag(EventFlagId(10_007_800)), None);
        assert_eq!(boss_flag(3002), EventFlagId(30_020_800));
    }

    #[test]
    fn names_are_real_with_a_map_id_fallback() {
        assert_eq!(dungeon_name(3002), "Stormfoot Catacombs");
        assert_eq!(dungeon_name(3099), "m30_99");
    }

    #[test]
    fn sweeps_the_dungeon_whose_boss_is_beaten() {
        let mut sweep = built();
        let mut checked = HashSet::from([1]);
        let out = sweep.poll(true, &mut checked, flags(&[30_020_800]));
        assert_eq!(
            out,
            vec![SweepResult { name: "Stormfoot Catacombs".into(), newly_checked: 1 }]
        );
        assert_eq!(checked, HashSet::from([1, 2]));
    }

    #[test]
    fn leaves_other_dungeons_and_unrelated_locations_alone() {
        let mut sweep = built();
        let mut checked = HashSet::new();
        sweep.poll(true, &mut checked, flags(&[30_020_800]));
        assert!(!checked.contains(&3) && !checked.contains(&4) && !checked.contains(&5));
    }

    #[test]
    fn does_nothing_when_disabled_or_boss_alive() {
        let mut sweep = built();
        let mut checked = HashSet::new();
        assert!(sweep.poll(false, &mut checked, flags(&[30_020_800])).is_empty());
        assert!(sweep.poll(true, &mut checked, flags(&[])).is_empty());
        assert!(checked.is_empty());
    }

    #[test]
    fn waits_for_groups_and_retries_unreadable_flags() {
        let mut sweep = DungeonSweep::default();
        let mut checked = HashSet::new();
        assert!(sweep.needs_groups());
        assert!(sweep.poll(true, &mut checked, flags(&[30_020_800])).is_empty());
        sweep.build_groups(&location_flags());
        assert!(sweep.poll(true, &mut checked, |_| None).is_empty());
        sweep.last_poll = None; // skip the rate limit
        assert_eq!(sweep.poll(true, &mut checked, flags(&[30_020_800])).len(), 1);
    }

    #[test]
    fn override_beats_the_server_setting() {
        let mut sweep = built();
        assert!(!sweep.is_enabled(false));
        assert!(sweep.is_enabled(true));
        sweep.set_override(true);
        assert!(sweep.is_enabled(false));
        sweep.set_override(false);
        assert!(!sweep.is_enabled(true));
    }

    #[test]
    fn enabling_later_sweeps_bosses_beaten_earlier() {
        let mut sweep = built();
        let mut checked = HashSet::new();
        assert!(sweep.poll(false, &mut checked, flags(&[30_020_800])).is_empty());
        sweep.last_poll = None;
        assert_eq!(sweep.poll(true, &mut checked, flags(&[30_020_800])).len(), 1);
    }

    #[test]
    fn swept_once_until_reset() {
        let mut sweep = built();
        let mut checked = HashSet::new();
        assert_eq!(sweep.poll(true, &mut checked, flags(&[30_020_800])).len(), 1);
        sweep.last_poll = None;
        assert!(sweep.poll(true, &mut checked, flags(&[30_020_800])).is_empty());
        sweep.reset();
        assert_eq!(sweep.poll(true, &mut checked, flags(&[30_020_800])).len(), 1);
    }

    #[test]
    fn uisweep_toggles_announcements_but_not_sweeping() {
        let mut sweep = built();
        assert!(sweep.announces());
        assert!(!sweep.toggle_announce());
        assert!(!sweep.announces());

        // Muted: still sweeps, nothing to announce.
        let mut checked = HashSet::new();
        let out = sweep.poll(true, &mut checked, flags(&[30_020_800]));
        assert_eq!(out.len(), 1);
        assert!(checked.contains(&2));
        assert!(sweep.to_announce(&out).is_none());

        assert!(sweep.toggle_announce());
        assert!(sweep.to_announce(&out).is_some());
    }

    #[test]
    fn only_the_most_recent_dungeon_is_announced() {
        let mut sweep = built();
        let mut checked = HashSet::new();
        // Both bosses already beaten, so one poll sweeps both dungeons.
        let out = sweep.poll(true, &mut checked, flags(&[30_020_800, 31_000_800]));
        assert_eq!(out.len(), 2);
        let announced = sweep.to_announce(&out).unwrap();
        assert_eq!(announced.name, "Coastal Cave");
        assert_eq!(announced, out.last().unwrap());
    }
}
