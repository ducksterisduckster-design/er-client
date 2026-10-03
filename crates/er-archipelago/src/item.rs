use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use eldenring::cs::{
    GameDataMan, ItemBuffer, ItemBufferEntry, ItemCategory, ItemId, MAP_ITEM_MAN_GRANT_ITEM_VA, MapItemMan, MapItemManEntry, SoloParamRepository,
};
use eldenring::param::{
    EquipParamStruct, EquipParamStructMut,
};
use fromsoftware_shared::FromStatic;
use ilhook::x64::*;
use log::*;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::{GetModuleInformation, MODULEINFO};
use windows::Win32::System::Threading::GetCurrentProcess;

/// A stripped-down stand-in for the game's `MenuGaitem` struct, with just the
/// two fields the native drop function reads. Not the real type, only enough
/// layout to make the call work.
#[repr(C)]
struct FakeMenuGaitem {
    unk0: [u8; 0x48],
    inventory_index: i32,
    item_id: ItemId,
}

type DropItemFn = unsafe extern "C" fn(&FakeMenuGaitem, i32, bool);

/// Address of the native function the inventory menu uses to drop an item.
/// Found with an AOB scan on first use, then cached.
///
/// TODO: if the mod ever gets a generated VA for this (like
/// `MAP_ITEM_MAN_GRANT_ITEM_VA` from `eldenring::cs`), use that instead of
/// scanning here.
static DROP_ITEM_FN: LazyLock<DropItemFn> = LazyLock::new(|| {
    // The pattern matches partway into the function, so the function starts
    // `OFFSET` bytes earlier. It's core engine code and has been stable across
    // patches, but if item removal silently stops working after a game update,
    // re-check it in Ghidra.
    const PATTERN: &[Option<u8>] = &[
        Some(0x45), Some(0x0F), Some(0xB6), Some(0xF0),
        Some(0x8B), Some(0xDA), Some(0x48), Some(0x8B), Some(0xE9),
    ];
    const OFFSET: usize = 0x1C;

    let match_addr =
        unsafe { find_pattern(PATTERN) }.expect("Failed to find native drop-item function pattern");
    unsafe { std::mem::transmute::<usize, DropItemFn>(match_addr - OFFSET) }
});

/// Scans the main module's image for `pattern` (`None` is a wildcard) and
/// returns the address of the first match.
///
/// # Safety
/// Reads raw process memory across the main module. That's fine as long as the
/// module is fully loaded (not before or during `DllMain`), which is always
/// true for our lazy call sites.
unsafe fn find_pattern(pattern: &[Option<u8>]) -> Option<usize> {
    let module: HMODULE = unsafe { GetModuleHandleW(None) }.ok()?;
    let mut info = MODULEINFO::default();
    unsafe {
        GetModuleInformation(
            GetCurrentProcess(),
            module,
            &mut info,
            std::mem::size_of::<MODULEINFO>() as u32,
        )
    }
    .ok()?;

    let base = info.lpBaseOfDll as *const u8;
    let size = info.SizeOfImage as usize;
    let haystack = unsafe { std::slice::from_raw_parts(base, size) };

    haystack
        .windows(pattern.len())
        .position(|window| {
            window
                .iter()
                .zip(pattern)
                .all(|(byte, expected)| expected.map_or(true, |expected| *byte == expected))
        })
        .map(|offset| base as usize + offset)
}

/// Finds `item_id` in the player's inventory, using the combined key/normal
/// index the native drop function expects: key items by their position in
/// `key_entries`, normal items continuing after `key_items_capacity`.
///
fn combined_inventory_index(
    items_data: &eldenring::cs::InventoryItemsData,
    item_id: ItemId,
) -> Option<i32> {
    if let Some(index) = items_data.key_entries().iter().position(|entry| {
        entry
            .as_option()
            .is_some_and(|entry| entry.item_id == item_id && entry.quantity > 0)
    }) {
        return Some(index as i32);
    }

    items_data
        .normal_entries()
        .iter()
        .position(|entry| {
            entry
                .as_option()
                .is_some_and(|entry| entry.item_id == item_id && entry.quantity > 0)
        })
        .map(|index| index as i32 + items_data.key_items_capacity as i32)
}

