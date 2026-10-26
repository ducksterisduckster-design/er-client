use std::{collections::HashMap, sync::LazyLock};

use archipelago_rs::TextColor;
use eldenring::cs::{CSEventFlagMan, ChrInsExt, PlayerIns};
use fromsoftware_shared::FromStatic;

/// Starting speffect for status traps.
const APPLY_EFFECT_BASE: i32 = 81000010;

/// Marker speffect to know when to allow deathblight.
/// This logic could be done on the client, but we would have to reset on death.
const HEATUP_EFFECT: i32 = 81000019;

/// Status effects which can be detected and applied.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
pub enum StatusEffect {
    Deathblight = 0,
    Poison,
    Rot,
    Bleed,
    Frost,
    Sleep,
    Madness,
}

static STATUS_BY_STATE_INFO: LazyLock<HashMap::<u16, StatusEffect>> = LazyLock::new(|| { HashMap::from([
    (2, StatusEffect::Poison),
    (5, StatusEffect::Rot),
    (6, StatusEffect::Bleed),
    (117, StatusEffect::Deathblight),
    (260, StatusEffect::Frost),
    (436, StatusEffect::Sleep),
    (437, StatusEffect::Madness),
])});

/// Get all status effects currently applied to the player.
///
/// Some of these are very short-lived, but if they cause a death they
/// will usually show up on the frame the player dies.
pub fn get_status_effects(player: &PlayerIns) -> Vec<StatusEffect> {
    player.chr_ins.special_effect.as_ref().entries()
        .flat_map(|sp| sp.param_data)
        .map(|sp_row| unsafe { sp_row.as_ref() }.state_info())
        .flat_map(|state_info| STATUS_BY_STATE_INFO.get(&state_info))
        .copied()
        .collect()
}

/// Apply the given status effect to the player.
fn apply_status_effect(player: &mut PlayerIns, status: StatusEffect) {
    let sp_effect = APPLY_EFFECT_BASE + (status as i32);
    player.apply_speffect(sp_effect, true);
    player.apply_speffect(HEATUP_EFFECT, true);
}

/// Randomly picks and applies a status effect to the player, returning the status if successful.
pub fn apply_random_status_effect(player: &mut PlayerIns) -> Option<StatusEffect> {
    let Ok(events) = (unsafe { CSEventFlagMan::instance_mut() }) else {
        return None;
    };

    // Don't apply when sitting at a grace. It's okay to apply immediately on standing up.
    if events.virtual_memory_flag.get_flag(9000) {
        return None;
    }

    let mut possible_statuses: Vec<StatusEffect> = STATUS_BY_STATE_INFO.values().copied().collect();
    let mut exclude_statuses = get_status_effects(player);
    // Don't give deathblight by default. Allow it only for multiple traps received in 1 minute.
    if !player.chr_ins.special_effect.entries().any(|sp| sp.param_id == HEATUP_EFFECT) {
        exclude_statuses.push(StatusEffect::Deathblight);
    }
    possible_statuses.retain(|status| !exclude_statuses.contains(status));
    let chosen = fastrand::choice(possible_statuses);
    chosen.map(|status| apply_status_effect(player, status));
    chosen
}

static STATUS_COLORS: LazyLock<HashMap::<StatusEffect, TextColor>> = LazyLock::new(|| { HashMap::from([
    (StatusEffect::Poison, TextColor::Green),
    (StatusEffect::Rot, TextColor::Red),
    (StatusEffect::Bleed, TextColor::Red),
    (StatusEffect::Deathblight, TextColor::Black),
    (StatusEffect::Frost, TextColor::Cyan),
    (StatusEffect::Sleep, TextColor::Blue),
    (StatusEffect::Madness, TextColor::Yellow),
])});

/// Scientifically accurate determination of status effect color.
pub fn get_status_color(status: StatusEffect) -> TextColor {
    STATUS_COLORS.get(&status).cloned().unwrap_or(TextColor::Red)
}
