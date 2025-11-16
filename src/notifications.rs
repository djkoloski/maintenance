use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context as _, Result};
use dbus::{
    arg::{RefArg, Variant},
    message::SignalArgs,
    nonblock::{MsgMatch, Proxy, SyncConnection},
};
use futures::{StreamExt as _, channel::oneshot};
use log::{error, info, warn};

use crate::interfaces::{
    OrgFreedesktopDBus as _, OrgFreedesktopDBusNameOwnerChanged as NameOwnerChanged,
    OrgFreedesktopNotifications as _, OrgFreedesktopNotificationsActionInvoked as ActionInvoked,
    OrgFreedesktopNotificationsNotificationClosed as NotificationClosed,
};

pub enum Notification {
    Closed(#[expect(unused)] NotificationClosed),
    ActionInvoked(ActionInvoked),
}

#[derive(Clone)]
pub struct Notifications {
    proxy: Proxy<'static, Arc<SyncConnection>>,

    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Notification>>>>,
    matchers: Arc<Matchers>,
}

struct Matchers {
    notification_closed_match: MsgMatch,
    action_invoked_match: MsgMatch,
}

impl Notifications {
    pub async fn start(connection: Arc<SyncConnection>) -> Result<Self> {
        // Wait up to 10 seconds for the notification service to start
        let dbus = Proxy::new(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            Duration::new(10, 0),
            connection.clone(),
        );

        let (incoming_signal, mut stream) = connection
            .add_match(NameOwnerChanged::match_rule(None, None))
            .await?
            .stream::<NameOwnerChanged>();
        if dbus.name_has_owner("org.freedesktop.Notifications").await? {
            info!("notifications service already started");
        } else {
            let (_, signal) = stream
                .next()
                .await
                .context("connection terminated while waiting for Notifications to be acquired")?;
            info!(
                "Owner for '{}' changed from '{}' to '{}'",
                signal.arg0, signal.arg1, signal.arg2
            );
        }
        connection.remove_match(incoming_signal.token()).await?;

        let pending = Arc::new(Mutex::new(
            HashMap::<u32, oneshot::Sender<Notification>>::new(),
        ));

        let handler_pending = pending.clone();
        let notification_closed_match = connection
            .add_match(NotificationClosed::match_rule(None, None))
            .await
            .context("failed to add notification closed match")?
            .cb(move |_, message: NotificationClosed| {
                info!("received a notification closed!");
                if let Some(sender) = handler_pending.lock().unwrap().remove(&message.arg_0) {
                    if sender.send(Notification::Closed(message)).is_err() {
                        error!("failed to send notification closed signal");
                    }
                } else {
                    warn!(
                        "received {:?} for notification that was not registered",
                        message
                    );
                }
                true
            });

        let handler_pending = pending.clone();
        let action_invoked_match = connection
            .add_match(ActionInvoked::match_rule(None, None))
            .await
            .context("failed to add action invoked match")?
            .cb(move |_, message: ActionInvoked| {
                info!("received an action invoked!");
                if let Some(sender) = handler_pending.lock().unwrap().remove(&message.arg_0) {
                    if sender.send(Notification::ActionInvoked(message)).is_err() {
                        error!("failed to send action invoked signal");
                    }
                } else {
                    warn!(
                        "received {:?} for notification that was not registered",
                        message
                    );
                }
                true
            });

        let proxy = Proxy::new(
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            Duration::new(5, 0),
            connection,
        );

        Ok(Self {
            proxy,

            pending,
            matchers: Arc::new(Matchers {
                notification_closed_match,
                action_invoked_match,
            }),
        })
    }

    pub async fn stop(self) -> Result<()> {
        self.proxy
            .connection
            .remove_match(self.matchers.notification_closed_match.token())
            .await
            .context("failed to remove notification closed match")?;

        self.proxy
            .connection
            .remove_match(self.matchers.action_invoked_match.token())
            .await
            .context("failed to remove action invoked match")?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<&str>,
        hints: HashMap<String, Variant<Box<dyn RefArg>>>,
        expire_timeout: i32,
    ) -> Result<Notification> {
        let id = self
            .proxy
            .notify(
                app_name,
                replaces_id,
                app_icon,
                summary,
                body,
                actions,
                hints,
                expire_timeout,
            )
            .await
            .context("failed to send notification")?;

        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, sender);

        info!("sent notification!");

        let response = receiver
            .await
            .context("failed to receive notification response")?;

        info!("received response!");

        Ok(response)
    }
}
