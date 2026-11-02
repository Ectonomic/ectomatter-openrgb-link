//! The OpenRGB Link plugin: everything OpenRGB lights, brought in as one device
//! whose profiles are OpenRGB's own saved profiles. A profile in OpenRGB holds
//! every device it lights, so the device here is all of them together, named
//! by what they are ("Motherboard, RAM, graphics card and mouse").
//!
//! It talks to OpenRGB's SDK server (OpenRGB's network protocol, on this
//! computer, the one address its manifest names) at protocol 5, the last
//! before the server began sending every light's every change to its
//! clients. Loading a profile there is all a switch needs: the server sets
//! the lights itself. What protocol 5 can't do is say which profile is in
//! use, which putting the lights back needs; for that alone the plugin makes
//! a short second connection at protocol 6, asks, and closes it.
//!
//! The server may be off (OpenRGB starts without it unless asked): the plugin
//! says so, for Settings to show, and looks again every few seconds.
//!
//! What to put back is the person's own profile, never one Ectomatter switches
//! to: before its first switch the plugin asks OpenRGB which profile is in use,
//! and takes the answer as the person's only when it isn't one of Ectomatter's;
//! when OpenRGB can't say, or names one of Ectomatter's, the person's profile as
//! last seen goes back, and with none known, nothing does. It's all written
//! down in the plugin's folder (KEPT) before the switch, so a run of Ectomatter
//! that ends without putting the lights back leaves it for the next to do.

use std::cell::RefCell;

use serde_json::{Value, json};

wit_bindgen::generate!({ path: "../../../wit", world: "core" });

use ectomatter::core::{host, net};

const ADDRESS: &str = "127.0.0.1:6742";
/// what's kept between runs (see State's switched, before, own and ours)
const KEPT: &str = "/data/put-back.json";
const DEVICE: &str = "openrgb";
/// the timers: looking for the server, and reading what it sends unasked
const LOOK: u32 = 1;
const LISTEN: u32 = 2;
const LOOK_EVERY: u32 = 5000;
const LISTEN_EVERY: u32 = 1000;

const OFF: &str = "OpenRGB’s SDK server is off. In OpenRGB, open the SDK Server tab and select Start Server. To have it start with OpenRGB, add --server to Custom Arguments under Settings, General Settings.";

// the packets used (OpenRGB's NetworkProtocol.h)
const CONTROLLER_COUNT: u32 = 0;
const CONTROLLER_DATA: u32 = 1;
const PROTOCOL_VERSION: u32 = 40;
const SET_CLIENT_NAME: u32 = 50;
const SET_CLIENT_FLAGS: u32 = 52;
const DEVICE_LIST_UPDATED: u32 = 100;
const PROFILE_LIST: u32 = 150;
const LOAD_PROFILE: u32 = 152;
const GET_ACTIVE_PROFILE: u32 = 156;
const PROFILE_ABOUT_TO_LOAD: u32 = 159;
const PROFILE_LIST_UPDATED: u32 = 160;

/// One of OpenRGB's devices, as far as Ectomatter needs it: its kind and name.
#[derive(Clone, Debug, PartialEq)]
struct Lit {
    kind: i32,
    name: String,
}

#[derive(Default)]
struct State {
    link: Option<net::TcpStream>,
    /// what the main connection has read that isn't a whole packet yet
    got: Vec<u8>,
    version: u32,
    devices: Vec<Lit>,
    profiles: Vec<String>,
    /// while Ectomatter has OpenRGB on profiles of its own choosing (`switched`), the
    /// person's to put back after (none when it couldn't be known); the person's own
    /// profile as OpenRGB last named it; and the profiles Ectomatter switches OpenRGB
    /// to, which are never taken for the person's own
    switched: bool,
    before: Option<String>,
    own: Option<String>,
    ours: Vec<String>,
    /// the profile just put back, and when: a look a moment later says what OpenRGB is on;
    /// a putting back owed, asked while OpenRGB was out of reach (done once it's met)
    check: Option<(String, u64)>,
    owed: bool,
    /// whether the plugin has met OpenRGB since it started: a run that ended with
    /// OpenRGB switched is put right at the first meeting only
    met: bool,
    told: bool,
    warned: Option<String>,
}

