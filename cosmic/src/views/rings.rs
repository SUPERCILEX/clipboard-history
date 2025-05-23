use cosmic::{
    Element, Task,
    iced::Length::{self, Fill},
    iced_widget::{Column, column, row},
    theme,
    widget::{self, button, icon},
};
use ringboard_sdk::{
    DatabaseReader, EntryReader, RingReader,
    core::{IoErr, PathView, dirs::data_dir, protocol::RingKind, ring::Ring},
};

use crate::{
    app::{self, App},
    config::GeneralConfig,
};

use super::main::View;

#[derive(Debug, Clone)]
pub enum Message {
    SearchQuery(String),
    CopyEntry(u32),
    // Not ideal
    ChangeMainSettings,
}

type EntryValues = String;

#[derive(Debug)]
pub struct Rings {
    search_query: String,
    entries: Vec<EntryValues>,
    entry_reader: EntryReader,
}

impl Rings {
    pub fn new(config: &GeneralConfig) -> Self {
        let mut database = data_dir();
        database
            .try_exists()
            .map_io_err(|| format!("Failed to check that database exists: {database:?}"))
            .unwrap();

        let mut entry_reader = EntryReader::open(&mut database).unwrap();

        let kind = RingKind::Main;
        let main_file_name = RingKind::Main.file_name();
        let pathview = PathView::new(&mut database, main_file_name);
        let ring = Ring::open(config.items_max, &*pathview).unwrap();

        let reader = RingReader::from_ring(&ring, kind);

        let mut entries = Vec::new();

        for entry in reader {
            let loaded = entry.to_slice(&mut entry_reader).unwrap();

            let mime_type = &*loaded.mime_type().unwrap();
            if mime_type.starts_with("image/") {
                println!("Entry is image");
            } else {
                let short = &loaded[..loaded.len().min(250)];

                let string = str::from_utf8(&short);

                if let Ok(string) = string {
                    println!("Entry is text: {string}");

                    entries.push(string.to_string());
                } else {
                    println!("Failed to decode of bytes: {}", loaded.len());
                }
            }
        }

        Self {
            search_query: String::new(),
            entries,
            entry_reader,
        }
    }

    pub fn view(&self, app: &App) -> Element<'static, Message> {
        let db = &app.database_reader;

        let favourites_column = Column::from_vec({
            let mut vec: Vec<Element<Message>> = Vec::new();
            for entry in db.favorites() {
                if vec.len() as u32 > app.config.items_max {
                    break;
                }

                /* let loaded = entry.to_slice(&app.entry_reader)?; */

                vec.push(row![widget::text("favourite entry")].into());
            }
            vec
        })
        .spacing(theme::spacing().space_xxs);

        let non_favourites_column = Column::from_vec({
            let mut vec: Vec<Element<Message>> = Vec::new();

            for index in (0..self.entries.len()).rev() {
                let entry = &self.entries[index];

                if vec.len() as u32 > app.config.items_max {
                    break;
                }

                /* let kind = ring.kind();
                match kind {
                    ringboard_sdk::Kind::File => {
                        println!("is file for sure");
                    }
                    ringboard_sdk::Kind::Bucket(ent) => {
                        if ent.is_file() {
                            println!("Is file, somehow");
                        }
                    }
                }; */

                let is_selected = index == 0;

                vec.push(
                    button::custom(widget::text(entry.clone()).width(Length::Fill))
                        .class(theme::Button::MenuItem)
                        .on_press(Message::CopyEntry(index as u32))
                        .selected(is_selected)
                        .into(),
                );
            }

            vec
        })
        .spacing(theme::spacing().space_xxs);

        let scroll_content = column![]
            .push_maybe(if app.config.show_favourites {
                Some(favourites_column)
            } else {
                None
            })
            .push(non_favourites_column)
            .spacing(theme::spacing().space_s)
            .padding(theme::spacing().space_m)
            .width(Length::Fill);

        let search_query = cosmic::widget::search_input("Search", self.search_query.clone())
            .on_input(|v| Message::SearchQuery(v));

        let header = row![]
            .push(search_query)
            .push(
                widget::button::icon(widget::icon::from_name("gear"))
                    .on_press(Message::ChangeMainSettings),
            )
            .spacing(theme::spacing().space_xs);

        let content = column![
            header,
            cosmic::widget::scrollable(scroll_content).scrollbar_width(8.0)
        ];

        content.into()
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SearchQuery(query) => {
                self.search_query = query;
                Task::none()
            }
            _ => Task::none(),
        }
    }
}
