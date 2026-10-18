//! One player: a task that reads its state, publishes it, keeps the event
//! subscriptions alive and carries out service calls. What plays is read
//! from and sent to the group's coordinator; volume belongs to the player.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::{json, Map, Value};
use thehome_plugin::{Error, Host};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::Instant;

use crate::soap::{self, Client, Service, SoapError, AV_TRANSPORT, RENDERING, TOPOLOGY};
use crate::state::{self, Favorite, Snapshot};

/// How often state is read when no events arrive.
pub const POLL: Duration = Duration::from_secs(5);
/// How often state is read anyway while events arrive.
const EVENT_POLL: Duration = Duration::from_secs(60);
const SUBSCRIPTION: u64 = 1800;
/// How long to wait before trying to subscribe again after it failed.
const SUBSCRIBE_RETRY: Duration = Duration::from_secs(300);
const VOLUME_STEP: i64 = 5;

const AVT: &str = "avt";
const RC: &str = "rc";
const ZGT: &str = "zgt";
const SERVICES: [(&str, &Service); 3] = [(AVT, &AV_TRANSPORT), (RC, &RENDERING), (ZGT, &TOPOLOGY)];

/// Wakes a player to read its state again.
#[derive(Default)]
pub struct Poke {
    notify: Notify,
    /// An event arrived since the subscriptions were made.
    events: AtomicBool,
}

impl Poke {
    pub fn poke(&self) {
        self.notify.notify_one();
    }

    pub fn event(&self) {
        self.events.store(true, Ordering::Relaxed);
        self.notify.notify_one();
    }
}

pub struct PlayerRef {
    pub addr: String,
    pub name: String,
    pub entity_id: Option<String>,
    pub poke: Arc<Poke>,
}

/// Where the players are and who coordinates whom.
#[derive(Default)]
pub struct View {
    pub players: HashMap<String, PlayerRef>,
    /// player uuid -> its group's coordinator
    pub coordinator: HashMap<String, String>,
}

impl View {
    pub fn coordinator_of(&self, uuid: &str) -> String {
        self.coordinator.get(uuid).cloned().unwrap_or_else(|| uuid.to_string())
    }

    /// The group's players, coordinator first.
    pub fn group_of(&self, uuid: &str) -> Vec<String> {
        let c = self.coordinator_of(uuid);
        let mut members: Vec<String> = self.coordinator.iter().filter(|(_, v)| **v == c).map(|(k, _)| k.clone()).filter(|k| *k != c).collect();
        members.sort();
        members.insert(0, c);
        members
    }

    /// Entity ids of the group, coordinator first; empty when the player is alone.
    pub fn group_entities(&self, uuid: &str) -> Vec<String> {
        let g = self.group_of(uuid);
        if g.len() < 2 {
            return vec![];
        }
        g.iter().filter_map(|u| self.players.get(u).and_then(|p| p.entity_id.clone())).collect()
    }

    pub fn by_entity(&self, entity_id: &str) -> Option<String> {
        self.players.iter().find(|(_, p)| p.entity_id.as_deref() == Some(entity_id)).map(|(u, _)| u.clone())
    }
}

/// What all players of a setup share.
pub struct Shared {
    pub host: Host,
    pub soap: Client,
    pub view: RwLock<View>,
    pub favorites: RwLock<Vec<Favorite>>,
    /// The event listener's port; `None` means polling only.
    pub events_port: Option<u16>,
    /// Read the zone group state again.
    pub topology: Notify,
}

impl Shared {
    fn addr(&self, uuid: &str) -> Option<String> {
        self.view.read().unwrap().players.get(uuid).map(|p| p.addr.clone())
    }

    /// The coordinator's uuid and address.
    fn coordinator(&self, uuid: &str) -> Option<(String, String)> {
        let v = self.view.read().unwrap();
        let c = v.coordinator_of(uuid);
        let addr = v.players.get(&c).map(|p| p.addr.clone())?;
        Some((c, addr))
    }
}

pub type Reply = oneshot::Sender<Result<(), Error>>;

pub enum Command {
    Service { service: String, data: Map<String, Value>, reply: Reply },
    Diagnostics(oneshot::Sender<Value>),
}

/// Holds the player's task; dropping it stops the task.
pub struct PlayerHandle {
    cmd: mpsc::UnboundedSender<Command>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PlayerHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct Player {
    pub uuid: String,
    pub name: String,
    pub line_in: bool,
    pub tv: bool,
}

impl PlayerHandle {
    pub fn spawn(shared: Arc<Shared>, player: Player, entity_id: String, poke: Arc<Poke>) -> Self {
        let (cmd, cmds) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(shared, player, entity_id, poke, cmds));
        Self { cmd, task }
    }

