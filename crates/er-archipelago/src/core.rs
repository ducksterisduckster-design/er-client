use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use archipelago_rs as ap;
use archipelago_rs::RichText;
use eldenring::cs::*;
use eldenring::param::EquipParamStructMut;
use fromsoftware_shared::FromStatic;
use log::*;
#[cfg(debug_assertions)]
use regex_macro::regex;

use crate::checks;
use crate::checks::{LocationFlagChanges, LocationFlagMapping};
use crate::item::{ItemIdExt, RegulationManager, passive_row, remove_sent_display_items};
use crate::save_data::*;
use crate::slot_data::{EventFlagId, I64Key, SlotData};
use shared::{Core as SharedCore, CoreBase};

const RECEIVED_ITEM_GRANT_INTERVAL: Duration = Duration::from_millis(33);
const ITEM_GET_DISPLAY_SUPPRESSION_DURATION: Duration = Duration::from_millis(75);

/// How many locations `poll_flag_based_checks` looks at per frame.
///
/// There are ~5000 flag-backed locations and reading a flag isn't free
/// (but is currently unnecessarily very inefficient). At this batch size
/// the whole table gets swept in ~1.3s at 60fps, which is acceptable for
/// anything missed by the flag hooks.
const FLAG_POLL_BATCH_SIZE: usize = 64;

/// How often the inventory is swept for placeholder items. The sweep walks
/// every carried item, and a placeholder can wait a few frames before it's
/// handled.
const INVENTORY_SCAN_INTERVAL: Duration = Duration::from_millis(100);

/// How often the virtual-location passes (expansion, local grants, foreign
/// notifications) run when no new location has been checked. They also run
/// right away whenever the checked set changes, so this is just a backstop for
/// state that changed elsewhere (like a late server connection).
const VIRTUAL_LOCATION_SYNC_INTERVAL: Duration = Duration::from_millis(250);

/// How long to wait for the server's answer to the location scout before asking
/// again. Without this, an answer that never arrives would block every
/// by-location grant for the whole session.
const LOCAL_ITEM_SCOUT_TIMEOUT: Duration = Duration::from_secs(15);

/// The Archipelago item name of the NG+ trap, matched case-insensitively.
const NG_PLUS_TRAP_ITEM_NAME: &str = "NG+ Trap";

/// Picks how many NG+ levels a trap raises the game by, from 1 to 7 (still
/// capped at NG+7 overall). Uses a lightweight PRNG rather than a `rand`
/// dependency, seeded from the current time.
fn random_ng_raise() -> u32 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static STATE: AtomicU64 = AtomicU64::new(0);

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mixed = STATE.fetch_add(seed | 1, Ordering::Relaxed) ^ seed;

    // xorshift64, then reduce to 1..=MAX_NG_LEVEL.
    let mut x = mixed;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    1 + (x % u64::from(MAX_NG_LEVEL)) as u32
}

/// The cause sent with, and shown for, a DeathLink when the local player dies.
const DEATH_LINK_CAUSE: &str = "Elden Ring died";

/// After a death is noticed, further deaths are ignored for this long. Death
/// is spotted through several signals that don't all change on the same frame,
/// and a real second death can't happen faster than the death screen and
/// respawn allow.
const DEATH_COOLDOWN: Duration = Duration::from_secs(10);

/// The highest NG+ cycle the game has.
const MAX_NG_LEVEL: u32 = 7;


/// Stored in `SaveData::local_virtual_items_granted` once a save has been
/// baselined for by-location grants. Never a real location ID.
const LOCAL_LOCATION_BASELINE_MARKER: i64 = -1;

/// Checks an outstanding location scout and returns the result once the server
/// has answered. Boxed so this file doesn't have to name the Archipelago
/// client's channel type.
type ScoutPoll = Box<dyn FnMut() -> Option<Result<Vec<ap::LocatedItem>, ap::Error>> + Send>;

/// The core of the Archipelago mod. Runs the non-UI game logic and talks to the
/// Archipelago client.
pub struct Core {
    /// The cross-game core.
    base: CoreBase<crate::game::EldenRing, SlotData>,

    /// When we last granted an item to the player. Used to throttle bursts of
    /// incoming items without making release queues take minutes to drain.
    last_item_time: Instant,

    /// How many locations we've sent to the server this session. Starts at 0 on
    /// every boot, so we resend anything that might have been missed.
    locations_sent: usize,

    /// Whether the player has reached their goal and we've told the server.
    /// Kept here instead of in the save data so it's resent on every launch, in
    /// case it got lost.
    sent_goal: bool,

    /// Item display param rows that are suppressed for incoming Archipelago
    /// grants and need to be restored later.
    item_get_display_restores: Vec<ItemGetDisplayRestore>,

    /// Two-way mapping between Archipelago locations and event flags, used for
    /// flag-based check detection (item lots and shop purchases that never
    /// touch the inventory). Built the first time the regulation is available.
    /// It doesn't change for the rest of the session.
    location_flag_mapping: Option<LocationFlagMapping>,

    /// Locations still waiting to be seen as checked for poll-based location
    /// checking, populated when `location_flag_mapping` is set. It is drained
    /// as locations are found.
    pending_flag_checks: Vec<i64>,

    /// How far through `pending_flag_checks` the current sweep has got.
    flag_poll_cursor: usize,

    /// Timers for the periodic sync passes, so they don't run every frame.
    last_inventory_scan: Option<Instant>,
    last_virtual_location_sync: Option<Instant>,

    /// How many locations were checked at the end of the last tick. Used to
    /// notice when a new one comes in.
    locations_seen: usize,

