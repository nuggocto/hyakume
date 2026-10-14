# Installing Hyakume on Linux

Check the download first:

```sh
sha256sum -c hyakume-VERSION-TARGET.tar.gz.sha256
tar -xzf hyakume-VERSION-TARGET.tar.gz
cd hyakume-VERSION-TARGET
```

## Install

```sh
install -D -m 0755 bin/hyakume ~/.local/bin/hyakume
install -D -m 0644 share/hyakume/hyakume.service ~/.config/systemd/user/hyakume.service
systemctl --user daemon-reload
systemctl --user enable --now hyakume
```

`hyakume service run` also runs the service in a terminal instead.

The service listens on the loopback address unless told otherwise. To pair
and send between two machines, give it this machine's LAN address in
`hyakume.service`, for example `--listen 192.168.1.20:7400`, then run
`systemctl --user daemon-reload` and `systemctl --user restart hyakume`.

## First start

```sh
hyakume init
```

`init` asks where to keep this device's key: in the Secret Service (your
desktop keyring) when it is unlocked, or in a key file protected by a
passphrase. Over SSH or on a machine without a desktop, choose the key file.
The choice is permanent; `hyakume reset` starts over with a new identity.

## After a restart

A key in the Secret Service is loaded when the service starts, if the
keyring is unlocked. A key file needs its passphrase:

```sh
hyakume unlock
hyakume status
```

To keep an unlocked service running after you log out of an SSH session,
let your user's services linger. This is your choice to make: while the
service is unlocked, its key is in memory.

```sh
loginctl enable-linger
```

`hyakume lock` removes the key from memory; work in progress pauses.

## Pair and send

```sh
# On the first device:
hyakume pair --invite --alias laptop
# On the second, paste the invitation when asked:
hyakume pair --alias desktop
# Both screens show four words; answer yes only if they match.

hyakume status                      # shows this device's route
hyakume receive --dir ~/Downloads   # on the receiving device
hyakume send desktop --at ROUTE notes.txt
```

## Uninstall

```sh
systemctl --user disable --now hyakume
rm ~/.config/systemd/user/hyakume.service ~/.local/bin/hyakume
```

Received files stay where they were published. Hyakume's own state stays in
`~/.local/state/hyakume`: the database, the encrypted key file if you chose
one, sealed spools of unfinished sends, and its own receive folder. Remove
that directory to forget this device completely. A key in the Secret Service
is removed by `hyakume reset`, before uninstalling.