    pub fn caller(&self) -> Caller {
        Caller(self.cmd.clone())
    }
}

#[derive(Clone)]
pub struct Caller(mpsc::UnboundedSender<Command>);

impl Caller {
    pub async fn call(&self, service: &str, data: Map<String, Value>) -> Result<(), Error> {
        let (reply, rx) = oneshot::channel();
        self.0
            .send(Command::Service { service: service.into(), data, reply })
            .map_err(|_| Error::unavailable("afspilleren er stoppet"))?;
        rx.await.unwrap_or_else(|_| Err(Error::unavailable("afspilleren blev stoppet")))
    }

    pub async fn diagnostics(&self) -> Value {
        let (tx, rx) = oneshot::channel();
        if self.0.send(Command::Diagnostics(tx)).is_err() {
            return Value::Null;
        }
        tokio::time::timeout(Duration::from_secs(10), rx).await.ok().and_then(Result::ok).unwrap_or(Value::Null)
    }
}

/// What was last sent, so unchanged states are not sent again.
#[derive(PartialEq)]
enum Published {
    Nothing,
    Unavailable,
    State(String, Map<String, Value>, Instant),
}

struct Out {
    host: Host,
    entity_id: String,
    last: Published,
}

/// Whether the position is not where it would have run to by itself.
fn jumped(state: &str, new: &Map<String, Value>, old: &Map<String, Value>, elapsed: Duration) -> bool {
    let pos = |a: &Map<String, Value>| a.get("media_position").and_then(Value::as_f64);
    match (pos(new), pos(old)) {
        (Some(n), Some(o)) => {
            let expected = if state == "playing" { o + elapsed.as_secs_f64() } else { o };
            (n - expected).abs() > 3.0
        }
        (a, b) => a.is_some() != b.is_some(),
    }
}

impl Out {
    async fn unavailable(&mut self) {
        if self.last != Published::Unavailable {
            self.last = Published::Unavailable;
            if let Err(e) = self.host.set_unavailable(&self.entity_id).await {
                crate::warn!("{}: {e}", self.entity_id);
            }
        }
    }

    async fn state(&mut self, state: String, attrs: Map<String, Value>) {
        let strip = |a: &Map<String, Value>| {
            let mut a = a.clone();
            a.remove("media_position_updated_at");
            a.remove("media_position");
            a
        };
        if let Published::State(s, a, at) = &self.last {
            if *s == state && strip(a) == strip(&attrs) && !jumped(&state, &attrs, a, at.elapsed()) {
                return;
            }
        }
        self.last = Published::State(state.clone(), attrs.clone(), Instant::now());
        if let Err(e) = self.host.set_state(&self.entity_id, state, attrs).await {
            crate::warn!("{}: {e}", self.entity_id);
        }
    }
}

struct Sub {
    name: &'static str,
    svc: &'static Service,
    addr: String,
    sid: String,
    renew_at: Instant,
}

/// The event subscriptions of one player.
#[derive(Default)]
struct Subs {
    list: Vec<Sub>,
    retry_at: Option<Instant>,
    logged: bool,
}

impl Subs {
    fn next_renewal(&self) -> Option<Instant> {
        self.list.iter().map(|s| s.renew_at).min()
    }

    async fn drop_all(&mut self, soap: &Client) {
        for s in self.list.drain(..) {
            soap.unsubscribe(&s.addr, s.svc, &s.sid).await;
        }
    }

