//! UPnP over HTTP to a player on port 1400: SOAP actions, event
//! subscriptions (GENA) and the device description.

use std::time::Duration;

use reqwest::Method;
use thehome_plugin::Error;

use crate::xml;

pub const PORT: u16 = 1400;
const TIMEOUT: Duration = Duration::from_secs(5);

pub struct Service {
    pub control: &'static str,
    pub event: &'static str,
    pub urn: &'static str,
}

pub const AV_TRANSPORT: Service = Service {
    control: "/MediaRenderer/AVTransport/Control",
    event: "/MediaRenderer/AVTransport/Event",
    urn: "urn:schemas-upnp-org:service:AVTransport:1",
};
pub const RENDERING: Service = Service {
    control: "/MediaRenderer/RenderingControl/Control",
    event: "/MediaRenderer/RenderingControl/Event",
    urn: "urn:schemas-upnp-org:service:RenderingControl:1",
};
pub const TOPOLOGY: Service = Service {
    control: "/ZoneGroupTopology/Control",
    event: "/ZoneGroupTopology/Event",
    urn: "urn:schemas-upnp-org:service:ZoneGroupTopology:1",
};
pub const CONTENT: Service = Service {
    control: "/MediaServer/ContentDirectory/Control",
    event: "/MediaServer/ContentDirectory/Event",
    urn: "urn:schemas-upnp-org:service:ContentDirectory:1",
};

#[derive(Debug)]
pub enum SoapError {
    /// No answer, or not one that makes sense.
    Transport(String),
    /// The player answered with a UPnP error.
    Fault { code: u32, description: String },
}

impl std::fmt::Display for SoapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SoapError::Transport(e) => write!(f, "{e}"),
            SoapError::Fault { code, description } if description.is_empty() => write!(f, "UPnP-fejl {code}"),
            SoapError::Fault { code, description } => write!(f, "UPnP-fejl {code}: {description}"),
        }
    }
}

impl From<SoapError> for Error {
    fn from(e: SoapError) -> Self {
        match e {
            SoapError::Transport(m) => Error::unavailable(format!("afspilleren svarer ikke: {m}")),
            SoapError::Fault { code: 701, .. } => Error::failed("det kan afspilleren ikke lige nu (UPnP-fejl 701)"),
            SoapError::Fault { code: 800, .. } => Error::failed("afspilleren er med i en gruppe; det gøres på gruppens koordinator (UPnP-fejl 800)"),
            f => Error::failed(format!("afspilleren afviste det: {f}")),
        }
    }
}

/// host:port, with port 1400 when none is given.
pub fn with_port(host: &str) -> String {
    let bare_v6 = host.matches(':').count() > 1 && !host.starts_with('[');
    if bare_v6 {
        format!("[{host}]:{PORT}")
    } else if host.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        host.to_string()
    } else {
        format!("{host}:{PORT}")
    }
}

#[derive(Clone)]
pub struct Client(reqwest::Client);

impl Default for Client {
    fn default() -> Self {
        Self(reqwest::Client::builder().timeout(TIMEOUT).build().expect("http-klient"))
    }
}

/// What a player says about itself in its device description.
#[derive(Clone, Debug, PartialEq)]
pub struct Description {
    /// `RINCON_...`
    pub uuid: String,
    pub room: String,
    pub model: Option<String>,
    pub software: Option<String>,
    pub line_in: bool,
    pub tv: bool,
}

pub fn parse_description(xml_text: &str) -> Option<Description> {
    let udn = xml::text_of(xml_text, "UDN")?;
    let uuid = udn.trim().trim_start_matches("uuid:").to_string();
    if uuid.is_empty() {
        return None;
    }
    let text = |n: &str| xml::text_of(xml_text, n).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    Some(Description {
        room: text("roomName").unwrap_or_else(|| uuid.clone()),
        model: text("displayName").or_else(|| text("modelName")),
        software: text("softwareVersion"),
        line_in: xml_text.contains(":service:AudioIn:"),
        tv: xml_text.contains(":service:HTControl:"),
        uuid,
    })
}

