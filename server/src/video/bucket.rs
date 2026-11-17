//! A bucket for joined videos, through S3's API: Cloudflare R2, Garage, S3... The server uploads,
//! lists and deletes there. The bucket stays private: Telegram downloads through links signed for
//! an hour, so the video never passes through the server or its front end, and nothing in the
//! bucket can be listed or linked to for long. Requests and links are signed with AWS
//! Signature Version 4, which is a few HMACs.

use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use url::Url;

use crate::extract::page::decode_entities;

const EXAMPLE: &str = "https://<account>.r2.cloudflarestorage.com/<bucket>";
/// How long a download link works: `/_/v/` signs one each time Telegram asks, and Telegram follows
/// it right away.
const LINK_VALID: Duration = Duration::from_secs(3600);

/// Where the bucket is and how to sign in, from `.env`.
#[derive(Clone, PartialEq, Eq)]
pub struct BucketConfig {
    /// The S3 API's origin, e.g. `https://<account>.r2.cloudflarestorage.com`. Telegram downloads
    /// from it too.
    pub api: Url,
    pub bucket: String,
    pub region: String,
    pub key_id: String,
    pub secret: String,
}

impl fmt::Debug for BucketConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BucketConfig")
            .field("api", &self.api.as_str())
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

impl BucketConfig {
    /// `VIDEO_BUCKET` and friends, or `None` when none are set.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let names = ["VIDEO_BUCKET", "VIDEO_BUCKET_KEY_ID", "VIDEO_BUCKET_SECRET"];
        let values: Vec<Option<String>> = names.iter().map(|name| get(name)).collect();
        if values.iter().all(Option::is_none) {
            if get("VIDEO_BUCKET_REGION").is_some() {
                bail!("VIDEO_BUCKET_REGION is set but VIDEO_BUCKET isn't; set the bucket's other variables too");
            }
            return Ok(None);
        }
        if let Some(missing) = names.iter().zip(&values).find(|(_, v)| v.is_none()).map(|(name, _)| name) {
            bail!("{missing} is missing: a bucket needs VIDEO_BUCKET, VIDEO_BUCKET_KEY_ID and VIDEO_BUCKET_SECRET");
        }
        let [Some(bucket), Some(key_id), Some(secret)] =
            <[Option<String>; 3]>::try_from(values).map_err(|_| anyhow!("three values"))?
        else {
            unreachable!("checked above")
        };

        let url = web_url(&bucket)
            .with_context(|| format!("VIDEO_BUCKET must be the S3 API address with the bucket, like {EXAMPLE}"))?;
        let segments: Vec<&str> =
            url.path_segments().map(|s| s.filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        let [name] = segments[..] else {
            bail!("VIDEO_BUCKET must end with the bucket's name, like {EXAMPLE}");
        };
        ensure!(url.query().is_none(), "VIDEO_BUCKET can't have a query (?...)");
        let api = Url::parse(&url.origin().ascii_serialization()).context("VIDEO_BUCKET")?;
        // R2 takes `auto`; Garage wants its `s3_region`.
        let region = get("VIDEO_BUCKET_REGION").unwrap_or_else(|| "auto".into());
        Ok(Some(Self { api, bucket: name.to_string(), region, key_id, secret }))
    }

    /// A link that downloads `key` for an hour, signed now. Only for GET: the method is signed.
    pub fn link(&self, key: &str) -> String {
        self.link_at(key, &amz_date(jiff::Timestamp::now()))
    }

    fn link_at(&self, key: &str, date: &str) -> String {
        let path = self.path(Some(key));
        let query = presigned_query(&Presign {
            key_id: &self.key_id,
            secret: &self.secret,
            region: &self.region,
            host: &self.host(),
            path: &path,
            date,
            expires: LINK_VALID,
        });
        format!("{}{path}?{query}", self.origin())
    }

    /// Whether Telegram can't reach the API: a loopback or private address, or `localhost`.
    pub fn is_private(&self) -> bool {
        match self.api.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local(),
            Some(url::Host::Domain(name)) => name == "localhost" || name.ends_with(".localhost"),
            None => true,
        }
    }

    /// The `Host` header requests are signed with.
    fn host(&self) -> String {
        let host = self.api.host_str().unwrap_or_default();
        match self.api.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        }
    }

    fn origin(&self) -> &str {
        self.api.as_str().trim_end_matches('/')
    }

    /// The encoded path of the bucket, or of a file in it.
    fn path(&self, key: Option<&str>) -> String {
        match key {
            Some(key) => canonical_path(&format!("/{}/{key}", self.bucket)),
            None => canonical_path(&format!("/{}", self.bucket)),
        }
    }
}