    /// Subscribe, renew what is due, or give up for a while.
    async fn maintain(&mut self, sh: &Shared, uuid: &str, addr: &str, poke: &Poke) {
        let Some(port) = sh.events_port else { return };
        if self.list.iter().any(|s| s.addr != addr) {
            self.drop_all(&sh.soap).await;
        }
        let now = Instant::now();
        if self.list.is_empty() {
            if self.retry_at.is_some_and(|t| t > now) {
                return;
            }
            let Some(ip) = crate::discovery::local_ip_towards(addr) else { return };
            let ip = if ip.is_ipv6() { format!("[{ip}]") } else { ip.to_string() };
            poke.events.store(false, Ordering::Relaxed);
            for (name, svc) in SERVICES {
                let callback = format!("http://{ip}:{port}/{uuid}/{name}");
                match sh.soap.subscribe(addr, svc, &callback, SUBSCRIPTION).await {
                    Ok((sid, secs)) => self.list.push(Sub { name, svc, addr: addr.into(), sid, renew_at: now + Duration::from_secs(secs / 2) }),
                    Err(e) => {
                        if !self.logged {
                            crate::info!("{uuid}: kan ikke abonnere på hændelser ({e}); læser tilstanden hvert {}. sekund", POLL.as_secs());
                            self.logged = true;
                        }
                        self.drop_all(&sh.soap).await;
                        self.retry_at = Some(now + SUBSCRIBE_RETRY);
                        return;
                    }
                }
            }
            return;
        }
        for i in 0..self.list.len() {
            if self.list[i].renew_at > now {
                continue;
            }
            let s = &self.list[i];
            let renewed = match sh.soap.renew(&s.addr, s.svc, &s.sid, SUBSCRIPTION).await {
                Ok(r) => Some(r),
                Err(_) => {
                    // the player may have forgotten it, e.g. after a restart
                    let ip = crate::discovery::local_ip_towards(addr).map(|ip| ip.to_string()).unwrap_or_default();
                    sh.soap.subscribe(addr, s.svc, &format!("http://{ip}:{port}/{uuid}/{}", s.name), SUBSCRIPTION).await.ok()
                }
            };
            match renewed {
                Some((sid, secs)) => {
                    self.list[i].sid = sid;
                    self.list[i].renew_at = now + Duration::from_secs(secs / 2);
                }
                None => {
                    self.drop_all(&sh.soap).await;
                    self.retry_at = Some(now + POLL);
                    poke.events.store(false, Ordering::Relaxed);
                    return;
                }
            }
        }
    }
}

async fn run(sh: Arc<Shared>, p: Player, entity_id: String, poke: Arc<Poke>, mut cmds: mpsc::UnboundedReceiver<Command>) {
    let mut out = Out { host: sh.host.clone(), entity_id, last: Published::Nothing };
    let mut subs = Subs::default();
    let mut last: Option<Snapshot>;
    let mut error: Option<String> = None;
    loop {
        match read(&sh, &p.uuid).await {
            Ok(s) => {
                if error.take().is_some() {
                    crate::info!("{}: svarer igen", p.name);
                }
                publish(&sh, &p, &s, &mut out).await;
                last = Some(s);
            }
            Err(e) => {
                if error.is_none() {
                    crate::info!("{}: svarer ikke: {e}", p.name);
                }
                error = Some(e.to_string());
                out.unavailable().await;
                subs.drop_all(&sh.soap).await;
                poke.events.store(false, Ordering::Relaxed);
                last = None;
            }
        }
        if error.is_none() {
            if let Some(addr) = sh.addr(&p.uuid) {
                subs.maintain(&sh, &p.uuid, &addr, &poke).await;
            }
        }
        let events = poke.events.load(Ordering::Relaxed) && !subs.list.is_empty();
        let mut wake = Instant::now() + if events { EVENT_POLL } else { POLL };
        if let Some(r) = subs.next_renewal() {
            wake = wake.min(r);
        }
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                None => {
                    subs.drop_all(&sh.soap).await;
                    return;
                }
                Some(Command::Service { service, data, reply }) => {
                    let r = match &last {
                        Some(s) => call(&sh, &p, s, &service, &data).await,
                        None => Err(Error::unavailable(format!("{} svarer ikke", p.name))),
                    };
                    let _ = reply.send(r);
                }
                Some(Command::Diagnostics(tx)) => {
                    let _ = tx.send(json!({
                        "addr": sh.addr(&p.uuid),
                        "coordinator": sh.coordinator(&p.uuid).map(|c| c.0),
                        "error": error,
                        "events": events,
                        "subscriptions": subs.list.iter().map(|s| s.name).collect::<Vec<_>>(),
                        "snapshot": last.as_ref().map(|s| format!("{s:?}")),
                    }));
                }
            },
            _ = poke.notify.notified() => {}
            _ = tokio::time::sleep_until(wake) => {}
        }
    }
}

const ZERO: [(&str, &str); 1] = [("InstanceID", "0")];

