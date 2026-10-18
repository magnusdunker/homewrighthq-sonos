//! From what a player and its group coordinator answer to a media_player
//! state with Home Assistant's attribute names.

use serde_json::{json, Map, Value};

use crate::xml;

// media_player supported_features, as in Home Assistant
pub const PAUSE: u64 = 1;
pub const SEEK: u64 = 2;
pub const VOLUME_SET: u64 = 4;
pub const VOLUME_MUTE: u64 = 8;
pub const PREVIOUS_TRACK: u64 = 16;
pub const NEXT_TRACK: u64 = 32;
pub const PLAY_MEDIA: u64 = 512;
pub const VOLUME_STEP: u64 = 1024;
pub const SELECT_SOURCE: u64 = 2048;
pub const STOP: u64 = 4096;
pub const CLEAR_PLAYLIST: u64 = 8192;
pub const PLAY: u64 = 16384;
pub const SHUFFLE_SET: u64 = 32768;
pub const REPEAT_SET: u64 = 262144;
pub const GROUPING: u64 = 524288;
pub const MEDIA_ENQUEUE: u64 = 2097152;

pub const FEATURES: u64 = PAUSE
    | SEEK
    | VOLUME_SET
    | VOLUME_MUTE
    | PREVIOUS_TRACK
    | NEXT_TRACK
    | PLAY_MEDIA
    | VOLUME_STEP
    | SELECT_SOURCE
    | STOP
    | CLEAR_PLAYLIST
    | PLAY
    | SHUFFLE_SET
    | REPEAT_SET
    | GROUPING
    | MEDIA_ENQUEUE;

pub const LINE_IN: &str = "Line-in";
pub const TV: &str = "TV";

/// The raw answers, read from the player itself (volume) and from its
/// group's coordinator (everything about what plays).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub transport: String,
    pub play_mode: String,
    pub track_uri: String,
    pub track_meta: String,
    pub duration: String,
    pub position: String,
    pub av_uri: String,
    pub av_meta: String,
    pub volume: Option<u32>,
    pub muted: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Favorite {
    pub id: String,
    pub title: String,
    pub uri: String,
    pub meta: String,
}

/// What else goes into the state.
pub struct Context<'a> {
    /// host:port of the coordinator, for album art paths
    pub coordinator_addr: &'a str,
    pub line_in: bool,
    pub tv: bool,
    pub favorites: &'a [Favorite],
    /// entity ids of the group, coordinator first; empty when alone
    pub group: Vec<String>,
}

/// `H:MM:SS` in seconds; `None` for `NOT_IMPLEMENTED` and the like.
pub fn seconds(t: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut parts = 0;
    for p in t.trim().split(':') {
        let n: f64 = p.parse().ok()?;
        total = total * 60.0 + n;
        parts += 1;
    }
    (parts >= 2).then_some(total)
}

/// Seconds as `H:MM:SS` for Seek.
pub fn hms(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Shuffle and repeat from a PlayMode.
pub fn play_mode(mode: &str) -> (bool, &'static str) {
    match mode {
        "SHUFFLE" => (true, "all"),
        "SHUFFLE_NOREPEAT" => (true, "off"),
        "SHUFFLE_REPEAT_ONE" => (true, "one"),
        "REPEAT_ALL" => (false, "all"),
        "REPEAT_ONE" => (false, "one"),
        _ => (false, "off"),
    }
}

/// The PlayMode for shuffle and repeat.
pub fn mode_of(shuffle: bool, repeat: &str) -> &'static str {
    match (shuffle, repeat) {
        (true, "all") => "SHUFFLE",
        (true, "one") => "SHUFFLE_REPEAT_ONE",
        (true, _) => "SHUFFLE_NOREPEAT",
        (false, "all") => "REPEAT_ALL",
        (false, "one") => "REPEAT_ONE",
        _ => "NORMAL",
    }
}

/// Radio and other streams, which play directly instead of from the queue.
pub fn is_stream(uri: &str) -> bool {
    ["x-sonosapi-stream:", "x-sonosapi-radio:", "x-rincon-mp3radio:", "hls-radio:", "x-sonosapi-hls:", "aac:", "x-rincon-stream:", "x-sonos-htastream:"]
        .iter()
        .any(|p| uri.starts_with(p))
}

fn text(v: Option<&String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && s != "NOT_IMPLEMENTED")
}

/// The source for the current URI: a favorite with that URI, or line-in/TV.
fn source_of(s: &Snapshot, ctx: &Context) -> Option<String> {
    if s.av_uri.starts_with("x-rincon-stream:") {
        return Some(LINE_IN.into());
    }
    if s.av_uri.starts_with("x-sonos-htastream:") {
        return Some(TV.into());
    }
    let same = |a: &str, b: &str| !a.is_empty() && a == b;
    ctx.favorites.iter().find(|f| same(&f.uri, &s.av_uri) || same(&f.uri, &s.track_uri)).map(|f| f.title.clone())
}

