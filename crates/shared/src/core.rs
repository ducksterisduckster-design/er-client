use std::collections::VecDeque;
use std::time::{Duration, Instant};
use std::{io, iter::ExactSizeIterator, mem};

use anyhow::{Error, Result, bail};
use archipelago_rs::{self as ap, RichText};
use log::*;
use serde::de::DeserializeOwned;
use ustr::Ustr;

use crate::{Game, SectionProfiler, config::Config};

/// The maximum number of log messages to store.
const LOG_BUFFER_LIMIT: usize = 1000;

/// The base struct for implementations of [Core].
pub struct CoreBase<G: Game, S: DeserializeOwned + Send + 'static> {
    /// The name of the game that's being played.
    game: Ustr,

    /// The configuration for the current Archipelago connection. This is not
    /// guaranteed to be complete *or* accurate; it's the mod's responsibility
    /// to ensure it makes sense before actually interacting with an individual
    /// game.
    config: Config<G>,

    /// The Archipelago client connection.
    connection: ap::Connection<S>,

    /// The log of prints that can be displayed in the overlay, along with the
    /// times they were received.
    log_buffer: VecDeque<(ap::Print, Instant)>,

    /// Events we're waiting to process until the player loads a save. This is
    /// always empty unless a connection is connected and the player is on the
    /// main menu (or in the initial waiting period during a load).
    event_buffer: Vec<ap::Event>,

    /// The time at which we noticed the game loading (as indicated by
    /// MapItemMan coming into existence). Used to compute the grace period
    /// before we start doing stuff in game. None if the game is not currently
    /// loaded.
    load_time: Option<Instant>,

    /// The fatal error that this has encountered, if any. If this is not
    /// `None`, most in-game processing will be disabled.
    error: Option<Error>,

    /// A profiler that can be used to track how long various sections of the
    /// mod take to run.
    profiler: SectionProfiler,
}

/// A full-screen color overlay drawn behind the Archipelago UI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenEffect {
    pub color: [u8; 3],
    pub opacity: f32,
}

