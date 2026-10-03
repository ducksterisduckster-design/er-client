use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    str::FromStr,
};

use eldenring::cs::ItemId;
use serde::{Deserialize, Deserializer};

/// Setup info the Archipelago server sends for this game.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotData {
    /// Event flags that must all be set for the player to count as having won.
    pub goal: Vec<EventFlagId>,

    /// Archipelago item IDs to Elden Ring item IDs.
    pub ap_ids_to_item_ids: HashMap<I64Key, DeserializableItemId>,

    /// How many of an item each Archipelago item ID grants.
    pub item_counts: HashMap<I64Key, u32>,

    /// Progression item IDs that aren't deprioritized.
    #[serde(default)]
    pub non_deprioritized_progression_item_ids: HashSet<i64>,

    /// Real locations to the AP-only virtual locations that get checked along
    /// with them.
    #[serde(default)]
    pub virtual_location_triggers: HashMap<I64Key, Vec<i64>>,

    /// Virtual locations that hold this player's own items. The server doesn't
    /// send those back, so the client grants them itself when the location is
    /// checked.
    #[serde(default)]
    pub local_virtual_location_items: HashMap<I64Key, i64>,

    /// Virtual locations that hold another player's item, mapped to a display
    /// item (a goods row baked into regulation.bin by the static randomizer) so
    /// the picker still sees a pickup pop-up.
    #[serde(default)]
    pub virtual_location_display_items: HashMap<I64Key, DeserializableItemId>,

    /// This player's options.
    pub options: Options,
}

impl SlotData {
    pub fn expand_virtual_location_checks(&self, locations: &mut HashSet<i64>) -> bool {
        expand_virtual_location_checks(locations, &self.virtual_location_triggers)
    }

    pub fn virtual_location_trigger(&self, virtual_location_id: i64) -> Option<i64> {
        self.virtual_location_triggers
            .iter()
            .find_map(|(trigger, children)| {
                children.contains(&virtual_location_id).then_some(trigger.0)
            })
    }

    pub fn local_virtual_location_item(&self, virtual_location_id: i64) -> Option<i64> {
        self.local_virtual_location_items
            .get(&I64Key(virtual_location_id))
            .copied()
    }

    pub fn virtual_location_display_item(&self, virtual_location_id: i64) -> Option<ItemId> {
        self.virtual_location_display_items
            .get(&I64Key(virtual_location_id))
            .map(|display_item| display_item.0)
    }
}

fn expand_virtual_location_checks(
    locations: &mut HashSet<i64>,
    virtual_location_triggers: &HashMap<I64Key, Vec<i64>>,
) -> bool {
    let mut changed = false;
    let mut pending = locations.iter().copied().collect::<Vec<_>>();

    while let Some(location_id) = pending.pop() {
        let Some(children) = virtual_location_triggers.get(&I64Key(location_id)) else {
            continue;
        };

        for &child in children {
            if locations.insert(child) {
                changed = true;
                pending.push(child);
            }
        }
    }

    changed
}

/// An Elden Ring event flag ID.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct EventFlagId(pub u32);

impl From<EventFlagId> for u32 {
    fn from(flag: EventFlagId) -> u32 {
        flag.0
    }
}

#[derive(Debug, Deserialize)]
pub struct Options {
    /// Whether the player expects the Elden Ring DLC to be installed.
    #[serde(deserialize_with = "int_to_bool")]
    pub enable_dlc: bool,
}

/// Reads an integer as a bool.
fn int_to_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(u64::deserialize(deserializer)? != 0)
}

#[derive(Debug, Clone, Copy, Deserialize, Hash, PartialEq, Eq)]
#[serde(try_from = "&str")]
#[repr(transparent)]
pub struct I64Key(pub i64);

impl TryFrom<&str> for I64Key {
    type Error = <i64 as FromStr>::Err;

    fn try_from(value: &str) -> Result<I64Key, Self::Error> {
        Ok(I64Key(i64::from_str(value)?))
    }
}

/// A deserializable wrapper around [ItemId].
#[derive(Debug, Deserialize)]
#[serde(try_from = "u32")]
#[repr(transparent)]
pub struct DeserializableItemId(pub ItemId);

impl TryFrom<u32> for DeserializableItemId {
    type Error = <ItemId as TryFrom<u32>>::Error;

    fn try_from(value: u32) -> Result<DeserializableItemId, Self::Error> {
        Ok(DeserializableItemId(value.try_into()?))
    }
}
