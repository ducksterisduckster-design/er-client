use std::{fs, io, marker::PhantomData, path::PathBuf};

use anyhow::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::{Game, utils};

fn default_show_item_popups() -> bool {
    true
}

fn default_show_progression_item_popups() -> bool {
    false
}

fn default_death_link_enabled() -> bool {
    true
}

/// The configuration file for the Archipelago connection.
#[derive(Deserialize, Serialize)]
pub struct Config<G: Game> {
    url: String,
    slot: String,
    seed: String,
    client_version: Option<String>,
    password: Option<String>,
    #[serde(default = "default_show_item_popups")]
    show_item_popups: bool,
    #[serde(default = "default_show_progression_item_popups")]
    show_progression_item_popups: bool,
    #[serde(default = "default_death_link_enabled")]
    death_link_enabled: bool,
    #[serde(skip)]
    _marker: PhantomData<G>,
}

impl<G: Game> Config<G> {
    /// Loads the config from disk.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        match fs::read_to_string(&path) {
            Ok(text) => json::from_str(&text).map_err(|err| {
                Error::from(err).context(format!(
                    "Failed to parse config file {}",
                    path.to_string_lossy()
                ))
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                Err(Error::from(err).context(format!(
                    "{} doesn't exist. Have you run randomizer\\{}?",
                    path.to_string_lossy(),
                    G::TYPE.static_randomizer_basename()
                )))
            }
            Err(err) => Err(Error::from(err).context(format!(
                "Failed to load config file {}",
                path.to_string_lossy()
            ))),
        }
    }

    /// Saves the config file to disk.
    pub fn save(&self) -> Result<()> {
        Ok(fs::write(Self::path()?, json::to_string(self)?)?)
    }

    /// The path to the configuration file.
    fn path() -> Result<PathBuf> {
        Ok(utils::mod_directory()?.join("apconfig.json"))
    }

    /// Returns the Archipelago server URL defined in the config, or None if it
    /// doesn't contain a URL.
    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    /// Sets the Archipelago server URL in the config file.
    pub fn set_url(&mut self, url: impl AsRef<str>) {
        self.url = url.as_ref().to_string()
    }

    /// Returns whether incoming items should use the game's normal item pickup
    /// popups.
    pub fn show_item_popups(&self) -> bool {
        self.show_item_popups
    }

    /// Sets whether incoming items should use the game's normal item pickup
    /// popups.
    pub fn set_show_item_popups(&mut self, show_item_popups: bool) {
        self.show_item_popups = show_item_popups;
    }

    /// Returns whether progression incoming items should use the game's normal
    /// item pickup popups when regular incoming item popups are disabled.
    pub fn show_progression_item_popups(&self) -> bool {
        self.show_progression_item_popups
    }

    /// Sets whether progression incoming items should use the game's normal
    /// item pickup popups when regular incoming item popups are disabled.
    pub fn set_show_progression_item_popups(&mut self, show_progression_item_popups: bool) {
        self.show_progression_item_popups = show_progression_item_popups;
    }

    /// Returns whether DeathLink is enabled for this connection.
    pub fn death_link_enabled(&self) -> bool {
        self.death_link_enabled
    }

    /// Sets whether DeathLink is enabled for this connection.
    pub fn set_death_link_enabled(&mut self, death_link_enabled: bool) {
        self.death_link_enabled = death_link_enabled;
    }

    /// Returns the slot that the config was created with.
    pub fn slot(&self) -> &str {
        self.slot.as_str()
    }

    /// Returns the seed that the config was created with.
    pub fn seed(&self) -> &str {
        self.seed.as_str()
    }

    /// Returns the version of the static randomizer that the config was created
    /// with, or None if it doesn't contain a version (such as for a local
    /// randomizer build).
    pub fn client_version(&self) -> Option<&str> {
        self.client_version.as_deref()
    }

    /// Returns the password that the config was created with, or None if it
    /// doesn't contain a password.
    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }
}
