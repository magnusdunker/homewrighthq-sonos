//! Just enough HTTP/1.1 to take the event notifications Sonos sends.

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

const MAX_BODY: usize = 1 << 20;

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// The next request on a connection, or `None` when the peer closed it.
pub async fn read_request<R: AsyncBufRead + Unpin>(r: &mut R) -> std::io::Result<Option<Request>> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
    let mut line = String::new();
    if r.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or_else(|| bad("tom forespørgsel"))?.to_string();
    let path = parts.next().ok_or_else(|| bad("ingen sti"))?.to_string();
    let mut headers = vec![];
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).await? == 0 {
            return Err(bad("forbindelsen lukkede midt i en forespørgsel"));
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut req = Request { method, path, headers, body: vec![] };
    let len: usize = req.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY {
        return Err(bad("for stor forespørgsel"));
    }
    req.body = vec![0; len];
    r.read_exact(&mut req.body).await?;
    Ok(Some(req))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn two_requests() {
        let raw = b"NOTIFY /a/b HTTP/1.1\r\nSID: uuid:x\r\nContent-Length: 4\r\n\r\nbodyGET / HTTP/1.1\r\n\r\n";
        let mut r = tokio::io::BufReader::new(&raw[..]);
        let a = read_request(&mut r).await.unwrap().unwrap();
        assert_eq!((a.method.as_str(), a.path.as_str(), a.header("sid")), ("NOTIFY", "/a/b", Some("uuid:x")));
        assert_eq!(a.body_text(), "body");
        let b = read_request(&mut r).await.unwrap().unwrap();
        assert_eq!(b.method, "GET");
        assert!(read_request(&mut r).await.unwrap().is_none());
    }
}
