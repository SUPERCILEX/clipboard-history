// SPDX-License-Identifier: Apache-2.0

use cosmic::cosmic_config::CosmicConfigEntry;
use cosmic::iced::futures::executor::block_on;
use cosmic::iced::keyboard::key::Named;
use cosmic::iced::keyboard::{Key, Modifiers, on_key_press};
use cosmic::iced::stream::channel;
use cosmic::iced::{Limits, Subscription, window};
use cosmic::iced_winit::commands::popup::{destroy_popup, get_popup};
use cosmic::widget::segmented_button::{Entity, SingleSelectModel};
use cosmic::widget::{MouseArea, Space};
use cosmic::{Action, cosmic_config, prelude::*};
use futures_util::SinkExt;
use ringboard_sdk::core::protocol::RingKind;
use ringboard_sdk::search::CancellationToken;
use ringboard_sdk::ui_actor::{Command, Message, UiEntry, controller};
use std::any::TypeId;
use std::ops::Deref;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::task::spawn_blocking;
use tracing::{info, warn};

use crate::config::{Config, FilterMode};
use crate::views::popup::popup_view;
use crate::views::settings::{filter_mode_model, settings_view};

pub struct AppModel {
    /// Application state which is managed by the COSMIC runtime.
    core: cosmic::Core,
    config: Config,
    config_handler: cosmic_config::Config,
    popup: Option<Popup>,
    search: String,
    filter_mode_model: SingleSelectModel,
    pending_search: Option<CancellationToken>,
    favorites: Vec<UiEntry>,
    entries: Vec<UiEntry>,
    notify: Arc<Notify>,
    command_sender: Sender<Command>,
    // we need mutex because Receiver is not Sync
    command_receiver: Arc<Mutex<Receiver<Command>>>,
}

struct Popup {
    id: window::Id,
    kind: PopupKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PopupKind {
    Search,
    Settings,
}

pub struct Flags {
    pub notify: Arc<Notify>,
    pub config: Config,
    pub config_handler: cosmic_config::Config,
}

#[derive(Debug, Clone)]
pub enum AppMessage {
    TogglePopup,
    ToggleSettings,
    ClosePopup,
    Search(String),
    SearchPending(CancellationToken),
    Items(Arc<Mutex<Vec<UiEntry>>>), // arc mutex is required because UiEntry is not cloneable and AppMessage needs to be cloneable
    Paste(u64),
    ChangeFavorite(u64, bool),
    Delete(u64),
    Deleted(u64),
    Reload,
    SelectFilterMode(Entity),
    ConfigUpdate(Config),
    KeyPressed(Key),
    Test,
}

impl AppModel {
    fn toggle_popup(&mut self, kind: PopupKind) -> Task<Action<AppMessage>> {
        info!("Toggling popup: {:?}", kind);
        match &self.popup {
            Some(popup) => {
                if popup.kind == PopupKind::Search {
                    self.close_popup()
                } else {
                    Task::batch(vec![self.close_popup(), self.open_popup(kind)])
                }
            }
            None => self.open_popup(kind),
        }
    }

    fn close_popup(&mut self) -> Task<Action<AppMessage>> {
        if let Some(Popup { id, .. }) = self.popup.take() {
            info!("Closing popup: {:?}", id);
            destroy_popup(id)
        } else {
            Task::none()
        }
    }

    fn open_popup(&mut self, kind: PopupKind) -> Task<Action<AppMessage>> {
        let id = window::Id::unique();
        self.popup.replace(Popup { id, kind });
        // directly focusing the text input does not work for some reason, so we can't select the text in the input
        // to be replaced when starting to type so we just clear the input when opening the popup
        self.search = String::new();
        // reload the first page when opening the popup again
        let _ = self.command_sender.send(Command::LoadFirstPage);

        let mut settings = self.core.applet.get_popup_settings(
            self.core.main_window_id().unwrap(),
            id,
            None,
            None,
            None,
        );

        settings.positioner.size_limits = Limits::NONE
            .min_width(300.0)
            .max_width(400.0)
            .min_height(200.0)
            .max_height(500.0);

        info!("Opening popup: {:?}", id);

        get_popup(settings)
    }
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;

