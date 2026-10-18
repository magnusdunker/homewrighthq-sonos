//! The plugin driven over a pipe the way Homewright drives it, against fake
//! Sonos players on localhost.

mod fake;
mod host;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fake::{FakeSonos, Household};
use homewright_sonos::Sonos;
use host::FakeHost;
use serde_json::{json, Value};

async fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    wait_up_to(what, Duration::from_secs(10), &mut f).await
}

async fn wait_up_to<T>(what: &str, limit: Duration, f: &mut impl FnMut() -> Option<T>) -> T {
    let end = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < end {
        if let Some(v) = f() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("ventede forgæves på {what}");
}

fn flow(source: &str, data: Value) -> Value {
    json!({ "flow_id": "f1", "source": source, "data": data })
}

async fn start(name: &str) -> FakeHost {
    let host = FakeHost::start(Sonos::default());
    let dir = std::env::temp_dir().join(format!("sonos-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    host.call("initialize", json!({ "api_version": 1, "data_dir": dir })).await.unwrap();
    host
}

const TRACK: &str = r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"><item id="-1" parentID="-1"><dc:title>Sang</dc:title><dc:creator>Kunstner</dc:creator><upnp:album>Album</upnp:album><upnp:albumArtURI>/getaa?s=1&amp;u=x</upnp:albumArtURI></item></DIDL-Lite>"#;

/// Stue (coordinator) and Køkken in one group, Stue playing a track.
async fn household() -> (FakeSonos, FakeSonos, Arc<Mutex<Household>>) {
    let home: Arc<Mutex<Household>> = Arc::default();
    let a = FakeSonos::start("RINCON_A", "Stue", home.clone()).await;
    let b = FakeSonos::start("RINCON_B", "Køkken", home.clone()).await;
    let rooms = HashMap::from([("RINCON_A".to_string(), "Stue".to_string()), ("RINCON_B".to_string(), "Køkken".to_string())]);
    {
        let mut h = home.lock().unwrap();
        h.zone_groups = fake::zone_groups(&[(&a, &[&a, &b])], &rooms);
        h.favorites = fake::FAVORITES.into();
    }
    {
        let mut s = a.state.lock().unwrap();
        s.transport = "PLAYING".into();
        s.track_uri = "x-file-cifs://nas/musik/sang.flac".into();
        s.track_meta = TRACK.into();
        s.duration = "0:03:45".into();
        s.position = "0:01:00".into();
        s.av_uri = "x-rincon-queue:RINCON_A#0".into();
        s.volume = 25;
    }
    b.state.lock().unwrap().volume = 40;
    (a, b, home)
}

async fn setup(host: &FakeHost, data: Value) {
    let entry = json!({ "entry_id": "e1", "domain": "sonos", "title": "Sonos", "data": data, "options": {} });
    host.call("setup_entry", json!({ "entry": entry })).await.unwrap();
}

async fn call(host: &FakeHost, entity: &str, service: &str, data: Value) -> Result<Value, Value> {
    host.call("entity/call_service", json!({ "entry_id": "e1", "entity_id": entity, "service": service, "data": data })).await
}

#[tokio::test]
async fn user_flow_checks_addresses() {
    let (a, _b, _home) = household().await;
    let host = start("flow").await;
    let data = json!({ "scan": false, "discovery": false,
                       "found": [{ "uuid": "RINCON_A", "name": "Stue", "model": "One", "addr": "10.0.0.20:1400" },
                                 { "uuid": "RINCON_B", "name": "Køkken", "addr": "10.0.0.21:1400" }] });
    let r = host.call("config_flow", json!({ "flow": flow("user", data), "step": "user" })).await.unwrap();
    assert_eq!(r["result"]["type"], "form");
    assert_eq!(r["unique_id"], "sonos");
    let props: Vec<&String> = r["result"]["schema"]["properties"].as_object().unwrap().keys().collect();
    assert_eq!(props, ["devices", "known_hosts"]);
    let options = &r["result"]["schema"]["properties"]["devices"]["items"]["oneOf"];
    assert_eq!(options[0]["title"], "Stue (One, 10.0.0.20)");
    assert_eq!(options[1]["title"], "Køkken (10.0.0.21)");
    let data = r["data"].clone();

    let step = |input: Value| json!({ "flow": flow("user", data.clone()), "step": "user", "input": input });
    let r = host.call("config_flow", step(json!({ "known_hosts": "ikke en:adresse" }))).await.unwrap();
    assert_eq!(r["result"]["errors"]["known_hosts"], "invalid_host");
    let r = host.call("config_flow", step(json!({ "known_hosts": "127.0.0.1:1" }))).await.unwrap();
    assert_eq!(r["result"]["errors"]["base"], "cannot_connect");

    let r = host.call("config_flow", step(json!({ "known_hosts": a.addr(), "devices": ["RINCON_A", "RINCON_B"] }))).await.unwrap();
    assert_eq!(r["result"]["type"], "create_entry");
    assert_eq!(r["result"]["data"], json!({ "discovery": false, "known_hosts": [a.addr()] }));
    let r = host.call("config_flow", step(json!({ "known_hosts": "", "devices": ["RINCON_B"] }))).await.unwrap();
    assert_eq!(r["result"]["data"], json!({ "discovery": false, "known_hosts": [], "devices": ["RINCON_B"] }));
}

#[tokio::test]
async fn zeroconf_flow_asks_once() {
    let host = start("zeroconf").await;
    let f = json!({ "flow_id": "f2", "source": "zeroconf", "discovery": { "name": "RINCON_48A6B8000001400@Soveværelse._sonos._tcp.local.", "port": 1443 }, "data": {} });
    let r = host.call("config_flow", json!({ "flow": f, "step": "zeroconf" })).await.unwrap();
    assert_eq!(r["result"]["step_id"], "confirm");
    assert_eq!(r["unique_id"], "sonos");
    assert!(r["result"]["description"].as_str().unwrap().contains("Soveværelse"));
    let r = host.call("config_flow", json!({ "flow": f, "step": "confirm", "input": {} })).await.unwrap();
    assert_eq!(r["result"]["type"], "create_entry");
}

#[tokio::test]
async fn players_groups_and_services() {
    let (a, b, _home) = household().await;
    let host = start("services").await;
    // only Stue is typed; Køkken comes from its zone group state
    setup(&host, json!({ "discovery": false, "known_hosts": [a.addr()] })).await;

    let (st, attrs) = wait_for("Stue spiller", || host.state_of("RINCON_A").filter(|(_, a)| a.get("group_members").is_some())).await;
    assert_eq!(st, "playing");
    assert_eq!(attrs["media_title"], "Sang");
    assert_eq!(attrs["media_artist"], "Kunstner");
    assert_eq!(attrs["volume_level"], 0.25);
    assert_eq!(attrs["entity_picture"], format!("http://{}/getaa?s=1&u=x", a.addr()));
    assert_eq!(attrs["media_duration"], 225.0);
    assert_eq!(attrs["source_list"], json!(["P3", "Morgenmusik", "Line-in"]));
    let stue = host.entity("RINCON_A");
    let kokken = host.entity("RINCON_B");
    assert_eq!(attrs["group_members"], json!([stue, kokken]));
    // Køkken plays what its coordinator plays, at its own volume
    let (st, attrs) = wait_for("Køkken", || host.state_of("RINCON_B").filter(|(_, a)| a.get("group_members").is_some())).await;
    assert_eq!(st, "playing");
    assert_eq!(attrs["media_title"], "Sang");
    assert_eq!(attrs["volume_level"], 0.4);
    {
        let rec = host.rec.lock().unwrap();
        assert_eq!(rec.devices.len(), 2);
        assert_eq!(rec.devices[0]["manufacturer"], "Sonos");
        assert!(rec.entities.values().all(|(_, handler)| *handler));
    }

    // volume belongs to the player itself
    call(&host, &kokken, "volume_set", json!({ "volume_level": 0.33 })).await.unwrap();
    assert_eq!(b.calls_of("SetVolume").last().unwrap()["DesiredVolume"], "33");
    assert!(a.calls_of("SetVolume").is_empty());
    call(&host, &stue, "volume_up", json!({})).await.unwrap();
    assert_eq!(a.calls_of("SetVolume").last().unwrap()["DesiredVolume"], "30");
    call(&host, &stue, "volume_mute", json!({ "is_volume_muted": true })).await.unwrap();
    assert_eq!(a.calls_of("SetMute").last().unwrap()["DesiredMute"], "1");

    // transport goes to the coordinator
    call(&host, &kokken, "media_pause", json!({})).await.unwrap();
    assert_eq!(a.calls_of("Pause").len(), 1);
    assert!(b.calls_of("Pause").is_empty());
    wait_for("pause", || host.state_of("RINCON_B").filter(|(s, _)| s == "paused")).await;
    call(&host, &kokken, "media_play_pause", json!({})).await.unwrap();
    assert_eq!(a.calls_of("Play").len(), 1);
    call(&host, &stue, "media_seek", json!({ "seek_position": 125 })).await.unwrap();
    assert_eq!(a.calls_of("Seek")[0]["Target"], "0:02:05");
    assert_eq!(a.calls_of("Seek")[0]["Unit"], "REL_TIME");
    call(&host, &stue, "shuffle_set", json!({ "shuffle": true })).await.unwrap();
    assert_eq!(a.calls_of("SetPlayMode").last().unwrap()["NewPlayMode"], "SHUFFLE_NOREPEAT");
    wait_for("shuffle", || host.state_of("RINCON_A").filter(|(_, a)| a["shuffle"] == true)).await;
    call(&host, &stue, "repeat_set", json!({ "repeat": "all" })).await.unwrap();
    assert_eq!(a.calls_of("SetPlayMode").last().unwrap()["NewPlayMode"], "SHUFFLE");
    let e = call(&host, &stue, "repeat_set", json!({ "repeat": "altid" })).await.unwrap_err();
    assert_eq!(e["data"]["kind"], "invalid_argument");

    // a UPnP error reaches the caller
    a.state.lock().unwrap().fail.insert("Next".into(), 701);
    let e = call(&host, &kokken, "media_next_track", json!({})).await.unwrap_err();
    assert_eq!(e["data"]["kind"], "failed");
    assert!(e["message"].as_str().unwrap().contains("701"));

    // favorites: radio plays directly, a playlist replaces the queue
    call(&host, &stue, "select_source", json!({ "source": "P3" })).await.unwrap();
    let set = a.calls_of("SetAVTransportURI");
    assert_eq!(set.last().unwrap()["CurrentURI"], "x-sonosapi-stream:s24861?sid=254");
    assert_eq!(set.last().unwrap()["CurrentURIMetaData"], "<DIDL-Lite>p3</DIDL-Lite>");
    wait_for("kilde", || host.state_of("RINCON_A").filter(|(_, a)| a["source"] == "P3")).await;
    call(&host, &stue, "select_source", json!({ "source": "Morgenmusik" })).await.unwrap();
    assert_eq!(a.calls_of("RemoveAllTracksFromQueue").len(), 1);
    assert_eq!(a.calls_of("AddURIToQueue").last().unwrap()["EnqueuedURI"], "x-rincon-cpcontainer:1006206cplaylist");
    assert_eq!(a.calls_of("SetAVTransportURI").last().unwrap()["CurrentURI"], "x-rincon-queue:RINCON_A#0");
    call(&host, &kokken, "select_source", json!({ "source": "Line-in" })).await.unwrap();
    assert_eq!(a.calls_of("SetAVTransportURI").last().unwrap()["CurrentURI"], "x-rincon-stream:RINCON_B");
    let e = call(&host, &stue, "select_source", json!({ "source": "Ingen" })).await.unwrap_err();
    assert_eq!(e["data"]["kind"], "invalid_argument");
    call(&host, &stue, "play_media", json!({ "media_content_id": "FV:2/1", "media_content_type": "favorite_item_id" })).await.unwrap();
    assert_eq!(a.calls_of("SetAVTransportURI").last().unwrap()["CurrentURI"], "x-sonosapi-stream:s24861?sid=254");

    // a URL
    call(&host, &stue, "play_media", json!({ "media_content_id": "http://nas.lan/sang.mp3", "media_content_type": "music", "extra": { "title": "Min sang" } }))
        .await
        .unwrap();
    let last = a.calls_of("SetAVTransportURI").last().unwrap().clone();
    assert_eq!(last["CurrentURI"], "http://nas.lan/sang.mp3");
    assert!(last["CurrentURIMetaData"].contains("<dc:title>Min sang</dc:title>"));
    call(&host, &stue, "play_media", json!({ "media_content_id": "http://nas.lan/to.mp3", "media_content_type": "music", "enqueue": "add" })).await.unwrap();
    assert_eq!(a.calls_of("AddURIToQueue").last().unwrap()["EnqueuedURI"], "http://nas.lan/to.mp3");
    assert_eq!(a.calls_of("AddURIToQueue").last().unwrap()["EnqueueAsNext"], "0");

    // grouping
    call(&host, &kokken, "unjoin", json!({})).await.unwrap();
    assert_eq!(b.calls_of("BecomeCoordinatorOfStandaloneGroup").len(), 1);
    call(&host, &stue, "join", json!({ "group_members": [kokken] })).await.unwrap();
    assert_eq!(b.calls_of("SetAVTransportURI").last().unwrap()["CurrentURI"], "x-rincon:RINCON_A");
    let e = call(&host, &stue, "join", json!({ "group_members": ["media_player.tv"] })).await.unwrap_err();
    assert_eq!(e["data"]["kind"], "invalid_argument");
    let e = call(&host, &stue, "turn_on", json!({})).await.unwrap_err();
    assert_eq!(e["data"]["kind"], "not_supported");
}

#[tokio::test]
async fn groups_change_with_the_topology() {
    let (a, b, home) = household().await;
    let host = start("topology").await;
    setup(&host, json!({ "discovery": false, "known_hosts": [a.addr(), b.addr()] })).await;
    wait_for("gruppe", || host.state_of("RINCON_B").filter(|(_, a)| a.get("group_members").is_some())).await;
    // Køkken leaves the group; the zone group state event says so
    let rooms = HashMap::from([("RINCON_A".to_string(), "Stue".to_string()), ("RINCON_B".to_string(), "Køkken".to_string())]);
    home.lock().unwrap().zone_groups = fake::zone_groups(&[(&a, &[&a]), (&b, &[&b])], &rooms);
    b.changed().await;
    let (st, attrs) = wait_for("alene", || host.state_of("RINCON_B").filter(|(_, a)| a.get("group_members").is_none())).await;
    // Køkken's own transport now, which is stopped
    assert_eq!(st, "idle");
    assert!(attrs.get("media_title").is_none());
}

#[tokio::test]
async fn events_bring_changes_at_once() {
    let (a, _b, _home) = household().await;
    let host = start("events").await;
    setup(&host, json!({ "discovery": false, "known_hosts": [a.addr()] })).await;
    wait_for("Stue", || host.state_of("RINCON_A")).await;
    let cbs = wait_for("abonnementer", || Some(a.callbacks.lock().unwrap().clone()).filter(|c| c.len() >= 3)).await;
    assert!(cbs.iter().any(|c| c.ends_with("/RINCON_A/avt")), "{cbs:?}");
    assert!(cbs.iter().any(|c| c.ends_with("/RINCON_A/rc")));
    // let the first events arrive, so the player stops polling every 5 seconds
    tokio::time::sleep(Duration::from_millis(1500)).await;
    a.state.lock().unwrap().volume = 60;
    tokio::time::sleep(Duration::from_millis(100)).await;
    a.changed().await;
    wait_up_to("lydstyrke via hændelse", Duration::from_secs(2), &mut || host.state_of("RINCON_A").filter(|(_, a)| a["volume_level"] == 0.6)).await;
}

#[tokio::test]
async fn polls_without_events_and_marks_a_silent_player() {
    let (a, b, _home) = household().await;
    a.state.lock().unwrap().refuse_subscriptions = true;
    b.state.lock().unwrap().refuse_subscriptions = true;
    let host = start("poll").await;
    setup(&host, json!({ "discovery": false, "known_hosts": [a.addr()] })).await;
    wait_for("Stue", || host.state_of("RINCON_A")).await;
    a.state.lock().unwrap().transport = "PAUSED_PLAYBACK".into();
    wait_for("pause ved polling", || host.state_of("RINCON_A").filter(|(s, _)| s == "paused")).await;
    drop(a);
    wait_for("utilgængelig", || host.state_of("RINCON_A").filter(|(s, _)| s == "unavailable")).await;
}
