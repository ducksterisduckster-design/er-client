//! Flag-based check detection.

//! `process_inventory_items` in `core.rs` spots a check when a placeholder item
//! lands in the inventory. That works for normal pickups, but not for item lots
//! and shop purchases that only show a popup or shop listing without granting
//! anything. Those checks are found by watching the game's own event flags.

//! Flags and locations are many-to-many (one flag covers a whole armor set, and
//! an item with two sources, like the Talisman Pouch, can have two flags for
//! one location). So we build a `location -> flags` map, and a location counts
//! as checked if any of its flags is set.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem;
use std::sync::{LazyLock, Mutex};

use eldenring::cs::{ItemLotParam_enemy, ItemLotParam_map, ShopLineupParam, SoloParamRepository};
use eldenring::param::ITEMLOT_PARAM_ST;
use fromsoftware_shared::{FromStatic, Program};
use shared::hook;
use log::*;
use pelite::pe::Pe;

use crate::item::{MenuGaitemList, RegulationManager};
use crate::rva;
use crate::slot_data::EventFlagId;

/// The top byte of `get_item_flag_id08` that marks a row as carrying an encoded
/// location ID instead of real 8th-slot data.
const LOCATION_TAG: u32 = 0xE000_0000;
const LOCATION_TAG_MASK: u32 = 0xFF00_0000;
const LOCATION_HIGH_BITS_MASK: u32 = 0x00FF_FFFF;

/// Flag change state which starts out empty and prioritizes flags to check in the core loop.
/// Reads and writes both occur in the game thread so a Mutex is sufficient.
static INSTANCE: LazyLock<Mutex<LocationFlagChanges>> = LazyLock::new(|| Mutex::new(Default::default()));

/// Pending flag changes which may correspond to locations getting checked.
/// To simplify concurrency only static methods are exposed.
#[derive(Debug, Default)]
pub struct LocationFlagChanges {
    /// All flags to report locations changes on.
    tracked_flags: HashSet<EventFlagId>,

    /// All watched flags which were set individually in-game functions since last processed.
    /// Calling code should verify the flag is actually set. Note event value base flags can
    /// be unset for values >1, but all tracked shop flags have quantity 1 currently.
    set_flags: HashSet<EventFlagId>,

    /// All watched flags which were viewed as a hint from a shop. These flags may or may
    /// not have been set.
    hint_flags: HashSet<EventFlagId>,
}

impl LocationFlagChanges {
    /// Mark the given flag as changed if it is being tracked.
    pub fn change_flag(flag: EventFlagId) {
        let mut changes = INSTANCE.lock().unwrap();
        if changes.tracked_flags.contains(&flag) {
            changes.set_flags.insert(flag);
        }
    }

    /// Mark the given flag as hinted if it is being tracked.
    pub fn hint_flag(flag: EventFlagId) {
        let mut changes = INSTANCE.lock().unwrap();
        if changes.tracked_flags.contains(&flag) {
            changes.hint_flags.insert(flag);
        }
    }

    pub fn add_tracked_flags<T>(flags: T) where T : IntoIterator<Item = EventFlagId> {
        let mut changes = INSTANCE.lock().unwrap();
        changes.tracked_flags.extend(flags);
    }

    /// Take all changed flags added since the last time this was called.
    pub fn take_changed_flags() -> Vec<EventFlagId> {
        let mut changes = INSTANCE.lock().unwrap();
        changes.set_flags.drain().collect()
    }

    /// Take all hinted flags added since the last time this was called.
    pub fn take_hinted_flags() -> Vec<EventFlagId> {
        let mut changes = INSTANCE.lock().unwrap();
        changes.hint_flags.drain().collect()
    }
}

type SetEventFlagFn = unsafe extern "C" fn(event_flag_man: *mut c_void, flag: u32, value: i32);
type SetEventValueFn = unsafe extern "C" fn(event_flag_man: *mut c_void, flag: *const u32, width: u32, value: u32);
type GetShopMenuListFn = unsafe extern "C" fn(list: *mut *mut MenuGaitemList, shop_type: u8, start: u32, end: u32, mult: f32) -> *mut *mut MenuGaitemList;

