// SPDX-License-Identifier: Apache-2.0

use cosmic::iced::futures::executor::block_on;
use cosmic::iced::stream::channel;
use cosmic::iced::{Alignment, Length, Limits, Subscription, futures, padding, window};
use cosmic::iced_winit::commands::popup::{destroy_popup, get_popup};
use cosmic::theme::Button;
use cosmic::widget::button::Catalog;
use cosmic::widget::{
    MouseArea, Space, button, column, container, horizontal_space, row, scrollable, search_input,
    text,
};
use cosmic::{Action, prelude::*};
use futures_util::SinkExt;
use ringboard_sdk::ui_actor::{Command, Message, UiEntry, UiEntryCache, controller};
use std::any::TypeId;
use std::ops::Deref;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use tokio::task::spawn_blocking;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const APP_ICON: &[u8] = include_bytes!("../resources/icons/hicolor/scalable/apps/icon.svg");

/// The application model stores app-specific state used to describe its interface and
/// drive its logic.
pub struct AppModel {
    /// Application state which is managed by the COSMIC runtime.
    core: cosmic::Core,
    popup: Option<window::Id>,
    search: String,
    // Arc because AppMessage needs to be Clone and Send
    entries: Arc<[UiEntry]>,
    command_sender: Sender<Command>,
    // we need mutex because Receiver is not Sync
    command_receiver: Arc<Mutex<Receiver<Command>>>,
}

/// Messages emitted by the application and its widgets.
#[derive(Debug, Clone)]
pub enum AppMessage {
    TogglePopup,
    ClosePopup,
    Search(String),
    Items(Arc<[UiEntry]>),
    Paste(u64),
}

impl AppModel {
    fn toggle_popup(&mut self) -> Task<Action<AppMessage>> {
        match &self.popup {
            Some(_) => self.close_popup(),
            None => self.open_popup(),
        }
    }

    fn close_popup(&mut self) -> Task<Action<AppMessage>> {
        if let Some(popup) = self.popup.take() {
            destroy_popup(popup)
        } else {
            Task::none()
        }
    }

    fn open_popup(&mut self) -> Task<Action<AppMessage>> {
        let id = window::Id::unique();
        self.popup.replace(id);

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

    fn popup_view(&self) -> Element<'_, AppMessage> {
        container(self.list_view())
            .height(Length::Fixed(530f32))
            .width(Length::Fixed(400f32))
            .into()
    }

    fn list_view(&self) -> Element<'_, AppMessage> {
        column()
            .push(
                container(
                    row()
                        .push(
                            search_input("search", &self.search)
                                .always_active()
                                .on_input(AppMessage::Search)
                                .on_paste(AppMessage::Search)
                                .on_clear(AppMessage::Search("".into()))
                                .width(Length::Fill),
                        )
                        .push(horizontal_space().width(5)),
                )
                .padding(padding::all(15f32).bottom(0)),
            )
            .push(
                container({
                    let entries: Vec<_> = self
                        .entries
                        .iter()
                        .map(|entry| self.entry(entry, false))
                        .collect();
                    let column = column::with_children(entries)
                        .spacing(5f32)
                        .padding(padding::right(10));
                    scrollable(column).apply(Element::from)
                })
                .padding(padding::all(20).top(0)),
            )
            .spacing(20)
            .align_x(Alignment::Center)
            .into()
    }

    fn entry(&self, entry: &UiEntry, is_focused: bool) -> Element<'_, AppMessage> {
        let content = if let UiEntryCache::Text { one_liner }
        | UiEntryCache::HighlightedText { one_liner, .. } = &entry.cache
        {
            one_liner
        } else {
            println!("Entry without text cache: {:?}", entry);
            "<loading...>"
        };

        let btn = button::custom(text(content.to_string()))
            .on_press(AppMessage::Paste(entry.entry.id()))
            .padding([8, 16])
            .class(Button::Custom {
                active: Box::new(move |focused, theme| {
                    let rad_s = theme.cosmic().corner_radii.radius_s;
                    let focused = is_focused || focused;

                    let a = if focused {
                        theme.hovered(focused, focused, &Button::Text)
                    } else {
                        theme.hovered(focused, focused, &Button::Standard)
                    };

                    button::Style {
                        border_radius: rad_s.into(),
                        outline_width: 0.0,
                        ..a
                    }
                }),
                disabled: Box::new(move |theme| theme.disabled(&Button::Text)),
                hovered: Box::new(move |focused, theme| {
                    let rad_s = theme.cosmic().corner_radii.radius_s;
                    let focused = is_focused || focused;

                    let text = theme.hovered(focused, focused, &Button::Text);
                    button::Style {
                        border_radius: rad_s.into(),
                        outline_width: 0.0,
                        ..text
                    }
                }),
                pressed: Box::new(move |focused, theme| {
                    let rad_s = theme.cosmic().corner_radii.radius_s;
                    let focused = is_focused || focused;

                    let text = theme.pressed(focused, focused, &Button::Text);
                    button::Style {
                        border_radius: rad_s.into(),
                        outline_width: 0.0,
                        ..text
                    }
                }),
            });

        let btn: Element<_> = btn.width(Length::Fill).into();

        btn
    }
}

