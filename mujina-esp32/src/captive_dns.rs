//! Pure RFC 1035 captive-portal DNS responder and HTTP header parser (host-testable).

/// ANS TTL matching ESP-Miner-NerdQAxePlus (`ANS_TTL_SEC = 300`).
const ANS_TTL_BE: [u8; 4] = 300u32.to_be_bytes();

/// Build a DNS reply pointing any `A` (`1`) or `ANY` (`255`) query to `ap_ip`,
/// while returning `ANCOUNT = 0` (NODATA) for `AAAA` (`28`), `HTTPS` (`65`), and
/// other record types so phones immediately use the IPv4 `A` answer.
pub fn build_captive_dns_reply(query: &[u8], ap_ip: [u8; 4]) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    // Reject packets that are already responses (QR=1) or have QDCOUNT == 0.
    if (query[2] & 0x80) != 0 || (query[4] == 0 && query[5] == 0) {
        return None;
    }

    let mut pos = 12usize;
    while pos < query.len() {
        let len = query[pos] as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if (len & 0xC0) != 0 || pos + 1 + len >= query.len() {
            return None;
        }
        pos += 1 + len;
    }
    if pos + 4 > query.len() {
        return None;
    }
    let q_end = pos + 4;
    let qtype = u16::from_be_bytes([query[pos], query[pos + 1]]);
    let want_a = qtype == 1 || qtype == 255;

    let mut resp = Vec::with_capacity(q_end + if want_a { 16 } else { 0 });
    // Header: preserve client ID and RD bit, set QR=1, AA=1, RA=1, QDCOUNT=1, ARCOUNT=0
    resp.extend_from_slice(&query[0..2]);
    resp.push(0x84 | (query[2] & 0x01));
    resp.push(0x80);
    resp.extend_from_slice(&[0x00, 0x01]);
    resp.extend_from_slice(&[0x00, if want_a { 0x01 } else { 0x00 }]);
    resp.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    resp.extend_from_slice(&query[12..q_end]);

    if want_a {
        resp.extend_from_slice(&[
            0xC0, 0x0C, // Name pointer to offset 12
            0x00, 0x01, // TYPE = A
            0x00, 0x01, // CLASS = IN
        ]);
        resp.extend_from_slice(&ANS_TTL_BE);
        resp.extend_from_slice(&[
            0x00, 0x04, // RDLENGTH = 4
            ap_ip[0], ap_ip[1], ap_ip[2], ap_ip[3],
        ]);
    }

    Some(resp)
}

/// Parsed HTTP/1.x request head supporting large (up to 8 KiB) smartphone headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHttpHead {
    pub method: String,
    pub path: String,
    pub query: String,
    pub host: String,
    pub content_length: usize,
    pub body_offset: usize,
}

/// Find `\r\n\r\n` and parse request line, `Host`, and `Content-Length`.
pub fn parse_http_head(buf: &[u8]) -> Option<ParsedHttpHead> {
    let hdr_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head_str = core::str::from_utf8(&buf[..hdr_end]).ok()?;
    let mut lines = head_str.split("\r\n");
    let req_line = lines.next()?;
    let mut parts = req_line.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    let raw_target = parts.next()?;

    let (path, query) = match raw_target.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (raw_target.to_owned(), String::new()),
    };

    let mut host = String::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            if key.eq_ignore_ascii_case("host") {
                host = val.to_ascii_lowercase();
            } else if key.eq_ignore_ascii_case("content-length") {
                content_length = val.parse().unwrap_or(0);
            }
        }
    }

    Some(ParsedHttpHead {
        method,
        path,
        query,
        host,
        content_length,
        body_offset: hdr_end + 4,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_query(qtype: u16, id: [u8; 2], rd: bool) -> Vec<u8> {
        let mut q = vec![
            id[0],
            id[1],
            if rd { 0x01 } else { 0x00 },
            0x00,
            0x00,
            0x01,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ];
        for label in [b"captive".as_slice(), b"apple".as_slice(), b"com".as_slice()] {
            q.push(label.len() as u8);
            q.extend_from_slice(label);
        }
        q.push(0x00);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&[0x00, 0x01]);
        q
    }

    #[test]
    fn captive_dns_a_query_preserves_id_and_rd_and_returns_ap_ip() {
        let mut q = sample_query(1, [0xAB, 0xCD], true);
        q.extend_from_slice(&[0x00, 0x00, 0x29, 0x05, 0xB4, 0x00, 0x00, 0x00, 0x00]);
        let r = build_captive_dns_reply(&q, [192, 168, 71, 1]).expect("valid A reply");
        assert_eq!(&r[0..4], &[0xAB, 0xCD, 0x85, 0x80]);
        assert_eq!(&r[4..8], &[0x00, 0x01, 0x00, 0x01]);
        assert_eq!(&r[8..12], &[0x00, 0x00, 0x00, 0x00]);
        assert_eq!(&r[r.len() - 4..], &[192, 168, 71, 1]);
    }

    #[test]
    fn captive_dns_aaaa_and_https_return_nodata() {
        for qtype in [28u16, 65u16] {
            let q = sample_query(qtype, [0x12, 0x34], false);
            let r = build_captive_dns_reply(&q, [192, 168, 71, 1]).expect("valid NODATA reply");
            assert_eq!(r[2], 0x84); // RD=0 preserved
            assert_eq!(&r[6..8], &[0x00, 0x00]); // ANCOUNT=0
            assert_eq!(r.len(), q.len());
        }
    }

    #[test]
    fn parse_http_head_handles_large_mobile_headers() {
        let ua = "Mozilla/5.0 ".repeat(90); // > 1080 bytes, far above ESP-IDF's 512-byte cap
        let raw = format!(
            "POST /api/v0/setup?src=portal HTTP/1.1\r\n\
             Host: 192.168.71.1\r\n\
             User-Agent: {ua}\r\n\
             Content-Length: 42\r\n\r\nwifi_ssid=MyNet"
        );
        let parsed = parse_http_head(raw.as_bytes()).expect("should parse >1KB headers");
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.path, "/api/v0/setup");
        assert_eq!(parsed.query, "src=portal");
        assert_eq!(parsed.host, "192.168.71.1");
        assert_eq!(parsed.content_length, 42);
        assert_eq!(&raw.as_bytes()[parsed.body_offset..], b"wifi_ssid=MyNet");
    }
}
