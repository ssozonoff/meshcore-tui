# MeshCore Chat — Rust TUI Client

A terminal-based chat application for [MeshCore](https://meshcore.co.uk) mesh networking, built with the [meshcore-rs](https://github.com/andrewdavidmackenzie/meshcore-rs) library.

```
┌─ Status ──────────────────────────────────────────────────────────────┐
│ Port: /dev/ttyACM0 | 869.618 MHz | BW: 62.5 kHz | SF: 8 | CR: 5    │
├───────────────────────────────────────────┬── Contacts (5) ──────────┤
│ 15:48:05 [MSG] NL-UTC-CM-Echo: hello!     │ ▶ NL-UTC-CM  (a1b2c3d4) │
│ 15:48:10 [SND] → NL-UTC-CM-Echo: hi!     │   WB Home    (e5f6a7b8) │
│ 15:48:15 [GRP] Orwell84: anyone here?     │   Orwell84   (c9d0e1f2) │
├───────────────────────────────────────────┴──────────────────────────┤
│ 15:48:37 [ACK] H:0 | tag:a1b2c3d4                                   │
│ 15:48:05 [ADV] H:0 | PE1KEV Repeater @52.0089,4.3531                │
├──────────────────────────────────────────────────────────────────────┤
│ Send to: NL-UTC-CM-Echo (Enter=send, Tab=panel, Esc=quit)           │
└──────────────────────────────────────────────────────────────────────┘
```

## Requirements

- A MeshCore companion radio (USB serial or TCP)
- Rust 1.70+

## Build & Run

```bash
cargo build --release

# Serial (default /dev/ttyACM0)
cargo run --release -- --port /dev/ttyACM0

# TCP
cargo run --release -- --tcp 192.168.1.100:4403

# Specific channel
cargo run --release -- --port /dev/ttyACM0 --channel 0
```

## Keyboard

| Key | Action |
|-----|--------|
| Enter | Send message |
| Tab | Cycle panel (Messages→Contacts→Packets) |
| ↑/↓ | Select contact (Contacts panel) |
| F1 | Deselect contact (channel mode) |
| Esc / Ctrl+C | Quit |

## Commands

`/help` `/contacts` `/ch <n>` `/public` `/to <name>` `/advert` `/bat` `/name <n>` `/info` `/quit`

## API Compatibility

Built against the `meshcore-rs` crate which implements the MeshCore companion radio protocol:
- `MeshCore::serial()` / `MeshCore::tcp()` for connections
- `commands().lock().await.send_appstart()` → `SelfInfo` (radio params, name, key)
- `commands().lock().await.get_contacts(0)` → `Vec<Contact>`
- `commands().lock().await.send_msg(&contact, text, None)` → `MsgSentInfo`
- `commands().lock().await.send_chan_msg(channel, text, None)` → channel broadcast
- `subscribe(EventType::ContactMsgRecv, ...)` for incoming DMs
- `subscribe(EventType::ChannelMsgRecv, ...)` for channel messages
- `subscribe(EventType::Advertisement, ...)` for node adverts
- `start_auto_message_fetching()` for push-based message retrieval

## License

MIT