fn set_event_flag_override(flag: u32, value: i32, original: &dyn Fn()) {
    original();
    // value is not bool and things will break if it's typed as bool.
    if value != 0 {
        LocationFlagChanges::change_flag(EventFlagId(flag));
    }
}

fn set_event_value_override(flag: *const u32, value: u32, original: &dyn Fn()) {
    original();
    if value > 0 {
        // Safety: The flag is unconditionally dereferenced in the expected original fn.
        let flag = unsafe { *flag };
        LocationFlagChanges::change_flag(EventFlagId(flag));
    }
}

fn get_shop_menu_list_override(original: &dyn Fn() -> *mut *mut MenuGaitemList) -> *mut *mut MenuGaitemList {
    let list_ptr = original();
    let Some(params) = unsafe { SoloParamRepository::instance() }.ok() else {
        return list_ptr;
    };
    if let Some(list) = unsafe { list_ptr.as_ref().and_then(|l| l.as_ref()) } {
        log::info!("Got shop list {:p} {:p} {:p}", &list.items.base.begin, &list.items.base.end, &list.items.base.capacity);
        for item in list.items.items() {
            log::info!("{}: Got shop {:?} {}x at {}", item.shop_lineup_param, item.id, item.quantity, item.price);
            if let Some(shop_row) = params.get::<ShopLineupParam>(item.shop_lineup_param)
                && shop_row.event_flag_for_stock() > 0 {
                LocationFlagChanges::hint_flag(EventFlagId(shop_row.event_flag_for_stock()));
            }
        }
    }
    list_ptr
}

/// Hooks event flag changes which can detect item pickups and shop purchases
/// immediately. These are tracked in the [LocationFlagChanges] instance.
pub unsafe fn hook_flag_changes() {
    let program = Program::current();
    let rvas = rva::get();
    let set_event_flag_addr = program.rva_to_va(rvas.set_event_flag).unwrap();
    let set_event_value_addr = program.rva_to_va(rvas.set_event_value).unwrap();
    let get_shop_menu_list_addr = program.rva_to_va(rvas.get_shop_menu_list).unwrap();
    unsafe {
        let set_event_flag = mem::transmute::<u64, SetEventFlagFn>(set_event_flag_addr);
        let set_event_value = mem::transmute::<u64, SetEventValueFn>(set_event_value_addr);
        let get_shop_menu_list = mem::transmute::<u64, GetShopMenuListFn>(get_shop_menu_list_addr);
        // This uses winhook instead of ilhook as it provides slightly higher-level access
        // to function args, is performant and thread-safe, etc. Both are fine to use together,
        // but committing to one or the other would be fine too.
        hook::hook(
            set_event_flag,
            |original| {
                move |event_flag_man, flag, value|
                    set_event_flag_override(flag, value, &|| original(event_flag_man, flag, value))
            });
        hook::hook(
            set_event_value,
            |original| {
                move |event_flag_man, flag, width, value|
                    set_event_value_override(flag, value, &|| original(event_flag_man, flag, width, value))
            });
        hook::hook(
            get_shop_menu_list,
            |original| {
                move |list, shop_type, start, end, mult|
                    get_shop_menu_list_override(&|| original(list, shop_type, start, end, mult))
            });
    }
}

/// Many-to-many mapping between location ids and event flags baked into
/// regulation data.
pub struct LocationFlagMapping {
    pub location_flags: HashMap<i64, Vec<EventFlagId>>,
    pub flag_locations: HashMap<EventFlagId, Vec<i64>>,
}

impl LocationFlagMapping {
    pub fn new(mapping: HashSet<(i64, EventFlagId)>) -> Self {
        let mut location_flags: HashMap<i64, Vec<EventFlagId>> = HashMap::new();
        let mut flag_locations: HashMap<EventFlagId, Vec<i64>> = HashMap::new();
        for (location, flag) in mapping {
            location_flags.entry(location).or_default().push(flag);
            flag_locations.entry(flag).or_default().push(location);
        }
        Self { location_flags, flag_locations }
    }
}