fn web_url(value: &str) -> Result<Url> {
    let url = Url::parse(value)?;
    ensure!(matches!(url.scheme(), "https" | "http") && url.host_str().is_some(), "not a web address");
    Ok(url)
}

/// A file in the bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub key: String,
    pub size: u64,
    /// When it was uploaded, in Unix seconds.
    pub modified: i64,
}

pub struct Bucket {
    config: BucketConfig,
    client: reqwest::Client,
}

impl Bucket {
    pub fn new(config: BucketConfig) -> Result<Self> {
        // One timeout for the whole exchange: reqwest's read timeout runs from sending until the
        // answer starts, so it would cut off the upload of a large video.
        let client = crate::net::client_builder()?
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { config, client })
    }

    pub const fn config(&self) -> &BucketConfig {
        &self.config
    }

    pub async fn put(&self, key: &str, body: Vec<u8>, content_type: &str) -> Result<()> {
        let extra = [("content-type", content_type), ("cache-control", "private, max-age=86400")];
        self.send("PUT", Some(key), &[], body, &extra).await.map(drop)
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        self.send("DELETE", Some(key), &[], Vec::new(), &[]).await.map(drop)
    }

    /// The files whose names start with `prefix`, so the bucket's other files aren't even read.
    pub async fn list(&self, prefix: &str) -> Result<Vec<Object>> {
        let mut objects = Vec::new();
        let mut token: Option<String> = None;
        // A million files at most, in pages of a thousand; a server that keeps saying there's
        // more without giving any can't keep this going either.
        for _ in 0..1000 {
            let mut query = vec![("list-type", "2".to_string()), ("max-keys", "1000".to_string())];
            if !prefix.is_empty() {
                query.push(("prefix", prefix.to_string()));
            }
            if let Some(token) = &token {
                query.push(("continuation-token", token.clone()));
            }
            let response = self.send("GET", None, &query, Vec::new(), &[]).await?;
            let xml = response
                .text()
                .await
                .map_err(|e| anyhow!("the bucket's list broke off: {}", crate::net::describe(e)))?;
            let (page, next) = parse_list(&xml)?;
            objects.extend(page);
            match next {
                Some(next) => token = Some(next),
                None => return Ok(objects),
            }
        }
        Ok(objects)
    }

    async fn send(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(&str, String)],
        mut body: Vec<u8>,
        extra: &[(&str, &str)],
    ) -> Result<reqwest::Response> {
        let c = &self.config;
        let path = c.path(key);
        let query_string = canonical_query(query);
        let mut url = format!("{}{path}", c.origin());
        if !query_string.is_empty() {
            url = format!("{url}?{query_string}");
        }
        let host = c.host();
        let payload = if body.len() < 64 * 1024 {
            hex(&Sha256::digest(&body))
        } else {
            // Hashing a whole video takes milliseconds; not on a thread other requests need.
            let hashed = tokio::task::spawn_blocking(move || {
                let payload = hex(&Sha256::digest(&body));
                (body, payload)
            });
            let payload;
            (body, payload) = hashed.await?;
            payload
        };
        let date = amz_date(jiff::Timestamp::now());
        let signed =
            [("host", host.as_str()), ("x-amz-content-sha256", payload.as_str()), ("x-amz-date", date.as_str())];
        let authorization = authorization(&Signing {
            key_id: &c.key_id,
            secret: &c.secret,
            region: &c.region,
            method,
            path: &path,
            query: &query_string,
            headers: &signed,
            payload: &payload,
            date: &date,
        });

        let method = reqwest::Method::from_bytes(method.as_bytes())?;
        let mut request = self
            .client
            .request(method, &url)
            .header("x-amz-content-sha256", &payload)
            .header("x-amz-date", &date)
            .header("authorization", authorization);
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let response = request
            .body(body)
            .send()
            .await
            .map_err(|e| {
                let why = crate::net::describe(e);
                anyhow!("the bucket at {} didn't answer: {why}; check that VIDEO_BUCKET is right and reachable from this server", c.api)
            })?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Err(refusal(status, &text))
    }
}

