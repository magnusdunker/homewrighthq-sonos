//! Reading the XML Sonos speaks: SOAP answers, DIDL-Lite metadata and the
//! zone group state. Element names are matched without their prefix.

use std::collections::HashMap;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

fn attrs(e: &BytesStart) -> HashMap<String, String> {
    e.attributes()
        .flatten()
        .filter_map(|a| {
            let key = String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned();
            Some((key, a.unescape_value().ok()?.into_owned()))
        })
        .collect()
}

/// The text inside the first element called `name`, unescaped. Sonos puts
/// whole XML documents escaped inside elements, which come out as XML again.
pub fn text_of(xml: &str, name: &str) -> Option<String> {
    let mut r = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut inside: Option<usize> = None;
    let mut out = String::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                depth += 1;
                if inside.is_none() && e.local_name().as_ref() == name.as_bytes() {
                    inside = Some(depth);
                }
            }
            Ok(Event::Empty(e)) => {
                if inside.is_none() && e.local_name().as_ref() == name.as_bytes() {
                    return Some(String::new());
                }
            }
            Ok(Event::Text(t)) if inside.is_some() => out.push_str(&t.unescape().ok()?),
            Ok(Event::CData(c)) if inside.is_some() => out.push_str(&String::from_utf8_lossy(&c)),
            Ok(Event::End(_)) => {
                if inside == Some(depth) {
                    return Some(out);
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// The `item` and `container` elements of a DIDL-Lite document, each as its
/// children's text by local name (`title`, `creator`, `res`, `resMD` ...)
/// plus the `id` attribute.
pub fn didl_items(xml: &str) -> Vec<HashMap<String, String>> {
    let mut r = Reader::from_str(xml);
    let mut items = vec![];
    let mut depth = 0usize;
    // (depth of the item, fields)
    let mut current: Option<(usize, HashMap<String, String>)> = None;
    let mut field: Option<String> = None;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                depth += 1;
                let name = e.local_name();
                match &current {
                    None if matches!(name.as_ref(), b"item" | b"container") => {
                        let a = attrs(&e);
                        let mut fields = HashMap::new();
                        if let Some(id) = a.get("id") {
                            fields.insert("id".to_string(), id.clone());
                        }
                        current = Some((depth, fields));
                    }
                    Some((d, _)) if depth == d + 1 => field = Some(String::from_utf8_lossy(name.as_ref()).into_owned()),
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if let (Some((_, fields)), Some(f)) = (current.as_mut(), field.as_ref()) {
                    if let Ok(s) = t.unescape() {
                        fields.entry(f.clone()).or_default().push_str(&s);
                    }
                }
            }
            Ok(Event::End(_)) => {
                if let Some((d, _)) = &current {
                    if depth == *d {
                        items.push(current.take().unwrap().1);
                    } else if depth == d + 1 {
                        field = None;
                    }
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    items
}

/// One visible player in the zone group state.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupMember {
    pub uuid: String,
    pub name: String,
    /// host:port from the member's Location
    pub addr: String,
    pub coordinator: String,
}

/// The players of `ZoneGroupState`. Invisible members (the second speaker of
/// a stereo pair, a Sub, surrounds) are left out; they follow the visible one.
pub fn zone_groups(xml: &str) -> Vec<GroupMember> {
    let mut r = Reader::from_str(xml);
    let mut out = vec![];
    let mut coordinator = String::new();
    loop {
        let (e, _empty) = match r.read_event() {
            Ok(Event::Start(e)) => (e, false),
            Ok(Event::Empty(e)) => (e, true),
            Ok(Event::Eof) | Err(_) => break,
            _ => continue,
        };
        match e.local_name().as_ref() {
            b"ZoneGroup" => coordinator = attrs(&e).get("Coordinator").cloned().unwrap_or_default(),
            b"ZoneGroupMember" => {
                let a = attrs(&e);
                if a.get("Invisible").map(String::as_str) == Some("1") {
                    continue;
                }
                let (Some(uuid), Some(location)) = (a.get("UUID"), a.get("Location")) else { continue };
                let Some(addr) = addr_of_url(location) else { continue };
                out.push(GroupMember {
                    uuid: uuid.clone(),
                    name: a.get("ZoneName").cloned().unwrap_or_else(|| uuid.clone()),
                    addr,
                    coordinator: if coordinator.is_empty() { uuid.clone() } else { coordinator.clone() },
                });
            }
            _ => {}
        }
    }
    out
}

/// `host:port` of an http URL.
pub fn addr_of_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let addr = rest.split('/').next()?;
    (!addr.is_empty()).then(|| addr.to_string())
}

/// Text made safe to put inside an element or attribute.
pub fn escape(s: &str) -> String {
    quick_xml::escape::escape(s).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_and_escaped_text() {
        let soap = r#"<s:Envelope xmlns:s="x"><s:Body><u:GetPositionInfoResponse xmlns:u="y"><Track>3</Track><TrackMetaData>&lt;DIDL-Lite&gt;&lt;item id=&quot;-1&quot;&gt;&lt;dc:title&gt;Sang &amp;amp; dans&lt;/dc:title&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;</TrackMetaData><TrackURI/></u:GetPositionInfoResponse></s:Body></s:Envelope>"#;
        assert_eq!(text_of(soap, "Track").as_deref(), Some("3"));
        assert_eq!(text_of(soap, "TrackURI").as_deref(), Some(""));
        assert_eq!(text_of(soap, "Nope"), None);
        let meta = text_of(soap, "TrackMetaData").unwrap();
        let items = didl_items(&meta);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], "-1");
        assert_eq!(items[0]["title"], "Sang & dans");
    }

    #[test]
    fn favorites() {
        let didl = r#"<DIDL-Lite xmlns:dc="d" xmlns:upnp="u" xmlns:r="r" xmlns="x">
            <item id="FV:2/1" parentID="FV:2"><dc:title>P3</dc:title><upnp:class>object.itemobject.item.sonos-favorite</upnp:class>
              <res protocolInfo="x">x-sonosapi-stream:s24861?sid=254</res><r:resMD>&lt;DIDL-Lite&gt;&lt;/DIDL-Lite&gt;</r:resMD></item>
            <item id="FV:2/2"><dc:title>Morgenmusik</dc:title><res>x-rincon-cpcontainer:1006206cplaylist</res></item>
        </DIDL-Lite>"#;
        let items = didl_items(didl);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["title"], "P3");
        assert_eq!(items[0]["res"], "x-sonosapi-stream:s24861?sid=254");
        assert_eq!(items[0]["resMD"], "<DIDL-Lite></DIDL-Lite>");
        assert_eq!(items[1]["id"], "FV:2/2");
        assert!(!items[1].contains_key("resMD"));
    }

    #[test]
    fn groups() {
        let xml = r#"<ZoneGroupState><ZoneGroups>
            <ZoneGroup Coordinator="RINCON_A" ID="RINCON_A:1">
              <ZoneGroupMember UUID="RINCON_A" Location="http://10.0.0.5:1400/xml/device_description.xml" ZoneName="Stue">
                <Satellite UUID="RINCON_S" Location="http://10.0.0.9:1400/xml/device_description.xml" ZoneName="Stue" Invisible="1"/>
              </ZoneGroupMember>
              <ZoneGroupMember UUID="RINCON_B" Location="http://10.0.0.6:1400/xml/device_description.xml" ZoneName="Køkken"/>
            </ZoneGroup>
            <ZoneGroup Coordinator="RINCON_C" ID="RINCON_C:2">
              <ZoneGroupMember UUID="RINCON_C" Location="http://10.0.0.7:1400/xml/device_description.xml" ZoneName="Bad"/>
              <ZoneGroupMember UUID="RINCON_D" Location="http://10.0.0.8:1400/xml/device_description.xml" ZoneName="Bad" Invisible="1"/>
            </ZoneGroup>
        </ZoneGroups><VanishedDevices/></ZoneGroupState>"#;
        let g = zone_groups(xml);
        assert_eq!(g.len(), 3);
        assert_eq!(g[0], GroupMember { uuid: "RINCON_A".into(), name: "Stue".into(), addr: "10.0.0.5:1400".into(), coordinator: "RINCON_A".into() });
        assert_eq!(g[1].coordinator, "RINCON_A");
        assert_eq!(g[1].name, "Køkken");
        assert_eq!(g[2].uuid, "RINCON_C");
    }
}
