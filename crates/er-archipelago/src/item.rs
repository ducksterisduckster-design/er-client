use eldenring::cs::{
    ItemBuffer, ItemCategory, ItemId, MAP_ITEM_MAN_GRANT_ITEM_VA, MapItemMan, MapItemManEntry, SoloParamRepository,
};
use fromsoftware_shared::FromStatic;
use ilhook::x64::*;
use log::*;

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

/// A DS3-style wrapper around ER's param repository.
pub struct RegulationManager(&'static SoloParamRepository);

impl RegulationManager {
    pub fn instance() -> Option<Self> {
        Some(Self(unsafe { SoloParamRepository::instance() }.ok()?))
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