/// The bucket's error as one line, with what to change.
fn refusal(status: reqwest::StatusCode, xml: &str) -> anyhow::Error {
    let code = tag(xml, "Code").unwrap_or_default();
    let message = tag(xml, "Message").map(decode_entities).unwrap_or_default();
    let hint = match code {
        "InvalidAccessKeyId" => "check VIDEO_BUCKET_KEY_ID",
        "SignatureDoesNotMatch" => "check VIDEO_BUCKET_SECRET",
        "NoSuchBucket" => "check the bucket's name at the end of VIDEO_BUCKET",
        "AccessDenied" => "the key needs read and write access to this bucket (R2: \"Object Read & Write\")",
        "AuthorizationHeaderMalformed" => "set VIDEO_BUCKET_REGION to the bucket's region (Garage: its s3_region)",
        "RequestTimeTooSkewed" => "this server's clock is off; keep it in sync (NTP)",
        _ if status == reqwest::StatusCode::FORBIDDEN => "check the key's permissions",
        _ if status == reqwest::StatusCode::NOT_FOUND => {
            "VIDEO_BUCKET must be the bucket's S3 API address, not a public or custom-domain one"
        }
        _ if status.is_server_error() => "the bucket's service had trouble; it's tried again 10 minutes later",
        _ => "",
    };
    let detail = [code, message.trim()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(": ");
    let mut line = format!("the bucket answered {status}");
    if !detail.is_empty() {
        line = format!("{line} ({detail})");
    }
    if !hint.is_empty() {
        line = format!("{line}; {hint}");
    }
    anyhow!(line)
}

/// One page of ListObjectsV2, and the token for the next.
fn parse_list(xml: &str) -> Result<(Vec<Object>, Option<String>)> {
    ensure!(xml.contains("<ListBucketResult"), "the bucket's list isn't a ListBucketResult");
    let mut objects = Vec::new();
    for part in xml.split("<Contents>").skip(1) {
        let part = part.split("</Contents>").next().unwrap_or_default();
        let key = tag(part, "Key").map(decode_entities).context("a listed file has no key")?;
        let size = tag(part, "Size").and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let modified = tag(part, "LastModified")
            .and_then(|t| t.trim().parse::<jiff::Timestamp>().ok())
            .map_or(0, jiff::Timestamp::as_second);
        objects.push(Object { key, size, modified });
    }
    let truncated = tag(xml, "IsTruncated").is_some_and(|t| t.trim() == "true");
    let next = tag(xml, "NextContinuationToken").map(decode_entities).filter(|_| truncated);
    Ok((objects, next))
}

/// The text inside the first `<name>…</name>`.
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let start = xml.find(&format!("<{name}>"))? + name.len() + 2;
    let end = xml[start..].find(&format!("</{name}>"))?;
    Some(&xml[start..start + end])
}

// ---- Signature Version 4 ----

/// What goes into a request's signature.
struct Signing<'a> {
    key_id: &'a str,
    secret: &'a str,
    region: &'a str,
    method: &'a str,
    /// Already encoded by [`canonical_path`].
    path: &'a str,
    /// Already encoded by [`canonical_query`].
    query: &'a str,
    /// Lowercase names; every header listed here is signed.
    headers: &'a [(&'a str, &'a str)],
    /// Hex SHA-256 of the body.
    payload: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    date: &'a str,
}

/// `YYYYMMDDTHHMMSSZ`, as signatures want it.
fn amz_date(at: jiff::Timestamp) -> String {
    at.strftime("%Y%m%dT%H%M%SZ").to_string()
}

/// `day/region/s3/aws4_request`.
fn scope(date: &str, region: &str) -> String {
    format!("{}/{region}/s3/aws4_request", &date[..8])
}

