use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use clap::Parser;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use meshcore_rs::events::{Contact, EventPayload, SelfInfo};
use meshcore_rs::{EventType, MeshCore};
use unicode_width::UnicodeWidthStr;

/// Strip Unicode replacement chars, control chars, and emoji from mesh strings.
/// Emoji have inconsistent terminal widths that break TUI border alignment.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| {
            if *c == '\u{FFFD}' || (*c != ' ' && c.is_control()) {
                return false;
            }
            // Strip emoji and other wide/variable-width Unicode blocks
            let cp = *c as u32;
            // Keep ASCII and basic Latin/extended chars
            if cp < 0x2600 {
                return true;
            }
            // Block various emoji and symbol ranges
            matches!(cp,
                // Allow box drawing, block elements, etc. used by TUI
                0x2500..=0x257F | // Box Drawing
                0x2580..=0x259F   // Block Elements
            )
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Truncate a string to fit within `max_width` display columns
fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut w = 0usize;
    let mut result = String::new();
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
        if w + cw > max_width {
            break;
        }
        w += cw;
        result.push(c);
    }
    result
}

/// Stored channel info
#[derive(Clone, Debug)]
struct ChannelEntry {
    name: String,
    secret: [u8; 16],
}
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};
use tokio::sync::Mutex;

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "meshcore-chat", about = "TUI chat for MeshCore mesh networking")]
struct Args {
    /// Serial port (e.g. /dev/ttyACM0)
    #[arg(short, long)]
    port: Option<String>,

    /// TCP host:port
    #[arg(short, long)]
    tcp: Option<String>,

    /// BLE device name (scans for MeshCore devices if omitted)
    #[arg(long)]
    ble: Option<Option<String>>,

    /// Baud rate
    #[arg(short, long, default_value_t = 115200)]
    baud: u32,

    /// Default channel (0 = public)
    #[arg(short, long, default_value_t = 0)]
    channel: u8,
}

// ── Data ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct ChatMessage {
    timestamp: String,
    sender: String,
    text: String,
    msg_type: MsgType,
}

#[derive(Clone, Debug, PartialEq)]
enum MsgType {
    Incoming,
    Outgoing,
    Channel(u8),
    System,
    Error,
}

#[derive(Clone, Debug)]
struct PacketLogEntry {
    timestamp: String,
    ptype: String,
    hops: u8,
    rssi: i16,
    snr: f32,
    info: String,
}

#[derive(Debug, Clone, PartialEq)]
enum Panel {
    Messages,
    Contacts,
    Packets,
}

struct App {
    input: String,
    cursor: usize,
    panel: Panel,
    messages: Vec<ChatMessage>,
    contacts: Vec<Contact>,
    channels: HashMap<u8, ChannelEntry>,
    /// Maps firmware channel_idx → local channel index for received message lookup
    fw_channel_map: HashMap<u8, u8>,
    packets: Vec<PacketLogEntry>,
    selected_contact: Option<usize>,
    self_info: Option<SelfInfo>,
    battery_level: Option<u16>,
    active_channel: u8,
    port_info: String,
    rx_count: u32,
    tx_count: u32,
    connected: bool,
    should_quit: bool,
}

impl App {
    fn new(channel: u8) -> Self {
        Self {
            input: String::new(),
            cursor: 0,
            panel: Panel::Messages,
            messages: Vec::new(),
            contacts: Vec::new(),
            channels: HashMap::new(),
            fw_channel_map: HashMap::new(),
            packets: Vec::new(),
            selected_contact: None,
            self_info: None,
            battery_level: None,
            active_channel: channel,
            port_info: String::new(),
            rx_count: 0,
            tx_count: 0,
            connected: false,
            should_quit: false,
        }
    }

    fn sys(&mut self, text: &str) {
        self.messages.push(ChatMessage {
            timestamp: now(),
            sender: "SYS".into(),
            text: text.into(),
            msg_type: MsgType::System,
        });
    }

    fn err(&mut self, text: &str) {
        self.messages.push(ChatMessage {
            timestamp: now(),
            sender: "ERR".into(),
            text: text.into(),
            msg_type: MsgType::Error,
        });
    }

    fn device_name(&self) -> String {
        self.self_info
            .as_ref()
            .map(|i| sanitize(&i.name))
            .unwrap_or_else(|| "Unknown".into())
    }

    fn target_label(&self) -> String {
        self.selected_contact
            .and_then(|i| self.contacts.get(i))
            .map(|c| sanitize(&c.adv_name))
            .unwrap_or_else(|| self.channel_label(self.active_channel))
    }

