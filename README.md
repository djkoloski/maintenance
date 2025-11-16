# `maintenance`

A routine system maintenance tool for Arch Linux.

`maintenance` performs system health and maintenance checks at startup. It integrates with systemd and uses D-Bus to send notifications to the desktop environment.

## Features

`maintenance` performs the following system health and maintenance checks:

- Checks `systemctl` for any services which failed to start.
- Checks `journalctl` for any errors that occurred since the last boot. It supports an allowlist of error messages so benign errors can be ignored.
- Uses `checkupdates` to check for any upgradeable packages.