/// The signed header names, and the signature.
fn signature(s: &Signing<'_>) -> (String, String) {
    let mut headers = s.headers.to_vec();
    headers.sort_by_key(|h| h.0);
    let mut canonical_headers = String::new();
    for (name, value) in &headers {
        canonical_headers.extend([name, ":", value.trim(), "\n"]);
    }
    let signed_headers = headers.iter().map(|h| h.0).collect::<Vec<_>>().join(";");
    let request =
        format!("{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}", s.method, s.path, s.query, s.payload);
    let scope = scope(s.date, s.region);
    let to_sign = format!("AWS4-HMAC-SHA256\n{}\n{scope}\n{}", s.date, hex(&Sha256::digest(request.as_bytes())));
    let mut key = hmac(format!("AWS4{}", s.secret).as_bytes(), &s.date.as_bytes()[..8]);
    for part in [s.region, "s3", "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    (signed_headers, hex(&hmac(&key, to_sign.as_bytes())))
}

fn authorization(s: &Signing<'_>) -> String {
    let (signed_headers, signature) = signature(s);
    let scope = scope(s.date, s.region);
    format!("AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={signed_headers},Signature={signature}", s.key_id)
}

/// What goes into a download link.
struct Presign<'a> {
    key_id: &'a str,
    secret: &'a str,
    region: &'a str,
    host: &'a str,
    /// Already encoded by [`canonical_path`].
    path: &'a str,
    date: &'a str,
    expires: Duration,
}

/// The query of a link that GETs `path` until `expires` after `date`, without other headers.
fn presigned_query(p: &Presign<'_>) -> String {
    let params = [
        ("X-Amz-Algorithm", "AWS4-HMAC-SHA256".to_string()),
        ("X-Amz-Credential", format!("{}/{}", p.key_id, scope(p.date, p.region))),
        ("X-Amz-Date", p.date.to_string()),
        ("X-Amz-Expires", p.expires.as_secs().to_string()),
        ("X-Amz-SignedHeaders", "host".to_string()),
    ];
    let query = canonical_query(&params);
    let (_, signature) = signature(&Signing {
        key_id: p.key_id,
        secret: p.secret,
        region: p.region,
        method: "GET",
        path: p.path,
        query: &query,
        headers: &[("host", p.host)],
        payload: "UNSIGNED-PAYLOAD",
        date: p.date,
    });
    format!("{query}&X-Amz-Signature={signature}")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(bytes)
}

/// Percent-encodes everything but S3's unreserved characters.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            data_encoding::HEXUPPER.encode_append(&[byte], &mut out);
        }
    }
    out
}

fn canonical_path(path: &str) -> String {
    path.split('/').map(encode).collect::<Vec<_>>().join("/")
}