    fn channel_label(&self, ch: u8) -> String {
        // Direct lookup by local index
        if let Some(entry) = self.channels.get(&ch) {
            if !entry.name.is_empty() {
                return entry.name.clone();
            }
        }
        // Lookup via firmware channel_idx → local index mapping
        if let Some(&local_idx) = self.fw_channel_map.get(&ch) {
            if let Some(entry) = self.channels.get(&local_idx) {
                if !entry.name.is_empty() {
                    return entry.name.clone();
                }
            }
        }
        format!("CH#{}", ch)
    }

    fn resolve_sender(&self, prefix: &[u8; 6]) -> String {
        self.contacts
            .iter()
            .find(|c| &c.public_key[..6] == prefix)
            .map(|c| sanitize(&c.adv_name))
            .unwrap_or_else(|| {
                let hex = meshcore_rs::parsing::hex_encode(prefix);
                // Trim leading zeros for cleaner display
                let trimmed = hex.trim_start_matches('0');
                if trimmed.is_empty() {
                    "0".into()
                } else {
                    trimmed.to_string()
                }
            })
    }
}

fn now() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

/// Simple pseudo-random byte (no external crate needed)
fn rand_byte() -> u8 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    (t ^ (t >> 8) ^ (t >> 16)) as u8
}

// ── MeshCore Integration ─────────────────────────────────────────────────────

async fn connect(app: &Arc<Mutex<App>>, args: &Args) -> Option<MeshCore> {
    let mc = if let Some(ref tcp_addr) = args.tcp {
        let parts: Vec<&str> = tcp_addr.split(':').collect();
        let host = parts[0];
        let port: u16 = parts.get(1).unwrap_or(&"4403").parse().unwrap_or(4403);
        app.lock().await.sys(&format!("Connecting TCP {}:{}...", host, port));
        match MeshCore::tcp(host, port).await {
            Ok(mc) => mc,
            Err(e) => {
                app.lock().await.err(&format!("TCP connect failed: {}", e));
                return None;
            }
        }
    } else if args.ble.is_some() {
        let device_name = args.ble.as_ref().unwrap().as_deref();
        match device_name {
            Some(name) => app.lock().await.sys(&format!("Scanning BLE for '{}'...", name)),
            None => app.lock().await.sys("Scanning for any MeshCore BLE device..."),
        }
        match MeshCore::ble(device_name).await {
            Ok(mc) => mc,
            Err(e) => {
                app.lock().await.err(&format!("BLE connect failed: {}", e));
                return None;
            }
        }
    } else {
        let port = args.port.as_deref().unwrap_or("/dev/ttyACM0");
        app.lock().await.port_info = port.to_string();
        app.lock()
            .await
            .sys(&format!("Connecting serial {} @ {}...", port, args.baud));
        match MeshCore::serial(port, args.baud).await {
            Ok(mc) => mc,
            Err(e) => {
                app.lock()
                    .await
                    .err(&format!("Serial connect failed: {}", e));
                return None;
            }
        }
    };

    // AppStart → get SelfInfo
    match mc.commands().lock().await.send_appstart().await {
        Ok(info) => {
            let mut a = app.lock().await;
            a.sys(&format!("Connected: {} (freq: {} MHz, SF{}, CR{}, BW {} kHz)",
                sanitize(&info.name),
                info.radio_freq as f64 / 1_000_000.0,
                info.sf,
                info.cr,
                info.radio_bw as f64 / 1_000.0,
            ));
            a.connected = true;
            if a.port_info.is_empty() {
                a.port_info = if args.ble.is_some() {
                    format!("BLE: {}", sanitize(&info.name))
                } else {
                    "TCP".into()
                };
            }
            a.self_info = Some(info);
        }
        Err(e) => {
            app.lock().await.err(&format!("AppStart failed: {}", e));
            return None;
        }
    }

    // Sync time
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32;
        if let Err(e) = mc.commands().lock().await.set_time(ts).await {
            app.lock()
                .await
                .sys(&format!("Time sync warning: {}", e));
        }
    }

    // Battery
    if let Ok(bat) = mc.commands().lock().await.get_bat().await {
        let mut a = app.lock().await;
        a.battery_level = Some(bat.level);
        a.sys(&format!("Battery: {:.3}V", bat.level as f64 / 1000.0));
    }

    // Contacts (BLE needs longer timeout)
    let contacts_timeout = if args.ble.is_some() {
        Duration::from_secs(30)
    } else {
        Duration::from_secs(5)
    };
    match mc.commands().lock().await.get_contacts_with_timeout(0, contacts_timeout).await {
        Ok(contacts) => {
            let mut a = app.lock().await;
            a.sys(&format!("Loaded {} contacts", contacts.len()));
            a.contacts = contacts;
        }
        Err(e) => {
            app.lock()
                .await
                .err(&format!("Failed to load contacts: {}", e));
        }
    }

    // Load channels sequentially
    {
        let mut loaded = 0u8;
        let mut names = Vec::new();
        for idx in 0..16u8 {
            match mc.commands().lock().await.get_channel(idx).await {
                Ok(info) => {
                    let name = sanitize(&info.name);
                    if !name.is_empty() {
                        let mut a = app.lock().await;
                        a.channels.insert(idx, ChannelEntry {
                            name: name.clone(),
                            secret: info.secret,
                        });
                        a.fw_channel_map.insert(info.channel_idx, idx);
                        names.push(name);
                        drop(a);
                        loaded += 1;
                    }
                }
                Err(_) => continue,
            }
        }
        if loaded > 0 {
            app.lock().await.sys(&format!("Loaded {} channels: {}", loaded, names.join(", ")));
        }
    }

    Some(mc)
}

