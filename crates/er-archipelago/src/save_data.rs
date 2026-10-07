use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use bincode::{Decode, Encode};
use eldenring::cs::MapItemMan;
use eldenring_extra::save;
use fromsoftware_shared::FromStatic;
use log::*;

/// The shared save data. It holds defaults until a save is loaded or it's set
/// directly.
static INSTANCE: LazyLock<RwLock<SaveData>> = LazyLock::new(|| RwLock::new(Default::default()));

/// How many times the player has gone back to the main menu this session.
static MENU_RETURNS: AtomicU32 = AtomicU32::new(0);

/// How the save data is encoded to bytes.
const CONFIG: bincode::config::Configuration = bincode::config::standard();

/// Data that's saved and loaded along with the player's game save.
#[derive(Debug, Decode, Encode, Default)]
pub struct SaveData {
    /// How many Archipelago items from other worlds have been given to the
    /// player in this run.
    pub items_granted: usize,

    /// Locations the player has already hit in this game. We don't strictly
    /// need it, but it keeps us from spamming the server.
    pub locations: HashSet<i64>,

    /// The seed this save was last connected to. Lets us catch someone loading
    /// a save while connected to the wrong multiworld.
    /// This is used to indicate whether the save data as a whole is initialized or not.
    pub seed: Option<String>,

    /// Locations whose own-item grant has already been given.
    pub local_virtual_items_granted: HashSet<i64>,

    /// Virtual locations whose "sent to someone else" pop-up has already been
    /// shown, so we don't show it twice.
    ///
    /// Now deprecated as popups are faked without giving the item.
    foreign_virtual_items_notified: HashSet<i64>,

    /// Set while an NG+ trap is active, so the original NG+ level can be
    /// restored on death, even across a quit and reload.
    pub ng_trap: Option<NgTrap>,
}

/// An active NG+ trap.
#[derive(Debug, Clone, Copy, Decode, Encode)]
pub struct NgTrap {
    /// The NG+ level to go back to when the player dies.
    pub original: u32,

    /// The NG+ level the trap put the game on.
    pub trapped: u32,
}

/// The save data layout from before `ng_trap` existed. Only used to keep
/// loading older saves.
#[derive(Decode)]
struct SaveDataV1 {
    items_granted: usize,
    locations: HashSet<i64>,
    seed: Option<String>,
    local_virtual_items_granted: HashSet<i64>,
    foreign_virtual_items_notified: HashSet<i64>,
}

impl From<SaveDataV1> for SaveData {
    fn from(old: SaveDataV1) -> Self {
        Self {
            items_granted: old.items_granted,
            locations: old.locations,
            seed: old.seed,
            local_virtual_items_granted: old.local_virtual_items_granted,
            foreign_virtual_items_notified: old.foreign_virtual_items_notified,
            ng_trap: None,
        }
    }
}

impl SaveData {
    /// How many times the player has gone back to the main menu this session.
    /// It goes up whenever a play session ends, so a change means whatever
    /// loads next may be a different character.
    pub fn menu_returns() -> u32 {
        MENU_RETURNS.load(Ordering::Relaxed)
    }

    /// Sets up the hooks for saving and loading. They're never removed.
    ///
    /// Safety: follow ilhook's safety rules.
    pub unsafe fn hook() {
        unsafe {
            std::mem::forget(save::on_save_load(
                || {
                    Self::instance().and_then(|data| {
                        if data.seed.is_none() {
                            // Don't write save data if it's not initialized, just in case,
                            // to prevent corrupting existing save files if alt saves are not used.
                            return None;
                        }
                        match bincode::encode_to_vec(&*data, CONFIG) {
                            Ok(bytes) => Some(bytes),
                            Err(err) => {
                                warn!("Failed to encode save data: {}", err);
                                None
                            },
                        }
                    })
                },
                |load_type| {
                    use save::OnLoadType::*;
                    let bytes = match load_type {
                        SavedData(bytes) => bytes,
                        MainMenu => {
                            // Back on the main menu: reset everything, so a new file
                            // can start fresh with no seed conflict or collected
                            // local locations.
                            *INSTANCE.write().unwrap() = Default::default();
                            MENU_RETURNS.fetch_add(1, Ordering::Relaxed);
                            return;
                        },
                        NoSavedData => {
                            // Loading into an uninitialized save file, don't carry
                            // over previous sav data either.
                            *INSTANCE.write().unwrap() = Default::default();
                            return;
                        },
                    };

                    // Fall back to the older layout so saves made before the NG+
                    // trap existed still load.
                    let decoded = decode_exact::<SaveData>(&bytes).or_else(|err| {
                        decode_exact::<SaveDataV1>(&bytes)
                            .map(SaveData::from)
                            .map_err(|_| err)
                    });
                    match decoded {
                        Ok(data) => *INSTANCE.write().unwrap() = data,
                        Err(err) => warn!("Failed to load save data: {}", err),
                    }
                },
            ));
        }
    }

    /// Read-only access to the [SaveData] singleton, or `None` if the player
    /// isn't in a game.
    pub fn instance<'a>() -> Option<RwLockReadGuard<'a, Self>> {
        // MapItemMan only exists once the player is in a game, not on the main
        // menu. That's a better test than "is a save loaded", since a new game
        // has no save file yet.
        //
        // Safety: we never use the man, we only check that it exists.
        if unsafe { MapItemMan::instance() }.is_ok() {
            Some(INSTANCE.read().unwrap())
        } else {
            None
        }
    }

    /// Write access to the [SaveData] singleton, or `None` if the player isn't
    /// in a game.
    pub fn instance_mut<'a>() -> Option<RwLockWriteGuard<'a, Self>> {
        // Same as above.
        if unsafe { MapItemMan::instance() }.is_ok() {
            Some(INSTANCE.write().unwrap())
        } else {
            None
        }
    }
}

fn decode_exact<T>(bytes: &[u8]) -> Result<T, String>
where
    T: Decode<()>,
{
    match bincode::decode_from_slice::<T, _>(bytes, CONFIG) {
        Ok((data, size)) if size == bytes.len() => Ok(data),
        Ok((_, size)) => Err(format!(
            "Archipelago save data had {} extra bytes; this probably means \
             that you tried to load a save file created by a different \
             version of the Archipelago mod, or by a different mod entirely",
            bytes.len() - size
        )),
        Err(err) => Err(err.to_string()),
    }
}