async fn read(sh: &Shared, uuid: &str) -> Result<Snapshot, SoapError> {
    let addr = sh.addr(uuid).ok_or_else(|| SoapError::Transport("ukendt adresse".into()))?;
    let coord = sh.coordinator(uuid).map(|c| c.1).unwrap_or_else(|| addr.clone());
    let master = [("InstanceID", "0"), ("Channel", "Master")];
    let (vol, mute, ti, pi, mi, ts) = tokio::join!(
        sh.soap.call(&addr, &RENDERING, "GetVolume", &master),
        sh.soap.call(&addr, &RENDERING, "GetMute", &master),
        sh.soap.call(&coord, &AV_TRANSPORT, "GetTransportInfo", &ZERO),
        sh.soap.call(&coord, &AV_TRANSPORT, "GetPositionInfo", &ZERO),
        sh.soap.call(&coord, &AV_TRANSPORT, "GetMediaInfo", &ZERO),
        sh.soap.call(&coord, &AV_TRANSPORT, "GetTransportSettings", &ZERO),
    );
    let vol = vol?;
    let get = |r: &Result<String, SoapError>, name: &str| r.as_ref().ok().and_then(|x| crate::xml::text_of(x, name)).unwrap_or_default();
    Ok(Snapshot {
        transport: get(&ti, "CurrentTransportState"),
        play_mode: get(&ts, "PlayMode"),
        track_uri: get(&pi, "TrackURI"),
        track_meta: get(&pi, "TrackMetaData"),
        duration: get(&pi, "TrackDuration"),
        position: get(&pi, "RelTime"),
        av_uri: get(&mi, "CurrentURI"),
        av_meta: get(&mi, "CurrentURIMetaData"),
        volume: crate::xml::text_of(&vol, "CurrentVolume").and_then(|v| v.trim().parse().ok()),
        muted: mute.ok().and_then(|m| crate::xml::text_of(&m, "CurrentMute")).map(|m| m.trim() == "1"),
    })
}

async fn publish(sh: &Shared, p: &Player, s: &Snapshot, out: &mut Out) {
    let coordinator_addr = sh.coordinator(&p.uuid).map(|c| c.1).unwrap_or_default();
    let favorites = sh.favorites.read().unwrap().clone();
    let group = sh.view.read().unwrap().group_entities(&p.uuid);
    let ctx = state::Context { coordinator_addr: &coordinator_addr, line_in: p.line_in, tv: p.tv, favorites: &favorites, group };
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (st, attrs) = state::state_of(s, &ctx, &now);
    out.state(st, attrs).await;
}

fn str_arg<'a>(data: &'a Map<String, Value>, key: &str) -> Result<&'a str, Error> {
    data.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| Error::invalid(format!("{key} mangler")))
}