async fn setup_subscriptions(mc: &MeshCore, app: Arc<Mutex<App>>) {
    // Direct messages
    {
        let app = app.clone();
        mc.subscribe(EventType::ContactMsgRecv, HashMap::new(), move |event| {
            if let EventPayload::Message(msg) = event.payload {
                let app = app.clone();
                tokio::spawn(async move {
                    let mut a = app.lock().await;
                    let sender = a.resolve_sender(&msg.sender_prefix);
                    a.rx_count += 1;
                    a.messages.push(ChatMessage {
                        timestamp: now(),
                        sender,
                        text: sanitize(&msg.text),
                        msg_type: MsgType::Incoming,
                    });
                });
            }
        })
        .await;
    }

    // Channel messages
    {
        let app = app.clone();
        mc.subscribe(
            EventType::ChannelMsgRecv,
            HashMap::new(),
            move |event| {
                if let EventPayload::Message(msg) = event.payload {
                    let app = app.clone();
                    tokio::spawn(async move {
                        let mut a = app.lock().await;
                        let sender = a.resolve_sender(&msg.sender_prefix);
                        let ch = msg.channel.unwrap_or(0);
                        a.rx_count += 1;
                        a.messages.push(ChatMessage {
                            timestamp: now(),
                            sender,
                            text: sanitize(&msg.text),
                            msg_type: MsgType::Channel(ch),
                        });
                    });
                }
            },
        )
        .await;
    }

    // Advertisements
    {
        let app = app.clone();
        mc.subscribe(
            EventType::Advertisement,
            HashMap::new(),
            move |event| {
                if let EventPayload::Advertisement(adv) = event.payload {
                    let app = app.clone();
                    tokio::spawn(async move {
                        let mut a = app.lock().await;
                        a.packets.push(PacketLogEntry {
                            timestamp: now(),
                            ptype: "ADV".into(),
                            hops: 0,
                            rssi: 0,
                            snr: 0.0,
                            info: format!(
                                "{} @{:.4},{:.4}",
                                sanitize(&adv.name),
                                adv.lat as f64 / 1e6,
                                adv.lon as f64 / 1e6
                            ),
                        });
                        if a.packets.len() > 100 {
                            a.packets.remove(0);
                        }
                    });
                }
            },
        )
        .await;
    }

    // ACKs
    {
        let app = app.clone();
        mc.subscribe(EventType::Ack, HashMap::new(), move |event| {
            if let EventPayload::Ack { tag } = event.payload {
                let app = app.clone();
                tokio::spawn(async move {
                    let mut a = app.lock().await;
                    a.packets.push(PacketLogEntry {
                        timestamp: now(),
                        ptype: "ACK".into(),
                        hops: 0,
                        rssi: 0,
                        snr: 0.0,
                        info: format!("tag:{}", meshcore_rs::parsing::hex_encode(&tag)),
                    });
                    if a.packets.len() > 100 {
                        a.packets.remove(0);
                    }
                });
            }
        })
        .await;
    }

    // Path updates
    {
        let app = app.clone();
        mc.subscribe(EventType::PathUpdate, HashMap::new(), move |event| {
            if let EventPayload::PathUpdate(pu) = event.payload {
                let app = app.clone();
                tokio::spawn(async move {
                    let mut a = app.lock().await;
                    a.packets.push(PacketLogEntry {
                        timestamp: now(),
                        ptype: "PATH".into(),
                        hops: pu.path_len as u8,
                        rssi: 0,
                        snr: 0.0,
                        info: format!(
                            "{} len={}",
                            meshcore_rs::parsing::hex_encode(&pu.prefix),
                            pu.path_len
                        ),
                    });
                    if a.packets.len() > 100 {
                        a.packets.remove(0);
                    }
                });
            }
        })
        .await;
    }

    mc.start_auto_message_fetching().await;
}