pub fn source_list(ctx: &Context) -> Vec<String> {
    let mut list: Vec<String> = ctx.favorites.iter().map(|f| f.title.clone()).collect();
    if ctx.line_in {
        list.push(LINE_IN.into());
    }
    if ctx.tv {
        list.push(TV.into());
    }
    list
}

pub fn state_of(s: &Snapshot, ctx: &Context, now: &str) -> (String, Map<String, Value>) {
    let mut a = Map::new();
    if let Some(v) = s.volume {
        a.insert("volume_level".into(), json!(f64::from(v.min(100)) / 100.0));
    }
    if let Some(m) = s.muted {
        a.insert("is_volume_muted".into(), json!(m));
    }
    let state = match s.transport.as_str() {
        "PLAYING" => "playing",
        "PAUSED_PLAYBACK" => "paused",
        "TRANSITIONING" => "buffering",
        _ => "idle",
    };
    let (shuffle, repeat) = play_mode(&s.play_mode);
    a.insert("shuffle".into(), json!(shuffle));
    a.insert("repeat".into(), json!(repeat));

    let source = source_of(s, ctx);
    let has_media = !s.track_uri.is_empty() || !s.av_uri.is_empty();
    if has_media && state != "idle" {
        let track = xml::didl_items(&s.track_meta).into_iter().next().unwrap_or_default();
        let current = xml::didl_items(&s.av_meta).into_iter().next().unwrap_or_default();
        let mut title = text(track.get("title"));
        let mut artist = text(track.get("creator"));
        let album = text(track.get("album"));
        if s.av_uri.starts_with("x-rincon-stream:") || s.av_uri.starts_with("x-sonos-htastream:") {
            title = source.clone();
            artist = None;
        } else if is_stream(&s.av_uri) {
            // radio: the station is the channel, streamContent is "artist - title"
            let channel = text(current.get("title")).or_else(|| source.clone());
            if let Some(c) = &channel {
                a.insert("media_channel".into(), json!(c));
            }
            match text(track.get("streamContent")) {
                Some(sc) => match sc.split_once(" - ") {
                    Some((ar, ti)) => {
                        artist = Some(ar.trim().into());
                        title = Some(ti.trim().into());
                    }
                    None => title = Some(sc),
                },
                None => {
                    // the stream's own URI as title is no help
                    if title.as_deref().is_some_and(|t| t.contains("://") || t.starts_with("x-")) {
                        title = None;
                    }
                    title = title.or(channel);
                }
            }
        }
        let id = if s.track_uri.is_empty() { &s.av_uri } else { &s.track_uri };
        a.insert("media_content_id".into(), json!(id));
        a.insert("media_content_type".into(), json!("music"));
        for (k, v) in [("media_title", title), ("media_artist", artist), ("media_album_name", album)] {
            if let Some(v) = v {
                a.insert(k.into(), json!(v));
            }
        }
        if let Some(art) = text(track.get("albumArtURI")) {
            let url = if art.starts_with('/') { format!("http://{}{art}", ctx.coordinator_addr) } else { art };
            a.insert("entity_picture".into(), json!(url));
        }
        if let Some(d) = seconds(&s.duration).filter(|d| *d > 0.0) {
            a.insert("media_duration".into(), json!(d));
        }
        if let Some(p) = seconds(&s.position) {
            if a.contains_key("media_duration") {
                a.insert("media_position".into(), json!(p));
                a.insert("media_position_updated_at".into(), json!(now));
            }
        }
    }
    if let Some(src) = source {
        a.insert("source".into(), json!(src));
    }
    a.insert("source_list".into(), json!(source_list(ctx)));
    if !ctx.group.is_empty() {
        a.insert("group_members".into(), json!(ctx.group));
    }
    a.insert("supported_features".into(), json!(FEATURES));
    (state.to_string(), a)
}

