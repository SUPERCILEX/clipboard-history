use std::{any::TypeId, future::pending, sync::Arc};

use cosmic::iced::{Subscription, futures::executor::block_on, stream::channel};
use futures_util::SinkExt;
use tokio::{sync::Notify, task::spawn_blocking};
use tracing::info;
use zbus::{
    Connection, Result,
    blocking::connection,
    interface,
    names::{ErrorName, OwnedErrorName},
    proxy,
};

use crate::app::AppMessage;

struct OpenToggle {
    notify: Arc<Notify>,
}

#[interface(name = "com.github.ringboard1")]
impl OpenToggle {
    async fn toggle(&mut self) -> bool {
        info!("Toggle called");
        self.notify.notify_one();
        true
    }
}

pub async fn server(notify: Arc<Notify>) -> Result<()> {
    // required because some dependency uses blocking zbus and the blocking feature doesn't get disabled
    spawn_blocking(move || match server_internal(notify) {
        Ok(()) => (),
        Err(e) => tracing::error!("D-Bus server error: {:?}", e),
    });
    Ok(())
}

fn server_internal(notify: Arc<Notify>) -> Result<()> {
    let toggle = OpenToggle { notify };
    info!("Starting D-Bus server");
    let _conn = connection::Builder::session()?
        .name("com.github.ringboard")?
        .serve_at("/com/github/ringboard", toggle)?
        .build()?;
    info!("D-Bus server started");

    // prevent server from being dropped
    block_on(pending::<()>());

    info!("D-Bus server stopped");

    Ok(())
}

#[proxy(
    interface = "com.github.ringboard1",
    default_service = "com.github.ringboard",
    default_path = "/com/github/ringboard"
)]
trait OpenToggle {
    async fn toggle(&self) -> Result<bool>;
}

// error when the service is not running
const SERVICE_UNKNOWN: ErrorName<'static> =
    ErrorName::from_static_str_unchecked("org.freedesktop.DBus.Error.ServiceUnknown");

/// Returns Ok(true) if the toggle method was called successfully,
/// Ok(false) if there is no instance running
pub async fn client() -> Result<bool> {
    let connection = Connection::session().await?;
    let proxy = OpenToggleProxy::new(&connection).await?;

    let serivce_unknown: OwnedErrorName = SERVICE_UNKNOWN.into();
    let res = match proxy.toggle().await {
        Ok(r) => r,
        Err(zbus::Error::MethodError(e, _, _)) if e == serivce_unknown => return Ok(false),
        Err(e) => return Err(e),
    };
    assert!(res, "Toggle failed");
    Ok(true)
}

struct WackupSubscription;

pub fn wackup_sub(notify: Arc<Notify>) -> Subscription<AppMessage> {
    Subscription::run_with_id(
        TypeId::of::<WackupSubscription>(),
        channel(1, move |mut output| async move {
            loop {
                notify.notified().await;
                let _ = output.send(AppMessage::TogglePopup).await;
            }
        }),
    )
}