async fn send_message(mc: &MeshCore, app: &Arc<Mutex<App>>, text: &str) {
    let (selected, channel) = {
        let a = app.lock().await;
        (a.selected_contact, a.active_channel)
    };

    if let Some(idx) = selected {
        // Direct message to selected contact
        let contact = {
            let a = app.lock().await;
            a.contacts.get(idx).cloned()
        };
        if let Some(contact) = contact {
            let name = sanitize(&contact.adv_name);
            match mc.commands().lock().await.send_msg(&contact, text, None).await {
                Ok(info) => {
                    let mut a = app.lock().await;
                    a.tx_count += 1;
                    a.messages.push(ChatMessage {
                        timestamp: now(),
                        sender: format!("→ {}", name),
                        text: text.into(),
                        msg_type: MsgType::Outgoing,
                    });
                    a.packets.push(PacketLogEntry {
                        timestamp: now(),
                        ptype: "SND".into(),
                        hops: 0,
                        rssi: 0,
                        snr: 0.0,
                        info: format!(
                            "ack:{} timeout:{}ms",
                            meshcore_rs::parsing::hex_encode(&info.expected_ack),
                            info.suggested_timeout
                        ),
                    });
                }
                Err(e) => app.lock().await.err(&format!("Send failed: {}", e)),
            }
        }
    } else {
        // Channel message
        match mc
            .commands()
            .lock()
            .await
            .send_chan_msg(channel, text, None)
            .await
        {
            Ok(_) => {
                let mut a = app.lock().await;
                a.tx_count += 1;
                let label = a.channel_label(channel);
                a.messages.push(ChatMessage {
                    timestamp: now(),
                    sender: format!("\u{2192} {}", label),
                    text: text.into(),
                    msg_type: MsgType::Outgoing,
                });
            }
            Err(e) => app.lock().await.err(&format!("Channel send failed: {}", e)),
        }
    }
}

// ── Slash Commands ───────────────────────────────────────────────────────────

