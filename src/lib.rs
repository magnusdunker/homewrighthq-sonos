//! Sonos: every player in the household as a media player in HomewrightHQ. An
//! external integration.
//!
//! One setup covers every player. Players are found with mDNS and SSDP, and
//! the zone group state of any one of them names the rest. The addresses
//! found are remembered, so a player that does not answer discovery after a
//! restart is still used. Each player subscribes to UPnP events, which
//! make it read its state again at once; without events it reads every 5
//! seconds. Control is local, over UPnP on port 1400.

pub mod discovery;
pub mod events;
pub mod http;
pub mod player;
pub mod soap;
pub mod state;
pub mod xml;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use thehome_plugin::{async_trait, json, Device, Entity, Entry, Error, Flow, FlowReply, Host, Integration, Map, Value};

use player::{Caller, Player, PlayerHandle, PlayerRef, Poke, Shared, View};
use soap::Description;

pub const DOMAIN: &str = "sonos";
const SCAN: Duration = Duration::from_secs(3);
/// How often an address the user typed is tried while the player is away.
const MANUAL_RETRY: Duration = Duration::from_secs(30);
/// How often mDNS and SSDP are asked again.
const REBROWSE: Duration = Duration::from_secs(300);
/// How often the zone group state is read without being asked to.
const TOPOLOGY_POLL: Duration = Duration::from_secs(60);
const FAVORITES_POLL: Duration = Duration::from_secs(300);

/// Where a player was found.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Source {
    /// mDNS or SSDP
    Network,
    /// Another player's zone group state.
    Topology,
    /// An address the user typed.
    Manual,
    /// The address discovery gave last time.
    Remembered,
}

/// Logging to HomewrightHQ's log through stderr.
#[doc(hidden)]
pub fn emit(level: &str, message: String) {
    thehome_plugin::log(level, message);
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::emit("INFO", format!($($t)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::emit("WARN", format!($($t)*)) };
}

/// A player as the setup form and the memory file know it.
#[derive(Clone, Debug, PartialEq)]
struct Found {
    desc: Description,
    addr: String,
}

fn title(f: &Found) -> String {
    let host = f.addr.strip_suffix(":1400").unwrap_or(&f.addr);
    match &f.desc.model {
        Some(m) => format!("{} ({m}, {host})", f.desc.room),
        None => format!("{} ({host})", f.desc.room),
    }
}

fn store(found: &[Found]) -> Value {
    found
        .iter()
        .map(|f| {
            json!({ "uuid": f.desc.uuid, "name": f.desc.room, "model": f.desc.model, "software": f.desc.software,
                    "line_in": f.desc.line_in, "tv": f.desc.tv, "addr": f.addr })
        })
        .collect()
}

fn stored(list: Option<&Value>) -> Vec<Found> {
    let Some(list) = list.and_then(Value::as_array) else { return vec![] };
    list.iter()
        .filter_map(|f| {
            Some(Found {
                desc: Description {
                    uuid: f["uuid"].as_str()?.into(),
                    room: f["name"].as_str()?.into(),
                    model: f["model"].as_str().map(String::from),
                    software: f["software"].as_str().map(String::from),
                    line_in: f["line_in"].as_bool().unwrap_or(false),
                    tv: f["tv"].as_bool().unwrap_or(false),
                },
                addr: f["addr"].as_str()?.into(),
            })
        })
        .collect()
}

fn schema(found: &[Found], selected: &[String], known_hosts: &str) -> Value {
    let mut props = Map::new();
    if !found.is_empty() {
        let options: Vec<Value> = found.iter().map(|f| json!({ "const": f.desc.uuid, "title": title(f) })).collect();
        props.insert(
            "devices".into(),
            json!({
                "type": "array", "title": "Afspillere", "items": { "type": "string", "oneOf": options }, "default": selected,
                "description": "Med alle valgt kommer nye afspillere også med, når de dukker op"
            }),
        );
    }
    props.insert(
        "known_hosts".into(),
        json!({
            "type": "string", "title": "Ekstra adresser", "default": known_hosts,
            "description": "IP-adresser eller værtsnavne adskilt af komma, til afspillere som søgningen ikke finder, f.eks. på et andet VLAN"
        }),
    );
    json!({ "type": "object", "properties": props })
}

fn with_description(mut r: FlowReply, text: &str) -> FlowReply {
    r.result["description"] = json!(text);
    r
}

fn str_list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().filter_map(Value::as_str).map(String::from).collect(),
        Value::String(s) => s.split([',', ' ', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
        _ => vec![],
    }
}

/// A host or host:port the user typed.
fn valid_host(h: &str) -> bool {
    let (host, port) = match h.rsplit_once(':') {
        Some((a, p)) if !a.contains(':') => (a, Some(p)),
        _ => (h, None),
    };
    let host_ok = !host.is_empty() && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) || host.parse::<std::net::IpAddr>().is_ok();
    host_ok && port.is_none_or(|p| p.parse::<u16>().is_ok())
}

