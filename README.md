# Hyakume ༼⁠ ⁠つ⁠ ⁠◕⁠‿⁠◕⁠ ⁠༽⁠つ

> A lovely bridge between trusted devices, where sharing is a choice and trust
> is earned.

Hyakume is an app for sending files, text, and links between your own devices,
without an account. It is still being built.

Its mascot is Meme, a friendly hundred-eyed creature. Each paired device is one
of Meme's eyes.

## How it works

1. Pair two devices and check that both show the same four words.
2. Choose a device and what to send.
3. The other device asks whether to accept it.
4. Hyakume sends it encrypted and checks it arrived intact before saving it,
   without overwriting existing files.

Paired devices remember each other. Transfers work on a local network without
internet access. Clipboard text is only sent when you ask for it.

Hyakume starts as a Linux command-line app, then gets a desktop app for Linux
and Windows, then Android. macOS and iOS come later. The website will be at
[hyakume.org](https://hyakume.org).

See [PROJECT.md](PROJECT.md) for the full plan.

## Development

Install [mise](https://mise.jdx.dev/), then:

```sh
mise trust
mise install rust
mise run ci
```

`mise run ci` checks formatting, runs Clippy and the tests, and builds the app.
`mise tasks` lists the other tasks.

Contact: [vincent@sshmoi.com](mailto:vincent@sshmoi.com).

## License

Licensed under [Apache-2.0](LICENSE). Dependencies must use licenses
compatible with Apache-2.0, and each release will include the notices of
every crate compiled into it.