/// Carry out a service call; the player is read again afterwards.
async fn call(sh: &Shared, p: &Player, last: &Snapshot, service: &str, data: &Map<String, Value>) -> Result<(), Error> {
    let own = sh.addr(&p.uuid).ok_or_else(|| Error::unavailable("ukendt adresse"))?;
    let (coord_uuid, coord) = sh.coordinator(&p.uuid).unwrap_or_else(|| (p.uuid.clone(), own.clone()));
    let soap = &sh.soap;
    let avt = |action: &'static str, args: Vec<(&'static str, String)>| {
        let coord = coord.clone();
        async move {
            let mut all: Vec<(&str, &str)> = vec![("InstanceID", "0")];
            all.extend(args.iter().map(|(k, v)| (*k, v.as_str())));
            soap.call(&coord, &AV_TRANSPORT, action, &all).await.map(|_| ()).map_err(Error::from)
        }
    };
    let service = match service {
        "media_play_pause" if last.transport == "PLAYING" => "media_pause",
        "media_play_pause" => "media_play",
        s => s,
    };
    match service {
        "media_play" => avt("Play", vec![("Speed", "1".into())]).await,
        "media_pause" => avt("Pause", vec![]).await,
        "media_stop" => avt("Stop", vec![]).await,
        "media_next_track" => avt("Next", vec![]).await,
        "media_previous_track" => avt("Previous", vec![]).await,
        "media_seek" => {
            let pos = data.get("seek_position").and_then(Value::as_f64).ok_or_else(|| Error::invalid("seek_position mangler"))?;
            avt("Seek", vec![("Unit", "REL_TIME".into()), ("Target", state::hms(pos))]).await
        }
        "volume_set" | "volume_up" | "volume_down" | "volume_mute" => {
            let master = |extra: (&'static str, String)| vec![("InstanceID", "0".to_string()), ("Channel", "Master".to_string()), (extra.0, extra.1)];
            let args = match service {
                "volume_mute" => {
                    let m = data.get("is_volume_muted").and_then(Value::as_bool).ok_or_else(|| Error::invalid("is_volume_muted mangler"))?;
                    master(("DesiredMute", if m { "1" } else { "0" }.into()))
                }
                _ => {
                    let current = i64::from(last.volume.unwrap_or(0));
                    let target = match service {
                        "volume_up" => current + VOLUME_STEP,
                        "volume_down" => current - VOLUME_STEP,
                        _ => {
                            let v = data.get("volume_level").and_then(Value::as_f64).ok_or_else(|| Error::invalid("volume_level mangler"))?;
                            (v * 100.0).round() as i64
                        }
                    };
                    master(("DesiredVolume", target.clamp(0, 100).to_string()))
                }
            };
            let action = if service == "volume_mute" { "SetMute" } else { "SetVolume" };
            let args: Vec<(&str, &str)> = args.iter().map(|(k, v)| (*k, v.as_str())).collect();
            soap.call(&own, &RENDERING, action, &args).await.map(|_| ()).map_err(Error::from)
        }
        "shuffle_set" | "repeat_set" => {
            let (shuffle, repeat) = state::play_mode(&last.play_mode);
            let mode = if service == "shuffle_set" {
                let s = data.get("shuffle").and_then(Value::as_bool).ok_or_else(|| Error::invalid("shuffle mangler"))?;
                state::mode_of(s, repeat)
            } else {
                let r = str_arg(data, "repeat")?;
                if !matches!(r, "off" | "all" | "one") {
                    return Err(Error::invalid(format!("repeat skal være off, all eller one, ikke {r}")));
                }
                state::mode_of(shuffle, r)
            };
            avt("SetPlayMode", vec![("NewPlayMode", mode.into())]).await
        }
        "clear_playlist" => avt("RemoveAllTracksFromQueue", vec![]).await,
        "select_source" => {
            let source = str_arg(data, "source")?;
            match source {
                state::LINE_IN if p.line_in => play_uri(sh, &coord, &format!("x-rincon-stream:{}", p.uuid), "").await,
                state::TV if p.tv => play_uri(sh, &coord, &format!("x-sonos-htastream:{}:spdif", p.uuid), "").await,
                _ => {
                    let fav = sh.favorites.read().unwrap().iter().find(|f| f.title == source).cloned();
                    let fav = fav.ok_or_else(|| Error::invalid(format!("kilden {source} findes ikke")))?;
                    play_favorite(sh, &coord_uuid, &coord, &fav).await
                }
            }
        }
        "play_media" => {
            let id = str_arg(data, "media_content_id")?;
            let kind = data.get("media_content_type").and_then(Value::as_str).unwrap_or("music");
            if kind == "favorite_item_id" {
                let fav = sh.favorites.read().unwrap().iter().find(|f| f.id == id).cloned();
                let fav = fav.ok_or_else(|| Error::invalid(format!("favoritten {id} findes ikke")))?;
                return play_favorite(sh, &coord_uuid, &coord, &fav).await;
            }
            let title = data.get("extra").and_then(|e| e["title"].as_str()).unwrap_or(id);
            let meta = if state::is_stream(id) { String::new() } else { state::didl_for(id, title) };
            match data.get("enqueue").and_then(Value::as_str).unwrap_or("replace") {
                "add" | "next" => {
                    let next = data["enqueue"] == "next";
                    avt(
                        "AddURIToQueue",
                        vec![
                            ("EnqueuedURI", id.into()),
                            ("EnqueuedURIMetaData", meta),
                            ("DesiredFirstTrackNumberEnqueued", "0".into()),
                            ("EnqueueAsNext", if next { "1" } else { "0" }.into()),
                        ],
                    )
                    .await
                }
                "play" | "replace" => play_uri(sh, &coord, id, &meta).await,
                other => Err(Error::invalid(format!("enqueue kan ikke være {other}"))),
            }
        }
        "join" => {
            let members: Vec<String> = match data.get("group_members") {
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(String::from).collect(),
                Some(Value::String(s)) => s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
                _ => return Err(Error::invalid("group_members mangler")),
            };
            let targets: Vec<(String, String)> = {
                let v = sh.view.read().unwrap();
                let mut t = vec![];
                for e in &members {
                    let u = v.by_entity(e).ok_or_else(|| Error::invalid(format!("{e} er ikke en Sonos-afspiller")))?;
                    if u != coord_uuid {
                        let addr = v.players.get(&u).map(|p| p.addr.clone()).unwrap_or_default();
                        t.push((u, addr));
                    }
                }
                t
            };
            let uri = format!("x-rincon:{coord_uuid}");
            for (_, addr) in &targets {
                soap.call(addr, &AV_TRANSPORT, "SetAVTransportURI", &[("InstanceID", "0"), ("CurrentURI", &uri), ("CurrentURIMetaData", "")]).await?;
            }
            sh.topology.notify_one();
            Ok(())
        }
        "unjoin" => {
            soap.call(&own, &AV_TRANSPORT, "BecomeCoordinatorOfStandaloneGroup", &ZERO).await?;
            sh.topology.notify_one();
            Ok(())
        }
        s => Err(Error::not_supported(format!("media_player.{s}"))),
    }
}