/// DIDL-Lite with a title, for play_media.
pub fn didl_for(uri: &str, title: &str) -> String {
    format!(
        r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" xmlns:r="urn:schemas-rinconnetworks-com:metadata-1-0/" xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"><item id="-1" parentID="-1" restricted="true"><dc:title>{}</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><res protocolInfo="http-get:*:*:*">{}</res></item></DIDL-Lite>"#,
        xml::escape(title),
        xml::escape(uri)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(favorites: &[Favorite]) -> Context<'_> {
        Context { coordinator_addr: "10.0.0.5:1400", line_in: true, tv: false, favorites, group: vec![] }
    }

    const TRACK: &str = r#"<DIDL-Lite xmlns:dc="d" xmlns:upnp="u"><item id="-1"><dc:title>Sang</dc:title><dc:creator>Kunstner</dc:creator><upnp:album>Album</upnp:album><upnp:albumArtURI>/getaa?s=1&amp;u=x</upnp:albumArtURI></item></DIDL-Lite>"#;

    #[test]
    fn times() {
        assert_eq!(seconds("0:03:45"), Some(225.0));
        assert_eq!(seconds("1:00:00"), Some(3600.0));
        assert_eq!(seconds("NOT_IMPLEMENTED"), None);
        assert_eq!(seconds(""), None);
        assert_eq!(hms(225.4), "0:03:45");
        assert_eq!(hms(3725.0), "1:02:05");
    }

    #[test]
    fn modes_both_ways() {
        for m in ["NORMAL", "REPEAT_ALL", "REPEAT_ONE", "SHUFFLE", "SHUFFLE_NOREPEAT", "SHUFFLE_REPEAT_ONE"] {
            let (s, r) = play_mode(m);
            assert_eq!(mode_of(s, r), m);
        }
    }

    #[test]
    fn a_track_from_the_queue() {
        let s = Snapshot {
            transport: "PLAYING".into(),
            play_mode: "SHUFFLE".into(),
            track_uri: "x-file-cifs://nas/musik/sang.flac".into(),
            track_meta: TRACK.into(),
            duration: "0:03:45".into(),
            position: "0:01:00".into(),
            av_uri: "x-rincon-queue:RINCON_A#0".into(),
            volume: Some(25),
            muted: Some(false),
            ..Default::default()
        };
        let (state, a) = state_of(&s, &ctx(&[]), "nu");
        assert_eq!(state, "playing");
        assert_eq!(a["volume_level"], 0.25);
        assert_eq!(a["media_title"], "Sang");
        assert_eq!(a["media_artist"], "Kunstner");
        assert_eq!(a["media_album_name"], "Album");
        assert_eq!(a["entity_picture"], "http://10.0.0.5:1400/getaa?s=1&u=x");
        assert_eq!(a["media_duration"], 225.0);
        assert_eq!(a["media_position"], 60.0);
        assert_eq!((a["shuffle"].clone(), a["repeat"].clone()), (json!(true), json!("all")));
        assert_eq!(a["source_list"], json!(["Line-in"]));
        assert!(a.get("source").is_none());
        assert!(a.get("group_members").is_none());
    }

    #[test]
    fn radio_from_a_favorite() {
        let fav = [Favorite { id: "FV:2/1".into(), title: "P3".into(), uri: "x-sonosapi-stream:s24861?sid=254".into(), meta: String::new() }];
        let s = Snapshot {
            transport: "PLAYING".into(),
            track_uri: "x-sonosapi-stream:s24861?sid=254".into(),
            track_meta: r#"<DIDL-Lite xmlns:r="r" xmlns:dc="d"><item id="-1"><dc:title>x-sonosapi-stream:s24861?sid=254</dc:title><r:streamContent>Kunstner - Sang</r:streamContent></item></DIDL-Lite>"#.into(),
            av_uri: "x-sonosapi-stream:s24861?sid=254".into(),
            av_meta: r#"<DIDL-Lite xmlns:dc="d"><item id="-1"><dc:title>DR P3</dc:title></item></DIDL-Lite>"#.into(),
            duration: "0:00:00".into(),
            ..Default::default()
        };
        let mut c = ctx(&fav);
        c.group = vec!["media_player.stue".into(), "media_player.kokken".into()];
        let (_, a) = state_of(&s, &c, "nu");
        assert_eq!(a["media_channel"], "DR P3");
        assert_eq!(a["media_title"], "Sang");
        assert_eq!(a["media_artist"], "Kunstner");
        assert_eq!(a["source"], "P3");
        assert_eq!(a["source_list"], json!(["P3", "Line-in"]));
        assert!(a.get("media_duration").is_none() && a.get("media_position").is_none());
        assert_eq!(a["group_members"], json!(["media_player.stue", "media_player.kokken"]));
        // no streamContent: the station's name
        let mut quiet = s.clone();
        quiet.track_meta = r#"<DIDL-Lite xmlns:dc="d"><item id="-1"><dc:title>x-sonosapi-stream:s24861?sid=254</dc:title></item></DIDL-Lite>"#.into();
        let (_, a) = state_of(&quiet, &c, "nu");
        assert_eq!(a["media_title"], "DR P3");
    }

    #[test]
    fn line_in_and_stopped() {
        let s = Snapshot { transport: "PLAYING".into(), av_uri: "x-rincon-stream:RINCON_A".into(), ..Default::default() };
        let (state, a) = state_of(&s, &ctx(&[]), "nu");
        assert_eq!(state, "playing");
        assert_eq!(a["source"], "Line-in");
        assert_eq!(a["media_title"], "Line-in");
        let s = Snapshot { transport: "STOPPED".into(), track_uri: "x".into(), track_meta: TRACK.into(), ..Default::default() };
        let (state, a) = state_of(&s, &ctx(&[]), "nu");
        assert_eq!(state, "idle");
        assert!(a.get("media_title").is_none());
        assert_eq!(state_of(&Snapshot { transport: "PAUSED_PLAYBACK".into(), ..Default::default() }, &ctx(&[]), "nu").0, "paused");
    }
}