/// Removes `quantity` of the inventory entry at `inventory_index` (combined
/// key/normal index) by calling the same native function as the in-game "drop"
/// action, instead of the id-based `GameDataMan::remove_item`.
///
/// # Safety
/// Call this on the main thread with `GameDataMan` loaded. `inventory_index`
/// must point at a real, non-empty slot.
unsafe fn drop_item(inventory_index: i32, item_id: ItemId, quantity: i32) {
    let gaitem = FakeMenuGaitem {
        unk0: [0u8; 0x48],
        inventory_index,
        item_id,
    };
    unsafe { (*DROP_ITEM_FN)(&gaitem, quantity, false) }
}

/// Where the static randomizer allocates Archipelago foreign ("Other
/// <player>'s ...") placeholder goods, until [ARCHIPELAGO_LOCAL_ID_START].
const ARCHIPELAGO_FOREIGN_ID_START: u32 = 8_000_000;

/// Where local placeholders start (items that belong to this game), for every
/// non-weapon item table.
const ARCHIPELAGO_LOCAL_ID_START: u32 = 8_100_000;

/// Where local placeholders start end for every non-weapon item table.
const ARCHIPELAGO_LOCAL_ID_END: u32 = 9_000_000;

/// Where local weapon placeholders start.
const ARCHIPELAGO_LOCAL_WEAPON_ID_START: u32 = 81_000_000;

/// Where local weapon placeholders end.
const ARCHIPELAGO_LOCAL_WEAPON_ID_END: u32 = 90_000_000;

/// Location IDs of zero or less mean the row has no Archipelago location.
/// Untagged rows keep the vanilla `-1` in the vagrant fields
/// and the `999999999` template row has `0`. Real location IDs start in
/// the millions, so neither can be mistaken for one.
const ARCHIPELAGO_PROGRESSION_ICON_ID: u16 = 15363;
const ARCHIPELAGO_USEFUL_ICON_ID: u16 = 15333;

/// A foreign pickup's display item ("rainbow stone"), waiting to be removed
/// once its location is confirmed sent to the server.
struct PendingDisplayItem {
    id: ItemId,
    location_id: i64,
}

