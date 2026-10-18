//! The HTTP listener Sonos sends its event notifications (GENA NOTIFY) to.
//! The callback path says who the event is for: `/<player uuid>/<service>`.

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;

use crate::http;

/// (player uuid, service, body)
pub type Event = (String, String, String);

pub struct Listener {
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Listener {
    /// Listen on `port` (0 picks one) on every interface.
    pub async fn start(port: u16, events: UnboundedSender<Event>) -> std::io::Result<Self> {
        let l = TcpListener::bind(("0.0.0.0", port)).await?;
        let port = l.local_addr()?.port();
        let task = tokio::spawn(async move {
            while let Ok((conn, _)) = l.accept().await {
                let events = events.clone();
                tokio::spawn(async move {
                    let (r, mut w) = conn.into_split();
                    let mut r = BufReader::new(r);
                    while let Ok(Some(req)) = http::read_request(&mut r).await {
                        let ok = req.method.eq_ignore_ascii_case("NOTIFY");
                        let status = if ok { "200 OK" } else { "405 Method Not Allowed" };
                        if ok {
                            let mut parts = req.path.trim_start_matches('/').splitn(2, '/');
                            if let (Some(uuid), Some(service)) = (parts.next(), parts.next()) {
                                let _ = events.send((uuid.to_string(), service.to_string(), req.body_text()));
                            }
                        }
                        let reply = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n");
                        if w.write_all(reply.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        Ok(Self { port, task })
    }
}