async fn play_uri(sh: &Shared, coord: &str, uri: &str, meta: &str) -> Result<(), Error> {
    sh.soap.call(coord, &AV_TRANSPORT, "SetAVTransportURI", &[("InstanceID", "0"), ("CurrentURI", uri), ("CurrentURIMetaData", meta)]).await?;
    sh.soap.call(coord, &AV_TRANSPORT, "Play", &[("InstanceID", "0"), ("Speed", "1")]).await?;
    Ok(())
}

/// A radio favorite plays directly; anything else replaces the queue.
async fn play_favorite(sh: &Shared, coord_uuid: &str, coord: &str, f: &Favorite) -> Result<(), Error> {
    if f.uri.is_empty() {
        return Err(Error::not_supported(format!("favoritten {} kan ikke afspilles herfra", f.title)));
    }
    if state::is_stream(&f.uri) {
        return play_uri(sh, coord, &f.uri, &f.meta).await;
    }
    sh.soap.call(coord, &AV_TRANSPORT, "RemoveAllTracksFromQueue", &ZERO).await?;
    sh.soap
        .call(
            coord,
            &AV_TRANSPORT,
            "AddURIToQueue",
            &[("InstanceID", "0"), ("EnqueuedURI", &f.uri), ("EnqueuedURIMetaData", &f.meta), ("DesiredFirstTrackNumberEnqueued", "0"), ("EnqueueAsNext", "0")],
        )
        .await?;
    play_uri(sh, coord, &format!("x-rincon-queue:{coord_uuid}#0"), "").await
}

/// The household's Sonos favorites, read from any player.
pub async fn favorites(soap: &Client, addr: &str) -> Result<Vec<Favorite>, SoapError> {
    let r = soap
        .call(
            addr,
            &soap::CONTENT,
            "Browse",
            &[("ObjectID", "FV:2"), ("BrowseFlag", "BrowseDirectChildren"), ("Filter", "*"), ("StartingIndex", "0"), ("RequestedCount", "100"), ("SortCriteria", "")],
        )
        .await?;
    let didl = crate::xml::text_of(&r, "Result").unwrap_or_default();
    Ok(crate::xml::didl_items(&didl)
        .into_iter()
        .filter_map(|i| {
            Some(Favorite {
                id: i.get("id")?.clone(),
                title: i.get("title")?.trim().to_string(),
                uri: i.get("res").map(|s| s.trim().to_string()).unwrap_or_default(),
                meta: i.get("resMD").cloned().unwrap_or_default(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(groups: &[(&str, &str)]) -> View {
        let mut v = View::default();
        for (u, c) in groups {
            v.players.insert(u.to_string(), PlayerRef { addr: format!("{u}:1400"), name: u.to_string(), entity_id: Some(format!("media_player.{}", u.to_lowercase())), poke: Default::default() });
            v.coordinator.insert(u.to_string(), c.to_string());
        }
        v
    }

    #[test]
    fn groups() {
        let v = view(&[("A", "A"), ("B", "A"), ("C", "C")]);
        assert_eq!(v.group_of("B"), ["A", "B"]);
        assert_eq!(v.group_entities("A"), ["media_player.a", "media_player.b"]);
        assert!(v.group_entities("C").is_empty());
        assert_eq!(v.by_entity("media_player.c").as_deref(), Some("C"));
        assert_eq!(v.coordinator_of("X"), "X");
    }

    #[test]
    fn position_news() {
        let at = |p: f64| Map::from_iter([("media_position".to_string(), json!(p))]);
        let second = Duration::from_secs(1);
        assert!(!jumped("playing", &at(65.0), &at(60.0), 5 * second));
        assert!(jumped("playing", &at(10.0), &at(60.0), 5 * second));
        assert!(!jumped("paused", &at(60.0), &at(60.0), 5 * second));
        assert!(jumped("paused", &at(90.0), &at(60.0), second));
        assert!(jumped("playing", &Map::new(), &at(60.0), second));
    }
}