async fn handle_command(cmd: &str, app: &Arc<Mutex<App>>, mc: Option<&MeshCore>) {
    let parts: Vec<&str> = cmd.trim().splitn(2, ' ').collect();
    let command = parts[0].to_lowercase();
    let arg = parts.get(1).copied().unwrap_or("");

    match command.as_str() {
        "/help" | "/h" => {
            let mut a = app.lock().await;
            a.sys("/help — commands  /contacts — reload  /ch <n> — switch channel");
            a.sys("/to <name> — DM contact  /public — CH#0  /advert — send advert");
            a.sys("/channels — list channels  /setch <idx> <name> [secret] — create channel");
            a.sys("/getch <idx> — show channel info");
            a.sys("/bat — battery  /name <n> — rename  /info — device info  /quit");
        }
        "/channels" | "/chs" => {
            let a = app.lock().await;
            if a.channels.is_empty() {
                drop(a);
                app.lock().await.sys("No channels loaded");
            } else {
                let mut lines_out: Vec<String> = Vec::new();
                let mut indices: Vec<u8> = a.channels.keys().copied().collect();
                indices.sort();
                for idx in indices {
                    if let Some(ch) = a.channels.get(&idx) {
                        let secret_hex = meshcore_rs::parsing::hex_encode(&ch.secret);
                        let active = if idx == a.active_channel { " \u{25c0}" } else { "" };
                        lines_out.push(format!(
                            "  #{}: {} [{}]{}",
                            idx, ch.name, secret_hex, active
                        ));
                    }
                }
                drop(a);
                let mut a = app.lock().await;
                a.sys("Channels:");
                for line in lines_out {
                    a.sys(&line);
                }
            }
        }
        "/contacts" | "/c" => {
            if let Some(mc) = mc {
                match mc.commands().lock().await.get_contacts(0).await {
                    Ok(c) => {
                        let mut a = app.lock().await;
                        a.sys(&format!("Loaded {} contacts", c.len()));
                        a.contacts = c;
                    }
                    Err(e) => app.lock().await.err(&format!("Contacts: {}", e)),
                }
            }
        }
        "/ch" | "/channel" => {
            if let Ok(ch) = arg.parse::<u8>() {
                let mut a = app.lock().await;
                a.active_channel = ch;
                a.selected_contact = None;
                let label = a.channel_label(ch);
                a.sys(&format!("Switched to {}", label));
            }
        }
        "/public" => {
            let mut a = app.lock().await;
            a.active_channel = 0;
            a.selected_contact = None;
            let label = a.channel_label(0);
            a.sys(&format!("Switched to {}", label));
        }
        "/to" => {
            let mut a = app.lock().await;
            if arg.is_empty() {
                a.err("Usage: /to <contact_name>");
            } else {
                let found = a
                    .contacts
                    .iter()
                    .position(|c| c.adv_name.to_lowercase().contains(&arg.to_lowercase()));
                if let Some(idx) = found {
                    let name = sanitize(&a.contacts[idx].adv_name);
                    a.selected_contact = Some(idx);
                    a.sys(&format!("Now chatting with: {}", name));
                } else {
                    a.err(&format!("Contact '{}' not found", arg));
                }
            }
        }
        "/advert" => {
            if let Some(mc) = mc {
                match mc.commands().lock().await.send_advert(true).await {
                    Ok(_) => app.lock().await.sys("Advertisement sent"),
                    Err(e) => app.lock().await.err(&format!("Advert: {}", e)),
                }
            }
        }
        "/bat" => {
            if let Some(mc) = mc {
                match mc.commands().lock().await.get_bat().await {
                    Ok(b) => {
                        let mut a = app.lock().await;
                        a.battery_level = Some(b.level);
                        a.sys(&format!("Battery: {:.3}V  Storage: {}", b.level as f64 / 1000.0, b.storage));
                    }
                    Err(e) => app.lock().await.err(&format!("Battery: {}", e)),
                }
            }
        }
        "/name" => {
            if arg.is_empty() {
                app.lock().await.err("Usage: /name <new_name>");
            } else if let Some(mc) = mc {
                match mc.commands().lock().await.set_name(arg).await {
                    Ok(_) => app.lock().await.sys(&format!("Name set to: {}", arg)),
                    Err(e) => app.lock().await.err(&format!("Set name: {}", e)),
                }
            }
        }
        "/info" | "/i" => {
            let a = app.lock().await;
            if let Some(ref info) = a.self_info {
                let lines = format!(
                    "Device: {} | Key: {} | Freq: {:.3} MHz | SF{} CR{} BW {:.1}kHz | TX: {}dBm | Bat: {}",
                    sanitize(&info.name),
                    meshcore_rs::parsing::hex_encode(&info.public_key[..6]),
                    info.radio_freq as f64 / 1e6,
                    info.sf,
                    info.cr,
                    info.radio_bw as f64 / 1e3,
                    info.tx_power,
                    a.battery_level.map(|b| format!("{:.3}V", b as f64 / 1000.0)).unwrap_or("N/A".into()),
                );
                drop(a);
                app.lock().await.sys(&lines);
            } else {
                drop(a);
                app.lock().await.err("No device info available");
            }
        }
        "/setch" | "/setchannel" => {
            // Usage: /setch <idx> <name> [hex_secret]
            // e.g.  /setch 1 MyChannel
            // e.g.  /setch 1 MyChannel 00112233445566778899aabbccddeeff
            let parts: Vec<&str> = arg.splitn(3, ' ').collect();
            if parts.len() < 2 {
                app.lock().await.err("Usage: /setch <idx> <name> [hex_secret_32chars]");
            } else if let Some(mc) = mc {
                let idx = match parts[0].parse::<u8>() {
                    Ok(i) => i,
                    Err(_) => {
                        app.lock().await.err("Channel index must be a number");
                        return;
                    }
                };
                let name = parts[1];
                let secret: [u8; 16] = if parts.len() >= 3 {
                    match meshcore_rs::parsing::hex_decode(parts[2]) {
                        Ok(bytes) if bytes.len() >= 16 => {
                            let mut s = [0u8; 16];
                            s.copy_from_slice(&bytes[..16]);
                            s
                        }
                        _ => {
                            app.lock().await.err("Secret must be 32 hex chars (16 bytes)");
                            return;
                        }
                    }
                } else {
                    // Generate random secret
                    let mut s = [0u8; 16];
                    for b in &mut s {
                        *b = rand_byte();
                    }
                    s
                };
                match mc.commands().lock().await.set_channel(idx, name, &secret).await {
                    Ok(_) => {
                        let secret_hex = meshcore_rs::parsing::hex_encode(&secret);
                        let mut a = app.lock().await;
                        a.channels.insert(idx, ChannelEntry {
                            name: name.to_string(),
                            secret,
                        });
                        a.sys(&format!(
                            "Channel #{} set: name='{}' secret={}",
                            idx, name, secret_hex
                        ));
                    }
                    Err(e) => app.lock().await.err(&format!("Set channel: {}", e)),
                }
            }
        }
        "/getch" | "/getchannel" => {
            if arg.is_empty() {
                app.lock().await.err("Usage: /getch <idx>");
            } else if let Some(mc) = mc {
                let idx = match arg.parse::<u8>() {
                    Ok(i) => i,
                    Err(_) => {
                        app.lock().await.err("Channel index must be a number");
                        return;
                    }
                };
                match mc.commands().lock().await.get_channel(idx).await {
                    Ok(info) => {
                        let name = sanitize(&info.name);
                        let secret_hex = meshcore_rs::parsing::hex_encode(&info.secret);
                        let mut a = app.lock().await;
                        if !name.is_empty() {
                            a.channels.insert(idx, ChannelEntry {
                                name: name.clone(),
                                secret: info.secret,
                            });
                            a.fw_channel_map.insert(info.channel_idx, idx);
                        }
                        a.sys(&format!(
                            "Channel #{} (fw#{}): name='{}' secret={}",
                            idx, info.channel_idx, name, secret_hex
                        ));
                    }
                    Err(e) => app.lock().await.err(&format!("Get channel: {}", e)),
                }
            }
        }
        "/quit" | "/q" => app.lock().await.should_quit = true,
        _ => app.lock().await.err(&format!("Unknown command: {}", command)),
    }
}

