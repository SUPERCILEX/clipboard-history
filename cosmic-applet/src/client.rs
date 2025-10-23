use std::{
    any::TypeId,
    ops::Deref,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender},
    },
};

use cosmic::iced::{Subscription, futures::executor::block_on, stream::channel};
use futures_util::SinkExt;
use ringboard_sdk::{
    core::protocol::RingKind,
    ui_actor::{Command, Message, UiEntry, UiEntryCache, controller},
};
use tokio::task::spawn_blocking;
use tracing::info;

use crate::{
    app::{AppMessage, Entry, EntryData},
    fl,
};

struct RingboardSubscription;

pub fn ringboard_client_sub(
    command_receiver: Arc<Mutex<Receiver<Command>>>,
    command_sender: Sender<Command>,
) -> Subscription<AppMessage> {
    Subscription::run_with_id(
        TypeId::of::<RingboardSubscription>(),
        channel(10, move |mut output| async move {
            spawn_blocking(move || {
                let command_receiver = command_receiver.lock().unwrap();
                info!("Starting ringboard client");
                controller::<anyhow::Error>(command_receiver.deref(), |m| {
                    match m {
                        Message::Error(e) => {
                            let _ = block_on(output.send(AppMessage::Error(format!(
                                "{}: {}",
                                fl!("error"),
                                e
                            ))));
                        }
                        Message::LoadedImage { id, image } => {
                            let _ = block_on(output.send(AppMessage::ImageLoaded(id, image)));
                        }
                        Message::EntryDetails { id, result } => {
                            let _ =
                                block_on(output.send(AppMessage::DetailsLoaded(match result {
                                    Ok(details) => Ok(Entry {
                                        id,
                                        favorite: false,
                                        data: if let Some(text) = details.full_text {
                                            EntryData::Text {
                                                text: text.to_string(),
                                                mime: "plain/text".to_string(),
                                            }
                                        } else {
                                            EntryData::Mime(details.mime_type.to_string())
                                        },
                                    }),
                                    Err(e) => Err(format!("{}: {e}", fl!("error"))),
                                })));
                        }
                        Message::FatalDbOpen(e) => {
                            let _ = block_on(output.send(AppMessage::FatalError(format!(
                                "{}: {}",
                                fl!("db-error"),
                                e
                            ))));
                        }
                        Message::FavoriteChange(_) => {
                            block_on(output.send(AppMessage::Reload))?; // because the id of the element changes when favoriting/unfavoriting we can't just update the entry in place
                        }
                        Message::SearchResults(results) => {
                            block_on(output.send(AppMessage::Items(convert_entries(
                                results,
                                &command_sender,
                            ))))?;
                        }
                        Message::PendingSearch(token) => {
                            block_on(output.send(AppMessage::SearchPending(token)))?;
                        }
                        Message::LoadedFirstPage { entries, .. } => {
                            block_on(output.send(AppMessage::Items(convert_entries(
                                entries,
                                &command_sender,
                            ))))?;
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
    )
}

fn convert_entries(entries: Box<[UiEntry]>, command_sender: &Sender<Command>) -> Vec<Entry> {
    entries
        .into_iter()
        .map(|entry| Entry {
            id: entry.entry.id(),
            favorite: entry.entry.ring() == RingKind::Favorites,
            data: convert_entry(entry, command_sender),
        })
        .collect()
}

fn convert_entry(entry: UiEntry, command_sender: &Sender<Command>) -> EntryData {
    match entry.cache {
        UiEntryCache::Text { one_liner } => EntryData::Text {
            text: one_liner.to_string(),
            mime: "plain/text".to_string(),
        },
        UiEntryCache::HighlightedText {
            one_liner,
            start,
            end,
        } => EntryData::HighlightedText {
            text: one_liner.to_string(),
            mime: "plain/text".to_string(),
            start,
            end,
        },
        UiEntryCache::Binary { mime_type } => EntryData::Mime(mime_type.to_string()),
        UiEntryCache::Error(e) => EntryData::Error(e.to_string()),
        UiEntryCache::Image => {
            command_sender
                .send(Command::LoadImage(entry.entry.id()))
                .unwrap();
            EntryData::Image {
                image: None,
                mime: "image/png".to_string(),
            }
        }
    }
}
