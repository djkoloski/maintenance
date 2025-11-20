mod interfaces;
mod notifications;

use core::fmt::{self, Write as _};
use std::{collections::HashMap, env, path::PathBuf};

use anyhow::{Context as _, Result};
use dbus::arg::Variant;
use dbus_tokio::connection;
use log::{LevelFilter, error, info};
use regex::RegexSet;
use serde::{Deserialize, Deserializer, de};
use systemd_journal_logger::JournalLog;
use tokio::{fs, process::Command};

use crate::notifications::{Notification, Notifications};

#[tokio::main]
async fn main() {
    JournalLog::new().unwrap().install().unwrap();
    log::set_max_level(LevelFilter::Info);

    info!("running maintenance");

    if let Err(e) = run_checks().await {
        error!("encountered an internal error: {e:?}");
    }

    info!("finished");
}

async fn run_checks() -> Result<()> {
    let (resource, connection) = connection::new_session_sync()?;

    let _handle = tokio::spawn(async {
        let err = resource.await;
        error!("lost connection to D-Bus: {err:?}");
    });

    let notifications = Notifications::start(connection.clone())
        .await
        .context("failed to start notifications")?;

    check_systemctl_failures(notifications.clone())
        .await
        .context("failed to check systemctl failures")?;
    check_journalctl_errors(notifications.clone())
        .await
        .context("failed to check journalctl errors")?;
    check_updates(notifications.clone())
        .await
        .context("failed to check for updates")?;

    notifications
        .stop()
        .await
        .context("failed to stop notifications")?;

    Ok(())
}

#[derive(Deserialize)]
struct SystemctlUnit {
    unit: String,
    description: String,
}

async fn check_systemctl_failures(notifications: Notifications) -> Result<()> {
    info!("checking for systemctl failures");

    let output = Command::new("/usr/bin/systemctl")
        .args(["--failed", "--output=json"])
        .output()
        .await
        .context("failed to run systemctl")?
        .stdout;
    let failed = serde_json::from_slice::<Vec<SystemctlUnit>>(&output)
        .context("failed to parse systemctl output as json")?;

    if failed.is_empty() {
        return Ok(());
    }

    let summary;
    let body;

    if failed.len() == 1 {
        summary = "Systemd unit failed to load";
        body = format!(
            "'{}' ({}) failed to start normally.",
            failed[0].description, failed[0].unit
        );
    } else {
        summary = "Multiple systemd units failed to load";
        body = format!("{} units failed to start normally.", failed.len());
    }

    let mut hints = HashMap::new();
    hints.insert("urgency".to_string(), Variant(Box::new(2u8) as _));

    let response = notifications
        .notify(
            "Maintenance",
            0,
            "dialog-warning-symbolid",
            summary,
            &body,
            vec!["default", "Investigate"],
            hints,
            -1,
        )
        .await?;

    if let Notification::ActionInvoked(action_invoked) = response {
        assert_eq!(action_invoked.arg_1, "default");

        Command::new("/usr/bin/kgx")
            .arg("--command=systemctl --failed")
            .output()
            .await
            .context("failed to spawn systemctl investigation terminal")?;
    }

    Ok(())
}

async fn check_updates(notifications: Notifications) -> Result<()> {
    info!("checking for package updates");

    let output = Command::new("/usr/bin/checkupdates")
        .output()
        .await
        .context("failed to run checkupdates")?
        .stdout;
    let updates = str::from_utf8(&output)
        .context("checkupdates output was not UTF-8")?
        .trim_end();

    if updates.is_empty() {
        return Ok(());
    }

    let count = updates.lines().count();
    let summary = "Updates available";
    let body = if count == 1 {
        let package = updates.split_once(' ').unwrap_or((updates, "")).0;
        format!("'{package}' is ready to update.")
    } else {
        format!("{count} packages are ready to update.")
    };

    let mut hints = HashMap::new();
    hints.insert("urgency".to_string(), Variant(Box::new(2u8) as _));

    let response = notifications
        .notify(
            "Maintenance",
            0,
            "software-update-available",
            summary,
            &body,
            vec!["default", "Update"],
            hints,
            -1,
        )
        .await?;

    if let Notification::ActionInvoked(action_invoked) = response {
        assert_eq!(action_invoked.arg_1, "default");

        Command::new("/usr/bin/kgx")
            .arg("--command=sudo pacman -Syu")
            .output()
            .await
            .context("failed to spawn upgrade terminal")?;
    }

    Ok(())
}