thread_local! {
    static S: RefCell<State> = RefCell::new(State::default());
}

// ---------------------------------------------------------------- the wire

fn header(dev: u32, id: u32, size: u32) -> Vec<u8> {
    let mut h = b"ORGB".to_vec();
    h.extend_from_slice(&dev.to_le_bytes());
    h.extend_from_slice(&id.to_le_bytes());
    h.extend_from_slice(&size.to_le_bytes());
    h
}

fn packet(dev: u32, id: u32, body: &[u8]) -> Vec<u8> {
    let mut p = header(dev, id, body.len() as u32);
    p.extend_from_slice(body);
    p
}

/// A name as OpenRGB's packets carry it in a request: its bytes, then a nul.
fn named(s: &str) -> Vec<u8> {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    b
}

/// What has come in, kept until it makes whole packets (the main
/// connection's kept between reads, so a packet split across two isn't lost).
struct Reader<'a> {
    link: &'a net::TcpStream,
    got: &'a mut Vec<u8>,
}

impl Reader<'_> {
    /// The next whole packet (its device, its id, its body), waiting up to
    /// `ms` for it; none if none came whole in that time.
    fn next(&mut self, ms: u32) -> Result<Option<(u32, u32, Vec<u8>)>, String> {
        let start = now_ms();
        loop {
            if self.got.len() >= 16 {
                if &self.got[..4] != b"ORGB" {
                    return Err("OpenRGB sent something that isn’t one of its packets".into());
                }
                let n = |i: usize| u32::from_le_bytes([self.got[i], self.got[i + 1], self.got[i + 2], self.got[i + 3]]);
                let (dev, id, size) = (n(4), n(8), n(12) as usize);
                if self.got.len() >= 16 + size {
                    let body = self.got[16..16 + size].to_vec();
                    self.got.drain(..16 + size);
                    return Ok(Some((dev, id, body)));
                }
            }
            let left = ms.saturating_sub((now_ms() - start) as u32);
            if left == 0 && ms > 0 {
                return Ok(None);
            }
            let more = self.link.read(1 << 16, left.max(if ms == 0 { 0 } else { 1 }))?;
            if more.is_empty() {
                if ms == 0 || now_ms() - start >= ms as u64 {
                    return Ok(None);
                }
                continue;
            }
            self.got.extend_from_slice(&more);
        }
    }

    /// The answer to a request, by its id: what else comes first is dealt
    /// with (a profile about to load is answered, as OpenRGB waits for that)
    /// and let go.
    fn answer(&mut self, want: u32, ms: u32) -> Result<Option<(u32, Vec<u8>)>, String> {
        let until = now_ms() + ms as u64;
        loop {
            let left = until.saturating_sub(now_ms()) as u32;
            match self.next(left.max(1))? {
                Some((dev, id, body)) if id == want => return Ok(Some((dev, body))),
                Some((_, PROFILE_ABOUT_TO_LOAD, _)) => {
                    self.link.write(&header(0, PROFILE_ABOUT_TO_LOAD, 0))?;
                }
                Some(_) => {}
                None => return Ok(None),
            }
            if now_ms() >= until {
                return Ok(None);
            }
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// A packet's body, read in order.
struct Body<'a> {
    b: &'a [u8],
    at: usize,
}

impl Body<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self.at.checked_add(n).filter(|e| *e <= self.b.len()).ok_or("OpenRGB’s answer ended early")?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16, String> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32, String> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i32(&mut self) -> Result<i32, String> {
        Ok(self.u32()? as i32)
    }
    /// a string: its length (with its nul), then it
    fn str(&mut self) -> Result<String, String> {
        let n = self.u16()? as usize;
        let s = self.take(n)?;
        Ok(String::from_utf8_lossy(s.strip_suffix(&[0]).unwrap_or(s)).into_owned())
    }
}

/// A device's kind and name, from its description as protocol `v` lays it
/// out (OpenRGB's RGBController::GetDeviceDescriptionData): read only as far
/// as the name, the rest left.
fn lit_of(body: &[u8], v: u32) -> Result<Lit, String> {
    let mut r = Body { b: body, at: 0 };
    let _size = r.u32()?;
    let kind = r.i32()?;
    let name = r.str()?;
    if v >= 1 {
        let _vendor = r.str()?;
    }
    Ok(Lit { kind, name })
}

