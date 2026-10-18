<img src="docs/icon.png" width="72" align="right" alt="shopot icon">

# shopot

[![CI](https://github.com/EryyynIT/shopot/actions/workflows/ci.yml/badge.svg)](https://github.com/EryyynIT/shopot/actions/workflows/ci.yml)

Desktop GUI client for [shopot-core](https://github.com/Puxxalwl/shopot-core/) —
a serverless P2P messaging engine with end-to-end encryption and forward secrecy.

Built with [iced](https://github.com/iced-rs/iced) on the tiny-skia software
renderer in Rust: dark content-first palette, one accent color, honest send
statuses, and no invented read receipts or presence.

[README на русском](README.ru.md)

![Screenshot](docs/screenshot.png)

## Status

Published release: **v0.1.1**. The capabilities below describe current
`main`, not the contents of that older release archive. Current main talks
to a real `shopot serve` daemon over its binary frame API 2.2.0
on a Unix domain socket (`$HOME/.shopot/api.sock`); `--mock` runs a built-in
demo scene instead.

The current client includes history, search, calls, formatting, reactions,
files and avatars. Their restart and persistence semantics follow the core API;
GUI memory is not a second store.

Done so far:

- typed core link: binary frame codec, request correlation, events, reconnect
  with resync; an e2e test runs invite → message → delivery ACK against two
  real daemons (gated by `SHOPOT_BIN`)
- meeting flow: one-time invite codes with a QR, adding a contact by someone
  else's code; LAN, address and relay codes
- contacts list with presence dots and unread badges
- message feed with day separators, system events and honest send statuses
  (sending → sent / queued; delivered only after a matched core ID; failed);
  compact feed mode
- history and API-gated search, pins, contact cards and chat deletion
- formatting, reaction controls, files, avatars and calls
- contact card and right-click menu with block/unblock; Escape closes
  dismissible dialogs, not an active call
- desktop notifications via `notify-send`; message body hiding is on by
  default, but the contact name is an external title and file notifications
  currently include filename/size; the focused open chat stays silent
- settings panel: notification text privacy, compact feed, font scale
  (80–150%)
- keyboard control: Ctrl+1..9 jumps to a chat by its position in the list,
  Ctrl+Q quits, Escape closes dismissible dialogs
- auto-growing composer with a send button
- headless UI tests (`iced_test`) plus unit tests; CI runs fmt, clippy, tests,
  a release build and cargo-deny on every push

Known limits: the chronological history window is at most 200 messages;
search/pin scroll anchors and replies are not implemented. Settings and
drafts are session-only; reactions are not restored from history. The GUI
does not write message or contact content to disk or logs. This is not a
guarantee about OS notification storage. Two-machine user acceptance, live
bidirectional voice quality and complete accessibility checks remain open.

### Design reference

The desktop contract adapts content-first hierarchy, explicit states and
measured validation from [Stille UI design (EN)](https://github.com/Puxxalwl/Stille-Messenger-Android/blob/main/docs/UI-DESIGN.md)
and [RU](https://github.com/Puxxalwl/Stille-Messenger-Android/blob/main/ru/UI-DESIGN.md).
It does not copy Android code, bottom tabs, swipe-only navigation, Back,
ripple, 48dp units or motion blur. Shopot keeps opaque surfaces and its
sidebar/conversation layout. This reference is provenance, not a second
guideline or permission to add dependencies.

## Download

Releases ship one archive with the client, the daemon and a launcher for
Linux x86_64 — no toolchains needed:

```sh
tar -xzf shopot-v0.1.1-x86_64-linux-gnu.tar.gz
cd shopot && ./run.sh
```

`run.sh` creates your identity on first start, runs the daemon in the
background and opens the client. Building from source is below.

Local packaging prototypes are available for development, but are not part of
remote releases yet. AppImage requires `appimagetool`; Flatpak requires
`flatpak-builder` and the Freedesktop 24.08 runtime:

```sh
scripts/build_appimage.sh dev
flatpak-builder --user --install build-flatpak \
  packaging/flatpak/io.github.EryyynIT.shopot.yaml
```

The packaging layout is intentionally provisional until the product rename and
visual redesign are complete.

## Build and run

Requires a stable Rust toolchain and [Zig 0.17.0](https://ziglang.org/) for
the daemon.

```sh
# the engine: build once, init your identity, run the daemon
cd core/zig && zig build -Doptimize=ReleaseSafe && cd ../..
./core/zig/zig-out/bin/shopot-zig init
./core/zig/zig-out/bin/shopot-zig serve

# the client, in another terminal
cargo run --release
```

To meet someone, press «Пригласить» in the sidebar and press «Выдать код»:
you get a one-time invite code with a QR — hand it over out of band. To add
a contact, paste their code into «Вставить чужой код» and press «Добавить»;
the chat opens once both sides are known to the daemon. Right-click a contact
to block or unblock them; Escape closes dismissible dialogs.

Implemented shortcuts: Ctrl+1..9 opens the Nth chat from the list, Ctrl+Q
quits, Escape closes dismissible dialogs (not an active call). Complete
arrow/Tab traversal and visible focus-ring acceptance remain pending.
Incoming messages pop up as desktop notifications (`notify-send`) with the
sender's name; body text is hidden by default unless you allow it in
«Настройки». File notifications currently include filename/size regardless of
that setting. The button is in the title bar, which also holds the compact feed
toggle and font scale.

Meeting across the internet (different networks, different cities): a code
without an address only works inside one LAN. The daemon prints
«слушаю 0.0.0.0:PORT» on start — forward that port on your router, then type
your public `host:port` into «Адрес» before issuing: the peer's daemon dials
you directly. The «через relay» checkbox routes the
connection through a `shopot relay` server so the peer never sees your IP;
on the pinned daemon build this path is experimental — the address code is
the reliable one.

## The core

shopot itself is the client; the engine — encrypted P2P transport, sessions,
storage — lives in [shopot-core](https://github.com/Puxxalwl/shopot-core) by
[puxxalwl](https://github.com/puxxalwl). The pinned API contract is vendored
as the `core` git submodule (binary protocol spec:
`core/zig/docs/CORE-API.md`).

## License

[MIT](LICENSE)