// ── TUI Rendering ────────────────────────────────────────────────────────────

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // status
            Constraint::Min(10),   // main
            Constraint::Length(7), // packets
            Constraint::Length(3), // input
        ])
        .split(f.area());

    draw_status(f, app, chunks[0]);
    draw_main(f, app, chunks[1]);
    draw_packets(f, app, chunks[2]);
    draw_input(f, app, chunks[3]);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let freq_info = app
        .self_info
        .as_ref()
        .map(|i| {
            format!(
                "{:.3} MHz | BW: {:.1} kHz | SF: {} | CR: {}",
                i.radio_freq as f64 / 1e6,
                i.radio_bw as f64 / 1e3,
                i.sf,
                i.cr
            )
        })
        .unwrap_or_default();

    let spans = vec![
        Span::styled("Port: ", Style::default().fg(Color::White)),
        Span::styled(&app.port_info, Style::default().fg(Color::Cyan)),
        Span::styled(" | ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            &freq_info,
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(" | ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("RX: {} | TX: {}", app.rx_count, app.tx_count),
            Style::default().fg(Color::Green),
        ),
        Span::styled(" | ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            if app.connected { "● Connected" } else { "○ Disconnected" },
            Style::default().fg(if app.connected {
                Color::Green
            } else {
                Color::Red
            }),
        ),
    ];

    let w = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Status ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(w, area);
}