/// Check the form and turn it into entry data, or show it again with errors.
#[allow(clippy::result_large_err)]
async fn submit(soap: &soap::Client, input: &Value, found_ids: &[String], extra: Map<String, Value>, show: impl Fn(Value) -> FlowReply) -> Result<Map<String, Value>, FlowReply> {
    let hosts = str_list(&input["known_hosts"]);
    if let Some(h) = hosts.iter().find(|h| !valid_host(h)) {
        info!("ugyldig adresse: {h}");
        return Err(show(json!({ "known_hosts": "invalid_host" })));
    }
    for h in &hosts {
        if let Err(e) = soap.describe(&soap::with_port(h)).await {
            info!("{h}: {e}");
            return Err(show(json!({ "base": "cannot_connect" })));
        }
    }
    let mut data = extra;
    data.insert("known_hosts".into(), json!(hosts));
    let chosen: Vec<String> = str_list(&input["devices"]).into_iter().filter(|d| found_ids.contains(d)).collect();
    // all of them means new players too, so nothing is stored
    if !found_ids.is_empty() && !found_ids.iter().all(|d| chosen.contains(d)) {
        data.insert("devices".into(), json!(chosen));
    }
    Ok(data)
}

/// Describe the addresses at the same time.
async fn describe_all(soap: &soap::Client, addrs: Vec<String>) -> Vec<Found> {
    let mut set = tokio::task::JoinSet::new();
    for addr in addrs {
        let soap = soap.clone();
        set.spawn(async move { soap.describe(&addr).await.ok().map(|desc| Found { desc, addr }) });
    }
    let mut out = vec![];
    while let Some(r) = set.join_next().await {
        out.extend(r.ok().flatten());
    }
    out
}

/// Describe each address, then add the players their zone group state names.
async fn identify(soap: &soap::Client, addrs: Vec<String>) -> Vec<Found> {
    let mut found: Vec<Found> = vec![];
    for f in describe_all(soap, addrs).await {
        if !found.iter().any(|x| x.desc.uuid == f.desc.uuid) {
            found.push(f);
        }
    }
    if let Some(first) = found.first().map(|f| f.addr.clone()) {
        if let Ok(members) = zone_members(soap, &first).await {
            let missing: Vec<String> = members.iter().filter(|m| !found.iter().any(|f| f.desc.uuid == m.uuid)).map(|m| m.addr.clone()).collect();
            found.extend(describe_all(soap, missing).await);
        }
    }
    found.sort_by_key(|f| f.desc.room.to_lowercase());
    found
}

async fn zone_members(soap: &soap::Client, addr: &str) -> Result<Vec<xml::GroupMember>, soap::SoapError> {
    let r = soap.call(addr, &soap::TOPOLOGY, "GetZoneGroupState", &[]).await?;
    let state = xml::text_of(&r, "ZoneGroupState").unwrap_or_default();
    Ok(xml::zone_groups(&state))
}

#[derive(Default)]
pub struct Sonos {
    soap: soap::Client,
    entries: Mutex<HashMap<String, Arc<Runtime>>>,
}

