use std::{
    any::TypeId,
    ops::Deref,
    sync::{Arc, Mutex, mpsc::Receiver},
};

use cosmic::iced::{Subscription, futures::executor::block_on, stream::channel};
use futures_util::SinkExt;
use ringboard_sdk::ui_actor::{Command, Message, controller};
use tokio::task::spawn_blocking;
use tracing::info;

use crate::{
    app::{AppMessage, DetailData, Details},
    fl,
};

struct RingboardSubscription;

pub fn ringboard_client_sub(
    command_receiver: Arc<Mutex<Receiver<Command>>>,
) -> Subscription<AppMessage> {
    Subscription::run_with_id(
        TypeId::of::<RingboardSubscription>(),
        channel(10, move |mut output| async move {
            spawn_blocking(move || {
                let command_receiver = command_receiver.lock().unwrap();
                info!("Starting ringboard client");
                controller::<anyhow::Error>(command_receiver.deref(), |m| {
                    match m {
                        Message::Error(e) => eprintln!("Error: {e}"),
                        Message::LoadedImage { id, .. } => println!("LoadedImage: {id}"),
                        Message::EntryDetails { id, result } => {
                            let _ =
                                block_on(output.send(AppMessage::DetailsLoaded(match result {
                                    Ok(details) => Ok(Details {
                                        id,
                                        favorite: false,
                                        entry: if let Some(text) = details.full_text {
                                            DetailData::Text(
                                                text.to_string(),
                                                "plain/text".to_string(),
                                            )
                                        } else {
                                            DetailData::Other(details.mime_type.to_string())
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
                            block_on(
                                output
                                    .send(AppMessage::Items(Arc::new(Mutex::new(results.into())))),
                            )?;
                        }
                        Message::PendingSearch(token) => {
                            block_on(output.send(AppMessage::SearchPending(token)))?;
                        }
                        Message::LoadedFirstPage { entries, .. } => {
                            block_on(
                                output
                                    .send(AppMessage::Items(Arc::new(Mutex::new(entries.into())))),
                            )?;
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