    type Flags = Flags;

    type Message = AppMessage;

    const APP_ID: &'static str = "com.github.ringboard.cosmic-applet";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(core: cosmic::Core, flags: Self::Flags) -> (Self, Task<cosmic::Action<Self::Message>>) {
        let (command_sender, command_receiver) = mpsc::channel();

        let filter_mode_model = filter_mode_model(&flags.config);

        // Construct the app model with the runtime's core.
        let app = AppModel {
            core,
            config: flags.config,
            config_handler: flags.config_handler,
            popup: None,
            search: String::new(),
            filter_mode_model,
            pending_search: None,
            favorites: vec![],
            entries: vec![],
            notify: flags.notify,
            command_sender,
            command_receiver: Arc::new(Mutex::new(command_receiver)),
        };

        (app, Task::none())
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Self::Message> {
        if let Some(Popup { id: popup, .. }) = &self.popup
            && *popup == id
        {
            return Some(AppMessage::ClosePopup);
        }
        None
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let icon = self
            .core
            .applet
            .icon_button(constcat::concat!(AppModel::APP_ID, "-symbolic"))
            .on_press(AppMessage::TogglePopup);

        MouseArea::new(icon)
            .on_right_press(AppMessage::ToggleSettings)
            .into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Self::Message> {
        let Some(popup) = &self.popup else {
            return Space::new(0, 0).into();
        };

        let view = match popup.kind {
            PopupKind::Search => popup_view(
                &self.entries,
                &self.favorites,
                &self.search,
                self.core.system_theme(),
            ),
            PopupKind::Settings => settings_view(&self.filter_mode_model),
        };

        self.core.applet.popup_container(view).into()
    }

    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            AppMessage::TogglePopup => return self.toggle_popup(PopupKind::Search),
            AppMessage::ToggleSettings => return self.toggle_popup(PopupKind::Settings),
            AppMessage::ClosePopup => return self.close_popup(),
            AppMessage::Search(search) => {
                self.search = search;
                if let Some(old_token) = self.pending_search.take() {
                    info!("Cancelling previous search");
                    old_token.cancel();
                }
                info!("Starting new search: {}", self.search);
                let _ = self.command_sender.send(Command::Search {
                    query: self.search.clone().into_boxed_str(),
                    kind: self.config.filter_mode.into(),
                });
            }
            AppMessage::SearchPending(token) => {
                if let Some(old_token) = self.pending_search.replace(token) {
                    info!("Cancelling previous search");
                    old_token.cancel();
                }
            }
            AppMessage::Items(items) => {
                if let Ok(mut items) = items.lock() {
                    let entries: Vec<UiEntry> = std::mem::replace(&mut items, vec![]);
                    let (favorites, others): (Vec<_>, Vec<_>) = entries
                        .into_iter()
                        .partition(|e| e.entry.ring() == RingKind::Favorites);
                    info!(
                        "Received {} items ({} favorites, {} others)",
                        favorites.len() + others.len(),
                        favorites.len(),
                        others.len()
                    );
                    self.favorites = favorites;
                    self.entries = others;
                }
            }
            AppMessage::Paste(id) => {
                info!("Pasting item with id: {}", id);
                let _ = self.command_sender.send(Command::Paste(id));
                return self.close_popup();
            }
            AppMessage::ChangeFavorite(id, is_favorite) => {
                info!(
                    "Changing favorite status of item with id: {} to {}",
                    id, !is_favorite
                );
                if is_favorite {
                    let _ = self.command_sender.send(Command::Unfavorite(id));
                } else {
                    let _ = self.command_sender.send(Command::Favorite(id));
                }
            }
            AppMessage::Reload => {
                info!("Reloading items");
                if self.search.is_empty() {
                    let _ = self.command_sender.send(Command::LoadFirstPage);
                } else {
                    let _ = self.command_sender.send(Command::Search {
                        query: self.search.clone().into_boxed_str(),
                        kind: self.config.filter_mode.into(),
                    });
                }
            }
            AppMessage::Delete(id) => {
                info!("Deleting item with id: {}", id);
                let _ = self.command_sender.send(Command::Delete(id));
            }
            AppMessage::Deleted(id) => {
                info!("Item with id: {} deleted", id);
                self.favorites.retain(|entry| entry.entry.id() != id);
                self.entries.retain(|entry| entry.entry.id() != id);
            }
            AppMessage::SelectFilterMode(e) => {
                let mode = self.filter_mode_model.data::<FilterMode>(e);
                let Some(&mode) = mode else {
                    warn!("Invalid filter mode selected");
                    return Task::none();
                };
                info!("Changing filter mode to: {:?}", mode);
                self.filter_mode_model.activate(e);
                let _ = self.config.set_filter_mode(&self.config_handler, mode);
            }
            AppMessage::ConfigUpdate(config) => {
                info!("Config updated: {:?}", config);
                self.config = config;
            }
            AppMessage::KeyPressed(key) => {
                if let Key::Named(Named::Escape) = key {
                    return self.close_popup();
                }
            }
            AppMessage::Test => {
                info!("Test message received");
            }
        }
        Task::none()
    }