    /// Goods IDs in the Archipelago range that carry no location and aren't
    /// blank placeholders, so they stay in the inventory. Remembered so the
    /// sweep doesn't re-read the regulation (or re-log) for the same item on
    /// every pass, which would be ten times a second.
    untagged_item_ids: HashSet<u32>,

    /// Whether the local player was dead as of the last tick.
    was_dead: bool,

    /// The player's death count as of the last tick, or `None` while not in a
    /// game. One of the ways [Core::detect_death] notices a death.
    last_death_count: Option<u32>,

    /// [SaveData::menu_returns] as of the last death check, so we can tell
    /// when the death count we remember belongs to a different session.
    last_menu_returns: u32,

    /// When the last death was noticed. See [DEATH_COOLDOWN].
    last_death_at: Option<Instant>,

    /// The NG+ level a trap just restored, waiting to be announced once the
    /// player has respawned (the overlay only keeps a message for a few
    /// seconds, and the death and loading screens can use them all up).
    ng_trap_restored: Option<u32>,

    /// The pending request asking the server which item sits at each of this
    /// game's locations, and when we sent it.
    local_item_scout: Option<ScoutPoll>,
    local_item_scout_requested: Option<Instant>,

    /// Location ID to Archipelago item ID, for every location holding one of
    /// this player's *own* items. `None` until the server has answered the
    /// scout. Locations with another player's item aren't listed, so those stay
    /// placeholders.
    local_location_items: Option<HashMap<i64, i64>>,
}

struct LocalVirtualItemGrant {
    location_id: i64,
    location_name: String,
    ap_item_id: i64,
    item_name: String,
    er_id: ItemId,
    quantity: u32,
}

impl shared::Core for Core {
    type SlotData = SlotData;
    type Game = crate::game::EldenRing;

    /// Creates a new instance of the mod.
    fn new() -> Result<Self> {
        Ok(Self {
            base: CoreBase::new("EldenRing")?,
            last_item_time: Instant::now(),
            locations_sent: 0,
            sent_goal: false,
            item_get_display_restores: Vec::new(),
            location_flag_mapping: None,
            pending_flag_checks: Vec::new(),
            flag_poll_cursor: 0,
            last_inventory_scan: None,
            last_virtual_location_sync: None,
            locations_seen: 0,
            untagged_item_ids: HashSet::new(),
            was_dead: false,
            last_death_count: None,
            last_menu_returns: 0,
            last_death_at: None,
            ng_trap_restored: None,
            local_item_scout: None,
            local_item_scout_requested: None,
            local_location_items: None,
        })
    }

    fn base(&self) -> &CoreBase<Self::Game, SlotData> {
        &self.base
    }

    fn base_mut(&mut self) -> &mut CoreBase<Self::Game, SlotData> {
        &mut self.base
    }

    /// Updates the game logic and checks for common errors. Does nothing if
    /// we're not connected to the server or the mod hit a fatal error.
    fn update_live(&mut self) -> Result<()> {
        self.check_seed_conflict()?;
        if let Some(save_data) = SaveData::instance_mut().as_mut()
            && save_data.seed.is_none()
        {
            save_data.seed = Some(self.seed().to_string());
        };

        
        self.restore_expired_item_get_displays();

        // Handle events that should only happen while the player has a save
        // loaded and is playing.
        self.take_events();

        self.process_incoming_items();
        self.process_inventory_items()?;
        if let Some(save_data) = SaveData::instance() {
            remove_sent_display_items(&save_data.locations);
        }
        self.handle_goal()?;
        let died = self.detect_death();
        self.handle_death_link(died)?;
        self.handle_ng_trap(died);

        Ok(())
    }

    fn handle_command(&mut self, command: &str, arg: Option<&str>) -> bool {
        let mut arg_error = |usage: &str| {
            self.log(vec![
                RichText::Color {
                    text: format!("Invalid {}.", command),
                    color: ap::TextColor::Red,
                },
                " Usage:\n".into(),
                usage.into(),
            ]);
        };

        match command {
            "!getevent" => {
                let Some(flag) = arg.and_then(|f| u32::from_str(f).ok()) else {
                    arg_error("!getevent EVENT_FLAG");
                    return true;
                };

                let Some(value) = get_event_flag(EventFlagId(flag)) else {
                    self.log(RichText::Color {
                        text: "CSEventFlagMan not loaded".into(),
                        color: ap::TextColor::Red,
                    });
                    return true;
                };

                self.log(vec![
                    "Event ".into(),
                    RichText::Color {
                        text: format!("{:?}", flag),
                        color: ap::TextColor::Blue,
                    },
                    ": ".into(),
                    RichText::Color {
                        text: format!("{:?}", value),
                        color: if value {
                            ap::TextColor::Green
                        } else {
                            ap::TextColor::Red
                        },
                    },
                ]);

                true
            }

            "!ngstatus" => {
                let level = unsafe { crate::game::EldenRing::ng_level() };
                let deaths = unsafe { crate::game::EldenRing::death_count() };
                self.log(RichText::Color {
                    text: format!(
                        "NG level: {}, deaths: {}",
                        level.map_or("?".to_string(), |n| n.to_string()),
                        deaths.map_or("?".to_string(), |n| n.to_string())
                    ),
                    color: ap::TextColor::Blue,
                });
                true
            }

            #[cfg(debug_assertions)]
            "!ngtrap" => {
                if let Some(save_data) = SaveData::instance_mut().as_mut() {
                    self.start_ng_trap(save_data);
                }
                true
            }

            #[cfg(debug_assertions)]
            "!setevent" => {
                let Some((flag, value)) = arg.and_then(|a| {
                    let args = regex!(" +").split(a).collect::<Vec<_>>();
                    if args.len() == 2 {
                        Some((u32::from_str(args[0]).ok()?, bool::from_str(args[1]).ok()?))
                    } else {
                        None
                    }
                }) else {
                    arg_error("!setevent EVENT_FLAG BOOL");
                    return true;
                };

                if !set_event_flag(EventFlagId(flag), value) {
                    self.log(RichText::Color {
                        text: "CSEventFlagMan not loaded".into(),
                        color: ap::TextColor::Red,
                    });
                    return true;
                };

                self.log(vec![
                    "Set event ".into(),
                    RichText::Color {
                        text: format!("{:?}", flag),
                        color: ap::TextColor::Blue,
                    },
                    " to ".into(),
                    RichText::Color {
                        text: format!("{:?}", value),
                        color: if value {
                            ap::TextColor::Green
                        } else {
                            ap::TextColor::Red
                        },
                    },
                ]);

                true
            }

            _ => false,
        }
    }

