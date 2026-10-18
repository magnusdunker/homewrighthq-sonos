//! A Sonos player on localhost: the device description, enough SOAP actions
//! to play something, and event subscriptions that send NOTIFY back.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use homewright_sonos::{http, xml};
use quick_xml::events::Event;
use quick_xml::Reader;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

pub struct State {
    pub transport: String,
    pub volume: u32,
    pub muted: bool,
    pub play_mode: String,
    pub track_uri: String,
    pub track_meta: String,
    pub duration: String,
    pub position: String,
    pub av_uri: String,
    pub av_meta: String,
    /// action -> UPnP error code it answers with
    pub fail: HashMap<String, u32>,
    pub refuse_subscriptions: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            transport: "STOPPED".into(),
            volume: 20,
            muted: false,
            play_mode: "NORMAL".into(),
            track_uri: String::new(),
            track_meta: String::new(),
            duration: "NOT_IMPLEMENTED".into(),
            position: "NOT_IMPLEMENTED".into(),
            av_uri: String::new(),
            av_meta: String::new(),
            fail: HashMap::new(),
            refuse_subscriptions: false,
        }
    }
}

/// An action and its arguments.
pub type Call = (String, HashMap<String, String>);

/// Shared by all players of a household.
#[derive(Default)]
pub struct Household {
    pub zone_groups: String,
    pub favorites: String,
}

pub struct FakeSonos {
    pub port: u16,
    pub uuid: String,
    pub state: Arc<Mutex<State>>,
    /// Every action the player got, with its arguments.
    pub calls: Arc<Mutex<Vec<Call>>>,
    /// The CALLBACK of each subscription.
    pub callbacks: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
    conns: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Drop for FakeSonos {
    fn drop(&mut self) {
        self.task.abort();
        // also the open connections
        for c in self.conns.lock().unwrap().drain(..) {
            c.abort();
        }
    }
}

fn description(uuid: &str, room: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><root xmlns="urn:schemas-upnp-org:device-1-0"><device>
<deviceType>urn:schemas-upnp-org:device:ZonePlayer:1</deviceType><friendlyName>127.0.0.1 - Sonos One</friendlyName>
<manufacturer>Sonos, Inc.</manufacturer><modelName>Sonos One</modelName><displayName>One</displayName>
<softwareVersion>80.1-55014</softwareVersion><roomName>{room}</roomName><UDN>uuid:{uuid}</UDN>
<serviceList><service><serviceType>urn:schemas-upnp-org:service:AudioIn:1</serviceType></service></serviceList>
</device></root>"#
    )
}