/// The profiles' names from their list (a u32 size, a u16 count, then each name).
fn profiles_of(body: &[u8]) -> Result<Vec<String>, String> {
    let mut r = Body { b: body, at: 0 };
    let _size = r.u32()?;
    let n = r.u16()?;
    (0..n).map(|_| r.str()).collect()
}

// ---------------------------------------------------------------- talking to OpenRGB

/// A connection made and agreed at a protocol (no higher than `want`): the
/// connection, and the protocol agreed.
fn open(want: u32, timeout: u32) -> Result<(net::TcpStream, u32), String> {
    let link = net::connect(ADDRESS, timeout)?;
    link.write(&packet(0, PROTOCOL_VERSION, &want.to_le_bytes()))?;
    let mut got = Vec::new();
    let mut r = Reader { link: &link, got: &mut got };
    // (a server too old to answer speaks protocol 0)
    let theirs = match r.answer(PROTOCOL_VERSION, 1000)? {
        Some((_, b)) if b.len() >= 4 => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        _ => 0,
    };
    let version = want.min(theirs);
    link.write(&packet(0, SET_CLIENT_NAME, &named("Ectomatter")))?;
    // (OpenRGB never sets a client's flags until told: told none, it sends nothing unasked for them)
    link.write(&packet(0, SET_CLIENT_FLAGS, &0u32.to_le_bytes()))?;
    if !got.is_empty() {
        return Err("OpenRGB sent more than expected as it was met".into());
    }
    Ok((link, version))
}

