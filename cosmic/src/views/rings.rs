use std::{io::Read, path::PathBuf};

use cosmic::{
    Element, Task,
    iced::{
        Alignment,
        Length::{self, Fill},
    },
    iced_core::image::Bytes,
    iced_widget::{Column, column, row},
    theme,
    widget::{self, button, container, icon, image},
};
use ringboard_sdk::{
    DatabaseReader, Entry, EntryReader, RingReader,
    api::MoveToFrontRequest,
    core::{IoErr, PathView, dirs::data_dir, protocol::RingKind, ring::Ring},
    ui_actor::UiEntryCache,
};

use crate::{
    app::{self, App},
    config::GeneralConfig,
};

use super::main::View;
use crate::components::ring_entry::FormattedEntry;

#[derive(Debug, Clone)]
pub enum Message {
    SearchQuery(String),
    CopyEntry(u64),
    // Not ideal
    ChangeMainSettings,
}

#[derive(Debug)]
pub struct Rings {
    search_query: String,
    entries: Vec<FormattedEntry>,
    entry_reader: EntryReader,
    selected_id: Option<u64>,
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

        for entry in reader.rev() {
            if entries.len() as u32 > config.items_max {
                break;
            }

            let formatted = FormattedEntry::from_entry(&entry, &mut entry_reader);
            if let Some(formatted) = formatted {
                entries.push(formatted);
            }
        }

        let selected_id = if let Some(last) = entries.last() {
            Some(last.id)
        } else {
            None
        };

        Self {
            search_query: String::new(),
            entries,
            entry_reader,
            selected_id,
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

        /* let non_favourites_column = Column::from_vec({
            let mut vec: Vec<Element<'static, Message>> = Vec::new();

            for entry in &self.entries {
                vec.push(entry.into_element(entry.id));
            }

            vec
        })
        .spacing(theme::spacing().space_xxs); */

        let non_favourites_column = Column::from_vec({
            let mut elements = Vec::new();
            for entry in &app.entries {
                let element = match &entry.cache {
                    UiEntryCache::Text { one_liner } => widget::text(one_liner.to_string()),
                    _ => widget::text("Error displaying entry"),
                };

                elements.push(element.into());
            }
            elements
        });

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
            Message::CopyEntry(id) => {
                self.selected_id = Some(id);

                // MoveToFrontRequest::send()

                Task::none()
            }
            _ => Task::none(),
        }
    }
}