impl Client {
    /// Run `action` and return the answer's XML.
    pub async fn call(&self, addr: &str, svc: &Service, action: &str, args: &[(&str, &str)]) -> Result<String, SoapError> {
        let mut body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{}">"#,
            svc.urn
        );
        for (k, v) in args {
            body.push_str(&format!("<{k}>{}</{k}>", xml::escape(v)));
        }
        body.push_str(&format!("</u:{action}></s:Body></s:Envelope>"));
        let r = self
            .0
            .post(format!("http://{addr}{}", svc.control))
            .header("Content-Type", r#"text/xml; charset="utf-8""#)
            .header("SOAPACTION", format!(r#""{}#{action}""#, svc.urn))
            .body(body)
            .send()
            .await
            .map_err(|e| SoapError::Transport(e.to_string()))?;
        let status = r.status();
        let text = r.text().await.map_err(|e| SoapError::Transport(e.to_string()))?;
        if status.is_success() {
            return Ok(text);
        }
        match xml::text_of(&text, "errorCode").and_then(|c| c.trim().parse().ok()) {
            Some(code) => Err(SoapError::Fault { code, description: xml::text_of(&text, "errorDescription").unwrap_or_default() }),
            None => Err(SoapError::Transport(format!("HTTP {status}"))),
        }
    }

    pub async fn describe(&self, addr: &str) -> Result<Description, SoapError> {
        let r = self
            .0
            .get(format!("http://{addr}/xml/device_description.xml"))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| SoapError::Transport(e.to_string()))?;
        let text = r.text().await.map_err(|e| SoapError::Transport(e.to_string()))?;
        parse_description(&text).ok_or_else(|| SoapError::Transport("ikke en Sonos-afspiller".into()))
    }

    /// Ask for events at `callback`. Returns the subscription id and how
    /// many seconds it lasts.
    pub async fn subscribe(&self, addr: &str, svc: &Service, callback: &str, seconds: u64) -> Result<(String, u64), SoapError> {
        let r = self
            .0
            .request(Method::from_bytes(b"SUBSCRIBE").unwrap(), format!("http://{addr}{}", svc.event))
            .header("CALLBACK", format!("<{callback}>"))
            .header("NT", "upnp:event")
            .header("TIMEOUT", format!("Second-{seconds}"))
            .send()
            .await
            .map_err(|e| SoapError::Transport(e.to_string()))?;
        Self::subscription(r, seconds)
    }

    pub async fn renew(&self, addr: &str, svc: &Service, sid: &str, seconds: u64) -> Result<(String, u64), SoapError> {
        let r = self
            .0
            .request(Method::from_bytes(b"SUBSCRIBE").unwrap(), format!("http://{addr}{}", svc.event))
            .header("SID", sid)
            .header("TIMEOUT", format!("Second-{seconds}"))
            .send()
            .await
            .map_err(|e| SoapError::Transport(e.to_string()))?;
        Self::subscription(r, seconds)
    }

    pub async fn unsubscribe(&self, addr: &str, svc: &Service, sid: &str) {
        let _ = self
            .0
            .request(Method::from_bytes(b"UNSUBSCRIBE").unwrap(), format!("http://{addr}{}", svc.event))
            .header("SID", sid)
            .send()
            .await;
    }

    fn subscription(r: reqwest::Response, asked: u64) -> Result<(String, u64), SoapError> {
        if !r.status().is_success() {
            return Err(SoapError::Transport(format!("HTTP {}", r.status())));
        }
        let header = |n: &str| r.headers().get(n).and_then(|v| v.to_str().ok()).map(String::from);
        let sid = header("SID").ok_or_else(|| SoapError::Transport("svaret havde intet SID".into()))?;
        let secs = header("TIMEOUT")
            .and_then(|t| t.trim().strip_prefix("Second-").and_then(|s| s.parse().ok()))
            .unwrap_or(asked);
        Ok((sid, secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        assert_eq!(with_port("10.0.0.5"), "10.0.0.5:1400");
        assert_eq!(with_port("10.0.0.5:1401"), "10.0.0.5:1401");
        assert_eq!(with_port("stue.lan"), "stue.lan:1400");
        assert_eq!(with_port("fe80::1"), "[fe80::1]:1400");
    }

    #[test]
    fn description() {
        let x = r#"<root><device><deviceType>urn:schemas-upnp-org:device:ZonePlayer:1</deviceType><roomName>Stue</roomName>
            <displayName>Era 100</displayName><modelName>Sonos Era 100</modelName><softwareVersion>80.1</softwareVersion>
            <UDN>uuid:RINCON_48A6B8000001400</UDN>
            <serviceList><service><serviceType>urn:schemas-upnp-org:service:AudioIn:1</serviceType></service></serviceList></device></root>"#;
        let d = parse_description(x).unwrap();
        assert_eq!(d.uuid, "RINCON_48A6B8000001400");
        assert_eq!(d.room, "Stue");
        assert_eq!(d.model.as_deref(), Some("Era 100"));
        assert!(d.line_in && !d.tv);
        assert!(parse_description("<root/>").is_none());
    }

    #[test]
    fn faults() {
        let e: Error = SoapError::Fault { code: 701, description: String::new() }.into();
        assert_eq!(e.kind, "failed");
        let e: Error = SoapError::Transport("x".into()).into();
        assert_eq!(e.kind, "unavailable");
    }
}