fn draw_main(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(40), Constraint::Length(30)])
        .split(area);

    // Messages
    let panel_width = (chunks[0].width as usize).saturating_sub(2); // border
    let items: Vec<ListItem> = app
        .messages
        .iter()
        .map(|m| {
            let (tag, tag_c, name_c) = match &m.msg_type {
                MsgType::Incoming => ("[MSG]".to_string(), Color::Cyan, Color::Cyan),
                MsgType::Outgoing => ("[SND]".to_string(), Color::Green, Color::Green),
                MsgType::Channel(ch) => {
                    let label = app.channel_label(*ch);
                    (format!("[{}]", label), Color::Yellow, Color::Yellow)
                }
                MsgType::System => ("[SYS]".to_string(), Color::DarkGray, Color::DarkGray),
                MsgType::Error => ("[ERR]".to_string(), Color::Red, Color::Red),
            };

            // Build prefix: "HH:MM:SS [TAG] sender: "
            let prefix = format!("{} {} {}: ", m.timestamp, tag, m.sender);
            let prefix_len = prefix.width();
            let text_display_width = m.text.width();

            if prefix_len + text_display_width <= panel_width || panel_width <= prefix_len {
                // Fits on one line or panel too narrow to wrap
                ListItem::new(Line::from(vec![
                    Span::styled(&m.timestamp, Style::default().fg(Color::DarkGray)),
                    Span::raw(" "),
                    Span::styled(tag, Style::default().fg(tag_c).add_modifier(Modifier::BOLD)),
                    Span::raw(" "),
                    Span::styled(
                        &m.sender,
                        Style::default().fg(name_c).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(": ", Style::default().fg(name_c)),
                    Span::styled(&m.text, Style::default().fg(Color::White)),
                ]))
            } else {
                // Wrap text across multiple lines
                let text_width = panel_width.saturating_sub(prefix_len);
                let continuation_indent = " ".repeat(prefix_len);
                let mut lines: Vec<Line> = Vec::new();
                let chars: Vec<char> = m.text.chars().collect();

                // Split chars into chunks that fit in text_width display columns
                let mut chunk_start = 0;
                let mut first = true;
                while chunk_start < chars.len() {
                    let mut w = 0usize;
                    let mut chunk_end = chunk_start;
                    while chunk_end < chars.len() {
                        let cw = unicode_width::UnicodeWidthChar::width(chars[chunk_end]).unwrap_or(1);
                        if w + cw > text_width {
                            break;
                        }
                        w += cw;
                        chunk_end += 1;
                    }
                    if chunk_end == chunk_start {
                        chunk_end += 1; // always advance at least one char
                    }
                    let chunk_text: String = chars[chunk_start..chunk_end].iter().collect();

                    if first {
                        lines.push(Line::from(vec![
                            Span::styled(&m.timestamp, Style::default().fg(Color::DarkGray)),
                            Span::raw(" "),
                            Span::styled(tag.clone(), Style::default().fg(tag_c).add_modifier(Modifier::BOLD)),
                            Span::raw(" "),
                            Span::styled(
                                &m.sender,
                                Style::default().fg(name_c).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(": ", Style::default().fg(name_c)),
                            Span::styled(chunk_text, Style::default().fg(Color::White)),
                        ]));
                        first = false;
                    } else {
                        lines.push(Line::from(vec![
                            Span::styled(
                                continuation_indent.clone(),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(chunk_text, Style::default().fg(Color::White)),
                        ]));
                    }
                    chunk_start = chunk_end;
                }

                ListItem::new(lines)
            }
        })
        .collect::<Vec<_>>();

    // Take items from the end that fit in visible height
    let visible_height = (chunks[0].height as usize).saturating_sub(2);
    let mut total_lines = 0usize;
    let mut start_idx = items.len();
    for (i, item) in items.iter().enumerate().rev() {
        let h = item.height();
        if total_lines + h > visible_height {
            break;
        }
        total_lines += h;
        start_idx = i;
    }
    let visible_items: Vec<ListItem> = items.into_iter().skip(start_idx).collect();

    let bc = if app.panel == Panel::Messages {
        Color::Yellow
    } else {
        Color::Cyan
    };
    let msg_w = List::new(visible_items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} — {} ", app.device_name(), app.target_label()))
            .border_style(Style::default().fg(bc)),
    );
    f.render_widget(msg_w, chunks[0]);

    // Contacts
    let cpanel_width = (chunks[1].width as usize).saturating_sub(2); // minus borders
    let citems: Vec<ListItem> = app
        .contacts
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let sel = app.selected_contact == Some(i);
            let sty = if sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let name = sanitize(&c.adv_name);
            let hex = c.prefix_hex();
            let hex_short = &hex[..8.min(hex.len())];

            // Build the full line and truncate to exactly cpanel_width
            let marker = if sel { "\u{25b6} " } else { "  " };
            let full = format!("{}{} ({})", marker, name, hex_short);
            let line = truncate_to_width(&full, cpanel_width);

            // Pad with spaces to fill the panel (needed for selection highlight)
            let line_w = UnicodeWidthStr::width(line.as_str());
            let padded = if line_w < cpanel_width {
                format!("{}{}", line, " ".repeat(cpanel_width - line_w))
            } else {
                line
            };

            ListItem::new(Span::styled(padded, sty))
        })
        .collect();

    let cc = if app.panel == Panel::Contacts {
        Color::Yellow
    } else {
        Color::Green
    };
    let c_w = List::new(citems).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Contacts ({}) ", app.contacts.len()))
            .border_style(Style::default().fg(cc)),
    );
    f.render_widget(c_w, chunks[1]);
}

