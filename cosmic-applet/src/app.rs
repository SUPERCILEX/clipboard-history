// SPDX-License-Identifier: Apache-2.0

use cosmic::iced::futures::executor::block_on;
use cosmic::iced::stream::channel;
use cosmic::iced::{Limits, Subscription, futures, window};
use cosmic::iced_winit::commands::popup::{destroy_popup, get_popup};
use cosmic::widget::{MouseArea, Space};
use cosmic::{Action, prelude::*};
use futures_util::SinkExt;
use ringboard_sdk::core::protocol::RingKind;
use ringboard_sdk::search::CancellationToken;
use ringboard_sdk::ui_actor::{Command, Message, UiEntry, controller};
use std::any::TypeId;
use std::ops::Deref;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use tokio::task::spawn_blocking;

use crate::views::popup::popup_view;

pub struct AppModel {
    /// Application state which is managed by the COSMIC runtime.
    core: cosmic::Core,
    popup: Option<Popup>,
    search: String,
    pending_search: Option<CancellationToken>,
    favorites: Vec<UiEntry>,
    entries: Vec<UiEntry>,
    command_sender: Sender<Command>,
    // we need mutex because Receiver is not Sync
    command_receiver: Arc<Mutex<Receiver<Command>>>,
}

struct Popup {
    id: window::Id,
}

#[derive(Debug, Clone)]
pub enum AppMessage {
    TogglePopup,
    ClosePopup,
    Search(String),
    SearchPending(CancellationToken),
    Items(Arc<Mutex<Vec<UiEntry>>>), // arc mutex is required because UiEntry is not cloneable and AppMessage needs to be cloneable
    Paste(u64),
    ChangeFavorite(u64, bool),
    Delete(u64),
    Deleted(u64),
    Reload,
}

impl AppModel {
    fn toggle_popup(&mut self) -> Task<Action<AppMessage>> {
        match &self.popup {
            Some(_) => self.close_popup(),
            None => self.open_popup(),
        }
    }

    fn close_popup(&mut self) -> Task<Action<AppMessage>> {
        if let Some(Popup { id, .. }) = self.popup.take() {
            destroy_popup(id)
        } else {
            Task::none()
        }
    }

    fn open_popup(&mut self) -> Task<Action<AppMessage>> {
        let id = window::Id::unique();
        self.popup.replace(Popup { id });
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

        get_popup(settings)
    }
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;

    type Flags = ();

    type Message = AppMessage;

    const APP_ID: &'static str = "com.github.ringboard.cosmic-applet";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(
        core: cosmic::Core,
        _flags: Self::Flags,
    ) -> (Self, Task<cosmic::Action<Self::Message>>) {
        let (command_sender, command_receiver) = mpsc::channel();

        // Construct the app model with the runtime's core.
        let app = AppModel {
            core,
            popup: None,
            search: String::new(),
            pending_search: None,
            favorites: vec![],
            entries: vec![],
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

        MouseArea::new(icon).into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Self::Message> {
        let Some(_) = &self.popup else {
            return Space::new(0, 0).into();
        };

        let view = popup_view(
            &self.entries,
            &self.favorites,
            &self.search,
            self.core.system_theme(),
        );

        self.core.applet.popup_container(view).into()
    }

    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            AppMessage::TogglePopup => return self.toggle_popup(),
            AppMessage::ClosePopup => return self.close_popup(),
            AppMessage::Search(search) => {
                self.search = search;
                if let Some(old_token) = self.pending_search.take() {
                    old_token.cancel();
                }
                let _ = self.command_sender.send(Command::Search {
                    query: self.search.clone().into_boxed_str(),
                    kind: ringboard_sdk::ui_actor::SearchKind::Plain,
                });
            }
            AppMessage::SearchPending(token) => {
                if let Some(old_token) = self.pending_search.replace(token) {
                    old_token.cancel();
                }
            }
            AppMessage::Items(items) => {
                if let Ok(mut items) = items.lock() {
                    let entries: Vec<UiEntry> = std::mem::replace(&mut items, vec![]);
                    let (favorites, others): (Vec<_>, Vec<_>) = entries
                        .into_iter()
                        .partition(|e| e.entry.ring() == RingKind::Favorites);
                    self.favorites = favorites;
                    self.entries = others;
                }
            }
            AppMessage::Paste(id) => {
                let _ = self.command_sender.send(Command::Paste(id));
                return self.close_popup();
            }
            AppMessage::ChangeFavorite(id, is_favorite) => {
                if is_favorite {
                    let _ = self.command_sender.send(Command::Unfavorite(id));
                } else {
                    let _ = self.command_sender.send(Command::Favorite(id));
                }
            }
            AppMessage::Reload => {
                if self.search.is_empty() {
                    let _ = self.command_sender.send(Command::LoadFirstPage);
                } else {
                    let _ = self.command_sender.send(Command::Search {
                        query: self.search.clone().into_boxed_str(),
                        kind: ringboard_sdk::ui_actor::SearchKind::Plain,
                    });
                }
            }
            AppMessage::Delete(id) => {
                let _ = self.command_sender.send(Command::Delete(id));
            }
            AppMessage::Deleted(id) => {
                self.favorites.retain(|entry| entry.entry.id() != id);
                self.entries.retain(|entry| entry.entry.id() != id);
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

        Subscription::run_with_id(
            TypeId::of::<RingboardSubscription>(),
            channel(10, move |mut output| async move {
                spawn_blocking(move || {
                    let command_receiver = command_receiver.lock().unwrap();
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

                futures::future::pending().await
            }),
        )
    }
}