    fn style(&self) -> Option<cosmic::iced_runtime::Appearance> {
        Some(cosmic::applet::style())
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        struct RingboardSubscription;
        let command_receiver = self.command_receiver.clone();

        let ringboard_client = Subscription::run_with_id(
            TypeId::of::<RingboardSubscription>(),
            channel(10, move |mut output| async move {
                spawn_blocking(move || {
                    let command_receiver = command_receiver.lock().unwrap();
                    info!("Starting ringboard client");
                    controller::<anyhow::Error>(command_receiver.deref(), |m| {
                        match m {
                            Message::Error(e) => eprintln!("Error: {e}"),
                            Message::FatalDbOpen(e) => eprintln!("FatalDbOpen: {e}"),
                            Message::EntryDetails { id, result } => {
                                println!("EntryDetails: {id}, {result:?}")
                            }
                            Message::LoadedImage { id, .. } => println!("LoadedImage: {id}"),
                            Message::FavoriteChange(_) => {
                                block_on(output.send(AppMessage::Reload))?; // because the id of the element changes when favoriting/unfavoriting we can't just update the entry in place
                            }
                            Message::SearchResults(results) => {
                                block_on(output.send(AppMessage::Items(Arc::new(Mutex::new(
                                    results.into(),
                                )))))?;
                            }
                            Message::PendingSearch(token) => {
                                block_on(output.send(AppMessage::SearchPending(token)))?;
                            }
                            Message::LoadedFirstPage { entries, .. } => {
                                block_on(output.send(AppMessage::Items(Arc::new(Mutex::new(
                                    entries.into(),
                                )))))?;
                            }
                            Message::Deleted(id) => {
                                block_on(output.send(AppMessage::Deleted(id)))?;
                            }
                            Message::Pasted => (), // we don't need to handle this because the popup is closed immediately after sending the paste command,
                        }
                        Ok(())
                    });
                })
                .await
                .unwrap();
                info!("Ringboard client stopped");
            }),
        );

        struct WackupSubscription;

        let notify = self.notify.clone();
        let wackup = Subscription::run_with_id(
            TypeId::of::<WackupSubscription>(),
            channel(1, move |mut output| async move {
                loop {
                    notify.notified().await;
                    let _ = output.send(AppMessage::TogglePopup).await;
                }
            }),
        );

        struct ConfigSubscription;
        let config_handler = cosmic_config::config_subscription(
            TypeId::of::<ConfigSubscription>(),
            Self::APP_ID.into(),
            Config::VERSION,
        )
        .map(|update| AppMessage::ConfigUpdate(update.config));

        let keyboard_listener = on_key_press(key_press);

        Subscription::batch(vec![
            ringboard_client,
            wackup,
            config_handler,
            keyboard_listener,
        ])
    }
}

fn key_press(key: Key, _modifiers: Modifiers) -> Option<AppMessage> {
    Some(AppMessage::KeyPressed(key))
}