impl Sonos {
    fn runtime(&self, entry_id: &str) -> Result<Arc<Runtime>, Error> {
        self.entries.lock().unwrap().get(entry_id).cloned().ok_or_else(|| Error::unavailable("opsætningen kører ikke"))
    }

    /// Players to offer in a form: a fresh search plus what the setup knows.
    async fn found(&self, flow: &Flow) -> Vec<Found> {
        let mut found = if flow.data.get("scan") == Some(&json!(false)) { vec![] } else { identify(&self.soap, discovery::scan(SCAN).await).await };
        let known = flow.entry.as_ref().and_then(|e| self.entries.lock().unwrap().get(&e.entry_id).cloned());
        let mut more = stored(flow.data.get("found"));
        if let Some(rt) = known {
            more.extend(rt.discovered().await);
        }
        for f in more {
            if !found.iter().any(|x| x.desc.uuid == f.desc.uuid) {
                found.push(f);
            }
        }
        found
    }
}

#[async_trait]
impl Integration for Sonos {
    async fn config_flow(&self, _host: &Host, flow: Flow, step: &str, input: Option<Value>) -> Result<FlowReply, Error> {
        match (step, input) {
            ("zeroconf", None) => {
                let d = flow.discovery.clone().unwrap_or(Value::Null);
                let name = d["name"].as_str().and_then(discovery::room_of_instance).unwrap_or_else(|| "en Sonos-afspiller".into());
                let r = FlowReply::form("confirm", json!({ "type": "object", "properties": {} })).with_unique_id(DOMAIN);
                Ok(with_description(r, &format!("Fandt {name}. Alle Sonos-afspillere på netværket bliver tilføjet, også dem der dukker op senere.")))
            }
            ("confirm", Some(_)) => Ok(FlowReply::create("Sonos", json!({})).with_unique_id(DOMAIN)),
            ("user", None) => {
                let found = self.found(&flow).await;
                let ids: Vec<String> = found.iter().map(|f| f.desc.uuid.clone()).collect();
                let mut data = flow.data.clone();
                data.insert("found".into(), store(&found));
                let r = FlowReply::form("user", schema(&found, &ids, "")).with_data(data).with_unique_id(DOMAIN);
                Ok(if found.is_empty() {
                    with_description(r, "Ingen Sonos-afspillere fundet på netværket lige nu. Afspillere, der dukker op senere, bliver tilføjet.")
                } else {
                    r
                })
            }
            ("user", Some(v)) => {
                let found = stored(flow.data.get("found"));
                let ids: Vec<String> = found.iter().map(|f| f.desc.uuid.clone()).collect();
                let mut extra = Map::new();
                for key in ["discovery", "events"] {
                    if flow.data.get(key) == Some(&json!(false)) {
                        extra.insert(key.into(), json!(false));
                    }
                }
                let typed = v["known_hosts"].as_str().unwrap_or_default().to_string();
                let chosen = str_list(&v["devices"]);
                let show = |errors| FlowReply::form_with_errors("user", schema(&found, &chosen, &typed), errors);
                match submit(&self.soap, &v, &ids, extra, show).await {
                    Ok(data) => Ok(FlowReply::create("Sonos", Value::Object(data)).with_unique_id(DOMAIN)),
                    Err(form) => Ok(form),
                }
            }
            _ => Ok(FlowReply::abort("unknown_step")),
        }
    }