    fn screen_effect(&self) -> Option<shared::ScreenEffect> {
        None
    }
}

impl Core {
    /// Errors if the server, the save and/or the config disagree about the
    /// current seed. If the save has no seed yet, fills it in from what's
    /// available.
    fn check_seed_conflict(&mut self) -> Result<()> {
        let client_seed = self.client().map(|c| c.seed_name());
        let save = SaveData::instance();
        let save_seed = save.as_ref().and_then(|s| s.seed.as_ref());

        match (client_seed, save_seed) {
            (Some(client_seed), _) if client_seed != self.seed() => bail!(
                "You've connected to a different Archipelago multiworld than the one that \
                 EldenRingArchipelagoRandomizer.exe used!\n\
                 \n\
		 Connected room seed: {}\n\
                 EldenRingArchipelagoRandomizer.exe seed: {}",
                client_seed,
                self.seed()
            ),
            (Some(client_seed), Some(save_seed)) if client_seed != save_seed => bail!(
                "You've connected to a different Archipelago multiworld than the one that \
                 you used before with this save!\n\
                 \n\
		 Connected room seed: {}\n\
		 Save file seed: {}",
                client_seed,
                save_seed
            ),
            (_, Some(save_seed)) if self.seed() != save_seed => bail!(
                "Your most recent EldenRingArchipelagoRandomizer.exe invocation connected to a \
                 different Archipealgo multiworld than the one that you used before with this \
                 save!\n\
                 \n\
                 EldenRingArchipelagoRandomizer.exe seed: {}\n\
                 Save file seed: {}",
                self.seed(),
                save_seed
            ),
            _ => Ok(()),
        }
    }

    

    /// Hands new items to the player when appropriate. Also sets up the
    /// [SaveData] for a new file.
    fn process_incoming_items(&mut self) {
        let show_item_popups = self.base().show_item_popups();
        let show_progression_item_popups = self.base().show_progression_item_popups();
        let Some(client) = self.client() else {
            return;
        };
        let Ok(item_man) = (unsafe { MapItemMan::instance_mut() }) else {
            return;
        };
        let mut save_data = SaveData::instance_mut();
        let Some(save_data) = save_data.as_mut() else {
            return;
        };

        // Don't grant several received items in one burst, but keep release
        // queues moving at a decent pace.
        if self.last_item_time.elapsed() < RECEIVED_ITEM_GRANT_INTERVAL {
            return;
        }

        if let Some(item) = client
            .received_items()
            .iter()
            .find(|item| item.index() >= save_data.items_granted)
        {
            // The NG+ trap isn't a real Elden Ring item, so it has no ER ID in
            // the slot data. Handle it here, before that lookup.
            if item
                .item()
                .name()
                .eq_ignore_ascii_case(NG_PLUS_TRAP_ITEM_NAME)
            {
                save_data.items_granted += 1;
                self.start_ng_trap(save_data);
                self.last_item_time = Instant::now();
                return;
            }

            let source_location = item.location();
            let source_location_name = source_location.name().to_string();
            let id_key = I64Key(item.item().id());
            let er_id = client
                .slot_data()
                .ap_ids_to_item_ids
                .get(&id_key)
                .unwrap_or_else(|| {
                    panic!(
                        "Archipelago item {:?} should have an ER ID defined in slot data",
                        item.item()
                    )
                })
                .0;
            let quantity = client
                .slot_data()
                .item_counts
                .get(&id_key)
                .copied()
                .unwrap_or(1);

            // With DLC start enabled, the randomizer's regulation edit already
            // puts a few items (Spirit Calling Bell and the like) straight into
            // the new character's starting inventory, on top of sending them
            // here as ordinary AP start-inventory items (location `Server`).
            // Granting this copy too would double them up, so skip it once the
            // player already has it. A non-DLC-start game never bakes these in,
            // so `has_item_in_inventory` stays false there and this is a no-op.
            if source_location == ap::Location::server()
                && client.slot_data().options.enable_dlc
                && has_item_in_inventory(er_id)
            {
                info!(
                    "Skipping start-inventory item {} (ER ID {:?}); the randomizer's \
                     DLC-start regulation edit already granted it",
                    item.item().name(),
                    er_id
                );
                save_data.items_granted += 1;
                self.last_item_time = Instant::now();
                return;
            }

            let source_display_name = received_item_location_display_name(
                client,
                item,
                source_location,
                &source_location_name,
            );

            info!(
                "Granting {} (AP ID {}, ER ID {:?} from {})",
                item.item().name(),
                item.item().id(),
                er_id,
                source_display_name
            );

            let show_item_popup = show_item_popups
                || (show_progression_item_popups
                    && client
                        .slot_data()
                        .non_deprioritized_progression_item_ids
                        .contains(&id_key.0));
            self.grant_received_item(item_man, er_id, quantity, show_item_popup);

            save_data.items_granted += 1;
            self.last_item_time = Instant::now();
        }
    }

