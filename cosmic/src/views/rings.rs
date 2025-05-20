use cosmic::{
    iced::Length, iced_widget::{button, column, Column}, theme, widget::{self, icon, row}, Element, Task
};
use ringboard_sdk::core::protocol::RingKind;

use crate::app::{self, App};

use super::main::View;

#[derive(Debug, Clone)]
pub enum Message {
    SearchQuery(String),
    // Not ideal
    ChangeMainSettings,
}

#[derive(Debug, Clone)]
pub struct Rings {
    search_query: String,
}

impl Default for Rings {
    fn default() -> Self {
        Self {
            search_query: String::new(),
        }
    }
}

impl Rings {
    pub fn view(&self, app: &App) -> Element<'static, Message> {
        let db = &app.database_reader;

        let favourites_column = Column::from_vec({
            let mut vec: Vec<Element<Message>> = Vec::new();
            for favourite in db.favorites() {
                if vec.len() as u32 > app.config.items_max {
                    break;
                }

                vec.push(widget::text("favourite entry").into());
            }
            vec
        }).spacing(theme::spacing().space_xxs);

        let rings = db.main();

        let non_favourites_column = Column::from_vec({
            let mut vec: Vec<Element<Message>> = Vec::new();

            for ring in rings {
                if vec.len() as u32 > app.config.items_max {
                    break;
                }

                let favourite_kind = ring.ring();
                // Don't display favourites, we do that above
                if favourite_kind == RingKind::Favorites {
                    continue;
                }

                let kind = ring.kind();
                match kind {
                    ringboard_sdk::Kind::File => {
                        println!("is file for sure");
                    }
                    ringboard_sdk::Kind::Bucket(ent) => {
                        if ent.is_file() {
                            println!("Is file, somehow");
                        }
                    }
                };

                vec.push(widget::text("non-favourite entry").into());
            }

            vec
        }).spacing(theme::spacing().space_xxs);

        let scroll_content = column![]
            .push_maybe(if app.config.show_favourites {
                Some(favourites_column)
            } else {
                None
            })
            .push(non_favourites_column)
            .spacing(theme::spacing().space_s)
            .padding(theme::spacing().space_m);

        let search_query = cosmic::widget::search_input("Search", self.search_query.clone())
            .on_input(|v| Message::SearchQuery(v));

        let header = row()
            .push(search_query)
            .push(widget::button::icon(widget::icon::from_name("gear")).on_press(Message::ChangeMainSettings))
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