    async fn options_flow(&self, _host: &Host, flow: Flow, _step: &str, input: Option<Value>) -> Result<FlowReply, Error> {
        let Some(entry) = flow.entry.clone() else { return Ok(FlowReply::abort("unknown")) };
        let hosts = str_list(entry.get("known_hosts").unwrap_or(&Value::Null)).join(", ");
        match input {
            None => {
                let found = self.found(&flow).await;
                let ids: Vec<String> = found.iter().map(|f| f.desc.uuid.clone()).collect();
                let current = entry.get("devices").map(str_list).unwrap_or_else(|| ids.clone());
                let mut data = flow.data.clone();
                data.insert("found".into(), store(&found));
                Ok(FlowReply::form("init", schema(&found, &current, &hosts)).with_data(data))
            }
            Some(v) => {
                let found = stored(flow.data.get("found"));
                let ids: Vec<String> = found.iter().map(|f| f.desc.uuid.clone()).collect();
                let typed = v["known_hosts"].as_str().unwrap_or_default().to_string();
                let chosen = str_list(&v["devices"]);
                let show = |errors| FlowReply::form_with_errors("init", schema(&found, &chosen, &typed), errors);
                match submit(&self.soap, &v, &ids, Map::new(), show).await {
                    Ok(data) => Ok(FlowReply::create("", Value::Object(data))),
                    Err(form) => Ok(form),
                }
            }
        }
    }

    async fn setup_entry(&self, host: &Host, entry: Entry) -> Result<(), Error> {
        let rt = Runtime::start(host.clone(), &entry).await;
        if let Some(old) = self.entries.lock().unwrap().insert(entry.entry_id.clone(), rt) {
            old.stop();
        }
        Ok(())
    }

    async fn unload_entry(&self, _host: &Host, entry_id: &str) -> Result<(), Error> {
        if let Some(rt) = self.entries.lock().unwrap().remove(entry_id) {
            rt.stop();
        }
        Ok(())
    }

    async fn entity_service(&self, _host: &Host, entry_id: &str, entity_id: &str, service: &str, data: Map<String, Value>) -> Result<(), Error> {
        let rt = self.runtime(entry_id)?;
        let caller = rt.caller(entity_id).await?;
        caller.call(service, data).await
    }

    async fn diagnostics(&self, _host: &Host, entry: Entry) -> Value {
        let mut d = json!({ "data": entry.data, "options": entry.options });
        if let Ok(rt) = self.runtime(&entry.entry_id) {
            d["events_port"] = json!(rt.shared.events_port);
            d["players"] = Value::Array(rt.diagnostics().await);
        }
        d
    }
}

struct Known {
    found: Found,
    manual: bool,
    entity_id: Option<String>,
    handle: Option<PlayerHandle>,
}

/// One running setup.
struct Runtime {
    shared: Arc<Shared>,
    entry_id: String,
    /// Chosen players; `None` is all of them.
    selection: Option<Vec<String>>,
    manual: Vec<String>,
    players: tokio::sync::Mutex<HashMap<String, Known>>,
    found_tx: tokio::sync::mpsc::UnboundedSender<(Found, Source)>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    daemon: Mutex<Option<mdns_sd::ServiceDaemon>>,
    listener: Mutex<Option<events::Listener>>,
}

impl Runtime {
    async fn start(host: Host, entry: &Entry) -> Arc<Self> {
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<events::Event>();
        let listener = if entry.get("events") == Some(&json!(false)) {
            None
        } else {
            match events::Listener::start(0, etx).await {
                Ok(l) => Some(l),
                Err(e) => {
                    warn!("kan ikke lytte efter hændelser: {e}; læser tilstanden hvert {}. sekund", player::POLL.as_secs());
                    None
                }
            }
        };
        let shared = Arc::new(Shared {
            host,
            soap: soap::Client::default(),
            view: RwLock::new(View::default()),
            favorites: RwLock::new(vec![]),
            events_port: listener.as_ref().map(|l| l.port),
            topology: Default::default(),
        });
        let manual: Vec<String> = str_list(entry.get("known_hosts").unwrap_or(&Value::Null)).iter().map(|h| soap::with_port(h)).collect();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Found, Source)>();
        let rt = Arc::new(Self {
            shared,
            entry_id: entry.entry_id.clone(),
            selection: entry.get("devices").map(str_list),
            manual: manual.clone(),
            players: Default::default(),
            found_tx: tx.clone(),
            tasks: Default::default(),
            daemon: Default::default(),
            listener: Mutex::new(listener),
        });