    /// Puts the game one NG+ cycle higher (capped at NG+7) until the player
    /// dies. Does nothing if a trap is already active.
    fn start_ng_trap(&mut self, save_data: &mut SaveData) {
        if save_data.ng_trap.is_some() {
            info!("NG+ trap received while one is already active; ignoring");
            return;
        }

        let Some(original) = (unsafe { crate::game::EldenRing::ng_level() }) else {
            warn!("NG+ trap received, but the game data isn't loaded; ignoring");
            return;
        };
        let raise = random_ng_raise();
        let trapped = (original + raise).min(MAX_NG_LEVEL);
        if trapped == original {
            info!("NG+ trap received, but the game is already on NG+{original}; ignoring");
            return;
        }
        let actual_raise = trapped - original;

        if unsafe { crate::game::EldenRing::set_ng_level(trapped) } {
            info!(
                "NG+ trap: NG+{original} -> NG+{trapped} (raised by {actual_raise}, rolled \
                 {raise})"
            );
            save_data.ng_trap = Some(NgTrap { original, trapped });
            let plural = if actual_raise == 1 { "" } else { "s" };
            self.log(RichText::Color {
                text: format!(
                    "NG+ Trap! Raised {actual_raise} level{plural}, to NG+{trapped}, until you die."
                ),
                color: ap::TextColor::Red,
            });
        }
    }

    /// Notices whether the local player died since the last tick. Uses every
    /// signal we have (the death flag, HP reaching zero, and the death count
    /// going up), since no single one has proven reliable on its own.
    fn detect_death(&mut self) -> bool {
        // While we're not in a game (the main menu, and possibly loading
        // screens) leave what we know alone. The death count can go up during
        // the respawn load, and we want to notice that once we're back.
        if SaveData::instance().is_none() {
            return false;
        }

        // Going back to the main menu ends the session, so the death count we
        // remember may belong to another character. Start over instead of
        // mistaking the difference for a death.
        let menu_returns = SaveData::menu_returns();
        if menu_returns != self.last_menu_returns {
            self.last_menu_returns = menu_returns;
            self.was_dead = false;
            self.last_death_count = None;
        }

        let is_dead = unsafe { crate::game::EldenRing::is_player_dead() };
        let death_count = unsafe { crate::game::EldenRing::death_count() };
        let flag_came_on = is_dead && !self.was_dead;
        let count_went_up = matches!(
            (death_count, self.last_death_count),
            (Some(now), Some(before)) if now > before
        );
        let previous_death_count = self.last_death_count;
        self.was_dead = is_dead;
        self.last_death_count = death_count;

        if !(flag_came_on || count_went_up) {
            return false;
        }
        if self
            .last_death_at
            .is_some_and(|at| at.elapsed() < DEATH_COOLDOWN)
        {
            return false;
        }

        info!(
            "Player death detected (dead: {is_dead}, came on: {flag_came_on}, \
             death count: {previous_death_count:?} -> {death_count:?})"
        );
        self.last_death_at = Some(Instant::now());
        true
    }

    /// While an NG+ trap is active, keeps the game on the trapped NG+ level,
    /// then restores the original level once the player dies.
    fn handle_ng_trap(&mut self, died: bool) {
        let mut save_data = SaveData::instance_mut();
        let Some(save_data) = save_data.as_mut() else {
            return;
        };

        if !self.was_dead && let Some(level) = self.ng_trap_restored.take() {
            self.log(RichText::Color {
                text: format!("NG+ Trap over. Back to NG+{level}."),
                color: ap::TextColor::Green,
            });
        }

        let Some(trap) = save_data.ng_trap else {
            return;
        };

        if died {
            info!("Player died; ending NG+ trap (back to NG+{})", trap.original);
            unsafe { crate::game::EldenRing::set_ng_level(trap.original) };
            save_data.ng_trap = None;
            self.ng_trap_restored = Some(trap.original);
            return;
        }

        if unsafe { crate::game::EldenRing::ng_level() } != Some(trap.trapped) {
            // The game reloaded the level from the save (which may predate the
            // trap), so put the trap back.
            unsafe { crate::game::EldenRing::set_ng_level(trap.trapped) };
        }
    }

    fn grant_received_item(
        &mut self,
        item_man: &mut MapItemMan,
        id: ItemId,
        quantity: u32,
        show_item_popups: bool,
    ) {
        if !show_item_popups && let Err(err) = self.suppress_item_get_display(id) {
            warn!(
                "Failed to suppress item pickup popup for {:?}; granting normally: {:#}",
                id, err
            );
        }

        item_man.grant_item(ItemBufferEntry::new(id, quantity));
    }

    fn suppress_item_get_display(&mut self, id: ItemId) -> Result<()> {
        let restore_at = Instant::now() + ITEM_GET_DISPLAY_SUPPRESSION_DURATION;

        if let Some(restore) = self
            .item_get_display_restores
            .iter_mut()
            .find(|restore| restore.id == id)
        {
            set_item_get_display(id, ItemGetDisplay::suppressed())?;
            restore.restore_at = restore_at;
            return Ok(());
        }

        let previous = set_item_get_display(id, ItemGetDisplay::suppressed())?;
        self.item_get_display_restores.push(ItemGetDisplayRestore {
            id,
            previous,
            restore_at,
        });
        Ok(())
    }

    fn restore_expired_item_get_displays(&mut self) {
        let now = Instant::now();
        let mut index = 0;

        while index < self.item_get_display_restores.len() {
            if self.item_get_display_restores[index].restore_at > now {
                index += 1;
                continue;
            }

            let restore = self.item_get_display_restores.swap_remove(index);
            if let Err(err) = set_item_get_display(restore.id, restore.previous) {
                error!(
                    "Failed to restore item pickup display settings for {:?}: {:#}",
                    restore.id, err
                );
            }
        }
    }

