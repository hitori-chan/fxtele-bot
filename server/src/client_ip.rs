//! The client's IP: the connection's own address, or a header set by a trusted front end.

use std::net::IpAddr;
use std::str::FromStr;

use anyhow::bail;
use axum::http::HeaderMap;
use ipnet::IpNet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientIpFrom {
    Peer,
    CfConnectingIp,
    XForwardedFor,
    XRealIp,
}

impl FromStr for ClientIpFrom {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> anyhow::Result<Self> {
        Ok(match value.to_ascii_lowercase().as_str() {
            "peer" => Self::Peer,
            "cf-connecting-ip" => Self::CfConnectingIp,
            "x-forwarded-for" => Self::XForwardedFor,
            "x-real-ip" => Self::XRealIp,
            _ => bail!("CLIENT_IP_FROM={value:?}: use peer, cf-connecting-ip, x-forwarded-for or x-real-ip"),
        })
    }
}

impl ClientIpFrom {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Peer => "peer",
            Self::CfConnectingIp => "cf-connecting-ip",
            Self::XForwardedFor => "x-forwarded-for",
            Self::XRealIp => "x-real-ip",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientIp {
    pub ip: IpAddr,
    /// The connection came from a trusted proxy and no usable header named the client,
    /// so `ip` is the proxy's own address.
    pub is_proxy: bool,
}

#[derive(Debug, Clone)]
pub struct Resolver {
    from: ClientIpFrom,
    trusted: Vec<IpNet>,
}

impl Resolver {
    pub const fn new(from: ClientIpFrom, trusted: Vec<IpNet>) -> Self {
        Self { from, trusted }
    }

    pub const fn source(&self) -> ClientIpFrom {
        self.from
    }

    fn is_trusted(&self, ip: IpAddr) -> bool {
        self.trusted.iter().any(|net| net.contains(&ip))
    }

    pub fn resolve(&self, peer: IpAddr, headers: &HeaderMap) -> ClientIp {
        let peer = peer.to_canonical();
        let peer_trusted = self.is_trusted(peer);
        if !peer_trusted {
            return ClientIp { ip: peer, is_proxy: false };
        }
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).and_then(parse_ip);
        let from_header = match self.from {
            ClientIpFrom::Peer => None,
            ClientIpFrom::CfConnectingIp => header("cf-connecting-ip"),
            ClientIpFrom::XRealIp => header("x-real-ip"),
            ClientIpFrom::XForwardedFor => self.forwarded_for(headers),
        };
        match from_header {
            Some(ip) => ClientIp { ip, is_proxy: false },
            None => ClientIp { ip: peer, is_proxy: true },
        }
    }

    /// The right-most address that isn't a trusted proxy; if all are, the left-most.
    fn forwarded_for(&self, headers: &HeaderMap) -> Option<IpAddr> {
        let hops: Vec<IpAddr> = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(parse_ip)
            .collect::<Option<Vec<_>>>()?;
        hops.iter().rev().find(|ip| !self.is_trusted(**ip)).or_else(|| hops.first()).copied()
    }
}

fn parse_ip(value: &str) -> Option<IpAddr> {
    let value = value.trim();
    // Some proxies add a port: "1.2.3.4:5678" or "[2001:db8::1]:443".
    let value = if let Some(rest) = value.strip_prefix('[') {
        rest.split_once(']').map_or(rest, |(ip, _)| ip)
    } else if value.matches(':').count() == 1 {
        value.split_once(':').map_or(value, |(ip, _)| ip)
    } else {
        value
    };
    value.parse::<IpAddr>().ok().map(|ip| ip.to_canonical())
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;
    use crate::config::DEFAULT_TRUSTED_PROXIES;

    fn resolver(from: ClientIpFrom) -> Resolver {
        Resolver::new(from, DEFAULT_TRUSTED_PROXIES.iter().map(|s| s.parse().unwrap()).collect())
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn peer_ignores_headers() {
        let r = resolver(ClientIpFrom::Peer);
        let h = headers(&[("cf-connecting-ip", "1.1.1.1"), ("x-forwarded-for", "1.1.1.1")]);
        assert_eq!(r.resolve(ip("149.154.161.1"), &h), ClientIp { ip: ip("149.154.161.1"), is_proxy: false });
        assert_eq!(r.resolve(ip("127.0.0.1"), &h), ClientIp { ip: ip("127.0.0.1"), is_proxy: true });
    }

    #[test]
    fn headers_only_from_trusted_proxies() {
        let r = resolver(ClientIpFrom::CfConnectingIp);
        let h = headers(&[("cf-connecting-ip", "149.154.161.1")]);
        assert_eq!(r.resolve(ip("127.0.0.1"), &h).ip, ip("149.154.161.1"));
        // An outsider can't forge it.
        assert_eq!(r.resolve(ip("203.0.113.9"), &h).ip, ip("203.0.113.9"));
        // Missing or garbage header: the proxy's address, flagged.
        assert!(r.resolve(ip("127.0.0.1"), &HeaderMap::new()).is_proxy);
        assert!(r.resolve(ip("127.0.0.1"), &headers(&[("cf-connecting-ip", "nope")])).is_proxy);
    }

    #[test]
    fn x_real_ip_and_ports() {
        let r = resolver(ClientIpFrom::XRealIp);
        assert_eq!(r.resolve(ip("10.0.0.2"), &headers(&[("x-real-ip", "1.2.3.4:5678")])).ip, ip("1.2.3.4"));
        assert_eq!(r.resolve(ip("10.0.0.2"), &headers(&[("x-real-ip", "[2001:db8::1]:443")])).ip, ip("2001:db8::1"));
        assert_eq!(r.resolve(ip("10.0.0.2"), &headers(&[("x-real-ip", "2001:db8::1")])).ip, ip("2001:db8::1"));
    }

    #[test]
    fn forwarded_for_takes_rightmost_untrusted() {
        let r = resolver(ClientIpFrom::XForwardedFor);
        let h = headers(&[("x-forwarded-for", "6.6.6.6, 149.154.161.1, 10.0.0.5")]);
        assert_eq!(r.resolve(ip("127.0.0.1"), &h).ip, ip("149.154.161.1"));
        // Split across several header lines.
        let h = headers(&[("x-forwarded-for", "6.6.6.6"), ("x-forwarded-for", "149.154.161.1")]);
        assert_eq!(r.resolve(ip("127.0.0.1"), &h).ip, ip("149.154.161.1"));
        // All trusted: the left-most.
        let h = headers(&[("x-forwarded-for", "192.168.1.9, 10.0.0.5")]);
        assert_eq!(r.resolve(ip("127.0.0.1"), &h).ip, ip("192.168.1.9"));
        // A malformed list isn't trusted at all.
        let h = headers(&[("x-forwarded-for", "6.6.6.6, garbage")]);
        assert!(r.resolve(ip("127.0.0.1"), &h).is_proxy);
    }

    #[test]
    fn mapped_ipv4_is_canonical() {
        let r = resolver(ClientIpFrom::Peer);
        assert_eq!(r.resolve(ip("::ffff:149.154.161.1"), &HeaderMap::new()).ip, ip("149.154.161.1"));
        assert!(r.resolve(ip("::ffff:127.0.0.1"), &HeaderMap::new()).is_proxy);
    }
}