/// The action's arguments: the children of the element inside the body.
fn args(body: &str) -> HashMap<String, String> {
    let mut r = Reader::from_str(body);
    let mut depth = 0;
    let mut out = HashMap::new();
    let mut key: Option<String> = None;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                depth += 1;
                // Envelope 1, Body 2, action 3, argument 4
                if depth == 4 {
                    let k = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                    out.insert(k.clone(), String::new());
                    key = Some(k);
                }
            }
            Ok(Event::Empty(e)) if depth == 3 => {
                out.insert(String::from_utf8_lossy(e.local_name().as_ref()).into_owned(), String::new());
            }
            Ok(Event::Text(t)) => {
                if let (Some(k), 4) = (&key, depth) {
                    out.get_mut(k).unwrap().push_str(&t.unescape().unwrap());
                }
            }
            Ok(Event::End(_)) => {
                if depth == 4 {
                    key = None;
                }
                depth -= 1;
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn answer(action: &str, fields: &[(&str, String)]) -> String {
    let mut body = String::new();
    for (k, v) in fields {
        body.push_str(&format!("<{k}>{}</{k}>", xml::escape(v)));
    }
    format!(r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action}Response xmlns:u="x">{body}</u:{action}Response></s:Body></s:Envelope>"#)
}

fn fault(code: u32) -> String {
    format!(r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>{code}</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#)
}

fn act(s: &mut State, home: &Household, action: &str, a: &HashMap<String, String>) -> Result<String, u32> {
    if let Some(code) = s.fail.get(action) {
        return Err(*code);
    }
    let fields: Vec<(&str, String)> = match action {
        "GetVolume" => vec![("CurrentVolume", s.volume.to_string())],
        "GetMute" => vec![("CurrentMute", if s.muted { "1" } else { "0" }.into())],
        "GetTransportInfo" => vec![("CurrentTransportState", s.transport.clone()), ("CurrentTransportStatus", "OK".into()), ("CurrentSpeed", "1".into())],
        "GetPositionInfo" => vec![
            ("Track", "1".into()),
            ("TrackDuration", s.duration.clone()),
            ("TrackMetaData", s.track_meta.clone()),
            ("TrackURI", s.track_uri.clone()),
            ("RelTime", s.position.clone()),
        ],
        "GetMediaInfo" => vec![("NrTracks", "1".into()), ("CurrentURI", s.av_uri.clone()), ("CurrentURIMetaData", s.av_meta.clone())],
        "GetTransportSettings" => vec![("PlayMode", s.play_mode.clone()), ("RecQualityMode", "NOT_IMPLEMENTED".into())],
        "GetZoneGroupState" => vec![("ZoneGroupState", home.zone_groups.clone())],
        "Browse" => vec![("Result", home.favorites.clone()), ("NumberReturned", "2".into()), ("TotalMatches", "2".into())],
        "SetVolume" => {
            s.volume = a["DesiredVolume"].parse().unwrap();
            vec![]
        }
        "SetMute" => {
            s.muted = a["DesiredMute"] == "1";
            vec![]
        }
        "Play" => {
            s.transport = "PLAYING".into();
            vec![]
        }
        "Pause" => {
            s.transport = "PAUSED_PLAYBACK".into();
            vec![]
        }
        "Stop" => {
            s.transport = "STOPPED".into();
            vec![]
        }
        "SetPlayMode" => {
            s.play_mode = a["NewPlayMode"].clone();
            vec![]
        }
        "SetAVTransportURI" => {
            s.av_uri = a["CurrentURI"].clone();
            s.av_meta = a["CurrentURIMetaData"].clone();
            vec![]
        }
        "AddURIToQueue" => vec![("FirstTrackNumberEnqueued", "1".into()), ("NumTracksAdded", "1".into()), ("NewQueueLength", "1".into())],
        _ => vec![],
    };
    Ok(answer(action, &fields))
}

const NOTIFY_BODY: &str = r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>&lt;Event/&gt;</LastChange></e:property></e:propertyset>"#;

/// Send a NOTIFY to `callback` the way a player does.
pub async fn notify(callback: &str) {
    let addr = xml::addr_of_url(callback).unwrap();
    let path = callback.trim_start_matches("http://").trim_start_matches(addr.as_str());
    if let Ok(mut c) = tokio::net::TcpStream::connect(&addr).await {
        let req = format!(
            "NOTIFY {path} HTTP/1.1\r\nHOST: {addr}\r\nCONTENT-TYPE: text/xml\r\nNT: upnp:event\r\nNTS: upnp:propchange\r\nSID: uuid:sub\r\nSEQ: 0\r\nContent-Length: {}\r\n\r\n{NOTIFY_BODY}",
            NOTIFY_BODY.len()
        );
        let _ = c.write_all(req.as_bytes()).await;
        let mut r = BufReader::new(c);
        let mut line = String::new();
        let _ = tokio::io::AsyncBufReadExt::read_line(&mut r, &mut line).await;
    }
}

impl FakeSonos {
    pub async fn start(uuid: &str, room: &str, home: Arc<Mutex<Household>>) -> Self {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let state: Arc<Mutex<State>> = Arc::default();
        let calls: Arc<Mutex<Vec<Call>>> = Arc::default();
        let callbacks: Arc<Mutex<Vec<String>>> = Arc::default();
        let desc = description(uuid, room);
        let (st, ca, cb) = (state.clone(), calls.clone(), callbacks.clone());
        let conns: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::default();
        let open = conns.clone();
        let task = tokio::spawn(async move {
            let mut n = 0u32;
            while let Ok((conn, _)) = l.accept().await {
                let (st, ca, cb, home, desc) = (st.clone(), ca.clone(), cb.clone(), home.clone(), desc.clone());
                n += 1;
                let sub_base = n * 100;
                let t = tokio::spawn(async move {
                    let (r, mut w) = conn.into_split();
                    let mut r = BufReader::new(r);
                    let mut subs = 0;
                    while let Ok(Some(req)) = http::read_request(&mut r).await {
                        let mut extra = String::new();
                        let (status, body) = match req.method.as_str() {
                            "GET" if req.path == "/xml/device_description.xml" => ("200 OK", desc.clone()),
                            "POST" => {
                                let action = req.header("soapaction").unwrap_or_default().trim_matches('"').rsplit('#').next().unwrap_or_default().to_string();
                                let a = args(&req.body_text());
                                ca.lock().unwrap().push((action.clone(), a.clone()));
                                let res = act(&mut st.lock().unwrap(), &home.lock().unwrap(), &action, &a);
                                match res {
                                    Ok(b) => ("200 OK", b),
                                    Err(code) => ("500 Internal Server Error", fault(code)),
                                }
                            }
                            "SUBSCRIBE" if st.lock().unwrap().refuse_subscriptions => ("503 Service Unavailable", String::new()),
                            "SUBSCRIBE" => {
                                subs += 1;
                                extra = format!("SID: uuid:sub-{}\r\nTIMEOUT: Second-1800\r\n", sub_base + subs);
                                if let Some(c) = req.header("callback") {
                                    let c = c.trim_matches(['<', '>']).to_string();
                                    cb.lock().unwrap().push(c.clone());
                                    // a player sends the current state right after subscribing
                                    tokio::spawn(async move {
                                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                        notify(&c).await;
                                    });
                                }
                                ("200 OK", String::new())
                            }
                            "UNSUBSCRIBE" => ("200 OK", String::new()),
                            _ => ("404 Not Found", String::new()),
                        };
                        let reply = format!("HTTP/1.1 {status}\r\n{extra}Content-Type: text/xml\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                        if w.write_all(reply.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
                open.lock().unwrap().push(t);
            }
        });
        Self { port, uuid: uuid.into(), state, calls, callbacks, task, conns }
    }

    pub fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub fn calls_of(&self, action: &str) -> Vec<HashMap<String, String>> {
        self.calls.lock().unwrap().iter().filter(|(a, _)| a == action).map(|(_, a)| a.clone()).collect()
    }

    /// Tell every subscriber that something changed.
    pub async fn changed(&self) {
        let cbs = self.callbacks.lock().unwrap().clone();
        for c in cbs {
            notify(&c).await;
        }
    }
}

/// Zone group state for groups of (coordinator, members).
pub fn zone_groups(groups: &[(&FakeSonos, &[&FakeSonos])], rooms: &HashMap<String, String>) -> String {
    let mut x = String::from("<ZoneGroupState><ZoneGroups>");
    for (c, members) in groups {
        x.push_str(&format!(r#"<ZoneGroup Coordinator="{}" ID="{}:1">"#, c.uuid, c.uuid));
        for m in members.iter() {
            x.push_str(&format!(
                r#"<ZoneGroupMember UUID="{}" Location="http://{}/xml/device_description.xml" ZoneName="{}"/>"#,
                m.uuid,
                m.addr(),
                rooms[&m.uuid]
            ));
        }
        x.push_str("</ZoneGroup>");
    }
    x.push_str("</ZoneGroups><VanishedDevices/></ZoneGroupState>");
    x
}

pub const FAVORITES: &str = r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" xmlns:r="urn:schemas-rinconnetworks-com:metadata-1-0/" xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"><item id="FV:2/1" parentID="FV:2" restricted="false"><dc:title>P3</dc:title><upnp:class>object.itemobject.item.sonos-favorite</upnp:class><res protocolInfo="x-rincon-mp3radio:*:*:*">x-sonosapi-stream:s24861?sid=254</res><r:resMD>&lt;DIDL-Lite&gt;p3&lt;/DIDL-Lite&gt;</r:resMD></item><item id="FV:2/2" parentID="FV:2" restricted="false"><dc:title>Morgenmusik</dc:title><res protocolInfo="x-rincon-cpcontainer:*:*:*">x-rincon-cpcontainer:1006206cplaylist</res><r:resMD>&lt;DIDL-Lite&gt;liste&lt;/DIDL-Lite&gt;</r:resMD></item></DIDL-Lite>"#;