    /// Builds and caches the list of locations to poll from the loaded
    /// regulation. Returns `false` if the regulation isn't available yet (e.g.
    /// no save loaded).
    fn ensure_location_flag_mapping(&mut self) -> bool {
        if self.location_flag_mapping.is_some() {
            return true;
        }

        let Some(regulation) = RegulationManager::instance() else {
            return false;
        };

        let mapping = checks::build_location_flag_map(&regulation);
        self.pending_flag_checks = mapping.location_flags.keys().copied().collect();
        LocationFlagChanges::add_tracked_flags(mapping.flag_locations.keys().copied());
        self.location_flag_mapping = Some(mapping);
        self.flag_poll_cursor = 0;
        true
    }

    /// Marks a location as checked once the game sets any of its event flags.
    /// This is how item lots and shop purchases are detected: unlike normal
    /// pickups, they aren't supposed to leave a placeholder in the inventory
    /// for `process_inventory_items` to find, so the location's own vanilla
    /// flag is the only signal.
    ///
    /// Only [FLAG_POLL_BATCH_SIZE] locations are checked per call, picking up
    /// where the last call stopped, and a location is dropped once it's known
    /// to be checked. So the cost per frame is capped and falls as the run goes
    /// on.
    fn poll_flag_based_checks(&mut self, save_data: &mut SaveData) {
        if !self.ensure_location_flag_mapping() {
            return;
        }
        let Some(mapping) = &self.location_flag_mapping else {
            return;
        };

        // Get the flag manager once for the whole batch, not once per location.
        let Ok(events) = (unsafe { CSEventFlagMan::instance() }) else {
            return;
        };

        let mut checked_locations: HashSet<i64> = HashSet::new();
        let changed_flags = LocationFlagChanges::take_changed_flags();
        for flag in changed_flags.iter().as_ref() {
            let Some(location_ids) = mapping.flag_locations.get(flag) else {
                continue;
            };
            for &location_id in location_ids {
                if self.check_location_flags(events, save_data, location_id) {
                    checked_locations.insert(location_id);
                }
            }
        }

        let batch = (self.pending_flag_checks.len() - changed_flags.len()).clamp(0, FLAG_POLL_BATCH_SIZE);
        for _ in 0..batch {
            if self.flag_poll_cursor >= self.pending_flag_checks.len() {
                self.flag_poll_cursor = 0;
            }
            let location_id = self.pending_flag_checks[self.flag_poll_cursor];
            if self.check_location_flags(events, save_data, location_id) {
                checked_locations.insert(location_id);
            }
        }

        // Retire checked locations from future polling.
        if !checked_locations.is_empty() {
            self.pending_flag_checks.retain(|location_id| !checked_locations.contains(location_id));
        }
    }

    fn check_location_flags(&self, events: &CSEventFlagMan, save_data: &mut SaveData, location_id: i64) -> bool {
        let Some(mapping) = &self.location_flag_mapping else {
            return false;
        };
        let Some(flags) = mapping.location_flags.get(&location_id) else {
            return false;
        };

        let already_checked = save_data.locations.contains(&location_id);
        let checked = already_checked
            || flags
                .iter()
                // TODO: This is abyssmally inefficient as it doesn't use tree lookup.
                // Update fromsoftware-rs or call the game's impl to fix this.
                .any(|flag| events.virtual_memory_flag.get_flag(u32::from(*flag)));

        if checked && !already_checked {
            info!("Archipelago location {} checked via event flag", location_id);
            save_data.locations.insert(location_id);
        }
        checked
    }