static DISPLAY_ITEMS_TO_REMOVE: LazyLock<Mutex<Vec<PendingDisplayItem>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// A DS3-style wrapper around ER's param repository.
pub struct RegulationManager(&'static SoloParamRepository);

impl RegulationManager {
    pub fn instance() -> Option<Self> {
        Some(Self(unsafe { SoloParamRepository::instance() }.ok()?))
    }

    pub fn get_equip_param(&self, id: ItemId) -> Option<EquipParamStruct<'_>> {
        self.0.get_equip_param(id)
    }

    /// Iterates over every row in a solo param table (like `ItemLotParam_map`
    /// or `ItemLotParam_enemy`), with each row's param ID, in ID order. Used to
    /// find rows the randomizer tagged with a location ID.
    pub fn rows<'a, P: eldenring::cs::SoloParam + 'a>(
        &'a self,
    ) -> impl Iterator<Item = (u32, &'a P::StructType)> + 'a {
        self.0.rows::<P>()
    }
}

/// Sets up the hooks that swap placeholder items (which may encode Archipelago
/// info) for the right in-game items.
pub unsafe fn hook_items() {
    let callback = |reg: *mut Registers| {
        let item_man = unsafe { &mut *((*reg).rcx as *mut MapItemMan) };
        let items = unsafe { &mut *((*reg).rdx as *mut ItemBuffer) };
        on_grant_items(item_man, items);
    };
    std::mem::forget(
        unsafe {
            hook_closure_jmp_back(
                *MAP_ITEM_MAN_GRANT_ITEM_VA as usize,
                callback,
                CallbackOption::None,
                HookFlags::empty(),
            )
        }
        .expect("Hooking MapItemMan::GrantItem failed"),
    );
}

/// Runs when the player receives items in a way that shows an on-screen
/// message.
fn on_grant_items(item_man: &mut MapItemMan, items: &mut ItemBuffer) {
    let mut index = 0;
    while index < items.len() {
        let item = items[index];

        if !item.id.is_archipelago() {
            // A vanilla item.
            info!("Received {}x {:?}", item.quantity, item.id);
            index += 1;
            continue;
        } else {
            // Archipelago items never show up in inventory and are only identified
            // by flag changes.
            info!("Received {}x {:?} (Archipelago item)", item.quantity, item.id);
            items.remove(index);
            if item.id.is_foreign_archipelago() {
                // Show dialog for foreign items, as local items will be later received by location.
                // This can fail when the queue is already full which is fine.
                let _ = item_man.item_award_queue.push(MapItemManEntry::new(item.id, 1));
            }
        }
    }
}

fn should_show_item_get_dialog(id: ItemId) -> bool {
    let Some(regulation_manager) = RegulationManager::instance() else {
        return false;
    };
    let Some(row) = regulation_manager.get_equip_param(id) else {
        return false;
    };
    let Some(row) = row.as_dyn().as_goods() else {
        return false;
    };

    matches!(
        row.icon_id(),
        ARCHIPELAGO_PROGRESSION_ICON_ID | ARCHIPELAGO_USEFUL_ICON_ID
    )
}

fn show_item_get_dialog(id: ItemId) -> bool {
    set_item_get_dialog(id, 2)
}

fn suppress_item_get_dialog(id: ItemId) -> bool {
    set_item_get_dialog(id, 0)
}

fn set_item_get_dialog(id: ItemId, show_dialog_cond_type: u8) -> bool {
    let Ok(solo_param_repository) = (unsafe { SoloParamRepository::instance_mut() }) else {
        return false;
    };
    let Some(row) = solo_param_repository.get_equip_param_mut(id) else {
        return false;
    };

    macro_rules! suppress {
        ($row:expr) => {{
            $row.set_show_log_cond_type(true);
            $row.set_show_dialog_cond_type(show_dialog_cond_type);
        }};
    }

    match row {
        EquipParamStructMut::EQUIP_PARAM_ACCESSORY_ST(row) => suppress!(row),
        EquipParamStructMut::EQUIP_PARAM_GEM_ST(row) => suppress!(row),
        EquipParamStructMut::EQUIP_PARAM_GOODS_ST(row) => suppress!(row),
        EquipParamStructMut::EQUIP_PARAM_PROTECTOR_ST(row) => suppress!(row),
        EquipParamStructMut::EQUIP_PARAM_WEAPON_ST(row) => suppress!(row),
    }

    true
}

fn queue_display_item_cleanup(id: ItemId, location_id: i64) {
    DISPLAY_ITEMS_TO_REMOVE
        .lock()
        .unwrap()
        .push(PendingDisplayItem { id, location_id });
}

/// Shows a one-off pickup pop-up for a foreign virtual location's display item,
/// then queues it for removal once `location_id` is confirmed sent to the
/// server.
///
/// Virtual (priority) locations have no world item, so unlike normal foreign
/// pickups they don't go through [on_grant_items]. We copy that behavior here:
/// progression/useful display items get the dialog, the rest only log.
pub(crate) fn show_virtual_location_display_item(
    item_man: &mut MapItemMan,
    display_item: ItemId,
    location_id: i64,
) {
    // Don't grant a display item that isn't in the regulation, or we'd feed
    // ItemGive an invalid ID.
    let display_item_exists = RegulationManager::instance()
        .is_some_and(|manager| manager.get_equip_param(display_item).is_some());
    if !display_item_exists {
        warn!(
            "Skipping foreign virtual location pop-up: display item {:?} is not in regulation",
            display_item
        );
        return;
    }

    if should_show_item_get_dialog(display_item) {
        show_item_get_dialog(display_item);
    } else {
        suppress_item_get_dialog(display_item);
    }

    item_man.grant_item(ItemBufferEntry::new(display_item, 1));
    queue_display_item_cleanup(display_item, location_id);
}

/// Removes display items (the "rainbow stones" shown for foreign pickups) once
/// their location is in `sent_locations`, meaning a `mark_checked` call to the
/// server included it.
///
/// Anything not sent yet (player offline, or this tick's send hasn't happened)
/// stays queued and is retried next call.
pub fn remove_sent_display_items(sent_locations: &HashSet<i64>) {
    let (ready, still_pending): (Vec<_>, Vec<_>) = {
        let mut queue = DISPLAY_ITEMS_TO_REMOVE.lock().unwrap();
        if queue.is_empty() {
            return;
        }
        queue
            .drain(..)
            .partition(|item| sent_locations.contains(&item.location_id))
    };

    if !still_pending.is_empty() {
        DISPLAY_ITEMS_TO_REMOVE
            .lock()
            .unwrap()
            .extend(still_pending);
    }
    if ready.is_empty() {
        return;
    }

    for item in ready {
        // Re-borrow for each item, tightly scoped, so we never hold a reference
        // into the inventory across the native call that changes it (and a
        // mid-loop return to the main menu is handled cleanly).
        let index = {
            let Ok(game_data_man) = (unsafe { GameDataMan::instance() }) else {
                DISPLAY_ITEMS_TO_REMOVE.lock().unwrap().push(item);
                continue;
            };
            combined_inventory_index(
                &game_data_man
                    .main_player_game_data
                    .equipment
                    .equip_inventory_data
                    .items_data,
                item.id,
            )
        };

        match index {
            Some(index) => unsafe { drop_item(index, item.id, 1) },
            None => warn!(
                "Display item {:?} not found in inventory; skipping removal",
                item.id
            ),
        }
    }
}

pub trait ItemIdExt {
    /// Whether this ID is a placeholder item added just for Archipelago, foreign or local.
    fn is_archipelago(&self) -> bool;

    /// Whether this ID is in the range used for local (this game's own) item
    /// placeholders.
    fn is_local_archipelago(&self) -> bool;

    /// Whether this ID is in the range used for foreign item placeholders.
    fn is_foreign_archipelago(&self) -> bool;
}

impl ItemIdExt for ItemId {
    fn is_archipelago(&self) -> bool {
        self.is_local_archipelago() || self.is_foreign_archipelago()
    }

    fn is_local_archipelago(&self) -> bool {
        match self.category() {
            ItemCategory::Weapon => (ARCHIPELAGO_LOCAL_WEAPON_ID_START..ARCHIPELAGO_LOCAL_WEAPON_ID_END).contains(&self.param_id()),
            _ => (ARCHIPELAGO_LOCAL_ID_START..ARCHIPELAGO_LOCAL_ID_END).contains(&self.param_id()),
        }
    }

    fn is_foreign_archipelago(&self) -> bool {
        self.category() == ItemCategory::Goods && (ARCHIPELAGO_FOREIGN_ID_START..ARCHIPELAGO_LOCAL_ID_START).contains(&self.param_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // This is a bit change-detector-y so keep it brief.
    #[test]
    fn non_ap_items_tagged() {
        let real_weapon = ItemId::new(ItemCategory::Weapon, 8_100_000).unwrap();
        let real_goods = ItemId::new(ItemCategory::Goods, 8010).unwrap();

        assert_eq!(false, real_weapon.is_archipelago());
        assert_eq!(false, real_goods.is_archipelago());
    }

    #[test]
    fn local_items_tagged() {
        let fake_weapon = ItemId::new(ItemCategory::Weapon, 81_510_000).unwrap();
        let fake_goods = ItemId::new(ItemCategory::Goods, 8_999_999).unwrap();

        assert_eq!(true, fake_weapon.is_local_archipelago());
        assert_eq!(false, fake_weapon.is_foreign_archipelago());
        assert_eq!(true, fake_goods.is_local_archipelago());
        assert_eq!(false, fake_goods.is_foreign_archipelago());
    }

    #[test]
    fn foreign_items_tagged() {
        let foreign_goods = ItemId::new(ItemCategory::Goods, 8_005_555).unwrap();

        assert_eq!(false, foreign_goods.is_local_archipelago());
        assert_eq!(true, foreign_goods.is_foreign_archipelago());
    }

}