/// Create a COSMIC application from the app model
impl cosmic::Application for AppModel {
    /// The async executor that will be used to run your application's commands.
    type Executor = cosmic::executor::Default;

    /// Data that your application receives to its init method.
    type Flags = ();

    /// Messages which the application and its widgets will emit.
    type Message = AppMessage;

    /// Unique identifier in RDNN (reverse domain name notation) format.
    const APP_ID: &'static str = "com.github.ringboard.cosmic-applet";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    /// Initializes the application with any given flags and startup commands.
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
            entries: Arc::new([]),
            command_sender,
            command_receiver: Arc::new(Mutex::new(command_receiver)),
        };

        (app, Task::none())
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Self::Message> {
        if let Some(popup) = &self.popup
            && *popup == id
        {
            return Some(AppMessage::ClosePopup);
        }
        None
    }

    /// Describes the interface based on the current state of the application model.
    ///
    /// Application events will be processed through the view. Any messages emitted by
    /// events received by widgets will be passed to the update method.
    fn view(&self) -> Element<'_, Self::Message> {
        let icon = self
            .core
            .applet
            .icon_button(constcat::concat!(AppModel::APP_ID, "-symbolic"))
            .on_press(AppMessage::TogglePopup);

        MouseArea::new(icon).into()
    }

    fn view_window(&self, id: window::Id) -> Element<'_, Self::Message> {
        let Some(popup) = &self.popup else {
            return Space::new(0, 0).into();
        };

        let view = self.popup_view();

        self.core.applet.popup_container(view).into()
    }

    /// Handles messages emitted by the application and its widgets.
    ///
    /// Tasks may be returned for asynchronous execution of code in the background
    /// on the application's async runtime.
    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            AppMessage::TogglePopup => return self.toggle_popup(),
            AppMessage::ClosePopup => return self.close_popup(),
            AppMessage::Search(search) => {
                self.search = search;
                let _ = self.command_sender.send(Command::Search {
                    query: self.search.clone().into_boxed_str(),
                    kind: ringboard_sdk::ui_actor::SearchKind::Plain,
                });
            }
            AppMessage::Items(items) => {
                self.entries = items;
            }
            AppMessage::Paste(id) => {
                let _ = self.command_sender.send(Command::Paste(id));
                return self.close_popup();
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
                            Message::Deleted(id) => println!("Deleted: {id}"),
                            Message::Error(e) => eprintln!("Error: {e}"),
                            Message::FatalDbOpen(e) => eprintln!("FatalDbOpen: {e}"),
                            Message::Pasted => println!("Pasted"),
                            Message::FavoriteChange(id) => println!("FavoriteChange: {id}"),
                            Message::PendingSearch(token) => println!("PendingSearch: {token:?}"),
                            Message::SearchResults(results) => {
                                println!("SearchResults: {}", results.len());
                                block_on(output.send(AppMessage::Items(results.into())))?;
                            }
                            Message::LoadedImage { id, .. } => println!("LoadedImage: {id}"),
                            Message::EntryDetails { id, result } => {
                                println!("EntryDetails: {id}, {result:?}")
                            }
                            Message::LoadedFirstPage {
                                entries,
                                default_focused_id,
                            } => {
                                println!(
                                    "LoadedFirstPage: {} entries, default_focused_id: {:?}",
                                    entries.len(),
                                    default_focused_id
                                );
                                block_on(output.send(AppMessage::Items(entries.into())))?;
                            }
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