    /// Removes placeholder items from the inventory and tells the server their
    /// locations were reached.
    fn process_inventory_items(&mut self) -> Result<()> {
        let Some(ref mut save_data) = SaveData::instance_mut() else {
            return Ok(());
        };
        let Ok(game_data_man) = (unsafe { GameDataMan::instance_mut() }) else {
            return Ok(());
        };
        let Ok(item_man) = (unsafe { MapItemMan::instance_mut() }) else {
            return Ok(());
        };
        let Some(regulation_manager) = RegulationManager::instance() else {
            return Ok(());
        };

        // Collect into a separate vector so we aren't borrowing while we make
        // changes. Filtering during the walk keeps it to a handful of entries
        // instead of copying the whole inventory every time.
        let ids = if due(&mut self.last_inventory_scan, INVENTORY_SCAN_INTERVAL) {
            game_data_man
                .main_player_game_data
                .equipment
                .equip_inventory_data
                .items_data
                .items()
                .map(|e| e.item_id)
                .filter(|id| id.is_archipelago())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for id in ids {
            if self.untagged_item_ids.contains(&id.param_id()) {
                continue;
            }

            if !matches!(id.category(), ItemCategory::Goods | ItemCategory::Accessory) {
                // Weapons, protectors, and gems in the Archipelago range are
                // already real, fully-working items with no hidden location
                // data to decode (only goods and accessory rows have the
                // repurposed "vagrant" fields for that); their pickup is
                // tracked separately, via the lot's event flag. Leave them
                // in the inventory untouched.
                continue;
            }

            let row = regulation_manager
                .get_equip_param(id)
                .unwrap_or_else(|| panic!("no row defined for Archipelago ID {:?}", id));
            let row = passive_row(row.as_dyn()).unwrap_or_else(|| {
                panic!("Archipelago ID {:?} should be Goods or Accessory", id)
            });

            let Some(location_id) = row.archipelago_location_id() else {
                // A placeholder with no location or item data. Local ones are
                // leftovers, since the real item is granted by location (see
                // `grant_local_location_items`). Foreign ones just stand in for
                // another player's item, and their pickup is reported through
                // the lot's event flag. Neither needs to stay in the inventory.
                // The `basic_price == 0` check limits this to the randomizer's
                // blank rows: a row with a price isn't one of them.
                if row.basic_price() == 0 {
                    info!(
                        "Removing untagged {} placeholder {:?}",
                        if id.is_local_archipelago() {
                            "local"
                        } else {
                            "foreign"
                        },
                        id
                    );
                    game_data_man.remove_item(id, 1);
                    continue;
                }

                // The ID is in the Archipelago range but the row has no
                // location and carries a price, so it isn't one of the
                // randomizer's blank placeholders. There's nothing to convert
                // it to or report. It has to stay in the inventory, because the
                // code below would delete it.
                info!(
                    "Item {:?} is in the Archipelago range but carries no location; \
                     leaving it in the inventory",
                    id
                );
                self.untagged_item_ids.insert(id.param_id());
                continue;
            };

            info!("Inventory contains Archipelago item {:?}", id);
            info!("  Archipelago location: {}", location_id);
            save_data.locations.insert(location_id);

            if let Some((real_id, quantity)) = row.archipelago_item() {
                info!("  Converting to {}x {:?}", quantity, real_id);
                // Handed over here, so the by-location grant must skip it.
                save_data.local_virtual_items_granted.insert(location_id);
                game_data_man.give_item_directly(real_id, quantity);
            } else {
                // Any item without local item data is presumably a foreign one,
                // but log extra details in case there's a bug to track down.
                info!(
                    "  Item has no local item data. Basic price: {}, sell value: {}",
                    row.basic_price(),
                    row.sell_value()
                );
            }
            info!("  Removing from inventory");
            game_data_man.remove_item(id, 1);
        }

        self.poll_flag_based_checks(save_data);

        // The virtual-location passes all walk the checked-location set, which
        // grows into the thousands. Run them when the set actually changed, and
        // otherwise only on a slow timer as a backstop for state that changed
        // elsewhere (a late server connection, say).
        let locations_changed = save_data.locations.len() != self.locations_seen;
        if locations_changed
            || due(
                &mut self.last_virtual_location_sync,
                VIRTUAL_LOCATION_SYNC_INTERVAL,
            )
        {
            if let Some(client) = self.client() {
                client
                    .slot_data()
                    .expand_virtual_location_checks(&mut save_data.locations);
            }
            self.grant_local_virtual_location_items(item_man, save_data);
            self.grant_local_location_items(item_man, save_data);
            self.notify_foreign_virtual_location_items(item_man, save_data);
        }
        self.locations_seen = save_data.locations.len();

        if save_data.locations.len() > self.locations_sent
            && let Some(client) = self.client_mut()
        {
            client.mark_checked(save_data.locations.iter().copied())?;
            self.locations_sent = save_data.locations.len();
        }
        Ok(())
    }

    fn grant_local_virtual_location_items(
        &mut self,
        item_man: &mut MapItemMan,
        save_data: &mut SaveData,
    ) {
        let pending_grants = {
            let Some(client) = self.client() else {
                return;
            };
            let slot_data = client.slot_data();

            save_data
                .locations
                .iter()
                .copied()
                .filter(|location_id| {
                    !save_data
                        .local_virtual_items_granted
                        .contains(location_id)
                })
                .filter_map(|location_id| {
                    let ap_item_id = slot_data.local_virtual_location_item(location_id)?;
                    let Some(er_id) = slot_data.ap_ids_to_item_ids.get(&I64Key(ap_item_id)) else {
                        warn!(
                            "Local virtual location {} contains AP item {}, but slot data has no ER item ID",
                            location_id, ap_item_id
                        );
                        return None;
                    };
                    let quantity = slot_data
                        .item_counts
                        .get(&I64Key(ap_item_id))
                        .copied()
                        .unwrap_or(1);
                    let item_name = client
                        .this_game()
                        .item(ap_item_id)
                        .map(|item| item.name().to_owned())
                        .unwrap_or_else(|| format!("<item #{}>", ap_item_id));
                    let location_name = client
                        .this_game()
                        .location(location_id)
                        .map(|location| location.name().to_owned())
                        .unwrap_or_else(|| format!("<location #{}>", location_id));

                    Some(LocalVirtualItemGrant {
                        location_id,
                        location_name,
                        ap_item_id,
                        item_name,
                        er_id: er_id.0,
                        quantity,
                    })
                })
                .collect::<Vec<_>>()
        };

        for grant in pending_grants {
            info!(
                "Granting local virtual {} (AP ID {}, ER ID {:?} from {})",
                grant.item_name, grant.ap_item_id, grant.er_id, grant.location_name
            );
            item_man.grant_item(ItemBufferEntry::new(grant.er_id, grant.quantity));
            save_data
                .local_virtual_items_granted
                .insert(grant.location_id);
            self.last_item_time = Instant::now();
        }
    }

    /// Makes sure the server has told us which item is at each of this game's
    /// locations, and returns whether that answer is in yet.
    ///
    /// The randomizer's local placeholders don't carry the real item, and the
    /// server doesn't send a player's own items back to them. So the only way
    /// to know what a local pickup should give is to ask the server (a
    /// "scout"). We do that once for every location and cache the answer.
    fn update_local_item_scout(&mut self) -> bool {
        if self.local_location_items.is_some() {
            return true;
        }

        // Pick up the answer, if it's arrived.
        let response = self.local_item_scout.as_mut().and_then(|poll| poll());
        match response {
            Some(Ok(located)) => {
                let own_items = {
                    let Some(client) = self.client() else {
                        return false;
                    };
                    let me = client.this_player();
                    located
                        .iter()
                        .filter(|entry| {
                            entry.receiver().team() == me.team()
                                && entry.receiver().slot() == me.slot()
                        })
                        .map(|entry| (entry.location().id(), entry.item().id()))
                        .collect::<HashMap<_, _>>()
                };
                info!(
                    "Scouted {} location(s): {} hold this player's own items",
                    located.len(),
                    own_items.len()
                );
                self.local_item_scout = None;
                self.local_location_items = Some(own_items);
                return true;
            }
            Some(Err(err)) => {
                warn!("Scouting locations failed; will retry: {:#}", err);
                self.local_item_scout = None;
            }
            None => {}
        }

        // Still waiting on a request that's out.
        if self.local_item_scout.is_some()
            && self
                .local_item_scout_requested
                .is_some_and(|at| at.elapsed() < LOCAL_ITEM_SCOUT_TIMEOUT)
        {
            return false;
        }

        // Ask (or ask again).
        let Some(client) = self.client_mut() else {
            return false;
        };
        let mut location_ids = client
            .unchecked_locations()
            .map(|location| location.id())
            .collect::<Vec<_>>();
        location_ids.extend(client.checked_locations().map(|location| location.id()));

        info!("Scouting {} location(s) for their items", location_ids.len());
        let receiver = client.scout_locations(location_ids, ap::CreateAsHint::No);
        self.local_item_scout = Some(Box::new(move || receiver.try_recv().ok()));
        self.local_item_scout_requested = Some(Instant::now());
        false
    }

    /// Grants the real item for every checked location of this game that holds
    /// one of the player's own items. Locations with another player's item are
    /// left alone and stay placeholders.
    ///
    /// Handled locations are tracked in
    /// `SaveData::local_virtual_items_granted`, which is saved, so each
    /// location grants at most once. The first time this runs for a save, every
    /// already-checked location is recorded as handled *without* granting, so
    /// an old save isn't flooded with items for pickups it made long ago.
    fn grant_local_location_items(&mut self, item_man: &mut MapItemMan, save_data: &mut SaveData) {
        if !save_data
            .local_virtual_items_granted
            .contains(&LOCAL_LOCATION_BASELINE_MARKER)
        {
            let already_checked = {
                let Some(client) = self.client() else {
                    return;
                };
                let slot_data = client.slot_data();
                save_data
                    .locations
                    .iter()
                    .copied()
                    // Virtual locations are granted by their own pass.
                    .filter(|location_id| slot_data.local_virtual_location_item(*location_id).is_none())
                    .collect::<Vec<_>>()
            };
            info!(
                "Baselining {} already-checked location(s) for by-location grants",
                already_checked.len()
            );
            for location_id in already_checked {
                save_data.local_virtual_items_granted.insert(location_id);
            }
            save_data
                .local_virtual_items_granted
                .insert(LOCAL_LOCATION_BASELINE_MARKER);
        }

        if !self.update_local_item_scout() {
            return;
        }

        let mut unmapped = Vec::new();
        let pending_grants = {
            let (Some(client), Some(own_items)) = (self.client(), self.local_location_items.as_ref())
            else {
                return;
            };
            let slot_data = client.slot_data();

            save_data
                .locations
                .iter()
                .copied()
                .filter(|location_id| {
                    !save_data
                        .local_virtual_items_granted
                        .contains(location_id)
                })
                .filter_map(|location_id| {
                    let ap_item_id = *own_items.get(&location_id)?;
                    // Virtual locations are granted by their own pass.
                    if slot_data.local_virtual_location_item(location_id).is_some() {
                        return None;
                    }
                    let Some(er_id) = slot_data.ap_ids_to_item_ids.get(&I64Key(ap_item_id)) else {
                        warn!(
                            "Location {} contains AP item {}, but slot data has no ER item ID",
                            location_id, ap_item_id
                        );
                        unmapped.push(location_id);
                        return None;
                    };
                    let quantity = slot_data
                        .item_counts
                        .get(&I64Key(ap_item_id))
                        .copied()
                        .unwrap_or(1);
                    let item_name = client
                        .this_game()
                        .item(ap_item_id)
                        .map(|item| item.name().to_owned())
                        .unwrap_or_else(|| format!("<item #{}>", ap_item_id));
                    let location_name = client
                        .this_game()
                        .location(location_id)
                        .map(|location| location.name().to_owned())
                        .unwrap_or_else(|| format!("<location #{}>", location_id));

                    Some(LocalVirtualItemGrant {
                        location_id,
                        location_name,
                        ap_item_id,
                        item_name,
                        er_id: er_id.0,
                        quantity,
                    })
                })
                .collect::<Vec<_>>()
        };

        // Nothing can ever be granted for these, so stop warning about them.
        for location_id in unmapped {
            save_data.local_virtual_items_granted.insert(location_id);
        }

        for grant in pending_grants {
            info!(
                "Granting own item {}x {} (AP ID {}, ER ID {:?}) for {}",
                grant.quantity, grant.item_name, grant.ap_item_id, grant.er_id, grant.location_name
            );
            // The placeholder is dropped before it's granted (see
            // `on_grant_items`), so this is the only pop-up the pickup gets,
            // and it should show.
            self.grant_received_item(item_man, grant.er_id, grant.quantity, true);
            save_data
                .local_virtual_items_granted
                .insert(grant.location_id);
            self.last_item_time = Instant::now();
        }
    }

    /// Shows a pickup pop-up for virtual locations whose item is for another
    /// player. They have no world item, so without this the picker gets no
    /// feedback for what they sent. Local virtual items are handled in
    /// [grant_local_virtual_location_items](Self::grant_local_virtual_location_items).
    fn notify_foreign_virtual_location_items(
        &mut self,
        item_man: &mut MapItemMan,
        save_data: &mut SaveData,
    ) {
        let pending_notifications = {
            let Some(client) = self.client() else {
                return;
            };
            let slot_data = client.slot_data();

            save_data
                .locations
                .iter()
                .copied()
                .filter(|location_id| {
                    !save_data
                        .foreign_virtual_items_notified
                        .contains(location_id)
                })
                .filter_map(|location_id| {
                    let display_item = slot_data.virtual_location_display_item(location_id)?;
                    Some((location_id, display_item))
                })
                .collect::<Vec<_>>()
        };

        for (location_id, display_item) in pending_notifications {
            info!(
                "Showing foreign virtual location pickup with {:?} from {}",
                display_item, location_id
            );
            crate::item::show_virtual_location_display_item(item_man, display_item, location_id);
            save_data.foreign_virtual_items_notified.insert(location_id);
            self.last_item_time = Instant::now();
        }
    }

    /// Detects when the player has won and tells the server.
    fn handle_goal(&mut self) -> Result<()> {
        if !self.sent_goal
            && let Some(client) = self.client_mut()
            && client
                .slot_data()
                .goal
                .iter()
                .all(|flag| get_event_flag(*flag).unwrap_or(false))
        {
            client.set_status(ap::ClientStatus::Goal)?;
            self.sent_goal = true;
        }

        Ok(())
    }

    /// Sends a DeathLink when the local player dies. Incoming DeathLinks are
    /// handled in [shared::Core::take_events], which kills the player. Does
    /// nothing unless DeathLink is enabled in the config.
    fn handle_death_link(&mut self, died: bool) -> Result<()> {
        if !died || !self.base().config().death_link_enabled() {
            return Ok(());
        }

        // A death caused by someone else's DeathLink isn't ours to send back.
        // Otherwise a single death would bounce between players forever.
        if crate::game::EldenRing::take_death_link_kill() {
            return Ok(());
        }

        self.log(vec![
            RichText::Color {
                text: "DeathLink: ".into(),
                color: ap::TextColor::Red,
            },
            DEATH_LINK_CAUSE.to_string().into(),
        ]);

        if let Some(client) = self.client_mut()
            && let Err(err) =
                client.death_link(ap::DeathLinkOptions::new().cause(DEATH_LINK_CAUSE.to_string()))
        {
            // Not being able to send one isn't worth stopping the whole mod.
            warn!("Failed to send DeathLink: {err:#}");
        }

        Ok(())
    }
}

fn has_item_in_inventory(item_id: ItemId) -> bool {
    let Ok(game_data_man) = (unsafe { GameDataMan::instance() }) else {
        return false;
    };
    game_data_man
        .main_player_game_data
        .equipment
        .equip_inventory_data
        .items_data
        .items()
        .any(|entry| entry.item_id == item_id && entry.quantity > 0)
}

/// Whether a periodic pass is due; records the time if so.
///
/// `last` is `None` until the first run, so every pass runs once on the first
/// tick and then settles into its interval.
fn due(last: &mut Option<Instant>, interval: Duration) -> bool {
    let now = Instant::now();
    match last {
        Some(last) if now.duration_since(*last) < interval => false,
        _ => {
            *last = Some(now);
            true
        }
    }
}

fn get_event_flag(flag: EventFlagId) -> Option<bool> {
    let events = unsafe { CSEventFlagMan::instance() }.ok()?;
    Some(events.virtual_memory_flag.get_flag(u32::from(flag)))
}

fn set_event_flag(flag: EventFlagId, value: bool) -> bool {
    let Ok(events) = (unsafe { CSEventFlagMan::instance_mut() }) else {
        return false;
    };
    events.virtual_memory_flag.set_flag(u32::from(flag), value);
    true
}

#[derive(Clone, Copy)]
struct ItemGetDisplay {
    show_log: bool,
    show_dialog: u8,
}

impl ItemGetDisplay {
    fn suppressed() -> Self {
        Self {
            show_log: true,
            show_dialog: 0,
        }
    }
}

struct ItemGetDisplayRestore {
    id: ItemId,
    previous: ItemGetDisplay,
    restore_at: Instant,
}

fn set_item_get_display(id: ItemId, display: ItemGetDisplay) -> Result<ItemGetDisplay> {
    let row = unsafe { SoloParamRepository::instance_mut() }?
        .get_equip_param_mut(id)
        .with_context(|| format!("no row for item ID {:?}", id))?;

    macro_rules! set {
        ($row:expr) => {{
            let previous = ItemGetDisplay {
                show_log: $row.show_log_cond_type(),
                show_dialog: $row.show_dialog_cond_type(),
            };
            $row.set_show_log_cond_type(display.show_log);
            $row.set_show_dialog_cond_type(display.show_dialog);
            previous
        }};
    }

    Ok(match row {
        EquipParamStructMut::EQUIP_PARAM_ACCESSORY_ST(row) => set!(row),
        EquipParamStructMut::EQUIP_PARAM_GEM_ST(row) => set!(row),
        EquipParamStructMut::EQUIP_PARAM_GOODS_ST(row) => set!(row),
        EquipParamStructMut::EQUIP_PARAM_PROTECTOR_ST(row) => set!(row),
        EquipParamStructMut::EQUIP_PARAM_WEAPON_ST(row) => set!(row),
    })
}

fn received_item_location_display_name(
    client: &ap::Client<SlotData>,
    item: &ap::ReceivedItem,
    source_location: ap::Location,
    source_location_name: &str,
) -> String {
    if item.sender().team() == client.this_player().team()
        && item.sender().slot() == client.this_player().slot()
        && let Some(trigger_id) = client
            .slot_data()
            .virtual_location_trigger(source_location.id())
        && let Some(trigger_location) = client.this_game().location(trigger_id)
    {
        format!("{} [{}]", trigger_location.name(), source_location_name)
    } else {
        source_location_name.to_owned()
    }
}