struct JournalctlAllow {
    matcher: RegexSet,
}

impl<'de> Deserialize<'de> for JournalctlAllow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BuildMatcher;

        impl<'de> de::Visitor<'de> for BuildMatcher {
            type Value = RegexSet;

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut matchers = Vec::new();

                while let Some(regex) = seq.next_element::<String>()? {
                    matchers.push(format!("^{regex}$"));
                }

                RegexSet::new(matchers.iter())
                    .map_err(|_| de::Error::custom("failed to build regex matchers"))
            }

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a set of regexes")
            }
        }

        Ok(Self {
            matcher: deserializer.deserialize_any(BuildMatcher)?,
        })
    }
}

#[derive(Deserialize)]
struct JournalctlEntry {
    #[serde(rename(deserialize = "SYSLOG_IDENTIFIER"))]
    identifier: String,
    #[serde(rename(deserialize = "MESSAGE"))]
    message: Option<String>,
}

async fn check_journalctl_errors(notifications: Notifications) -> Result<()> {
    info!("checking for journalctl errors from boot");

    let home = env::var_os("HOME").context("missing HOME environment variable")?;

    let mut allowlist_path = PathBuf::from(&home);
    allowlist_path.extend([".local", "state", "maintenance", "journalctl_allow.json"]);

    let allowlist = fs::read_to_string(&allowlist_path)
        .await
        .context("failed to read allowlist from journalctl_allow.json")?;
    let allowlist = serde_json::from_str::<HashMap<String, JournalctlAllow>>(&allowlist)
        .context("failed to deserialize allowlist from journalctl_allow.json")?;

    let output = Command::new("/usr/bin/journalctl")
        .args(["--boot", "--priority=err", "--output=json"])
        .output()
        .await
        .context("failed to run journalctl")?
        .stdout;
    let errors = serde_json::Deserializer::from_str(
        str::from_utf8(&output).context("journalctl output was invalid UTF-8")?,
    )
    .into_iter::<JournalctlEntry>();

    let mut error_log_contents = String::new();
    let mut unmatched_count = 0;
    for error in errors {
        let error = error.context("journalctl produced invalid JSON")?;
        if let Some(message) = &error.message
            && let Some(allow) = allowlist.get(&error.identifier)
            && allow.matcher.is_match(message)
        {
            continue;
        }

        unmatched_count += 1;

        writeln!(
            &mut error_log_contents,
            "{}: {}",
            error.identifier,
            error.message.as_deref().unwrap_or("")
        )?;
    }

    let mut error_log_path = PathBuf::from(&home);
    error_log_path.extend([".local", "state", "maintenance", "journalctl_new.log"]);

    fs::write(error_log_path, error_log_contents)
        .await
        .context("failed to create journalctl log")?;

    if unmatched_count == 0 {
        return Ok(());
    }

    let summary = "Unrecognized errors in journalctl";
    let body = if unmatched_count == 1 {
        "1 error not found in allowlist.".to_string()
    } else {
        format!("{unmatched_count} errors not found in allowlist.")
    };

    let mut hints = HashMap::new();
    hints.insert("urgency".to_string(), Variant(Box::new(2u8) as _));

    let response = notifications
        .notify(
            "Maintenance",
            0,
            "dialog-warning-symbolic",
            summary,
            &body,
            vec!["default", "View Errors"],
            hints,
            -1,
        )
        .await?;

    if let Notification::ActionInvoked(action_invoked) = response {
        assert_eq!(action_invoked.arg_1, "default");

        Command::new("/usr/bin/xdg-open")
            .arg("journalctl_new.log")
            .output()
            .await
            .context("failed to open journalctl log")?;
    }

    Ok(())
}
