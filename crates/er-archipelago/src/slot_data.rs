use std::{
    collections::HashMap,
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

    /// This player's options.
    pub options: Options,
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