fn canonical_query(query: &[(&str, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query.iter().map(|(k, v)| (encode(k), encode(v))).collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

#[cfg(test)]
pub mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::{Path, Query, State};
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;

    use super::*;

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    fn sign(method: &str, path: &str, query: &[(&str, String)], headers: &[(&str, &str)], payload: &str) -> String {
        authorization(&Signing {
            key_id: KEY_ID,
            secret: SECRET,
            region: "us-east-1",
            method,
            path: &canonical_path(path),
            query: &canonical_query(query),
            headers,
            payload,
            date: "20130524T000000Z",
        })
    }

    /// The examples in AWS's "Signature Calculations for the Authorization Header" for S3.
    #[test]
    fn signs_like_aws_examples() {
        let host = ("host", "examplebucket.s3.amazonaws.com");
        let date = ("x-amz-date", "20130524T000000Z");
        let get_object = sign(
            "GET",
            "/test.txt",
            &[],
            &[host, ("range", "bytes=0-9"), ("x-amz-content-sha256", EMPTY), date],
            EMPTY,
        );
        assert!(get_object.ends_with("Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"));
        assert!(get_object.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,"
        ));

        let body = "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072";
        let put_object = sign(
            "PUT",
            "/test$file.text",
            &[],
            &[
                ("date", "Fri, 24 May 2013 00:00:00 GMT"),
                host,
                ("x-amz-content-sha256", body),
                date,
                ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ],
            body,
        );
        assert!(put_object.ends_with("Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"));

        let list = sign(
            "GET",
            "/",
            &[("prefix", "J".into()), ("max-keys", "2".into())],
            &[host, ("x-amz-content-sha256", EMPTY), date],
            EMPTY,
        );
        assert!(list.ends_with("Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"));
    }

    /// AWS's example in "Authenticating Requests: Using Query Parameters".
    #[test]
    fn signs_links_like_the_aws_example() {
        let query = presigned_query(&Presign {
            key_id: KEY_ID,
            secret: SECRET,
            region: "us-east-1",
            host: "examplebucket.s3.amazonaws.com",
            path: "/test.txt",
            date: "20130524T000000Z",
            expires: Duration::from_secs(86_400),
        });
        assert_eq!(
            query,
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
             &X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host\
             &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        );
    }

    fn config(vars: &[(&str, &str)]) -> Result<Option<BucketConfig>> {
        let vars: HashMap<&str, &str> = vars.iter().copied().collect();
        BucketConfig::from_lookup(|name| vars.get(name).map(|v| (*v).to_string()))
    }

    const R2: [(&str, &str); 3] = [
        ("VIDEO_BUCKET", "https://0123abcd.r2.cloudflarestorage.com/videos"),
        ("VIDEO_BUCKET_KEY_ID", "id"),
        ("VIDEO_BUCKET_SECRET", "very secret"),
    ];

    #[test]
    fn reads_the_bucket_from_env() {
        assert!(config(&[]).unwrap().is_none());
        let c = config(&R2).unwrap().unwrap();
        assert_eq!(
            (c.api.as_str(), c.bucket.as_str(), c.region.as_str()),
            ("https://0123abcd.r2.cloudflarestorage.com/", "videos", "auto")
        );
        // The signing itself is checked against AWS's examples above.
        let link = c.link_at("a-0.mp4", "20261116T000000Z");
        assert!(link.starts_with("https://0123abcd.r2.cloudflarestorage.com/videos/a-0.mp4?X-Amz-"), "{link}");
        assert!(!c.is_private());
        assert!(!format!("{c:?}").contains("very secret"), "the secret stays out of logs");

        let mut garage = R2.to_vec();
        garage[0].1 = "http://127.0.0.1:3900/videos/";
        garage.push(("VIDEO_BUCKET_REGION", "garage"));
        let c = config(&garage).unwrap().unwrap();
        assert_eq!((c.region.as_str(), c.bucket.as_str()), ("garage", "videos"));
        assert!(c.link_at("a-0.mp4", "20261116T000000Z").starts_with("http://127.0.0.1:3900/videos/a-0.mp4?"));
        assert!(c.is_private(), "Telegram can't reach it");

        let error = |vars: &[(&str, &str)]| format!("{:#}", config(vars).unwrap_err());
        assert!(error(&R2[..2]).contains("VIDEO_BUCKET_SECRET is missing"));
        for (address, message) in [
            ("https://0123abcd.r2.cloudflarestorage.com", "must end with the bucket's name"),
            ("https://0123abcd.r2.cloudflarestorage.com/videos/more", "must end with the bucket's name"),
            ("ftp://0123abcd.r2.cloudflarestorage.com/videos", "S3 API address"),
        ] {
            let mut vars = R2;
            vars[0].1 = address;
            assert!(error(&vars).contains(message), "{address}");
        }
        assert!(error(&[("VIDEO_BUCKET_REGION", "auto")]).contains("VIDEO_BUCKET isn't"));
    }

    #[test]
    fn reads_lists_and_errors() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>videos</Name><IsTruncated>true</IsTruncated>
<Contents><Key>0123456789abcdef-0.mp4</Key><LastModified>2026-11-16T02:24:20.000Z</LastModified><Size>3393773</Size></Contents>
<Contents><Key>a&amp;b.txt</Key><LastModified>bad</LastModified><Size>1</Size></Contents>
<NextContinuationToken>1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=</NextContinuationToken></ListBucketResult>"#;
        let (objects, next) = parse_list(xml).unwrap();
        assert_eq!(
            objects[0],
            Object { key: "0123456789abcdef-0.mp4".into(), size: 3_393_773, modified: 1_794_795_860 }
        );
        assert_eq!((objects[1].key.as_str(), objects[1].modified), ("a&b.txt", 0));
        assert_eq!(next.as_deref(), Some("1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM="));
        assert!(parse_list(&xml.replace("<IsTruncated>true", "<IsTruncated>false")).unwrap().1.is_none());
        assert!(parse_list("<html>").is_err());

        let denied = "<Error><Code>SignatureDoesNotMatch</Code><Message>The request signature we calculated does not match</Message></Error>";
        let line = refusal(StatusCode::FORBIDDEN, denied).to_string();
        assert_eq!(
            line,
            "the bucket answered 403 Forbidden (SignatureDoesNotMatch: The request signature we calculated does not match); check VIDEO_BUCKET_SECRET"
        );
        assert!(refusal(StatusCode::BAD_GATEWAY, "").to_string().contains("tried again 10 minutes later"));
        let skewed = "<Error><Code>RequestTimeTooSkewed</Code></Error>";
        assert!(refusal(StatusCode::FORBIDDEN, skewed).to_string().contains("clock is off"));
        assert!(refusal(StatusCode::NOT_FOUND, "<html>").to_string().contains("S3 API address"));
    }

    /// A stored file: body, content type, uploaded at.
    pub type File = (Vec<u8>, String, i64);

    /// A small S3 server that checks signatures and download links, with the bucket `videos`
    /// under `/s3`.
    #[derive(Clone, Default)]
    pub struct FakeS3 {
        pub files: Arc<Mutex<BTreeMap<String, File>>>,
        /// Answer every request with this status, as a broken bucket would.
        pub refuse: Arc<Mutex<Option<StatusCode>>>,
        listed: Arc<AtomicUsize>,
    }

    impl FakeS3 {
        pub async fn start() -> (Self, BucketConfig) {
            let fake = Self::default();
            let app = Router::new()
                .route("/s3/videos", get(Self::api))
                .route("/s3/videos/{*key}", get(Self::api).put(Self::api).delete(Self::api))
                .with_state(fake.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr: SocketAddr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let vars = [
                ("VIDEO_BUCKET", format!("http://{addr}/videos")),
                ("VIDEO_BUCKET_KEY_ID", "id".into()),
                ("VIDEO_BUCKET_SECRET", "secret".into()),
            ];
            let mut config = BucketConfig::from_lookup(|name| vars.iter().find(|v| v.0 == name).map(|v| v.1.clone()))
                .unwrap()
                .unwrap();
            // The API lives under /s3 here.
            config.api = Url::parse(&format!("http://{addr}/s3")).unwrap();
            (fake, config)
        }

        async fn api(
            State(fake): State<Self>,
            method: Method,
            uri: Uri,
            key: Option<Path<String>>,
            Query(query): Query<Vec<(String, String)>>,
            headers: HeaderMap,
            body: Bytes,
        ) -> Response {
            if let Some(status) = *fake.refuse.lock().unwrap() {
                return (status, "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>")
                    .into_response();
            }
            let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            if query.iter().any(|(k, _)| k == "X-Amz-Signature") {
                return fake.download(&uri, &query, &header("host"), key.map(|k| k.0));
            }
            let (host, payload, date) = (header("host"), header("x-amz-content-sha256"), header("x-amz-date"));
            assert_eq!(payload, hex(&Sha256::digest(&body)), "the body is what was signed");
            let query: Vec<(&str, String)> = query.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
            let path = uri.path().strip_prefix("/s3").unwrap();
            let expected = authorization(&Signing {
                key_id: "id",
                secret: "secret",
                region: "auto",
                method: method.as_str(),
                path,
                query: &canonical_query(&query),
                headers: &[("host", &host), ("x-amz-content-sha256", &payload), ("x-amz-date", &date)],
                payload: &payload,
                date: &date,
            });
            if header("authorization") != expected {
                return (StatusCode::FORBIDDEN, "<Error><Code>SignatureDoesNotMatch</Code></Error>").into_response();
            }
            let mut files = fake.files.lock().unwrap();
            match (method, key) {
                (Method::PUT, Some(Path(key))) => {
                    let at = jiff::Timestamp::now().as_second();
                    files.insert(key, (body.to_vec(), header("content-type"), at));
                    StatusCode::OK.into_response()
                }
                (Method::DELETE, Some(Path(key))) => {
                    files.remove(&key);
                    StatusCode::NO_CONTENT.into_response()
                }
                (Method::GET, None) => {
                    fake.listed.fetch_add(1, Ordering::Relaxed);
                    let prefix = query.iter().find(|(k, _)| *k == "prefix").map_or("", |(_, v)| v.as_str());
                    let contents: String = files
                        .iter()
                        .filter(|(key, _)| key.starts_with(prefix))
                        .map(|(key, (body, _, at))| {
                            let at = jiff::Timestamp::from_second(*at).unwrap();
                            format!(
                                "<Contents><Key>{key}</Key><LastModified>{at}</LastModified><Size>{}</Size></Contents>",
                                body.len()
                            )
                        })
                        .collect();
                    format!("<ListBucketResult><IsTruncated>false</IsTruncated>{contents}</ListBucketResult>")
                        .into_response()
                }
                _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
            }
        }

        /// A GET through a download link: signed for this file and not expired.
        fn download(&self, uri: &Uri, query: &[(String, String)], host: &str, key: Option<String>) -> Response {
            let param = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or_default();
            let (date, expires) = (param("X-Amz-Date"), param("X-Amz-Expires").parse::<u64>().unwrap_or(0));
            let path = uri.path().strip_prefix("/s3").unwrap();
            let expected = presigned_query(&Presign {
                key_id: "id",
                secret: "secret",
                region: "auto",
                host,
                path,
                date,
                expires: Duration::from_secs(expires),
            });
            let signed_at = jiff::civil::DateTime::strptime("%Y%m%dT%H%M%SZ", date)
                .ok()
                .and_then(|t| t.to_zoned_datetime(jiff::tz::TimeZone::UTC).ok())
                .map_or(0, |t| t.timestamp().as_second());
            let expired = signed_at + i64::try_from(expires).unwrap() < jiff::Timestamp::now().as_second();
            if uri.query() != Some(expected.as_str()) || expired {
                return (StatusCode::FORBIDDEN, "<Error><Code>AccessDenied</Code></Error>").into_response();
            }
            match key.and_then(|key| self.files.lock().unwrap().get(&key).cloned()) {
                Some((body, kind, _)) => ([("content-type", kind)], body).into_response(),
                None => (StatusCode::NOT_FOUND, "<Error><Code>NoSuchKey</Code></Error>").into_response(),
            }
        }

        /// How many times the bucket was listed.
        pub fn lists(&self) -> usize {
            self.listed.load(Ordering::Relaxed)
        }
    }

    #[tokio::test]
    async fn uploads_lists_and_deletes() {
        let (fake, config) = FakeS3::start().await;
        let bucket = Bucket::new(config.clone()).unwrap();
        bucket.put("a b+c.txt", b"hello".to_vec(), "text/plain").await.unwrap();
        // Big enough to be hashed off the async threads.
        bucket.put("facebook/0123456789abcdef-0.mp4", vec![1; 200_000], "video/mp4").await.unwrap();
        let listed = bucket.list("").await.unwrap();
        assert_eq!(
            listed.iter().map(|o| (o.key.as_str(), o.size)).collect::<Vec<_>>(),
            [("a b+c.txt", 5), ("facebook/0123456789abcdef-0.mp4", 200_000)]
        );
        assert!(listed[1].modified > 0);
        assert_eq!(fake.files.lock().unwrap()["facebook/0123456789abcdef-0.mp4"].1, "video/mp4");
        let prefixed = bucket.list("facebook/").await.unwrap();
        assert_eq!(prefixed.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["facebook/0123456789abcdef-0.mp4"]);
        bucket.delete("a b+c.txt").await.unwrap();
        assert_eq!(bucket.list("").await.unwrap().len(), 1);

        // Download links work for that file, and only until they expire.
        let key = "facebook/0123456789abcdef-0.mp4";
        let fetch = |link: String| async move { reqwest::get(link).await.unwrap() };
        let ok = fetch(config.link(key)).await;
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(ok.bytes().await.unwrap().len(), 200_000);
        assert_eq!(fetch(config.link_at(key, "20200101T000000Z")).await.status(), StatusCode::FORBIDDEN, "expired");
        let other = config.link(key).replace("-0.mp4", "-1.mp4");
        assert_eq!(fetch(other).await.status(), StatusCode::FORBIDDEN, "signed for another file");

        let wrong = Bucket::new(BucketConfig { secret: "nope".into(), ..config }).unwrap();
        let error = wrong.put("x.txt", Vec::new(), "text/plain").await.unwrap_err().to_string();
        assert!(error.contains("403") && error.contains("VIDEO_BUCKET_SECRET"), "{error}");
    }
}