/// Everything OpenRGB lights, and its profiles, read anew.
fn read_all(link: &net::TcpStream, got: &mut Vec<u8>, v: u32) -> Result<(Vec<Lit>, Vec<String>), String> {
    let mut r = Reader { link, got };
    link.write(&header(0, CONTROLLER_COUNT, 0))?;
    let n = match r.answer(CONTROLLER_COUNT, 2000)? {
        Some((_, b)) if b.len() >= 4 => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        _ => return Err("OpenRGB didn’t say how many devices it has".into()),
    };
    let mut lits = Vec::new();
    for i in 0..n.min(256) {
        link.write(&packet(i, CONTROLLER_DATA, &v.to_le_bytes()))?;
        if let Some((_, b)) = r.answer(CONTROLLER_DATA, 2000)? {
            lits.push(lit_of(&b, v)?);
        }
    }
    let profiles = if v >= 2 {
        link.write(&header(0, PROFILE_LIST, 0))?;
        match r.answer(PROFILE_LIST, 2000)? {
            Some((_, b)) => profiles_of(&b)?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    Ok((lits, profiles))
}

/// The profile OpenRGB is on, asked over a connection of its own at protocol
/// 6 (the only one that says), closed at once.
fn active() -> Option<String> {
    let (link, v) = open(6, 500).ok()?;
    if v < 6 {
        return None;
    }
    link.write(&header(0, GET_ACTIVE_PROFILE, 0)).ok()?;
    let mut got = Vec::new();
    let mut r = Reader { link: &link, got: &mut got };
    let (_, b) = r.answer(GET_ACTIVE_PROFILE, 1000).ok()??;
    let name = String::from_utf8_lossy(b.strip_suffix(&[0]).unwrap_or(&b)).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// What OpenRGB lights, in words: its devices by kind, in OpenRGB's order of
/// kinds ("Motherboard, RAM, graphics card and mouse").
fn summary(lits: &[Lit]) -> String {
    const KINDS: [(i32, &str); 21] = [
        (0, "motherboard"), (1, "RAM"), (2, "graphics card"), (3, "cooler"), (4, "LED strips"), (5, "keyboard"), (6, "mouse"),
        (7, "mouse pad"), (8, "headset"), (9, "headset stand"), (10, "controller"), (11, "lights"), (12, "speakers"), (13, "virtual devices"),
        (14, "drives"), (15, "case"), (16, "microphone"), (17, "accessories"), (18, "keypad"), (19, "laptop"), (20, "monitor"),
    ];
    let mut kinds: Vec<i32> = lits.iter().map(|l| l.kind).collect();
    kinds.sort_unstable();
    kinds.dedup();
    let words: Vec<String> = kinds.iter().map(|k| KINDS.iter().find(|(n, _)| n == k).map_or("other lights".to_string(), |(_, w)| w.to_string())).collect();
    let s = match words.len() {
        0 => return String::new(),
        1 => words[0].clone(),
        n => format!("{} and {}", words[..n - 1].join(", "), words[n - 1]),
    };
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().collect::<String>() + c.as_str())
}

/// What the device is, for device/added: one device, its profiles OpenRGB's.
fn described(s: &State) -> Value {
    json!({
        "device": DEVICE,
        "name": "OpenRGB",
        "vendor": "OpenRGB",
        "kind": "lights",
        "sub": summary(&s.devices),
        "capabilities": { "profiles": { "stored": "device", "list": s.profiles.iter().map(|p| json!({ "id": p, "name": p })).collect::<Vec<_>>(), "active": false, "restore": true, "choose": true } },
    })
}

fn warn(s: &mut State, w: Option<&str>) {
    if s.warned.as_deref() != w {
        s.warned = w.map(String::from);
        host::notify("plugin/status", &json!({ "warning": w.unwrap_or("") }).to_string());
    }
}

/// Look for the server, and meet it: its devices and profiles told.
fn look() {
    S.with(|s| {
        let mut s = s.borrow_mut();
        if s.link.is_none() {
            match open(5, 300) {
                Ok((link, v)) => match read_all(&link, &mut Vec::new(), v) {
                    Ok((lits, profiles)) => {
                        s.devices = lits;
                        s.profiles = profiles;
                        s.version = v;
                        s.got.clear();
                        s.link = Some(link);
                        put_right(&mut s);
                        s.met = true;
                        warn(&mut s, None);
                        host::notify("device/added", &described(&s).to_string());
                        s.told = true;
                        host::log(&format!("met OpenRGB: {} devices, {} profiles, protocol {v}", s.devices.len(), s.profiles.len()));
                    }
                    Err(e) => warn(&mut s, Some(&format!("OpenRGB’s SDK server didn’t answer as expected: {e}"))),
                },
                Err(_) => {
                    if s.told {
                        host::notify("device/removed", &json!({ "device": DEVICE }).to_string());
                        s.told = false;
                    }
                    warn(&mut s, Some(OFF));
                }
            }
        }
    });
    let linked = S.with(|s| s.borrow().link.is_some());
    host::after(if linked { LISTEN_EVERY } else { LOOK_EVERY }, if linked { LISTEN } else { LOOK });
}

/// Read what the server sent unasked: its devices or profiles changed (read
/// anew), a profile about to load (answered); the server gone (looked for again).
fn listen() {
    let gone = S.with(|s| {
        let mut s = s.borrow_mut();
        let Some(link) = s.link.take() else { return true };
        let mut got = std::mem::take(&mut s.got);
        let mut r = Reader { link: &link, got: &mut got };
        let mut again = false;
        let ok = loop {
            match r.next(0) {
                Ok(Some((_, DEVICE_LIST_UPDATED | PROFILE_LIST_UPDATED, _))) => again = true,
                Ok(Some((_, PROFILE_ABOUT_TO_LOAD, _))) => {
                    let _ = link.write(&header(0, PROFILE_ABOUT_TO_LOAD, 0));
                }
                Ok(Some(_)) => {}
                Ok(None) => break true,
                Err(_) => break false,
            }
        };
        if !ok {
            return true;
        }
        if again {
            let v = s.version;
            if let Ok((lits, profiles)) = read_all(&link, &mut got, v) {
                s.devices = lits;
                s.profiles = profiles;
                host::notify("device/added", &described(&s).to_string());
            }
        }
        // (a moment after a profile was put back: what OpenRGB says it's on, if not that)
        if let Some((b, _)) = s.check.clone().filter(|(_, at)| now_ms() >= at + 2000) {
            s.check = None;
            let now = active();
            if now.as_deref() != Some(b.as_str()) {
                host::log(&format!("OpenRGB says it’s on {} after {b} was put back", now.as_deref().unwrap_or("no profile")));
            }
        }
        s.got = got;
        s.link = Some(link);
        // (its profiles listed anew: a putting back owed for want of the list, done now)
        if again && s.owed {
            put_right(&mut s);
        }
        false
    });
    if gone {
        S.with(|s| {
            let mut s = s.borrow_mut();
            s.link = None;
            if s.told {
                host::notify("device/removed", &json!({ "device": DEVICE }).to_string());
                s.told = false;
            }
            warn(&mut s, Some(OFF));
        });
        host::after(LOOK_EVERY, LOOK);
    } else {
        host::after(LISTEN_EVERY, LISTEN);
    }
}

/// What's kept between runs, written as it changes (in full, then put in place).
fn keep(s: &State) {
    let v = json!({ "switched": s.switched, "before": s.before, "own": s.own, "ours": s.ours });
    let tmp = format!("{KEPT}.tmp");
    if let Err(e) = std::fs::write(&tmp, v.to_string()).and_then(|_| std::fs::rename(&tmp, KEPT)) {
        host::log(&format!("couldn’t write down what to put back: {e}"));
    }
}

/// What the last run kept, into the plugin's state.
fn kept(s: &mut State) {
    let v: Value = std::fs::read_to_string(KEPT).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    s.switched = v["switched"] == true;
    s.before = v["before"].as_str().map(String::from);
    s.own = v["own"].as_str().map(String::from);
    s.ours = v["ours"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(String::from)).collect();
}

/// A putting back left undone, done as OpenRGB is met: the last run's, which
/// ended with OpenRGB switched (at the first meeting after start only), or one
/// asked while OpenRGB was out of reach. Done while OpenRGB is still on one of
/// Ectomatter's profiles; a profile the person chose since stays; unasked, it waits.
fn put_right(s: &mut State) {
    let last_run = !s.met && s.switched;
    if !last_run && !s.owed {
        return;
    }
    let left = if last_run { "from Ectomatter’s last run" } else { "while it was out of reach" };
    match active() {
        Some(now) if s.ours.contains(&now) => {
            host::log(&format!("OpenRGB was still on {now} {left}"));
            let _ = put_back(s);
        }
        Some(now) => {
            host::log(&format!("OpenRGB is on {now}, not one Ectomatter switched it to, so it stays"));
            s.own = Some(now);
            s.switched = false;
            s.before = None;
            s.owed = false;
        }
        None => host::log("OpenRGB didn’t say which profile it’s on, so putting back waits"),
    }
    keep(s);
}

/// The person's profile put back, as a restore asks or as owed: the one kept,
/// or (gone from OpenRGB since) theirs as last named; with neither, nothing.
/// OpenRGB out of reach, it's owed, to be done once OpenRGB is met again.
fn put_back(s: &mut State) -> Result<Option<String>, String> {
    if !s.switched {
        return Ok(None);
    }
    if s.link.is_none() {
        s.owed = true;
        return Err("Ectomatter can’t reach OpenRGB’s SDK server.".into());
    }
    // (no list is OpenRGB not saying, not every profile gone: it waits)
    if s.profiles.is_empty() {
        s.owed = true;
        return Err("OpenRGB didn’t list its profiles.".into());
    }
    let pick = s.before.clone().filter(|b| s.profiles.contains(b)).or_else(|| s.own.clone().filter(|o| s.profiles.contains(o)));
    let Some(b) = pick else {
        host::log(&match &s.before {
            Some(b) => format!("OpenRGB has no profile named {b} now, so nothing was put back"),
            None => "OpenRGB named no profile of the person’s before the switch, so nothing was put back".to_string(),
        });
        s.switched = false;
        s.before = None;
        s.owed = false;
        keep(s);
        return Ok(None);
    };
    if let Err(e) = load(s, &b) {
        s.owed = true;
        return Err(e);
    }
    match s.before.as_ref().filter(|was| **was != b) {
        Some(was) => host::log(&format!("OpenRGB has no profile named {was} now; put it back on {b}")),
        None => host::log(&format!("OpenRGB put back on {b}")),
    }
    s.switched = false;
    s.before = None;
    s.owed = false;
    s.check = Some((b.clone(), now_ms()));
    keep(s);
    Ok(Some(b))
}

/// A profile loaded: OpenRGB sets the lights itself.
fn load(s: &State, name: &str) -> Result<(), String> {
    let link = s.link.as_ref().ok_or("Ectomatter can’t reach OpenRGB’s SDK server.")?;
    if !s.profiles.iter().any(|p| p == name) {
        return Err(format!("OpenRGB has no profile named {name}."));
    }
    link.write(&packet(0, LOAD_PROFILE, &named(name)))?;
    Ok(())
}

fn answer(method: &str, p: &Value) -> Result<Value, String> {
    match method {
        "initialize" => {
            let met = ectomatter_protocol::meet(p, "OpenRGB Link", env!("CARGO_PKG_VERSION"))?;
            S.with(|s| kept(&mut s.borrow_mut()));
            host::after(0, LOOK);
            Ok(met)
        }
        "profiles/list" => S.with(|s| Ok(json!(s.borrow().profiles.iter().map(|p| json!({ "id": p, "name": p })).collect::<Vec<_>>()))),
        // a game's or a scene's profile; the person's, the first time, kept to put back
        "profiles/activate" => {
            let want = p["profile"].as_str().ok_or("Which profile?")?.to_string();
            if !S.with(|s| s.borrow().switched) {
                let said = active();
                S.with(|s| {
                    let mut s = s.borrow_mut();
                    // (the person's: what OpenRGB is on, unless that's one of Ectomatter's or the
                    // one about to load; else the person's as last named, if known)
                    let theirs = said.clone().filter(|a| *a != want && !s.ours.contains(a));
                    if theirs.is_some() {
                        s.own = theirs.clone();
                    }
                    s.before = theirs.or_else(|| s.own.clone());
                    s.switched = true;
                    host::log(&format!("OpenRGB says it’s on {}; switching it to {want}, to put {} back after", said.as_deref().unwrap_or("no profile"), s.before.as_deref().unwrap_or("nothing")));
                });
            }
            S.with(|s| {
                let mut s = s.borrow_mut();
                if s.own.as_ref() != Some(&want) && !s.ours.contains(&want) {
                    s.ours.push(want.clone());
                }
                s.check = None;
                s.owed = false;
                // (written down before the switch is made)
                keep(&s);
                load(&s, &want)
            })?;
            Ok(json!({ "profile": want }))
        }
        // a profile chosen by hand (another device's control's job): on at once, and the
        // person's own from then on, so it's what goes back after Ectomatter's switches
        "profiles/choose" => {
            let want = p["profile"].as_str().ok_or("Which profile?")?.to_string();
            S.with(|s| {
                let mut s = s.borrow_mut();
                if s.link.is_none() {
                    return Err("Ectomatter can’t reach OpenRGB’s SDK server.".to_string());
                }
                s.own = Some(want.clone());
                s.ours.retain(|o| *o != want);
                if s.switched {
                    s.before = Some(want.clone());
                }
                s.check = None;
                // (written down before the switch is made)
                keep(&s);
                load(&s, &want)?;
                host::log(&format!("OpenRGB switched to {want} by hand"));
                Ok(json!({ "profile": want }))
            })
        }
        // back to the person's profile (kept, should it not go)
        "profiles/restore" => S.with(|s| put_back(&mut s.borrow_mut()).map(|b| json!({ "profile": b }))),
        "shutdown" => {
            S.with(|s| s.borrow_mut().link = None);
            Ok(Value::Null)
        }
        _ => Err(format!("OpenRGB Link doesn’t answer {method}.")),
    }
}

struct Plugin;

impl Guest for Plugin {
    fn call(method: String, params: String) -> Result<String, String> {
        let p: Value = if params.is_empty() { Value::Null } else { serde_json::from_str(&params).map_err(|e| format!("{method}: {e}"))? };
        answer(&method, &p).map(|v| v.to_string())
    }

    fn timer(tag: u32) {
        match tag {
            LOOK => look(),
            LISTEN => listen(),
            _ => {}
        }
    }
}

export!(Plugin);