        // first, so discovery finds them known and only moves them if they moved
        for f in stored(Some(&remembered(&rt.memory_file()))) {
            let _ = tx.send((f, Source::Remembered));
        }
        if entry.get("discovery") != Some(&json!(false)) {
            let (atx, mut arx) = tokio::sync::mpsc::unbounded_channel::<String>();
            *rt.daemon.lock().unwrap() = discovery::browse(atx.clone());
            let me = rt.clone();
            rt.spawn(async move {
                while let Some(addr) = arx.recv().await {
                    me.on_address(addr).await;
                }
            });
            let me = rt.clone();
            rt.spawn(async move {
                loop {
                    discovery::ssdp(SCAN, atx.clone()).await;
                    tokio::time::sleep(REBROWSE).await;
                    if let Some(d) = me.daemon.lock().unwrap().as_ref() {
                        discovery::rebrowse(d, atx.clone());
                    }
                }
            });
        }
        for addr in manual {
            let tx = tx.clone();
            let soap = rt.shared.soap.clone();
            rt.spawn(async move {
                loop {
                    match soap.describe(&addr).await {
                        Ok(desc) => {
                            let _ = tx.send((Found { desc, addr }, Source::Manual));
                            break;
                        }
                        Err(e) => info!("{addr}: {e}"),
                    }
                    tokio::time::sleep(MANUAL_RETRY).await;
                }
            });
        }
        let me = rt.clone();
        rt.spawn(async move {
            while let Some((f, source)) = rx.recv().await {
                me.on_found(f, source).await;
            }
        });
        let me = rt.clone();
        rt.spawn(async move {
            while let Some((uuid, service, _body)) = erx.recv().await {
                me.on_event(&uuid, &service);
            }
        });
        let me = rt.clone();
        rt.spawn(async move { me.topology_loop().await });
        rt
    }

    fn spawn(&self, f: impl std::future::Future<Output = ()> + Send + 'static) {
        self.tasks.lock().unwrap().push(tokio::spawn(f));
    }

    fn stop(&self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        if let Some(d) = self.daemon.lock().unwrap().take() {
            let _ = d.shutdown();
        }
        self.listener.lock().unwrap().take();
        // the handles stop their tasks when dropped
        if let Ok(mut p) = self.players.try_lock() {
            p.clear();
        }
    }

    fn memory_file(&self) -> std::path::PathBuf {
        self.shared.host.data_dir().join(format!("players-{}.json", self.entry_id))
    }

    /// Write down where the players found by discovery are.
    fn remember(&self, players: &HashMap<String, Known>) {
        let mut found: Vec<Found> = players.values().filter(|k| !k.manual).map(|k| k.found.clone()).collect();
        found.sort_by(|a, b| a.desc.uuid.cmp(&b.desc.uuid));
        let path = self.memory_file();
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, serde_json::to_vec_pretty(&store(&found))?)
        };
        if let Err(e) = write() {
            warn!("kan ikke gemme afspillernes adresser i {}: {e}", path.display());
        }
    }

    /// An address from mDNS or SSDP: who is it, and is it new?
    async fn on_address(&self, addr: String) {
        if self.manual.contains(&addr) {
            return;
        }
        let known = self.players.lock().await.values().any(|k| k.found.addr == addr);
        if known {
            return;
        }
        match self.shared.soap.describe(&addr).await {
            Ok(desc) => {
                let _ = self.found_tx.send((Found { desc, addr }, Source::Network));
            }
            Err(e) => info!("{addr}: {e}"),
        }
    }

    async fn on_found(&self, f: Found, source: Source) {
        let manual = source == Source::Manual;
        if !manual && self.selection.as_ref().is_some_and(|s| !s.contains(&f.desc.uuid)) {
            return;
        }
        let uuid = f.desc.uuid.clone();
        let mut players = self.players.lock().await;
        if let Some(k) = players.get_mut(&uuid) {
            if (k.manual && !manual) || source == Source::Remembered || k.found.addr == f.addr {
                return;
            }
            info!("{}: ny adresse {}", k.found.desc.room, f.addr);
            k.found.addr = f.addr.clone();
            if let Some(p) = self.shared.view.write().unwrap().players.get_mut(&uuid) {
                p.addr = f.addr.clone();
            }
            self.remember(&players);
            return;
        }
        let how = if source == Source::Remembered { "husker" } else { "fandt" };
        info!("{how} {} ({}) på {}", f.desc.room, f.desc.model.as_deref().unwrap_or("ukendt model"), f.addr);
        let dev = Device {
            identifiers: vec![(DOMAIN.into(), uuid.clone())],
            name: f.desc.room.clone(),
            manufacturer: Some("Sonos".into()),
            model: f.desc.model.clone(),
            sw_version: f.desc.software.clone(),
            suggested_area: Some(f.desc.room.clone()),
            ..Default::default()
        };
        let mut known = Known { found: f.clone(), manual, entity_id: None, handle: None };
        let poke = Arc::new(Poke::default());
        match self.shared.host.register_device(&self.entry_id, dev).await {
            Ok(dev_id) => match self.shared.host.add_entity(&self.entry_id, Entity::new("media_player", uuid.clone()).device(&dev_id), true).await {
                Ok(entity_id) => known.entity_id = entity_id,
                Err(e) => warn!("{}: {e}", f.desc.room),
            },
            Err(e) => warn!("{}: {e}", f.desc.room),
        }
        self.shared.view.write().unwrap().players.insert(
            uuid.clone(),
            PlayerRef { addr: f.addr.clone(), name: f.desc.room.clone(), entity_id: known.entity_id.clone(), poke: poke.clone() },
        );
        if let Some(entity_id) = &known.entity_id {
            let p = Player { uuid: uuid.clone(), name: f.desc.room.clone(), line_in: f.desc.line_in, tv: f.desc.tv };
            known.handle = Some(PlayerHandle::spawn(self.shared.clone(), p, entity_id.clone(), poke));
        }
        players.insert(uuid, known);
        if source != Source::Manual {
            self.remember(&players);
        }
        drop(players);
        self.shared.topology.notify_one();
    }

    fn on_event(&self, uuid: &str, service: &str) {
        let view = self.shared.view.read().unwrap();
        if service == "zgt" {
            self.shared.topology.notify_one();
            return;
        }
        if let Some(p) = view.players.get(uuid) {
            p.poke.event();
        }
        // what plays changed for the whole group
        if service == "avt" {
            for m in view.group_of(uuid).iter().filter(|m| *m != uuid) {
                if let Some(p) = view.players.get(m) {
                    p.poke.poke();
                }
            }
        }
    }

    /// Read the zone group state and the favorites, on request and now and then.
    async fn topology_loop(&self) {
        let mut favorites_at: Option<tokio::time::Instant> = None;
        loop {
            let addrs: Vec<String> = {
                let v = self.shared.view.read().unwrap();
                let mut a: Vec<String> = v.players.values().map(|p| p.addr.clone()).collect();
                a.sort();
                a
            };
            for addr in &addrs {
                if let Ok(members) = zone_members(&self.shared.soap, addr).await {
                    self.apply_topology(members).await;
                    break;
                }
            }
            if favorites_at.is_none_or(|t| t.elapsed() >= FAVORITES_POLL) {
                for addr in &addrs {
                    if let Ok(f) = player::favorites(&self.shared.soap, addr).await {
                        favorites_at = Some(tokio::time::Instant::now());
                        let changed = *self.shared.favorites.read().unwrap() != f;
                        if changed {
                            *self.shared.favorites.write().unwrap() = f;
                            self.poke_all();
                        }
                        break;
                    }
                }
            }
            let _ = tokio::time::timeout(TOPOLOGY_POLL, self.shared.topology.notified()).await;
        }
    }

    async fn apply_topology(&self, members: Vec<xml::GroupMember>) {
        let mut new = vec![];
        let changed = {
            let mut v = self.shared.view.write().unwrap();
            let mut changed = false;
            for m in &members {
                if v.coordinator.get(&m.uuid) != Some(&m.coordinator) {
                    v.coordinator.insert(m.uuid.clone(), m.coordinator.clone());
                    changed = true;
                }
                if !v.players.contains_key(&m.uuid) {
                    new.push(m.addr.clone());
                }
            }
            // gone from the household, or hidden in a pair: on its own
            let listed: Vec<&String> = members.iter().map(|m| &m.uuid).collect();
            let stale: Vec<String> = v.coordinator.keys().filter(|u| !listed.contains(u)).cloned().collect();
            for u in stale {
                v.coordinator.remove(&u);
                changed = true;
            }
            changed
        };
        if changed {
            self.poke_all();
        }
        for addr in new {
            if self.manual.contains(&addr) {
                continue;
            }
            if let Ok(desc) = self.shared.soap.describe(&addr).await {
                let _ = self.found_tx.send((Found { desc, addr }, Source::Topology));
            }
        }
        // a player that moved shows up at its new address here first
        let mut players = self.players.lock().await;
        let mut moved = false;
        for m in &members {
            if let Some(k) = players.get_mut(&m.uuid) {
                if !k.manual && k.found.addr != m.addr {
                    info!("{}: ny adresse {}", k.found.desc.room, m.addr);
                    k.found.addr = m.addr.clone();
                    if let Some(p) = self.shared.view.write().unwrap().players.get_mut(&m.uuid) {
                        p.addr = m.addr.clone();
                    }
                    moved = true;
                }
            }
        }
        if moved {
            self.remember(&players);
        }
    }

    fn poke_all(&self) {
        for p in self.shared.view.read().unwrap().players.values() {
            p.poke.poke();
        }
    }

    async fn caller(&self, entity_id: &str) -> Result<Caller, Error> {
        let players = self.players.lock().await;
        let k = players
            .values()
            .find(|k| k.entity_id.as_deref() == Some(entity_id))
            .ok_or_else(|| Error::new("not_found", format!("{entity_id} findes ikke")))?;
        Ok(k.handle.as_ref().ok_or_else(|| Error::unavailable(format!("{entity_id} er slået fra")))?.caller())
    }

    async fn discovered(&self) -> Vec<Found> {
        self.players.lock().await.values().filter(|k| !k.manual).map(|k| k.found.clone()).collect()
    }

    async fn diagnostics(&self) -> Vec<Value> {
        let list: Vec<(Found, bool, Option<String>, Option<Caller>)> =
            self.players.lock().await.values().map(|k| (k.found.clone(), k.manual, k.entity_id.clone(), k.handle.as_ref().map(PlayerHandle::caller))).collect();
        let mut out = vec![];
        for (f, manual, entity_id, caller) in list {
            let status = match caller {
                Some(c) => c.diagnostics().await,
                None => Value::Null,
            };
            out.push(json!({ "player": store(std::slice::from_ref(&f))[0], "manual": manual, "entity_id": entity_id, "status": status }));
        }
        out.push(json!({ "favorites": self.shared.favorites.read().unwrap().iter().map(|f| json!({ "id": f.id, "title": f.title, "uri": f.uri })).collect::<Vec<_>>() }));
        out
    }
}

/// The players written down last time.
fn remembered(path: &std::path::Path) -> Value {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_values() {
        assert_eq!(str_list(&json!("10.0.0.1, stue.lan:1400\n10.0.0.2")), ["10.0.0.1", "stue.lan:1400", "10.0.0.2"]);
        assert!(valid_host("10.0.0.1") && valid_host("stue.lan:1400") && valid_host("fe80::1"));
        assert!(!valid_host("x:y") && !valid_host("") && !valid_host("ikke en"));
        let found = vec![Found {
            desc: Description { uuid: "RINCON_A".into(), room: "Stue".into(), model: Some("Era 100".into()), software: None, line_in: true, tv: false },
            addr: "10.0.0.5:1400".into(),
        }];
        assert_eq!(stored(Some(&store(&found))), found);
        assert_eq!(title(&found[0]), "Stue (Era 100, 10.0.0.5)");
    }
}