/// If the randomizer tagged `row` with a location ID, decodes it and returns it
/// with the event flag to poll for it.
fn decode_archipelago_row(row: &ITEMLOT_PARAM_ST) -> Option<(i64, EventFlagId)> {
    let high = row.get_item_flag_id08();
    if high & LOCATION_TAG_MASK != LOCATION_TAG {
        return None;
    }

    let high_bits = (high & LOCATION_HIGH_BITS_MASK) as i64;
    // The bindings make `lot_item_id08` signed, but what was stored is the raw
    // bit pattern. Reinterpret it as unsigned before widening.
    let low_bits = row.lot_item_id08() as u32 as i64;
    let location_id = (high_bits << 32) | low_bits;

    Some((location_id, EventFlagId(row.get_item_flag_id())))
}

/// Scans every row of one `ItemLotParam` table and records the
/// randomizer-tagged ones in `map`.
fn collect_location_flags<P>(regulation: &RegulationManager, mapping: &mut HashSet<(i64, EventFlagId)>)
where
    P: eldenring::cs::SoloParam<StructType = ITEMLOT_PARAM_ST>,
{
    for (_, row) in regulation.rows::<P>() {
        mapping.extend(decode_archipelago_row(row));
    }
}

/// Builds the `location ID -> flags` map from the tagged rows in
/// `ItemLotParam_map` and `ItemLotParam_enemy` (which also holds the fake rows standing in for shop entries).

/// Only call this once per session, since the regulation doesn't change while
/// the game runs. Cache the result instead of rebuilding it every tick.
pub fn build_location_flag_map(regulation: &RegulationManager) -> LocationFlagMapping {
    let mut mapping = HashSet::new();
    collect_location_flags::<ItemLotParam_map>(regulation, &mut mapping);
    collect_location_flags::<ItemLotParam_enemy>(regulation, &mut mapping);

    let mapping = LocationFlagMapping::new(mapping);
    info!(
        "Built Archipelago location/flag map: {} location(s), {} flag(s)",
        mapping.location_flags.len(),
        mapping.flag_locations.len()
    );

    mapping
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Makes a zeroed `ITEMLOT_PARAM_ST` and applies the given setters. Zeroing
    /// is fine here: the struct is `#[repr(C)]` and all plain integers, so all-zero is always a valid value.
    
    fn row(set: impl FnOnce(&mut ITEMLOT_PARAM_ST)) -> ITEMLOT_PARAM_ST {
        let mut row = unsafe { std::mem::zeroed::<ITEMLOT_PARAM_ST>() };
        set(&mut row);
        row
    }

    #[test]
    fn decodes_a_tagged_row() {
        let location_id: i64 = 0x00AB_CDEF_1234_5678;
        let flag = 92_000u32;

        let row = row(|r| {
            r.set_lot_item_id08((location_id & 0xFFFF_FFFF) as i32);
            r.set_get_item_flag_id08(LOCATION_TAG | ((location_id >> 32) as u32));
            r.set_get_item_flag_id(flag);
        });

        let decoded = decode_archipelago_row(&row);
        assert_eq!(decoded, Some((location_id, EventFlagId(flag))));
    }

    #[test]
    fn decodes_a_location_id_with_high_bit_of_low_word_set() {
        // `lot_item_id08` is signed, so a location ID whose low 32 bits have
        // the top bit set comes back as a negative `i32`. Make sure that  doesn't get sign-extended.
        
        let location_id: i64 = 0x0000_0001_8000_0000;

        let row = row(|r| {
            r.set_lot_item_id08((location_id & 0xFFFF_FFFF) as i32); // negative as i32
            r.set_get_item_flag_id08(LOCATION_TAG | ((location_id >> 32) as u32));
        });

        assert_eq!(decode_archipelago_row(&row).map(|(id, _)| id), Some(location_id));
    }

    #[test]
    fn ignores_an_untagged_row() {
        // A row that happens to use a real flag ID in its 8th slot, with no
        // `0xE` tag, mustn't be read as an encoded location.
        let row = row(|r| {
            r.set_get_item_flag_id08(12_345);
            r.set_lot_item_id08(6789);
        });

        assert_eq!(decode_archipelago_row(&row), None);
    }

    #[test]
    fn ignores_a_zeroed_row() {
        // Most real rows have both fields at 0 (8th slot unused). Make sure
        // that isn't read as location 0 tagged with `0xE0000000`.
        let row = row(|_| {});
        assert_eq!(decode_archipelago_row(&row), None);
    }
}