fn draw_packets(f: &mut Frame, app: &App, area: Rect) {
    let visible = (area.height as usize).saturating_sub(2);
    let items: Vec<ListItem> = app
        .packets
        .iter()
        .rev()
        .take(visible)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|p| {
            let tc = match p.ptype.as_str() {
                "ADV" => Color::Magenta,
                "ACK" => Color::Cyan,
                "SND" => Color::Green,
                "PATH" => Color::Blue,
                _ => Color::White,
            };
            ListItem::new(Line::from(vec![
                Span::styled(&p.timestamp, Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(
                    format!("[{}]", p.ptype),
                    Style::default().fg(tc).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" H:{}", p.hops),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    if p.rssi != 0 || p.snr != 0.0 {
                        format!(" R:{}dBm S:{:.1}dB", p.rssi, p.snr)
                    } else {
                        String::new()
                    },
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!(" | {}", p.info),
                    Style::default().fg(Color::White),
                ),
            ]))
        })
        .collect();

    let pc = if app.panel == Panel::Packets {
        Color::Yellow
    } else {
        Color::Magenta
    };
    let w = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Received Packets ")
            .border_style(Style::default().fg(pc)),
    );
    f.render_widget(w, area);
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let w = Paragraph::new(app.input.as_str())
        .style(Style::default().fg(Color::White))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " Send to: {} (Enter=send, Tab=panel, ↑↓=contact, F1=deselect, Esc=quit) ",
                    app.target_label()
                ))
                .border_style(Style::default().fg(Color::Cyan)),
        );
    f.render_widget(w, area);
    f.set_cursor_position((area.x + app.cursor as u16 + 1, area.y + 1));
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Tracing: suppress all output by default (TUI uses the terminal).
    // Set MESHCORE_LOG=1 to write debug logs to meshcore-chat.log
    if std::env::var("MESHCORE_LOG").is_ok() {
        let log_file = std::fs::File::create("meshcore-chat.log").ok();
        if let Some(file) = log_file {
            tracing_subscriber::fmt()
                .with_env_filter("meshcore_rs=debug,btleplug=warn")
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .init();
        }
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let app = Arc::new(Mutex::new(App::new(args.channel)));

    {
        let mut a = app.lock().await;
        a.sys("MeshCore Chat — Rust TUI Client v0.1");
        a.sys("Tab=panel  ↑↓=contact  F1=deselect  Enter=send  /help=commands  Esc=quit");
    }

    let mc = connect(&app, &args).await;

    if let Some(ref mc) = mc {
        app.lock().await.sys("Ready! Type a message and press Enter.");
        setup_subscriptions(mc, app.clone()).await;
    } else {
        app.lock()
            .await
            .sys("Offline mode — fix connection and restart.");
    }

    // Main loop
    loop {
        {
            let a = app.lock().await;
            terminal.draw(|f| draw(f, &*a))?;
        }

        if event::poll(Duration::from_millis(50))? {
            if let CEvent::Key(key) = event::read()? {
                let mut a = app.lock().await;
                match key.code {
                    KeyCode::Esc => {
                        a.should_quit = true;
                    }
                    KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'c' => {
                        a.should_quit = true;
                    }
                    KeyCode::Enter => {
                        if !a.input.is_empty() {
                            let text = a.input.clone();
                            a.input.clear();
                            a.cursor = 0;
                            drop(a);

                            if text.starts_with('/') {
                                handle_command(&text, &app, mc.as_ref()).await;
                            } else if let Some(ref mc) = mc {
                                send_message(mc, &app, &text).await;
                            } else {
                                app.lock().await.err("Not connected");
                            }
                        }
                    }
                    KeyCode::Char(c) => {
                        let pos = a.cursor;
                        a.input.insert(pos, c);
                        a.cursor += 1;
                    }
                    KeyCode::Backspace => {
                        if a.cursor > 0 {
                            let pos = a.cursor - 1;
                            a.input.remove(pos);
                            a.cursor -= 1;
                        }
                    }
                    KeyCode::Left => {
                        a.cursor = a.cursor.saturating_sub(1);
                    }
                    KeyCode::Right => {
                        a.cursor = (a.cursor + 1).min(a.input.len());
                    }
                    KeyCode::Home => a.cursor = 0,
                    KeyCode::End => a.cursor = a.input.len(),
                    KeyCode::Tab => {
                        a.panel = match a.panel {
                            Panel::Messages => Panel::Contacts,
                            Panel::Contacts => Panel::Packets,
                            Panel::Packets => Panel::Messages,
                        };
                    }
                    KeyCode::Up if a.panel == Panel::Contacts => {
                        if let Some(s) = a.selected_contact {
                            if s > 0 {
                                a.selected_contact = Some(s - 1);
                            }
                        } else if !a.contacts.is_empty() {
                            a.selected_contact = Some(0);
                        }
                    }
                    KeyCode::Down if a.panel == Panel::Contacts => {
                        if let Some(s) = a.selected_contact {
                            if s + 1 < a.contacts.len() {
                                a.selected_contact = Some(s + 1);
                            }
                        } else if !a.contacts.is_empty() {
                            a.selected_contact = Some(0);
                        }
                    }
                    KeyCode::F(1) => {
                        a.selected_contact = None;
                    }
                    _ => {}
                }
            }
        }

        if app.lock().await.should_quit {
            break;
        }
    }

    // Cleanup
    if let Some(mc) = mc {
        let _ = mc.disconnect().await;
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    println!("MeshCore Chat terminated.");
    Ok(())
}