impl<G: Game, S: DeserializeOwned + Send + 'static> CoreBase<G, S> {
    /// Creates a new instance of [CoreBase].
    pub fn new(game: impl Into<Ustr>) -> Result<Self> {
        let game = game.into();
        let config = Config::load()?;
        let connection = Self::new_connection(game, &config);

        Ok(Self {
            game,
            config,
            connection,
            log_buffer: Default::default(),
            event_buffer: vec![],
            load_time: None,
            error: None,
            profiler: Default::default(),
        })
    }

    /// Creates a new [ClientConnection] based on the connection information in [config].
    fn new_connection(game: Ustr, config: &Config<G>) -> ap::Connection<S> {
        let mut tags = vec![];
        if config.death_link_enabled() {
            tags.push("DeathLink");
        }

        let mut options = ap::ConnectionOptions::new()
            .receive_items(ap::ItemHandling::OtherWorlds {
                own_world: false,
                starting_inventory: true,
            })
            .tags(tags);
        if let Some(password) = config.password() {
            options = options.password(password);
        }

        ap::Connection::new(config.url(), config.slot(), Some(game), options)
    }

    /// The section profiler.
    pub fn profiler(&mut self) -> &mut SectionProfiler {
        &mut self.profiler
    }

    /// Returns the current connection type.
    pub(crate) fn connection_state_type(&self) -> ap::ConnectionStateType {
        self.connection.state_type()
    }

    /// Returns whether the current connection is disconnected.
    pub(crate) fn is_disconnected(&self) -> bool {
        self.connection.is_disconnected()
    }

    /// Retries the Archipelago connection with the same information.
    pub(crate) fn reconnect(&mut self) {
        if self.connection_state_type() == ap::ConnectionStateType::Disconnected {
            self.log("Reconnecting...");
        }

        self.connection = Self::new_connection(self.game, &self.config);
    }

    /// Updates the URL to use to connect to Archipelago and reconnects the
    /// Archipelago session.
    pub(crate) fn update_url(&mut self, url: impl AsRef<str>) -> Result<()> {
        if self.connection_state_type() == ap::ConnectionStateType::Disconnected {
            self.log("Reconnecting...");
        }

        self.config.set_url(url);
        self.config.save()?;
        self.connection = Self::new_connection(self.game, &self.config);
        Ok(())
    }

    /// Updates whether incoming items should use the game's normal item pickup
    /// popups.
    pub(crate) fn update_show_item_popups(&mut self, show_item_popups: bool) -> Result<()> {
        self.config.set_show_item_popups(show_item_popups);
        self.config.save()
    }

    /// Updates whether progression incoming items should use the game's normal
    /// item pickup popups when regular incoming item popups are disabled.
    pub(crate) fn update_show_progression_item_popups(
        &mut self,
        show_progression_item_popups: bool,
    ) -> Result<()> {
        self.config
            .set_show_progression_item_popups(show_progression_item_popups);
        self.config.save()
    }

    /// Updates whether DeathLink is enabled for this connection and
    /// reconnects, since the "DeathLink" tag we advertise to the server is
    /// only set at connection time.
    pub(crate) fn update_death_link_enabled(&mut self, death_link_enabled: bool) -> Result<()> {
        self.config.set_death_link_enabled(death_link_enabled);
        self.config.save()?;
        self.connection = Self::new_connection(self.game, &self.config);
        Ok(())
    }

    /// If this client has encountered a fatal error, takes ownership of it.
    pub(crate) fn take_error(&mut self) -> Option<Error> {
        if let Some(err) = self.error.take() {
            self.error = Some(ap::Error::Elsewhere.into());
            Some(err)
        } else {
            None
        }
    }

    /// Returns the current user config.
    pub fn config(&self) -> &Config<G> {
        &self.config
    }

    /// Returns whether incoming items should use the game's normal item pickup
    /// popups.
    pub fn show_item_popups(&self) -> bool {
        self.config.show_item_popups()
    }

    /// Returns whether progression incoming items should use the game's normal
    /// item pickup popups when regular incoming item popups are disabled.
    pub fn show_progression_item_popups(&self) -> bool {
        self.config.show_progression_item_popups()
    }

    /// Returns whether the player has been loaded into the game for the given
    /// duration. This should be used in [Core::update_live] to prevent instant
    /// in-game popups and otherwise allow game state to catch up before processing.
    pub fn past_grace_period(&self, duration: Duration) -> bool {
        self.load_time.map(|time| time.elapsed() > duration).unwrap_or(false)
    }

    /// Returns the list of all logs that have been emitted in the current
    /// session.
    pub(crate) fn logs(&self) -> impl ExactSizeIterator<Item = &(ap::Print, Instant)> {
        self.log_buffer.iter()
    }

    /// Updates the Archipelago connection, adds any events that need processing
    /// to [event_buffer].
    ///
    /// This is always run regardless of whether the client is connected or the
    /// mod has experienced a fatal error.
    fn update_always(&mut self) {
        use ap::Event::*;
        let mut state = self.connection.state_type();
        let mut events = self.connection.update();

        // Process events that should happen even when the player isn't in an
        // active save.
        for event in events.extract_if(.., |e| matches!(e, Connected | Error(_) | Print(_))) {
            match event {
                Connected => {
                    state = ap::ConnectionStateType::Connected;
                }
                Error(err) if err.is_fatal() => {
                    let err = self.connection.err();
                    self.log(
                        if let ap::Error::WebSocket(tungstenite::Error::Io(io)) = err
                            && matches!(
                                io.kind(),
                                io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
                            )
                        {
                            vec![
                                ap::RichText::Color {
                                    text: "Connection refused. ".into(),
                                    color: ap::TextColor::Red,
                                },
                                "Make sure the server session is running and the URL is \
                                 up-to-date."
                                    .into(),
                            ]
                        } else if state == ap::ConnectionStateType::Connected {
                            vec![
                                ap::RichText::Color {
                                    text: "Connection failed: ".into(),
                                    color: ap::TextColor::Red,
                                },
                                err.to_string().into(),
                            ]
                        } else {
                            vec![
                                ap::RichText::Color {
                                    text: "Disconnected: ".into(),
                                    color: ap::TextColor::Red,
                                },
                                err.to_string().into(),
                            ]
                        },
                    );
                    self.event_buffer.clear();
                }
                Error(err) => self.log(err.to_string()),
                Print(print) => {
                    info!("[APS] {print}");
                    if !self.should_suppress_print(&print) {
                        if self.log_buffer.len() >= LOG_BUFFER_LIMIT {
                            self.log_buffer.pop_front();
                        }
                        self.log_buffer.push_back((print, Instant::now()));
                    }
                }
                _ => {}
            }
        }

        if state == ap::ConnectionStateType::Connected {
            self.event_buffer.extend(events);
        } else {
            debug_assert!(self.event_buffer.is_empty());
        }
    }

    /// Returns an error if the user's static randomizer version doesn't match
    /// this mod's version.
    fn check_version_conflict(&self, expected_version: &str) -> Result<()> {
        if let Some(client_version) = self.config().client_version()
            && client_version != expected_version
        {
            bail!(
                "Your apconfig.json was generated using static randomizer v{}, but this client is \
                 v{}. Re-run the static randomizer with the current version.",
                client_version,
                expected_version,
            );
        } else {
            Ok(())
        }
    }

    /// Whether to suppress non-actionable server messages to prevent client
    /// user/developer toil. This appears in server logs and local logs so
    /// interested parties can still track it.
    fn should_suppress_print(&self, print: &ap::Print) -> bool {
        // TODO: Update to websocket library supporting it as soon as available.
        // Sorry, website hosts.
        print.data().iter().any(|part|
            matches!(part, RichText::Text(text) if text.contains("Warning: your client does not support compressed websocket connections")))
    }

    /// Writes a message to the log buffer that we display to the user in the
    /// overlay, as well as to the internal logger.
    fn log(&mut self, message: impl Into<ap::Print>) {
        let print = message.into();
        info!("[APC] {print}");
        // Consider making this a circular buffer if it ends up eating too much
        // memory over time.
        if self.log_buffer.len() >= LOG_BUFFER_LIMIT {
            self.log_buffer.pop_front();
        }
        self.log_buffer.push_back((print, Instant::now()));
    }
}

