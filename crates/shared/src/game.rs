use anyhow::Result;

use crate::{Core, InputBlocker};

/// A trait that encapsulates specific behavior for each individual game that's
/// used by the shared library. We try to keep this minimal, with most game
/// interactions being left in the individual game mod crates.
pub trait Game: Send + Sync + 'static {
    /// This game's core mod type.
    type Core: Core;

    /// The hudhook type for this game's graphics implementation.
    type GraphicsHooks: hudhook::Hooks;

    /// The input blocker type to block input to this game.
    type InputBlocker: InputBlocker;

    /// Which game this represents.
    const TYPE: GameType;

    /// The version of this client.
    const CLIENT_VERSION: &str;

    /// Whether this game supports toggling item pickup popups for incoming
    /// Archipelago items.
    const SUPPORTS_ITEM_POPUP_SETTING: bool = false;

    /// Schedules `task` to be run each frame, ideally at the beginning of the
    /// frame, on the game's main thread.
    ///
    /// This blocks until the task running infrastructure is available, and so
    /// should not be called on the game's main thread.
    fn run_recurring_task(task: impl FnMut() + 'static + Send) -> Result<()>;

    /// Returns whether the game is currently showing the main menu (or earlier
    /// during the initial load process).
    ///
    /// ## Safety
    ///
    /// This must be called on the main thread when no other references exist to
    /// the game's internal state.
    unsafe fn is_main_menu() -> bool;

    /// Forces the cursor to be visible on-screen.
    ///
    /// By default, does nothing.
    ///
    /// ## Safety
    ///
    /// This must be called on the main thread when no other references exist to
    /// the game's internal state.
    unsafe fn force_cursor_visible() {}

    /// Returns whether the player is currently in a menu, as opposed to
    /// actively playing the game.
    ///
    /// By default, this always returns false.
    ///
    /// ## Safety
    ///
    /// This must be called on the main thread when no other references exist to
    /// the game's internal state.
    unsafe fn is_menu_open() -> bool {
        false
    }

    /// Kills the local player, in response to a DeathLink received from
    /// another slot.
    ///
    /// This is only ever invoked while a save is loaded and past the initial
    /// grace period (see [CoreBase]'s `GRACE_PERIOD`), so implementations
    /// don't need to guard against being called at the main menu. They should
    /// still no-op gracefully if the relevant game state happens to be
    /// unavailable, the same as other unsafe accessors on this trait.
    ///
    /// By default, this does nothing, so games don't need to implement it
    /// until they wire up DeathLink support.
    ///
    /// ## Safety
    ///
    /// This must be called on the main thread when no other references exist to
    /// the game's internal state.
    unsafe fn kill_player() {}
}

/// An enum of From Software games, for situtations where the shared code just
/// needs to do some small difference for each one.
pub enum GameType {
    DarkSoulsIII,
    EldenRing,
    Sekiro,
}

impl GameType {
    /// Returns a short, human-friendly name for this game.
    pub fn short_name(&self) -> &str {
        match self {
            GameType::DarkSoulsIII => "DS3",
            GameType::EldenRing => "ER",
            GameType::Sekiro => "Sekiro",
        }
    }

    /// The basename for the static randomizer for this game.
    pub fn static_randomizer_basename(&self) -> &str {
        match self {
            GameType::DarkSoulsIII => "DS3Randomizer.exe",
            GameType::EldenRing => "EldenRingArchipelagoRandomizer.exe",
            GameType::Sekiro => "SekiroRandomizer.exe",
        }
    }
}
