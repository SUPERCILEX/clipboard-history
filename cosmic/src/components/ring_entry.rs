use std::{io::Read, path::PathBuf};

use cosmic::{
    iced::{Alignment, Length::{self, Fill}}, iced_core::image::Bytes, iced_widget::{column, row, Column}, theme, widget::{self, button, container, icon, image}, Element, Task
};
use ringboard_sdk::{
    api::MoveToFrontRequest, core::{dirs::data_dir, protocol::RingKind, ring::Ring, IoErr, PathView}, DatabaseReader, Entry, EntryReader, RingReader
};

use crate::{
    app::{self, App},
    config::GeneralConfig, views::rings,
};

#[derive(Debug)]
pub enum FormattedEntryContent {
    Text(String),
    Image(Vec<u8>),
}

#[derive(Debug)]
pub struct FormattedEntry {
    pub id: u64,
    pub content: FormattedEntryContent,
}

impl FormattedEntry {
    pub fn from_entry(entry: &Entry, entry_reader: &mut EntryReader) -> Option<Self> /* Result<Self, ringboard_sdk::core::Error> */
    {
        let loaded = entry.to_slice(entry_reader).unwrap();

        let mime_type = &*loaded.mime_type().unwrap();
        if mime_type.starts_with("image/") {
            let x = &loaded[0..loaded.len()];
            println!("Entry is image {:?}", x);

            let bytes = Bytes::copy_from_slice(x);
            /* let img = image::Handle::from_bytes(bytes); */

            return Some(FormattedEntry {
                id: entry.id(),
                content: FormattedEntryContent::Image(x.to_vec()),
            });
        } else {
            let short = &loaded[..loaded.len().min(250)];

            let string = str::from_utf8(&short);

            if let Ok(string) = string {
                println!("Entry is text: {string}");

                return Some(FormattedEntry {
                    id: entry.id(),
                    content: FormattedEntryContent::Text(string.to_string()),
                });
            } else {
                println!("Failed to decode of bytes: {}", loaded.len());
            }
        }

        None
    }

    fn is_selected(&self, selected_id: u64) -> bool {
        self.id == selected_id
    }

    pub fn into_element(&self, selected_id: u64) -> Element<'static, rings::Message> {
        match &self.content {
            FormattedEntryContent::Text(text) => {
                button::custom(widget::text(text.clone()).width(Length::Fill))
                    .class(theme::Button::MenuItem)
                    .on_press(rings::Message::CopyEntry(self.id))
                    .selected(self.is_selected(selected_id))
                    .padding(theme::spacing().space_xs)
                    .into()
            }
            FormattedEntryContent::Image(image) => {

                let bytes = Bytes::copy_from_slice(image);
                let handle = image::Handle::from_bytes(bytes);

                let image = widget::image(handle).width(Length::Fill).height(Length::Fill);
                let btn = button::custom(image)
                    .class(theme::Button::MenuItem)
                    .on_press(rings::Message::CopyEntry(self.id))
                    .selected(self.is_selected(selected_id))
                    .width(Length::Fill)
                    .padding(theme::spacing().space_xs);

                container(btn.width(Length::Fill).height(Length::Fill))
                .max_width(350f32)
                .into()

                    // button::image(image.clone())
                    //     .class(theme::Button::MenuItem)
                    //     .on_press(Message::CopyEntry(self.id))
                    //     .selected(self.is_selected(selected_id))
                    //     .into()
                },
        }
    }
}