/// A trait for the core runners of FromSoftware game mods. This encapsulates
/// the interface that the shared overlay logic needs to interact with these
/// games.
pub trait Core: Send + Sized {
    /// The slot data for this runner.
    type SlotData: DeserializeOwned + Send + 'static;

    /// The game this is for.
    type Game: Game;

    /// Creates a new instance of the mod.
    fn new() -> Result<Self>;

    /// Returns the base struct.
    fn base(&self) -> &CoreBase<Self::Game, Self::SlotData>;

    /// Returns the mutable base struct.
    fn base_mut(&mut self) -> &mut CoreBase<Self::Game, Self::SlotData>;

    /// Updates the game logic and checks for common errors. This is only run if
    /// we're currently connected to the Archipelago server and the mod has not
    /// encountered a fatal error.
    fn update_live(&mut self) -> Result<()>;

    /// Implementors may override this to handles custom command inputs via the
    /// say console. Returns whether a command was handled.
    ///
    /// By default, this doesn't handle any commands.
    fn handle_command(&mut self, _command: &str, _arg: Option<&str>) -> bool {
        false
    }

    /// Returns a full-screen effect to draw over the game view but behind the
    /// Archipelago UI.
    fn screen_effect(&self) -> Option<ScreenEffect> {
        None
    }

    /// Returns a reference to the Archipelago client, if it's connected.
    fn client(&self) -> Option<&ap::Client<Self::SlotData>> {
        self.base().connection.client()
    }

    /// Returns a mutable reference to the Archipelago client, if it's connected.
    fn client_mut(&mut self) -> Option<&mut ap::Client<Self::SlotData>> {
        self.base_mut().connection.client_mut()
    }

    /// Returns the seed the game expects to connect to.
    fn seed(&self) -> &str {
        self.base().config.seed()
    }

    /// Writes a message to the log buffer that we display to the user in the
    /// overlay, as well as to the internal logger.
    fn log(&mut self, message: impl Into<ap::Print>) {
        self.base_mut().log(message);
    }

    /// Consumes and returns all the as-yet-unprocessed events from the player's
    /// save.
    ///
    /// As a side effect, this also handles [ap::Event::DeathLink]: if the
    /// player has DeathLink enabled, it kills the local player via
    /// [Game::kill_player]. This happens here rather than requiring each game
    /// to match on it in the events it gets back, since killing the player
    /// doesn't need any game-specific state. Since DeathLink events are only
    /// ever buffered while connected (see [CoreBase::update_always]), and this
    /// is only ever called from [Core::update_live] -- which only runs once a
    /// save is loaded and past the initial grace period -- it's safe to touch
    /// the game's in-memory player state here.
    fn take_events(&mut self) -> Vec<ap::Event> {
        let events = mem::take(&mut self.base_mut().event_buffer);

        if self.base().config().death_link_enabled() {
            // The server bounces our own DeathLinks back to us, so we have to
            // recognize and skip them or we'd be killed by our own death.
            let own_alias = self
                .client()
                .map(|client| client.this_player().alias().to_string());

            for event in &events {
                let ap::Event::DeathLink { source, cause, .. } = event else {
                    continue;
                };

                if Some(source) == own_alias.as_ref() {
                    continue;
                }

                let reason = cause.clone().unwrap_or_else(|| format!("{source} died."));
                self.log(vec![
                    ap::RichText::Color {
                        text: "DeathLink: ".into(),
                        color: ap::TextColor::Red,
                    },
                    reason.into(),
                ]);

                // Safety: see the doc comment above.
                unsafe {
                    Self::Game::kill_player();
                }
            }
        }

        events
    }

    /// Runs the core logic of the mod. This may set [error], which should be
    /// surfaced to the user. Implementations should not override this; they
    /// should override [Self::update_live] instead.
    fn update(&mut self, is_main_menu: bool) {
        self.base_mut().update_always();

        if self.base().connection.client().is_none() || self.base().error.is_some() {
            return;
        }

        if is_main_menu {
            self.base_mut().load_time = None;
            // Nobody's playing, so a DeathLink that arrives now is stale by the
            // time a save loads. Don't let it kill the player on load.
            self.base_mut()
                .event_buffer
                .retain(|event| !matches!(event, ap::Event::DeathLink { .. }));
        } else if self.base().load_time.is_none() {
            self.base_mut().load_time = Some(Instant::now());
        }

        self.base_mut().error = match self
            .base()
            .check_version_conflict(Self::Game::CLIENT_VERSION)
        {
            Err(err) => Some(err),
            Ok(_) => self.update_live().err(),
        }
    }
}