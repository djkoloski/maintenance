use std::{collections::HashMap, sync::Arc};

use anyhow::{Context as _, Result};
use futures::{StreamExt as _, channel::oneshot};
use log::{error, info};
use tokio::sync::Mutex;
use zbus::{AsyncDrop as _, Connection, fdo::DBusProxy, names::BusName, zvariant::Value};

use crate::interfaces::{ActionInvoked, NotificationClosed, NotificationsProxy};

pub enum Notification {
    Closed(#[expect(unused)] NotificationClosed),
    ActionInvoked(ActionInvoked),
}

struct State {
    pending: HashMap<u32, oneshot::Sender<Notification>>,
}

impl State {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct Notifications {
    notifications: NotificationsProxy<'static>,
    state: Arc<Mutex<State>>,
}

impl Notifications {
    pub async fn start(connection: &Connection) -> Result<Self> {
        // Wait up to 10 seconds for the notification service to start
        let dbus = DBusProxy::new(connection)
            .await
            .context("failed to initialize D-Bus")?;

        let mut notification_owner_changed = dbus
            .receive_name_owner_changed_with_args(&[(0, "org.freedesktop.Notifications"), (1, "")])
            .await?;
        let notifications = BusName::try_from("org.freedesktop.Notifications").unwrap();
        if dbus.name_has_owner(notifications.as_ref()).await? {
            let owner = dbus.get_name_owner(notifications.as_ref()).await?;
            info!("notifications service already owned by {owner}");
        } else {
            let signal = notification_owner_changed.next().await.context(
                "connection terminated while waiting for Notifications service to be acquired",
            )?;
            let args = signal.args()?;

            info!(
                "owner for '{}' changed from '{:?}' to '{:?}'",
                args.name, args.old_owner, args.new_owner,
            );
        };
        notification_owner_changed.async_drop().await;

        let notifications = NotificationsProxy::new(connection)
            .await
            .context("failed to connect to notifications service")?;
        let state = Arc::new(Mutex::new(State::new()));

        let n = notifications.clone();
        let s = state.clone();
        tokio::spawn(async move {
            let result = n.receive_notification_closed().await;
            let Ok(mut stream) = result else {
                error!(
                    "failed to receive notification closed signals: {}",
                    result.unwrap_err()
                );
                return;
            };

            while let Some(closed) = stream.next().await {
                let result = closed.args();
                let Ok(args) = result else {
                    error!("failed to get closed event args");
                    break;
                };

                info!(
                    "notification {} was closed (reason: {})",
                    args.arg_0(),
                    args.arg_1()
                );

                let Some(sender) = s.lock().await.pending.remove(args.arg_0()) else {
                    error!(
                        "received a notification closed event for a notification which was not registered"
                    );
                    break;
                };

                let _ = sender.send(Notification::Closed(closed));
            }
            stream.async_drop().await;
        });

        let n = notifications.clone();
        let s = state.clone();
        tokio::spawn(async move {
            let result = n.receive_action_invoked().await;
            let Ok(mut stream) = result else {
                error!(
                    "failed to receive action invoked signals: {}",
                    result.unwrap_err()
                );
                return;
            };

            while let Some(action_invoked) = stream.next().await {
                let result = action_invoked.args();
                let Ok(args) = result else {
                    error!("failed to get action invoked event args");
                    break;
                };

                info!("notification {} was invoked", args.arg_0());

                let Some(sender) = s.lock().await.pending.remove(args.arg_0()) else {
                    error!(
                        "received an action invoked event for a notification which was not registered"
                    );
                    break;
                };

                let id = args.arg_0;
                let _ = sender.send(Notification::ActionInvoked(action_invoked));

                if n.close_notification(id).await.is_err() {
                    error!("failed to close notification with an invoked action");
                    break;
                }
            }
            stream.async_drop().await;
        });

        Ok(Self {
            notifications,
            state,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        expire_timeout: i32,
    ) -> Result<Notification> {
        let mut state_guard = self.state.lock().await;

        let mut hints = HashMap::new();
        hints.insert("resident", &Value::Bool(true));

        let id = self
            .notifications
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
        state_guard.pending.insert(id, sender);
        drop(state_guard);

        info!("sent notification {}!", id);

        let response = receiver
            .await
            .context("failed to receive notification response")?;

        info!("received response!");

        Ok(response)
    }
}
