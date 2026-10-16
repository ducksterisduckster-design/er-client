use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use eldenring::cs::{CSEventFlagMan, ChrInsExt};
use eldenring::{
    cs::{
        CSFeManHudState, CSFeManImp, CSTaskGroupIndex, CSTaskImp, GameDataMan, MapItemMan, PlayerIns,
    },
    fd4::FD4TaskData,
};
use fromsoftware_shared::{FromStatic, SharedTaskImpExt};

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
        unsafe { PlayerIns::local_player() }.is_ok_and(|player| {
            let data = &player.chr_ins.modules.data;
            player.chr_ins.chr_flags1c5.death_flag() || (data.max_hp > 0 && data.hp <= 0)
        })
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

    /// A description of the game's own death bookkeeping, for debugging.
    ///
    /// ## Safety
    ///
    /// Call this on the main thread, with no other references to the game's
    /// internal state alive.
    #[allow(unused)]
    pub unsafe fn death_debug() -> String {
        match unsafe { GameDataMan::instance() } {
            Ok(data) => format!("state {:?}, just died {}", data.death_state, data.just_died),
            Err(_) => "no game data".to_string(),
        }
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
