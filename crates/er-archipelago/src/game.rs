use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use eldenring::cs::{CSEventFlagMan, ChrInsExt, WorldChrMan};
use eldenring::{
    cs::{
        CSFeManHudState, CSFeManImp, CSTaskGroupIndex, CSTaskImp, GameDataMan, MapItemMan, PlayerIns,
    },
    fd4::FD4TaskData,
};
use fromsoftware_shared::{FromStatic, SharedTaskImpExt};
use serde::{Deserialize, Serialize};
use shared::utils::LazyFileReader;

use crate::game::status::StatusEffect;

pub mod status;

pub struct EldenRing;

/// How long after a DeathLink kills the player that their death still counts
/// as caused by that DeathLink.
const DEATH_LINK_KILL_WINDOW: Duration = Duration::from_secs(10);

/// When the last DeathLink killed the player, if it hasn't been claimed yet.
static DEATH_LINK_KILL_AT: Mutex<Option<Instant>> = Mutex::new(None);

impl shared::Game for EldenRing {
    type Core = crate::core::Core;
    type GraphicsHooks = hudhook::hooks::dx12::ImguiDx12Hooks;
    type InputBlocker = EldenRingInputBlocker;
    const TYPE: shared::GameType = shared::GameType::EldenRing;
    const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
    const SUPPORTS_ITEM_POPUP_SETTING: bool = true;

    fn run_recurring_task(mut task: impl FnMut() + 'static + Send) -> Result<()> {
        CSTaskImp::wait_for_instance(Duration::MAX)?.run_recurring(
            move |_: &'_ FD4TaskData| task(),
            CSTaskGroupIndex::FrameBegin,
        );
        Ok(())
    }

    unsafe fn is_main_menu() -> bool {
        // If MapItemMan doesn't exist, we're probably on the main menu. There
        // may be a better check, but we haven't found one yet.
        unsafe { MapItemMan::instance() }.is_err()
    }

    unsafe fn is_menu_open() -> bool {
        if unsafe { Self::is_main_menu() } {
            return true;
        }

        unsafe { CSFeManImp::instance() }.is_ok_and(|fe_man| {
            matches!(
                fe_man.hud_state,
                CSFeManHudState::ShowAll | CSFeManHudState::PopupMenu
            )
        })
    }

    unsafe fn kill_player() {
        if let Ok(player) = unsafe { PlayerIns::local_player_mut() } {
            // Someone who's already dead can't be killed again, and their death
            // wasn't caused by this DeathLink.
            if player.chr_ins.chr_flags1c5.death_flag() {
                return;
            }

            // Save runes on death. This could be an option but it's hard to access here;
            // it may be better to move all deathlink processing into `update_live`.
            player.apply_speffect(4290, true);
            player.chr_ins.modules.data.hp = 0;
            *DEATH_LINK_KILL_AT.lock().unwrap() = Some(Instant::now());
        }
    }
}

// Game constants for received_damage_type
const DAMAGE_TYPE_DEATH: u32 = 2;
const DAMAGE_TYPE_DEATH_FALLING: u32 = 6;

impl EldenRing {
    /// Whether the local player is dead, either because the game flagged them
    /// dead or because their HP hit zero. Returns `false` if there's no player
    /// loaded (like on the main menu).
    ///
    /// ## Safety
    ///
    /// Call this on the main thread, with no other references to the game's
    /// internal state alive.
    pub unsafe fn is_player_dead() -> bool {
        // Preload messages. This could be done from a game-specific init function, alongside icons.
        Messages::get();
        unsafe { PlayerIns::local_player() }.is_ok_and(|player| {
            let data = &player.chr_ins.modules.data;
            let damage_type = player.chr_ins.modules.action_flag.received_damage_type;
            // This condition is used in c0000.hks
            damage_type == DAMAGE_TYPE_DEATH || (data.max_hp > 0 && data.hp <= 0)
        })
    }

    /// Returns the cause of the player's death on the frame the death is detected.
    /// Nothing is returned if `messages.json` was not loaded or a specific cause could
    /// not be determined.
    ///
    /// ## Safety
    ///
    /// Call this on the main thread, with no other references to the game's
    /// internal state alive.
    pub unsafe fn get_death_cause(name: &str) -> Option<String> {
        let Ok(world_chr_man) = (unsafe { WorldChrMan::instance() }) else {
            return None;
        };
        let Some(player) = &world_chr_man.main_player else {
            return None;
        };
        let Some(messages) = Messages::get() else {
            return None;
        };
        let passives = vec![
            "died to", "was killed by", "was vanquished by", "was outsmarted by",
            "lost to", "valiantly faced", "couldn't dodge",
        ];
        let passive = fastrand::choice(passives).unwrap_or("died to");
        let damage_type = player.chr_ins.modules.action_flag.received_damage_type;
        let last_hit = &player.last_hit_by;
        if damage_type == DAMAGE_TYPE_DEATH
            && !last_hit.is_empty()
            && let Some(hitter) = world_chr_man.chr_ins_by_handle(last_hit) {
            let npc_param_id = if hitter.npc_param_id >= 20000000 { hitter.npc_param_id / 100 * 100 } else { -1 };
            let enemy = messages.npc_id_names.get(&npc_param_id).map(String::as_str)
                .or_else(|| messages.npc_id_names.get(&hitter.npc_id).map(String::as_str))
                .unwrap_or("an enemy");
            return Some(format!("{} {} {}", name, passive, enemy));
        };
        if damage_type == DAMAGE_TYPE_DEATH_FALLING || Self::get_fall_height(player) >= 15.0 {
            if fastrand::bool() {
                return Some(format!("{} {} gravity", name, passive));
            } else {
                return Some(format!("{} fell to their death", name));
            }
        }
        if let Some(verb) = player.chr_ins.special_effect.entries()
                .find_map(|sp| messages.sp_effect_phrases.get(&sp.param_id)) {
            return Some(format!("{} {}", name, verb));
        }
        status::get_status_effects(&player).last().map(|status| match status {
            StatusEffect::Poison => "died of poison",
            StatusEffect::Rot => "died of rot",
            StatusEffect::Bleed => "bled to death",
            StatusEffect::Deathblight => "got death blighted",
            StatusEffect::Frost => "froze to death",
            StatusEffect::Sleep => "went to sleep",
            StatusEffect::Madness => "went mad",
        }).map(|msg| format!("{} {}", name, msg))
    }

