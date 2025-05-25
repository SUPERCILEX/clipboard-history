// SPDX-License-Identifier: GPL-3.0-only

use cosmic::app::{Core, Task};
use cosmic::iced::window::Id;
use cosmic::iced::{Length, Limits};
use cosmic::iced_widget::{Column, column};
use cosmic::iced_winit::commands::popup::{destroy_popup, get_popup};
use cosmic::widget::{self, container, row};
use cosmic::{Application, Apply, Element, theme};
use ringboard_sdk::core::ring::Ring;
use ringboard_sdk::core::{IoErr, PathView};
use ringboard_sdk::core::dirs::data_dir;
use ringboard_sdk::core::protocol::RingKind;
use ringboard_sdk::{DatabaseReader, EntryReader, RingReader};

use crate::config::GeneralConfig;
use crate::fl;
use crate::views::main::{self, Main};
use crate::views::rings::{self, Rings};
use crate::views::settings::{self, Settings};

/// This is the struct that represents your application.
/// It is used to define the data that will be used by your application.
pub struct App {
    /// Application state which is managed by the COSMIC runtime.
    core: Core,
    /// The popup id.
    popup: Option<Id>,
    /// Example row toggler.
    pub database_reader: DatabaseReader,
    pub entry_reader: EntryReader,
    pub config: GeneralConfig,
    main: Main,
}

/// This is the enum that contains all the possible variants that your application will need to transmit messages.
/// This is used to communicate between the different parts of your application.
/// If your application does not need to send messages, you can use an empty enum or `()`.
#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(Id),
    SearchQuery(String),
    MainMessage(main::Message),
}

/// Implement the `Application` trait for your application.
/// This is where you define the behavior of your application.
///
/// The `Application` trait requires you to define the following types and constants:
/// - `Executor` is the async executor that will be used to run your application's commands.
/// - `Flags` is the data that your application needs to use before it starts.
/// - `Message` is the enum that contains all the possible variants that your application will need to transmit messages.
/// - `APP_ID` is the unique identifier of your application.
impl Application for App {
    type Executor = cosmic::executor::Default;

    type Flags = ();

    type Message = Message;

    const APP_ID: &'static str = "com.example.ClipboardHistory";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    /// This is the entry point of your application, it is where you initialize your application.
    ///
    /// Any work that needs to be done before the application starts should be done here.
    ///
    /// - `core` is used to passed on for you by libcosmic to use in the core of your own application.
    /// - `flags` is used to pass in any data that your application needs to use before it starts.
    /// - `Command` type is used to send messages to your application. `Command::none()` can be used to send no messages to your application.
    fn init(core: Core, _flags: Self::Flags) -> (Self, Task<Self::Message>) {
        let mut database = data_dir();
        database
            .try_exists()
            .map_io_err(|| format!("Failed to check that database exists: {database:?}"))
            .unwrap();

        let database_reader = DatabaseReader::open(&mut database).unwrap();
        let entry_reader = EntryReader::open(&mut database).unwrap();

        let config = GeneralConfig::default();
        let main = Main::new(&config);

        let app = App {
            core,
            popup: None,
            database_reader,
            entry_reader,
            config,
            main,
        };

        (app, Task::none())
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    /// This is the main view of your application, it is the root of your widget tree.
    ///
    /// The `Element` type is used to represent the visual elements of your application,
    /// it has a `Message` associated with it, which dictates what type of message it can send.
    ///
    /// To get a better sense of which widgets are available, check out the `widget` module.
    fn view(&self) -> Element<Self::Message> {
        self.core
            .applet
            .icon_button("edit-paste-symbolic")
            .on_press(Message::TogglePopup)
            .into()
    }

    fn view_window(&self, _id: Id) -> Element<Self::Message> {
        /* let content = column![widget::text("Hello"), widget::text("World"),]; */

        let content = match &self.main {
            Main::Rings(rings) => rings
                .view(self)
                .map(|msg| Message::MainMessage(main::Message::Rings(msg))),
            Main::Settings(settings) => settings
                .view(self)
                .map(|msg| Message::MainMessage(main::Message::Settings(msg))),
        };

        let container = container(content)
            .padding(theme::spacing().space_s)
            .width(Length::Fill);
        self.core.applet.popup_container(container).into()
    }

    /// Application messages are handled here. The application state can be modified based on
    /// what message was received. Commands may be returned for asynchronous execution on a
    /// background thread managed by the application's executor.
    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::TogglePopup => {
                return if let Some(p) = self.popup.take() {
                    destroy_popup(p)
                } else {
                    let new_id = Id::unique();
                    self.popup.replace(new_id);
                    let mut popup_settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );
                    popup_settings.positioner.size_limits = Limits::NONE
                        .max_width(372.0)
                        .min_width(300.0)
                        .min_height(200.0)
                        .max_height(1080.0);
                    get_popup(popup_settings)
                };
            }
            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                }
            }
            Message::SearchQuery(q) => {
                println!("Search query: {}", q);
            }
            Message::MainMessage(main_message) => {
                match main_message {
                    main::Message::Rings(rings_message) => {
                        match rings_message {
                            rings::Message::ChangeMainSettings => {
                                self.main = Main::Settings(Settings::new());
                            }
                            _ => {}
                        }

                        // Not ideal
                        match &mut self.main {
                            Main::Rings(view) => {
                                let _ = view.update(rings_message);
                            }
                            _ => {}
                        }
                    }
                    main::Message::Settings(settings_message) => {
                        match settings_message {
                            settings::Message::ChangeEntriesLimit(limit) => {
                                if let Some(limit) = limit {
                                    self.config.items_max = limit;
                                }
                            }
                            settings::Message::ToggleShowFavourites(state) => {
                                self.config.show_favourites = !state;
                            }
                            settings::Message::ToggleOneLineLimit(state) => {
                                self.config.one_line_limit = !state;
                            }
                            settings::Message::ChangeMainRings => {
                                self.main = Main::Rings(Rings::new(&self.config));
                            }
                        }

                        // Not ideal
                        match &mut self.main {
                            Main::Settings(view) => {
                                let _ = view.update(settings_message);
                            }
                            _ => {}
                        }
                    }
                    main::Message::ChangeMain(route) => match route {
                        main::MainRoute::Settings => {
                            self.main = Main::Settings(Settings::new());
                        }
                        main::MainRoute::Rings => {
                            self.main = Main::Rings(Rings::new(&self.config));
                        }
                    },
                }
            }
        }
        Task::none()
    }

    fn style(&self) -> Option<cosmic::iced_runtime::Appearance> {
        Some(cosmic::applet::style())
    }
}
