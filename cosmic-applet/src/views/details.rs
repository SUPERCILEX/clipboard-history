use cosmic::{
    Element,
    iced_widget::text,
    widget::{column, text::heading},
};

use crate::app::{AppMessage, Details};

pub fn details_view<'a>(details: Details) -> Element<'a, AppMessage> {
    let column = column()
        .push(heading("Details"))
        .push(text(format!("ID: {}", details.id)));

    column.into()
}