    fn get_fall_height(_player: &PlayerIns) -> f32 {
        // TODO: needs `last_grounded_position`, which this fromsoft-rs doesn't have.
        0.0
    }

    /// Returns whether the player's latest death was caused by a received
    /// DeathLink, and forgets that so it's only reported once. Those deaths
    /// mustn't be sent back out as new DeathLinks.
    pub fn take_death_link_kill() -> bool {
        DEATH_LINK_KILL_AT
            .lock()
            .unwrap()
            .take()
            .is_some_and(|at| at.elapsed() < DEATH_LINK_KILL_WINDOW)
    }

    /// A description of the game's own death bookkeeping and related player state.
    ///
    /// ## Safety
    ///
    /// Call this on the main thread, with no other references to the game's
    /// internal state alive.
    #[allow(unused)]
    pub unsafe fn death_debug() -> Result<String> {
        let Ok(game_data_man) = (unsafe { GameDataMan::instance() }) else {
            bail!("no game data")
        };
        let Ok(world_chr_man) = (unsafe { WorldChrMan::instance() }) else {
            bail!("no world detected");
        };
        let Some(player) = &world_chr_man.main_player else {
            bail!("no player detected");
        };
        let last_hit = &player.last_hit_by;
        let mut last_hit_desc = "none".to_owned();
        if !last_hit.is_empty()
            && let Ok(world_chr_man) = unsafe { WorldChrMan::instance() }
            && let Some(hitter) = world_chr_man.chr_ins_by_handle(last_hit) {
            last_hit_desc = format!("{} {} {}", hitter.block_id(), hitter.npc_id, hitter.npc_param_id);
        };
        let mut sps: Vec<i32> = player.chr_ins.special_effect.as_ref().entries()
            .map(|sp| sp.param_id)
            .collect();
        sps.sort();
        Ok(format!(
            "hp {}, damage type {}, state {:?}, just died {}, fall time {} height {}, hit by {}, sp {:?}",
            player.chr_ins.modules.data.hp,
            player.chr_ins.modules.action_flag.received_damage_type,
            game_data_man.death_state,
            game_data_man.just_died,
            player.chr_ins.modules.fall.fall_timer,
            Self::get_fall_height(player),
            last_hit_desc,
            sps))
    }

    /// How many times the player has died on this character. Returns `None`
    /// if the game data isn't loaded yet.
    ///
    /// ## Safety
    ///
    /// Call this on the main thread, with no other references to the game's
    /// internal state alive.
    pub unsafe fn death_count() -> Option<u32> {
        unsafe { GameDataMan::instance() }
            .ok()
            .map(|data| data.death_count)
    }

    /// The current NG+ cycle: 0 is NG, 1 is NG+, and so on up to 7. Returns
    /// `None` if the game data isn't loaded yet.
    ///
    /// ## Safety
    ///
    /// Call this on the main thread, with no other references to the game's
    /// internal state alive.
    pub unsafe fn ng_level() -> Option<u32> {
        unsafe { GameDataMan::instance() }.ok().map(|data| data.ng_lvl)
    }

    /// Sets the current NG+ cycle. Enemies pick up the new scaling as they
    /// (re)spawn. Returns `false` if the game data isn't loaded yet.
    ///
    /// ## Safety
    ///
    /// Call this on the main thread, with no other references to the game's
    /// internal state alive.
    pub unsafe fn set_ng_level(level: u32) -> bool {
        let Ok(events) = (unsafe { CSEventFlagMan::instance_mut() }) else {
            return false;
        };
        let Ok(game_data_man) = (unsafe { GameDataMan::instance_mut() }) else {
            return false;
        };
        game_data_man.ng_lvl = level;
        // These flags are also used for some environment traps plus Romina's butterflies
        let ng_flag = 50 + level.min(7);
        for flag in 50u32..58 {
            events.virtual_memory_flag.set_flag(flag, flag == ng_flag);
        }
        return true;
    }
}

pub struct EldenRingInputBlocker(pub &'static eldenring_extra::input::InputBlocker);

impl shared::InputBlocker for EldenRingInputBlocker {
    fn block_only(&self, input_flags: shared::InputFlags) {
        self.0
            .block_only(eldenring_extra::input::InputFlags::from_bits(input_flags.bits()).unwrap())
    }
}

#[derive(Deserialize, Serialize, Default, Debug, Clone)]
struct Messages {
    pub npc_id_names: HashMap<i32, String>,
    pub sp_effect_phrases: HashMap<i32, String>,
}

impl Messages {
    pub fn get() -> Option<&'static Self> {
        static INSTANCE: OnceLock<Messages> = OnceLock::new();
        // Based on randomizer name data, could be embedded in the future if this results in problems.
        static READER: LazyFileReader = LazyFileReader::new("../diste/Names/names.json");
        if let Some(data) = READER.take_data() {
            match json::from_slice::<Messages>(&data) {
                Ok(config) => INSTANCE.set(config).unwrap_or(()),
                Err(err) => {
                    log::warn!("Error reading messages: {}", err);
                }
            }
        }
        INSTANCE.get()
    }
}
